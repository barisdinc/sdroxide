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

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use rtrb::Producer;
use sdroxide_types::{SdrPlayConfig, SdrPlayModel};

use crate::api;
use crate::device;
use crate::error::{Error, Result};
use crate::ffi;
use crate::handle::{Ctrl, Pending, RxStats, SdrPlayHandle, Shared, push_iq, ring_for};

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
    let (fs, decim) = device::fs_and_decim(cfg.sample_rate_hz);
    let effective = fs / decim as f64;
    let (rx_prod, rx_cons) = ring_for(effective);

    let cfg = cfg.clone();
    let thread_shared = Arc::clone(&shared);
    let join = std::thread::Builder::new()
        .name("sdroxide-sdrplay".into())
        .spawn(move || {
            run(cfg, center_hz, ctrl_rx, rx_prod, Arc::clone(&thread_shared), ready_tx);
            thread_shared.alive.store(false, Ordering::Relaxed);
        })
        .map_err(|e| Error::Api {
            call: "spawn",
            text: format!("could not start the SDRplay thread: {e}"),
        })?;

    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(SdrPlayHandle::from_parts(
            rx_cons,
            ctrl_tx,
            shared,
            join,
            info.label,
            info.serial,
            info.model,
            info.out_rate_hz,
            info.analog_bw_hz,
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
    label: String,
    serial: String,
    model: SdrPlayModel,
    out_rate_hz: f64,
    analog_bw_hz: f64,
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
    scratch: Vec<f32>,
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
    antenna: String,
    hdr: bool,
}

fn run(
    cfg: SdrPlayConfig,
    center_hz: f64,
    ctrl: Receiver<Ctrl>,
    ring: Producer<f32>,
    shared: Arc<Shared>,
    ready: crossbeam_channel::Sender<Result<DeviceInfo>>,
) {
    let (api, mut dev) = match api::select(&cfg.serial, cfg.duo_tuner) {
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
    let ch_params = unsafe {
        if dev.tuner == ffi::TUNER_B {
            (*params_ptr).rx_channel_b
        } else {
            (*params_ptr).rx_channel_a
        }
    };
    if dev_params.is_null() || ch_params.is_null() {
        api::release(&mut dev);
        let _ = ready.send(Err(Error::Api {
            call: "GetDeviceParams",
            text: "the service returned no parameter block for the selected tuner".into(),
        }));
        return;
    }

    let (fs, decim) = device::fs_and_decim(cfg.sample_rate_hz);
    let effective = fs / decim as f64;
    let bw = device::bw_khz_for(cfg.bw_khz, effective);
    let center = center_hz.clamp(MIN_RF_HZ, MAX_RF_HZ);
    let hiz = device::antenna_is_hiz(model, &cfg.antenna);
    let hdr = cfg.hdr && model.has_hdr();
    let lna_wanted = cfg.lna_state.min(model.max_lna_state());
    let lna_programmed = lna_wanted.min(device::max_lna_state(model, center, hiz, hdr));

    let mut live =
        Live { center_hz: center, lna_wanted, lna_programmed, antenna: cfg.antenna.clone(), hdr };

    unsafe {
        let dp = &mut *dev_params;
        dp.ppm = cfg.ppm;
        dp.fs_freq.fs_hz = fs;

        let ch = &mut *ch_params;
        ch.tuner_params.bw_type = bw;
        ch.tuner_params.if_type = ffi::IF_ZERO;
        ch.tuner_params.lo_mode = ffi::LO_AUTO;
        ch.tuner_params.rf_freq.rf_hz = center;
        ch.tuner_params.gain.g_rdb =
            cfg.if_gr_db.clamp(SdrPlayConfig::IF_GR_MIN, SdrPlayConfig::IF_GR_MAX);
        ch.tuner_params.gain.lna_state = lna_programmed;
        ch.tuner_params.gain.min_gr = ffi::NORMAL_MIN_GR;
        ch.ctrl_params.decimation.enable = (decim > 1) as u8;
        ch.ctrl_params.decimation.decimation_factor = decim;
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

    // The callback context: leaked here, reclaimed strictly after Uninit.
    let ctx = Box::into_raw(Box::new(CbCtx {
        shared: Arc::clone(&shared),
        stream: Mutex::new(StreamState {
            ring,
            scratch: Vec::new(),
            stats: RxStats::new(effective),
        }),
        epoch: Instant::now(),
    }));
    // Both stream slots point at the same handler: in single-tuner mode only
    // one of them ever fires, and which one is the service's business.
    let mut cbfns = ffi::CallbackFnsT {
        stream_a: Some(stream_cb),
        stream_b: Some(stream_cb),
        event: Some(event_cb),
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
        "{label} streaming: fs {:.3} Msps / {decim} -> {:.3} Msps, bw {bw} kHz, {} samples/pkt",
        fs / 1e6,
        effective / 1e6,
        unsafe { (*dev_params).samples_per_pkt },
    );
    let _ = ready.send(Ok(DeviceInfo {
        label,
        serial,
        model,
        out_rate_hz: effective,
        analog_bw_hz: bw as f64 * 1000.0,
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
        // event callback so no Update ever runs on the service's thread.
        if shared.overload_ack_pending.swap(false, Ordering::Relaxed) {
            update(&api, &dev, ffi::UPDATE_CTRL_OVERLOAD_ACK, ffi::UPDATE_EXT1_NONE);
        }
        if pending.shutdown {
            break;
        }
        if shared.removed.load(Ordering::Relaxed) {
            tracing::warn!("SDRplay: device removed, stopping the session");
            break;
        }
        if !pending.is_empty() {
            apply(&api, &dev, dev_params, ch_params, model, &shared, &mut live, &mut pending);
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

/// One `sdrplay_api_Update`, with failures logged rather than fatal: a slider
/// tweak that the service refuses should not take the receiver down.
fn update(api: &ffi::Api, dev: &ffi::DeviceT, reason: ffi::Reason, ext1: ffi::ReasonExt1) {
    if reason == ffi::UPDATE_NONE && ext1 == ffi::UPDATE_EXT1_NONE {
        return;
    }
    let tuner = if dev.tuner == ffi::TUNER_B { ffi::TUNER_B } else { ffi::TUNER_A };
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
    ch_params: *mut ffi::RxChannelParamsT,
    model: SdrPlayModel,
    shared: &Shared,
    live: &mut Live,
    p: &mut Pending,
) {
    let mut reason = ffi::UPDATE_NONE;
    let mut ext1 = ffi::UPDATE_EXT1_NONE;
    let dp = unsafe { &mut *dev_params };
    let ch = unsafe { &mut *ch_params };

    if let Some(hz) = p.center {
        live.center_hz = hz.clamp(MIN_RF_HZ, MAX_RF_HZ);
        ch.tuner_params.rf_freq.rf_hz = live.center_hz;
        reason |= ffi::UPDATE_TUNER_FRF;
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

    if p.agc.is_some() || p.agc_setpoint.is_some() {
        if let Some(code) = p.agc {
            ch.ctrl_params.agc.enable = code as ffi::AgcControl;
        }
        if let Some(sp) = p.agc_setpoint {
            ch.ctrl_params.agc.set_point_dbfs = sp.clamp(-72, -20);
        }
        reason |= ffi::UPDATE_CTRL_AGC;
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
            }
            SdrPlayModel::RspDx | SdrPlayModel::RspDxR2 => {
                dp.rsp_dx_params.rf_dab_notch_enable = on as u8;
                ext1 |= ffi::UPDATE_RSPDX_DAB_NOTCH;
            }
            _ => {}
        }
    }

    update(api, dev, reason, ext1);
}

/// The service's sample delivery. Foreign thread — see the module doc.
unsafe extern "C" fn stream_cb(
    xi: *mut i16,
    xq: *mut i16,
    _params: *mut ffi::StreamCbParamsT,
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
        st.scratch.resize(2 * n, 0.0);
        for i in 0..n {
            st.scratch[2 * i] = xi[i] as f32 * SCALE;
            st.scratch[2 * i + 1] = xq[i] as f32 * SCALE;
        }
        let paused = ctx.shared.rx_paused.load(Ordering::Relaxed);
        let dropped = push_iq(&mut st.ring, &st.scratch[..2 * n], &mut st.stats, paused);
        if dropped > 0 {
            ctx.shared.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        st.stats.tick();
        ctx.shared.last_rx_ms.store(ctx.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
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
    _tuner: ffi::TunerSelect,
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
                // The API insists every overload message is acknowledged; the
                // session thread does that, not this foreign one.
                shared.overload_ack_pending.store(true, Ordering::Relaxed);
                if over && !was {
                    tracing::warn!(
                        "SDRplay: RF overload — raise the LNA slider (more attenuation), lower \
                         IF gain, or enable AGC"
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
