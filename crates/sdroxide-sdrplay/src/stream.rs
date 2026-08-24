//! The session thread that owns an RSP, and the callbacks the service feeds.
//!
//! # Ownership and threads
//!
//! Unlike the USB backends there is no endpoint to pump: after
//! `sdrplay_api_Init` the *service* pushes samples into [`stream_cb`] on a
//! thread it owns. The session thread here exists to own everything else —
//! device selection, the parameter block, every `sdrplay_api_Update` — so all
//! control stays on one thread, the same invariant the RX-888 backend keeps
//! for its USB handle.
//!
//! Two rules the safety of this module rests on:
//!
//! * The `DeviceParamsT` pointer from `GetDeviceParams` refers to storage the
//!   service owns, valid from `SelectDevice` to `ReleaseDevice`. It is
//!   dereferenced **only on the session thread** — never in a callback.
//! * The callback context is a `Box` leaked before `Init` and reclaimed only
//!   **after `Uninit` returns**, which is the API's guarantee that no
//!   callback is still running or will run again.
//!
//! Callbacks arrive on a foreign thread, so they must never unwind into the
//! service: every callback body is wrapped in `catch_unwind`, and a panic
//! marks the session dead (the engine reopens it) instead of corrupting the
//! service's stack.
//!
//! # Two tuners
//!
//! An RSPduo asked for diversity runs in the API's dual-tuner mode, where both
//! stream callbacks fire and each carries one tuner. Everything below then
//! happens twice: two parameter blocks are configured, and every
//! `sdrplay_api_Update` is issued once per tuner, because the call names the
//! tuner it applies to and `Tuner_Both` is a selection, not an addressee. The
//! samples are put back together in [`crate::pair`] before they reach the
//! ring.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use rtrb::{Consumer, Producer};
use sdroxide_types::{SdrPlayConfig, SdrPlayDuoTuner, SdrPlayModel};

use crate::api;
use crate::device;
use crate::error::{Error, Result};
use crate::ffi;
use crate::handle::{Ctrl, Pending, RxStats, SdrPlayHandle, Shared, push_iq, ring_for};
use crate::pair::{Pairer, QUAD, Side};

/// 16-bit wire samples to full-scale ±1.0.
const SCALE: f32 = 1.0 / 32768.0;

/// How long the session loop waits for control before housekeeping (overload
/// acknowledgements) runs anyway.
const CTRL_TIMEOUT: Duration = Duration::from_millis(100);

/// The tuner's own range: 1 kHz to 2 GHz on every model.
const MIN_RF_HZ: f64 = 1_000.0;
const MAX_RF_HZ: f64 = 2_000_000_000.0;

/// Open the device and start streaming.
///
/// The device is selected *on the session thread* so all control stays there;
/// this function blocks on a handshake so the caller still gets a synchronous
/// `Result`.
pub fn spawn(cfg: &SdrPlayConfig, center_hz: f64) -> Result<SdrPlayHandle> {
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded::<Ctrl>();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<DeviceInfo>>(1);

    let shared = Arc::new(Shared::new());

    let cfg = cfg.clone();
    let thread_shared = Arc::clone(&shared);
    let join = std::thread::Builder::new()
        .name("sdroxide-sdrplay".into())
        .spawn(move || {
            run(cfg, center_hz, ctrl_rx, Arc::clone(&thread_shared), ready_tx);
            thread_shared.alive.store(false, Ordering::Relaxed);
        })
        .map_err(|e| Error::Api {
            call: "spawn",
            text: format!("could not start the SDRplay thread: {e}"),
        })?;

    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(SdrPlayHandle::from_parts(
            info.rx,
            ctrl_tx,
            shared,
            join,
            info.label,
            info.serial,
            info.model,
            info.out_rate_hz,
            info.analog_bw_hz,
            info.dual,
            info.low_if_khz,
        )),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(Error::Api {
                call: "spawn",
                text: "the SDRplay thread stopped before it opened the device".into(),
            })
        }
    }
}

struct DeviceInfo {
    /// The read end of the ring, built on the session thread — only there is
    /// it known how many tuners the device actually opened with, and that is
    /// what decides both the depth and the shape of it.
    rx: Consumer<f32>,
    label: String,
    serial: String,
    model: SdrPlayModel,
    out_rate_hz: f64,
    analog_bw_hz: f64,
    dual: bool,
    low_if_khz: i32,
}

/// What the requested sample rate turns into on this device, which depends on
/// how many tuners are running.
///
/// One tuner samples at baseband and the ADC follows the rate. Two share one
/// ADC at a fixed 6 MHz and hand the service a low IF, from which its own
/// downconverter delivers 2 Msps — decimated further for anything narrower.
/// A rate the second arrangement cannot reach is clamped rather than refused:
/// a configuration carried over from single-tuner operation should still open.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RatePlan {
    dual: bool,
    /// ADC rate to program.
    fs_hz: f64,
    /// The API's decimation factor.
    decim: u8,
    /// What comes out, after that decimation.
    out_hz: f64,
    if_type: ffi::IfType,
}

impl RatePlan {
    /// `dual` is what the device *opened* as, not what was asked for: a
    /// diversity setting left behind by an RSPduo must not put an RSP1A into
    /// a low IF and a 2 Msps ceiling it has no reason to be in.
    fn for_device(cfg: &SdrPlayConfig, dual: bool) -> RatePlan {
        if dual {
            let decim = device::dual_decim(cfg.sample_rate_hz);
            RatePlan {
                dual: true,
                fs_hz: device::DUAL_FS_HZ,
                decim,
                out_hz: device::DUAL_OUT_HZ / f64::from(decim),
                if_type: ffi::IF_1_620,
            }
        } else {
            let (fs, decim) = device::fs_and_decim(cfg.sample_rate_hz);
            RatePlan {
                dual: false,
                fs_hz: fs,
                decim,
                out_hz: fs / f64::from(decim),
                if_type: ffi::IF_ZERO,
            }
        }
    }

    /// Floats per sample in the ring — a pair, or both tuners' pairs.
    fn stride(&self) -> usize {
        if self.dual { QUAD } else { 2 }
    }
}

/// Everything the callbacks touch. Leaked to a raw pointer for the life of
/// the Init..Uninit span; see the module doc.
struct CbCtx {
    shared: Arc<Shared>,
    stream: Mutex<StreamState>,
    epoch: Instant,
}

struct StreamState {
    ring: Producer<f32>,
    /// Where one tuner's block is converted to `f32` on its way to the ring.
    /// The paired path has no use for it: [`Pairer`] converts as it stages.
    scratch: Vec<f32>,
    /// `Some` with both tuners running — see [`crate::pair`].
    pair: Option<Pairer>,
    stats: RxStats,
}

/// Mutable session state the apply loop tracks between updates.
struct Live {
    center_hz: f64,
    /// What the operator asked the LNA to be — kept separate from what the
    /// band allows, so retuning out of a restrictive band restores it.
    lna_wanted: u8,
    /// What is actually programmed after the per-band clamp.
    lna_programmed: u8,
    /// The same pair for the second tuner, when both are running.
    aux_lna_wanted: u8,
    aux_lna_programmed: u8,
    antenna: String,
    hdr: bool,
}

/// The parameter block and `Update` addressee for each tuner in use.
///
/// The API's channel blocks are named for the tuners, not for the jobs: `A` is
/// always tuner 1. Which of them carries the main aerial is the operator's
/// choice ([`SdrPlayConfig::duo_tuner`]), so it is resolved once, here, and
/// everything downstream says main and aux.
struct Tuners {
    main: *mut ffi::RxChannelParamsT,
    main_sel: ffi::TunerSelect,
    /// `None` with one tuner running.
    aux: Option<*mut ffi::RxChannelParamsT>,
    aux_sel: ffi::TunerSelect,
}

fn run(
    cfg: SdrPlayConfig,
    center_hz: f64,
    ctrl: Receiver<Ctrl>,
    shared: Arc<Shared>,
    ready: crossbeam_channel::Sender<Result<DeviceInfo>>,
) {
    let (api, mut dev) = match api::select(&cfg.serial, cfg.duo_tuner, cfg.wants_dual_tuner()) {
        Ok(v) => v,
        Err(e) => {
            if matches!(e, Error::ServiceDown(_)) {
                // Reconnect from scratch next time: the service may have been
                // restarted under us.
                api::reset();
            }
            let _ = ready.send(Err(e));
            return;
        }
    };
    let model = SdrPlayModel::from_hw_ver(dev.hw_ver);
    let serial = dev.serial();
    if let Some(w) = sdroxide_types::SdrPlayDevice::degraded_warning(&serial, model) {
        // Streaming still starts — the operator may know better — but this in
        // the log turns "deaf for days" into a one-line diagnosis.
        tracing::warn!("{w}");
    }

    let mut params_ptr: *mut ffi::DeviceParamsT = std::ptr::null_mut();
    let err = unsafe { (api.get_device_params)(dev.dev, &mut params_ptr) };
    if err != ffi::ERR_SUCCESS || params_ptr.is_null() {
        api::release(&mut dev);
        let _ = ready.send(Err(Error::from_status(&api, "GetDeviceParams", err)));
        return;
    }
    // Service-owned storage; see the module doc for the lifetime rule.
    let dev_params = unsafe { (*params_ptr).dev_params };
    // Believe the device record rather than the configuration: `select` only
    // puts an RSPduo into dual-tuner mode, and a diversity setting left behind
    // by one must not have the driver look for a second tuner on an RSP1A.
    let dual = dev.rsp_duo_mode == ffi::RSPDUO_MODE_DUAL_TUNER;
    let plan = RatePlan::for_device(&cfg, dual);
    let (ch_a, ch_b) = unsafe { ((*params_ptr).rx_channel_a, (*params_ptr).rx_channel_b) };
    let tuners = if dual {
        let main_is_a = cfg.duo_tuner == SdrPlayDuoTuner::Tuner1;
        Tuners {
            main: if main_is_a { ch_a } else { ch_b },
            main_sel: if main_is_a { ffi::TUNER_A } else { ffi::TUNER_B },
            aux: Some(if main_is_a { ch_b } else { ch_a }),
            aux_sel: if main_is_a { ffi::TUNER_B } else { ffi::TUNER_A },
        }
    } else {
        let b = dev.tuner == ffi::TUNER_B;
        Tuners {
            main: if b { ch_b } else { ch_a },
            main_sel: if b { ffi::TUNER_B } else { ffi::TUNER_A },
            aux: None,
            aux_sel: ffi::TUNER_NEITHER,
        }
    };
    if dev_params.is_null() || tuners.main.is_null() || tuners.aux == Some(std::ptr::null_mut()) {
        api::release(&mut dev);
        let _ = ready.send(Err(Error::Api {
            call: "GetDeviceParams",
            text: "the service returned no parameter block for the selected tuner".into(),
        }));
        return;
    }

    let effective = plan.out_hz;
    let bw = device::bw_khz_for(cfg.bw_khz, effective, dual);
    let center = center_hz.clamp(MIN_RF_HZ, MAX_RF_HZ);
    let hiz = device::antenna_is_hiz(model, &cfg.antenna);
    let hdr = cfg.hdr && model.has_hdr();
    let lna_wanted = cfg.lna_state.min(model.max_lna_state());
    let lna_programmed = lna_wanted.min(device::max_lna_state(model, center, hiz, hdr));
    // The second tuner has no port choice of its own — its 50 Ω input is all
    // it has — so its clamp never sees a Hi-Z path.
    let aux_lna_wanted = cfg.diversity.lna_state.min(model.max_lna_state());
    let aux_lna_programmed = aux_lna_wanted.min(device::max_lna_state(model, center, false, false));

    let mut live = Live {
        center_hz: center,
        lna_wanted,
        lna_programmed,
        aux_lna_wanted,
        aux_lna_programmed,
        antenna: cfg.antenna.clone(),
        hdr,
    };

    unsafe {
        let dp = &mut *dev_params;
        dp.ppm = cfg.ppm;
        dp.fs_freq.fs_hz = plan.fs_hz;

        // The second tuner is configured exactly like the first apart from its
        // gains: same span, same filter, same decimation, same notches. That
        // is not tidiness — two branches filtered differently are two branches
        // the adaptive filter cannot line up.
        if let Some(aux) = tuners.aux {
            let ch = &mut *aux;
            ch.tuner_params.bw_type = bw;
            ch.tuner_params.if_type = plan.if_type;
            ch.tuner_params.lo_mode = ffi::LO_AUTO;
            ch.tuner_params.rf_freq.rf_hz = center;
            ch.tuner_params.gain.g_rdb =
                cfg.diversity.if_gr_db.clamp(SdrPlayConfig::IF_GR_MIN, SdrPlayConfig::IF_GR_MAX);
            ch.tuner_params.gain.lna_state = aux_lna_programmed;
            ch.tuner_params.gain.min_gr = ffi::NORMAL_MIN_GR;
            ch.ctrl_params.decimation.enable = (plan.decim > 1) as u8;
            ch.ctrl_params.decimation.decimation_factor = plan.decim;
            ch.ctrl_params.agc.enable = cfg.agc.code() as ffi::AgcControl;
            ch.ctrl_params.agc.set_point_dbfs = cfg.agc_setpoint_dbfs.clamp(-72, -20);
            ch.rsp_duo_tuner_params.rf_notch_enable = cfg.rf_notch as u8;
            ch.rsp_duo_tuner_params.rf_dab_notch_enable = cfg.dab_notch as u8;
        }

        let ch = &mut *tuners.main;
        ch.tuner_params.bw_type = bw;
        ch.tuner_params.if_type = plan.if_type;
        ch.tuner_params.lo_mode = ffi::LO_AUTO;
        ch.tuner_params.rf_freq.rf_hz = center;
        ch.tuner_params.gain.g_rdb =
            cfg.if_gr_db.clamp(SdrPlayConfig::IF_GR_MIN, SdrPlayConfig::IF_GR_MAX);
        ch.tuner_params.gain.lna_state = lna_programmed;
        ch.tuner_params.gain.min_gr = ffi::NORMAL_MIN_GR;
        ch.ctrl_params.decimation.enable = (plan.decim > 1) as u8;
        ch.ctrl_params.decimation.decimation_factor = plan.decim;
        ch.ctrl_params.agc.enable = cfg.agc.code() as ffi::AgcControl;
        ch.ctrl_params.agc.set_point_dbfs = cfg.agc_setpoint_dbfs.clamp(-72, -20);

        match model {
            SdrPlayModel::Rsp1a | SdrPlayModel::Rsp1b => {
                dp.rsp1a_params.rf_notch_enable = cfg.rf_notch as u8;
                dp.rsp1a_params.rf_dab_notch_enable = cfg.dab_notch as u8;
                ch.rsp1a_tuner_params.bias_t_enable = cfg.bias_tee as u8;
            }
            SdrPlayModel::Rsp2 => {
                ch.rsp2_tuner_params.bias_t_enable = cfg.bias_tee as u8;
                ch.rsp2_tuner_params.rf_notch_enable = cfg.rf_notch as u8;
                if let Some((ant, port)) = device::rsp2_antenna(&cfg.antenna) {
                    ch.rsp2_tuner_params.antenna_sel = ant;
                    ch.rsp2_tuner_params.am_port_sel = port;
                }
            }
            SdrPlayModel::RspDuo => {
                ch.rsp_duo_tuner_params.bias_t_enable = cfg.bias_tee as u8;
                ch.rsp_duo_tuner_params.rf_notch_enable = cfg.rf_notch as u8;
                ch.rsp_duo_tuner_params.rf_dab_notch_enable = cfg.dab_notch as u8;
                if let Some(port) = device::rspduo_tuner1_amport(&cfg.antenna) {
                    ch.rsp_duo_tuner_params.tuner1_am_port_sel = port;
                }
            }
            SdrPlayModel::RspDx | SdrPlayModel::RspDxR2 => {
                dp.rsp_dx_params.hdr_enable = hdr as u8;
                dp.rsp_dx_params.bias_t_enable = cfg.bias_tee as u8;
                dp.rsp_dx_params.rf_notch_enable = cfg.rf_notch as u8;
                dp.rsp_dx_params.rf_dab_notch_enable = cfg.dab_notch as u8;
                if let Some(ant) = device::rspdx_antenna(&cfg.antenna) {
                    dp.rsp_dx_params.antenna_sel = ant;
                }
            }
            SdrPlayModel::Rsp1 | SdrPlayModel::Unknown => {}
        }
    }
    shared.lna_state.store(lna_programmed, Ordering::Relaxed);
    shared.aux_lna_state.store(aux_lna_programmed, Ordering::Relaxed);

    let (ring, rx) = ring_for(effective, plan.stride());
    // The callback context: leaked here, reclaimed strictly after Uninit.
    let ctx = Box::into_raw(Box::new(CbCtx {
        shared: Arc::clone(&shared),
        stream: Mutex::new(StreamState {
            ring,
            scratch: Vec::new(),
            pair: dual.then(|| Pairer::new(effective)),
            stats: RxStats::new(effective),
        }),
        epoch: Instant::now(),
    }));
    // With one tuner running both slots point at the same handler: only one of
    // them ever fires, and which one is the service's business. With two, the
    // slot *is* the tuner — that is the only thing that says which aerial a
    // block came off — so each gets a handler that knows which it is.
    let mut cbfns = if dual {
        let main_is_a = tuners.main_sel == ffi::TUNER_A;
        ffi::CallbackFnsT {
            stream_a: Some(if main_is_a { stream_main_cb } else { stream_aux_cb }),
            stream_b: Some(if main_is_a { stream_aux_cb } else { stream_main_cb }),
            event: Some(event_cb),
        }
    } else {
        ffi::CallbackFnsT {
            stream_a: Some(stream_cb),
            stream_b: Some(stream_cb),
            event: Some(event_cb),
        }
    };

    let err = unsafe { (api.init)(dev.dev, &mut cbfns, ctx.cast::<c_void>()) };
    if err != ffi::ERR_SUCCESS {
        let e = Error::from_status(&api, "Init", err);
        if matches!(e, Error::ServiceDown(_)) {
            api::reset();
        }
        api::release(&mut dev);
        // No callback was registered, so the context comes straight back.
        drop(unsafe { Box::from_raw(ctx) });
        let _ = ready.send(Err(e));
        return;
    }

    let label = format!("SDRplay {} (serial {serial})", model.label());
    tracing::info!(
        "{label} streaming: fs {:.3} Msps / {} -> {:.3} Msps, bw {bw} kHz, IF {} kHz, {} \
         samples/pkt",
        plan.fs_hz / 1e6,
        plan.decim,
        effective / 1e6,
        plan.if_type,
        unsafe { (*dev_params).samples_per_pkt },
    );
    if dual {
        tracing::info!(
            "both RSPduo tuners are running: {} is the main aerial, {} the second (LNA state \
             {aux_lna_programmed}, IF gain reduction {} dB). Dual-tuner mode fixes the ADC at \
             {:.0} MHz and the widest span at {:.3} Msps.",
            cfg.duo_tuner.short_label(),
            cfg.aux_tuner().short_label(),
            cfg.diversity.if_gr_db,
            device::DUAL_FS_HZ / 1e6,
            device::DUAL_OUT_HZ / 1e6,
        );
    }
    let _ = ready.send(Ok(DeviceInfo {
        rx,
        label,
        serial,
        model,
        out_rate_hz: effective,
        analog_bw_hz: bw as f64 * 1000.0,
        dual,
        low_if_khz: plan.if_type,
    }));

    let mut pending = Pending::default();
    loop {
        match ctrl.recv_timeout(CTRL_TIMEOUT) {
            Ok(c) => {
                pending.absorb(c);
                while let Ok(c) = ctrl.try_recv() {
                    pending.absorb(c);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // The overload acknowledgement the API requires, moved out of the
        // event callback so no Update ever runs on the service's thread. It
        // goes to the tuner that reported it, and to no other.
        let acks = shared.overload_ack_pending.swap(0, Ordering::Relaxed);
        for sel in [Some(tuners.main_sel), tuners.aux.map(|_| tuners.aux_sel)].into_iter().flatten()
        {
            if acks & ack_bit(sel) != 0 {
                update(&api, &dev, sel, ffi::UPDATE_CTRL_OVERLOAD_ACK, ffi::UPDATE_EXT1_NONE);
            }
        }
        if pending.shutdown {
            break;
        }
        if shared.removed.load(Ordering::Relaxed) {
            tracing::warn!("SDRplay: device removed, stopping the session");
            break;
        }
        if !pending.is_empty() {
            apply(&api, &dev, dev_params, &tuners, model, &shared, &mut live, &mut pending);
            pending = Pending::default();
        }
    }

    let err = unsafe { (api.uninit)(dev.dev) };
    if err != ffi::ERR_SUCCESS {
        tracing::debug!("Uninit: {}", api.err_text(err));
    }
    api::release(&mut dev);
    // Only now is the API done with the callbacks; the context can come back.
    drop(unsafe { Box::from_raw(ctx) });
    tracing::debug!("SDRplay session ended");
}

/// Which bit of [`Shared::overload_ack_pending`] belongs to a tuner.
fn ack_bit(tuner: ffi::TunerSelect) -> u8 {
    if tuner == ffi::TUNER_B { 0b10 } else { 0b01 }
}

/// One `sdrplay_api_Update`, with failures logged rather than fatal: a slider
/// tweak that the service refuses should not take the receiver down.
///
/// The tuner is named by the caller rather than read off the device: with both
/// running, `dev.tuner` is `Tuner_Both`, which is how the pair was *selected*
/// and not somewhere an update can be sent.
fn update(
    api: &ffi::Api,
    dev: &ffi::DeviceT,
    tuner: ffi::TunerSelect,
    reason: ffi::Reason,
    ext1: ffi::ReasonExt1,
) {
    if reason == ffi::UPDATE_NONE && ext1 == ffi::UPDATE_EXT1_NONE {
        return;
    }
    let err = unsafe { (api.update)(dev.dev, tuner, reason, ext1) };
    if err != ffi::ERR_SUCCESS {
        tracing::warn!("sdrplay_api_Update({reason:#x},{ext1:#x}) failed: {}", api.err_text(err));
    }
}

/// Apply coalesced control to the parameter block. Runs only on the session
/// thread — the one place the params pointers may be dereferenced.
#[allow(clippy::too_many_arguments)]
fn apply(
    api: &ffi::Api,
    dev: &ffi::DeviceT,
    dev_params: *mut ffi::DevParamsT,
    tuners: &Tuners,
    model: SdrPlayModel,
    shared: &Shared,
    live: &mut Live,
    p: &mut Pending,
) {
    let mut reason = ffi::UPDATE_NONE;
    let mut ext1 = ffi::UPDATE_EXT1_NONE;
    // What the second tuner is told, which is not the same set: it follows the
    // dial and the filters but keeps its own gains.
    let mut aux_reason = ffi::UPDATE_NONE;
    let dp = unsafe { &mut *dev_params };
    let ch = unsafe { &mut *tuners.main };
    // Safety: the same service-owned storage rule as `ch`, and the two are
    // different tuners' blocks, so this never aliases.
    let mut aux = tuners.aux.map(|a| unsafe { &mut *a });

    if let Some(hz) = p.center {
        live.center_hz = hz.clamp(MIN_RF_HZ, MAX_RF_HZ);
        ch.tuner_params.rf_freq.rf_hz = live.center_hz;
        reason |= ffi::UPDATE_TUNER_FRF;
        if let Some(a) = aux.as_mut() {
            // Both aerials on one frequency is what makes the pair coherent in
            // the first place; a second tuner left behind on the old dial is
            // two receivers, not diversity.
            a.tuner_params.rf_freq.rf_hz = live.center_hz;
            aux_reason |= ffi::UPDATE_TUNER_FRF;
        }
    }
    if let Some(gr) = p.aux_if_gr
        && let Some(a) = aux.as_mut()
    {
        a.tuner_params.gain.g_rdb = gr.clamp(SdrPlayConfig::IF_GR_MIN, SdrPlayConfig::IF_GR_MAX);
        aux_reason |= ffi::UPDATE_TUNER_GR;
    }
    if let Some(lna) = p.aux_lna {
        live.aux_lna_wanted = lna.min(model.max_lna_state());
    }
    if let Some(gr) = p.if_gr {
        ch.tuner_params.gain.g_rdb = gr.clamp(SdrPlayConfig::IF_GR_MIN, SdrPlayConfig::IF_GR_MAX);
        reason |= ffi::UPDATE_TUNER_GR;
    }
    if let Some(lna) = p.lna {
        live.lna_wanted = lna.min(model.max_lna_state());
    }
    if let Some(name) = p.antenna.take() {
        match model {
            SdrPlayModel::Rsp2 => {
                if let Some((ant, port)) = device::rsp2_antenna(&name) {
                    ch.rsp2_tuner_params.antenna_sel = ant;
                    ch.rsp2_tuner_params.am_port_sel = port;
                    reason |= ffi::UPDATE_RSP2_ANTENNA | ffi::UPDATE_RSP2_AM_PORT;
                    live.antenna = name;
                }
            }
            SdrPlayModel::RspDx | SdrPlayModel::RspDxR2 => {
                if let Some(ant) = device::rspdx_antenna(&name) {
                    dp.rsp_dx_params.antenna_sel = ant;
                    ext1 |= ffi::UPDATE_RSPDX_ANTENNA;
                    live.antenna = name;
                }
            }
            SdrPlayModel::RspDuo => {
                if let Some(port) = device::rspduo_tuner1_amport(&name) {
                    ch.rsp_duo_tuner_params.tuner1_am_port_sel = port;
                    reason |= ffi::UPDATE_RSPDUO_AM_PORT;
                    live.antenna = name;
                }
            }
            _ => {}
        }
    }
    if let Some(on) = p.hdr
        && model.has_hdr()
    {
        live.hdr = on;
        dp.rsp_dx_params.hdr_enable = on as u8;
        ext1 |= ffi::UPDATE_RSPDX_HDR_ENABLE;
    }

    // The LNA clamp depends on band, port and HDR path, so re-derive it after
    // any of those may have moved and republish only a real change.
    let hiz = device::antenna_is_hiz(model, &live.antenna);
    let clamped = live.lna_wanted.min(device::max_lna_state(model, live.center_hz, hiz, live.hdr));
    if clamped != live.lna_programmed || p.lna.is_some() {
        live.lna_programmed = clamped;
        ch.tuner_params.gain.lna_state = clamped;
        shared.lna_state.store(clamped, Ordering::Relaxed);
        reason |= ffi::UPDATE_TUNER_GR;
    }
    if let Some(a) = aux.as_mut() {
        let clamped =
            live.aux_lna_wanted.min(device::max_lna_state(model, live.center_hz, false, false));
        if clamped != live.aux_lna_programmed || p.aux_lna.is_some() {
            live.aux_lna_programmed = clamped;
            a.tuner_params.gain.lna_state = clamped;
            shared.aux_lna_state.store(clamped, Ordering::Relaxed);
            aux_reason |= ffi::UPDATE_TUNER_GR;
        }
    }

    if p.agc.is_some() || p.agc_setpoint.is_some() {
        if let Some(code) = p.agc {
            ch.ctrl_params.agc.enable = code as ffi::AgcControl;
        }
        if let Some(sp) = p.agc_setpoint {
            ch.ctrl_params.agc.set_point_dbfs = sp.clamp(-72, -20);
        }
        reason |= ffi::UPDATE_CTRL_AGC;
        if let Some(a) = aux.as_mut() {
            if let Some(code) = p.agc {
                a.ctrl_params.agc.enable = code as ffi::AgcControl;
            }
            if let Some(sp) = p.agc_setpoint {
                a.ctrl_params.agc.set_point_dbfs = sp.clamp(-72, -20);
            }
            aux_reason |= ffi::UPDATE_CTRL_AGC;
        }
    }
    if let Some(ppm) = p.ppm {
        dp.ppm = ppm.clamp(-1000.0, 1000.0);
        reason |= ffi::UPDATE_DEV_PPM;
    }
    if let Some(on) = p.bias_tee {
        match model {
            SdrPlayModel::Rsp1a | SdrPlayModel::Rsp1b => {
                ch.rsp1a_tuner_params.bias_t_enable = on as u8;
                reason |= ffi::UPDATE_RSP1A_BIAS_T;
            }
            SdrPlayModel::Rsp2 => {
                ch.rsp2_tuner_params.bias_t_enable = on as u8;
                reason |= ffi::UPDATE_RSP2_BIAS_T;
            }
            SdrPlayModel::RspDuo => {
                ch.rsp_duo_tuner_params.bias_t_enable = on as u8;
                reason |= ffi::UPDATE_RSPDUO_BIAS_T;
            }
            SdrPlayModel::RspDx | SdrPlayModel::RspDxR2 => {
                dp.rsp_dx_params.bias_t_enable = on as u8;
                ext1 |= ffi::UPDATE_RSPDX_BIAS_T;
            }
            _ => {}
        }
    }
    if let Some(on) = p.rf_notch {
        match model {
            SdrPlayModel::Rsp1a | SdrPlayModel::Rsp1b => {
                dp.rsp1a_params.rf_notch_enable = on as u8;
                reason |= ffi::UPDATE_RSP1A_RF_NOTCH;
            }
            SdrPlayModel::Rsp2 => {
                ch.rsp2_tuner_params.rf_notch_enable = on as u8;
                reason |= ffi::UPDATE_RSP2_RF_NOTCH;
            }
            SdrPlayModel::RspDuo => {
                ch.rsp_duo_tuner_params.rf_notch_enable = on as u8;
                reason |= ffi::UPDATE_RSPDUO_RF_NOTCH;
                if let Some(a) = aux.as_mut() {
                    a.rsp_duo_tuner_params.rf_notch_enable = on as u8;
                    aux_reason |= ffi::UPDATE_RSPDUO_RF_NOTCH;
                }
            }
            SdrPlayModel::RspDx | SdrPlayModel::RspDxR2 => {
                dp.rsp_dx_params.rf_notch_enable = on as u8;
                ext1 |= ffi::UPDATE_RSPDX_RF_NOTCH;
            }
            _ => {}
        }
    }
    if let Some(on) = p.dab_notch {
        match model {
            SdrPlayModel::Rsp1a | SdrPlayModel::Rsp1b => {
                dp.rsp1a_params.rf_dab_notch_enable = on as u8;
                reason |= ffi::UPDATE_RSP1A_DAB_NOTCH;
            }
            SdrPlayModel::RspDuo => {
                ch.rsp_duo_tuner_params.rf_dab_notch_enable = on as u8;
                reason |= ffi::UPDATE_RSPDUO_DAB_NOTCH;
                if let Some(a) = aux.as_mut() {
                    a.rsp_duo_tuner_params.rf_dab_notch_enable = on as u8;
                    aux_reason |= ffi::UPDATE_RSPDUO_DAB_NOTCH;
                }
            }
            SdrPlayModel::RspDx | SdrPlayModel::RspDxR2 => {
                dp.rsp_dx_params.rf_dab_notch_enable = on as u8;
                ext1 |= ffi::UPDATE_RSPDX_DAB_NOTCH;
            }
            _ => {}
        }
    }

    update(api, dev, tuners.main_sel, reason, ext1);
    if tuners.aux.is_some() {
        update(api, dev, tuners.aux_sel, aux_reason, ffi::UPDATE_EXT1_NONE);
    }
}

/// The service's sample delivery with one tuner running. Foreign thread — see
/// the module doc.
unsafe extern "C" fn stream_cb(
    xi: *mut i16,
    xq: *mut i16,
    params: *mut ffi::StreamCbParamsT,
    num_samples: u32,
    reset: u32,
    cb_context: *mut c_void,
) {
    unsafe { deliver(None, xi, xq, params, num_samples, reset, cb_context) }
}

/// The same for the tuner carrying the main aerial, with both running.
unsafe extern "C" fn stream_main_cb(
    xi: *mut i16,
    xq: *mut i16,
    params: *mut ffi::StreamCbParamsT,
    num_samples: u32,
    reset: u32,
    cb_context: *mut c_void,
) {
    unsafe { deliver(Some(Side::Main), xi, xq, params, num_samples, reset, cb_context) }
}

/// ...and for the one carrying the second.
unsafe extern "C" fn stream_aux_cb(
    xi: *mut i16,
    xq: *mut i16,
    params: *mut ffi::StreamCbParamsT,
    num_samples: u32,
    reset: u32,
    cb_context: *mut c_void,
) {
    unsafe { deliver(Some(Side::Aux), xi, xq, params, num_samples, reset, cb_context) }
}

/// One block of samples, from whichever tuner. Foreign thread — see the module
/// doc; nothing here may unwind, and nothing here may touch the parameter
/// block.
///
/// # Safety
///
/// The pointers are the service's, valid for `num_samples` for the duration of
/// the call, and `cb_context` is the leaked [`CbCtx`].
#[allow(clippy::too_many_arguments)]
unsafe fn deliver(
    side: Option<Side>,
    xi: *mut i16,
    xq: *mut i16,
    params: *mut ffi::StreamCbParamsT,
    num_samples: u32,
    reset: u32,
    cb_context: *mut c_void,
) {
    let ctx = cb_context.cast::<CbCtx>();
    if ctx.is_null() {
        return;
    }
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        if reset != 0 {
            tracing::debug!("SDRplay stream reset (retune or rate change settled)");
        }
        let n = num_samples as usize;
        if n == 0 || xi.is_null() || xq.is_null() {
            return;
        }
        let xi = unsafe { std::slice::from_raw_parts(xi, n) };
        let xq = unsafe { std::slice::from_raw_parts(xq, n) };
        let Ok(mut guard) = ctx.stream.lock() else {
            return;
        };
        let st = &mut *guard;
        let paused = ctx.shared.rx_paused.load(Ordering::Relaxed);
        let (delivered, dropped) = match (side, st.pair.as_mut()) {
            (Some(side), Some(pair)) => {
                // The sample number is the only thing that says which of the
                // other tuner's samples these belong with; a service that does
                // not fill it in is noticed inside the pairing.
                let first =
                    if params.is_null() { 0 } else { unsafe { (*params).first_sample_num } };
                pair.push(side, xi, xq, first);
                let out = pair.drain(&mut st.ring, &mut st.stats, paused);
                ctx.shared.pair_slips.store(pair.slips(), Ordering::Relaxed);
                ctx.shared.aux_stalled.store(pair.stalled(), Ordering::Relaxed);
                ctx.shared.pair_stamped.store(pair.believes_sample_numbers(), Ordering::Relaxed);
                out
            }
            _ => {
                st.scratch.resize(2 * n, 0.0);
                for i in 0..n {
                    st.scratch[2 * i] = f32::from(xi[i]) * SCALE;
                    st.scratch[2 * i + 1] = f32::from(xq[i]) * SCALE;
                }
                (n, push_iq(&mut st.ring, &st.scratch[..2 * n], &mut st.stats, paused, 2))
            }
        };
        if dropped > 0 {
            ctx.shared.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        st.stats.tick();
        // Only what actually reached the ring counts as the receiver being
        // alive. A pairing that produces nothing while blocks keep arriving is
        // a receiver that has stopped, and the watchdog has to be able to see
        // that.
        if delivered > 0 {
            ctx.shared.last_rx_ms.store(ctx.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    }))
    .is_err();
    if panicked {
        // Unwinding across the FFI boundary is UB; declare the session dead
        // instead and let the engine's reopen machinery rebuild it.
        let shared = &unsafe { &*ctx }.shared;
        shared.alive.store(false, Ordering::Relaxed);
    }
}

/// The service's event delivery. Foreign thread — see the module doc.
unsafe extern "C" fn event_cb(
    event_id: ffi::EventId,
    tuner: ffi::TunerSelect,
    params: *mut ffi::EventParamsT,
    cb_context: *mut c_void,
) {
    let ctx = cb_context.cast::<CbCtx>();
    if ctx.is_null() {
        return;
    }
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        let shared = &ctx.shared;
        match event_id {
            ffi::EVENT_GAIN_CHANGE => {
                if params.is_null() {
                    return;
                }
                let g = unsafe { (*params).gain };
                shared.ev_gr_db.store(g.g_rdb as i64, Ordering::Relaxed);
                shared.ev_lna_gr_db.store(g.lna_g_rdb as i64, Ordering::Relaxed);
                shared
                    .ev_curr_gain_milli_db
                    .store((g.curr_gain * 1000.0) as i64, Ordering::Relaxed);
            }
            ffi::EVENT_POWER_OVERLOAD => {
                if params.is_null() {
                    return;
                }
                let kind = unsafe { (*params).power_overload.power_overload_change_type };
                let over = kind == ffi::OVERLOAD_DETECTED;
                let was = shared.overload.swap(over, Ordering::Relaxed);
                // The API insists every overload message is acknowledged, by
                // the tuner it came from; the session thread does that, not
                // this foreign one. An event naming neither tuner (or both) is
                // marked for both, and the session thread only answers for the
                // ones it is running.
                let bits = match tuner {
                    ffi::TUNER_A => 0b01,
                    ffi::TUNER_B => 0b10,
                    _ => 0b11,
                };
                shared.overload_ack_pending.fetch_or(bits, Ordering::Relaxed);
                if over && !was {
                    tracing::warn!(
                        "SDRplay: RF overload on {} — raise the LNA slider (more \
                         attenuation), lower IF gain, or enable AGC",
                        match tuner {
                            ffi::TUNER_A => "tuner 1",
                            ffi::TUNER_B => "tuner 2",
                            _ => "the receiver",
                        }
                    );
                } else if !over && was {
                    tracing::info!("SDRplay: overload corrected");
                }
            }
            ffi::EVENT_DEVICE_REMOVED | ffi::EVENT_DEVICE_FAILURE => {
                tracing::warn!("SDRplay: the service reports the device gone");
                shared.removed.store(true, Ordering::Relaxed);
                shared.alive.store(false, Ordering::Relaxed);
            }
            ffi::EVENT_RSPDUO_MODE_CHANGE => {
                tracing::debug!("SDRplay: RSPduo mode change event");
            }
            _ => {}
        }
    }))
    .is_err();
    if panicked {
        let shared = &unsafe { &*ctx }.shared;
        shared.alive.store(false, Ordering::Relaxed);
    }
}
