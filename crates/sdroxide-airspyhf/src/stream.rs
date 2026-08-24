//! The one blocking thread that owns the receiver.
//!
//! Queued bulk transfers on endpoint `0x81` via nusb's
//! [`nusb::Endpoint::wait_next_complete`], which blocks with a timeout — so
//! this is a plain `std::thread` with no executor, matching every other backend
//! in the workspace. Control arrives over a crossbeam channel and is coalesced
//! (see [`Pending`]) so a panadapter drag cannot starve the sample stream.
//!
//! # Why one thread and not two
//!
//! The RX-888 backend splits the USB drain from the arithmetic because it moves
//! 129.6 MB/s and runs an 8192-point FFT some sixteen thousand times a second.
//! This receiver tops out at 912 kSPS — **3.65 MB/s**, less than the RTL-SDR at
//! 2.4 Msps, which this workspace services on one thread. The image balancer's
//! FFTs run about a hundred and twenty times a second and cost well under a
//! percent of a core, against sixteen queued transfers holding some seventy
//! milliseconds of samples. A hand-off queue and a drop policy would be
//! machinery for a problem that does not exist here.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use nusb::MaybeFuture;
use nusb::transfer::{Bulk, In, TransferError};
use rtrb::Producer;
use sdroxide_dsp::Complex32;
use sdroxide_types::{AirspyHfConfig, AirspyHfModel};

use crate::convert::{Deconstructor, HostDsp};
use crate::device::Device;
use crate::error::{Error, Result};
use crate::handle::{AirspyHfHandle, Ctrl, Pending, RxStats, Shared, push_iq, ring_for};
use crate::protocol::{BULK_EP, TRANSFER_BYTES};
use crate::trace::{self, Trace};

/// How long to wait for a transfer before going back to serve control
/// messages. Short enough that a retune feels immediate, long enough that an
/// idle loop costs nothing.
const COMPLETE_TIMEOUT: Duration = Duration::from_millis(5);

/// Transfers kept in flight, as upstream queues them. At 912 kSPS this is about
/// 72 ms of hardware-side buffering — several times the worst case for a burst
/// of control transfers plus an image-balancer update.
const IN_FLIGHT: usize = 16;

/// What the thread reports back once the receiver is up.
pub(crate) struct DeviceInfo {
    pub label: String,
    pub model: AirspyHfModel,
    pub firmware: String,
    pub board_serial: u64,
    pub rates_hz: Vec<f64>,
    pub low_if: Vec<bool>,
    pub sample_rate_hz: f64,
    pub snapped_from: Option<f64>,
    pub att_max_db: f64,
    pub att_step_db: f64,
    pub bias_tee_supported: bool,
    pub calibration_ppb: i32,
    pub calibration_from_flash: bool,
}

/// Open the receiver and start the stream thread.
///
/// The device is opened *on the thread* so that all control transfers happen on
/// one thread; this function blocks on a handshake until that has succeeded or
/// failed, so the caller still gets a synchronous `Result`.
pub(crate) fn spawn(cfg: &AirspyHfConfig, center_hz: f64) -> Result<AirspyHfHandle> {
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded::<Ctrl>();
    // Rendezvous for the open result, so the caller learns about a permission
    // problem or a missing receiver as a normal error rather than as a stream
    // that silently never starts.
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<DeviceInfo>>(1);

    let shared = Arc::new(Shared::new());
    let t = Trace::new();
    trace::remember(&t);

    // The configured rate may be snapped at open, but every rate this receiver
    // has is within a factor of five of every other, so the ring is generous
    // either way.
    let (rx_prod, rx_cons) = ring_for(cfg.sample_rate_hz);

    let cfg = cfg.clone();
    let thread_shared = Arc::clone(&shared);
    let thread_trace = t.clone();
    let join = std::thread::Builder::new()
        .name("sdroxide-airspyhf".into())
        .spawn(move || {
            run(
                cfg,
                center_hz,
                ctrl_rx,
                rx_prod,
                Arc::clone(&thread_shared),
                ready_tx,
                thread_trace,
            );
            thread_shared.alive.store(false, Ordering::Relaxed);
        })
        .map_err(|e| Error::Access(format!("could not start the Airspy HF+ thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(AirspyHfHandle::from_parts(rx_cons, ctrl_tx, shared, join, t, info)),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        // The thread died before reporting; joining surfaces a panic message.
        Err(_) => {
            let _ = join.join();
            Err(Error::Access("the Airspy HF+ thread stopped before it opened the receiver".into()))
        }
    }
}

fn run(
    cfg: AirspyHfConfig,
    center_hz: f64,
    ctrl: crossbeam_channel::Receiver<Ctrl>,
    mut rx: Producer<f32>,
    shared: Arc<Shared>,
    ready: crossbeam_channel::Sender<Result<DeviceInfo>>,
    trace: Trace,
) {
    let mut dev = match Device::open(&cfg, center_hz, &trace) {
        Ok(d) => d,
        Err(e) => {
            trace.note(format!("open failed: {e}"));
            let _ = ready.send(Err(e));
            return;
        }
    };

    let (att_max_db, att_step_db) = dev.attenuator_range_db();
    let _ = ready.send(Ok(DeviceInfo {
        label: dev.describe(),
        model: dev.model,
        firmware: dev.firmware.clone(),
        board_serial: dev.board_serial,
        rates_hz: dev.rates_hz.clone(),
        low_if: dev.low_if.clone(),
        sample_rate_hz: dev.sample_rate_hz(),
        snapped_from: dev.snapped_from,
        att_max_db,
        att_step_db,
        bias_tee_supported: dev.bias_tee_supported,
        calibration_ppb: dev.calibration_ppb(),
        calibration_from_flash: dev.flash().valid && cfg.calibration_ppb.is_none(),
    }));

    if let Err(e) = pump(&mut dev, &ctrl, &mut rx, &shared, &trace) {
        tracing::warn!("Airspy HF+ stream stopped: {e}");
        trace.note(format!("stream stopped: {e}"));
    }
    dev.shutdown();
}

fn pump(
    dev: &mut Device,
    ctrl: &crossbeam_channel::Receiver<Ctrl>,
    rx: &mut Producer<f32>,
    shared: &Arc<Shared>,
    trace: &Trace,
) -> Result<()> {
    dev.assert_has_rates()?;
    tracing::info!(
        "Airspy HF+ streaming: {IN_FLIGHT} transfers x {} KiB = {:.0} ms of buffering at {:.0} kSPS",
        TRANSFER_BYTES / 1024,
        (IN_FLIGHT * TRANSFER_BYTES / 4) as f64 / dev.sample_rate_hz() * 1000.0,
        dev.sample_rate_hz() / 1000.0,
    );

    let mut ep = dev
        .usb()
        .interface()
        .endpoint::<Bulk, In>(BULK_EP)
        .map_err(|e| Error::Access(format!("cannot open the Airspy HF+ bulk endpoint: {e}")))?;

    let mut deconstruct = Deconstructor::new();
    deconstruct.set_filter_gain(dev.filter_gain());
    let mut dsp = HostDsp::new(dev.sample_rate_hz());
    dsp.set_enabled(dev.lib_dsp());
    dsp.set_low_if(dev.is_low_if());
    dsp.set_shift(dev.shift_hz());

    let mut stats = RxStats::new(dev.sample_rate_hz());
    let started = Instant::now();
    let mut samples: Vec<Complex32> = Vec::with_capacity(TRANSFER_BYTES / 4);
    let mut interleaved: Vec<f32> = Vec::with_capacity(TRANSFER_BYTES / 2);
    let mut logged_first = false;

    dev.start()?;
    for _ in 0..IN_FLIGHT {
        ep.submit(ep.allocate(TRANSFER_BYTES));
    }

    loop {
        // 1. Collapse the whole control channel, then apply each field once.
        let mut pending = Pending::default();
        while let Ok(c) = ctrl.try_recv() {
            pending.absorb(c);
        }
        if pending.shutdown {
            break;
        }
        if !pending.is_empty() {
            // Keep the hardware fed while we talk on endpoint 0. This matters
            // more here than on the RTL-SDR: a retune is two control transfers
            // and a rate change is five plus a re-tune.
            while ep.pending() < IN_FLIGHT {
                ep.submit(ep.allocate(TRANSFER_BYTES));
            }
            if let Err(e) = apply(dev, &pending) {
                // A setting that cannot be applied is worth saying out loud,
                // but it is not a reason to tear the stream down — the operator
                // can pick another value.
                tracing::warn!("Airspy HF+: {e}");
                trace.note(format!("control change failed: {e}"));
            }
            // Everything the DSP needs to know can be changed from the panel,
            // so it is all re-read rather than tracked field by field. Each
            // setter is a no-op when the value has not moved — which matters
            // for the shift, because acting on it restarts the image estimate
            // and a slider drag must not do that.
            deconstruct.set_filter_gain(dev.filter_gain());
            dsp.set_enabled(dev.lib_dsp());
            dsp.set_low_if(dev.is_low_if());
            dsp.set_shift(dev.shift_hz());
        }

        // 2. Refill before draining, so the queue is never empty while the
        //    receiver is producing.
        while ep.pending() < IN_FLIGHT {
            ep.submit(ep.allocate(TRANSFER_BYTES));
        }

        // 3. Drain every completion that is ready, not just one. Blocking only
        //    on the first; the rest are polled. Taking one per iteration
        //    starves the stream under a sustained dial drag — measured on the
        //    RTL-SDR backend, and the arithmetic is no kinder here.
        let mut drained = 0usize;
        loop {
            let timeout = if drained == 0 { COMPLETE_TIMEOUT } else { Duration::ZERO };
            let Some(completion) = ep.wait_next_complete(timeout) else {
                break;
            };
            match completion.status {
                Ok(()) => {
                    let bytes = &completion.buffer[..];
                    samples.clear();
                    deconstruct.push(bytes, &mut samples);

                    if !logged_first && !samples.is_empty() {
                        logged_first = true;
                        // The one line a developer without this hardware cannot
                        // produce: point the receiver at a known carrier and
                        // these bytes settle whether Q really does come first.
                        trace.first_samples(bytes, &samples);
                    }

                    dsp.process(&mut samples);

                    if !samples.is_empty() {
                        interleaved.clear();
                        interleaved.reserve(samples.len() * 2);
                        for s in &samples {
                            interleaved.push(s.re);
                            interleaved.push(s.im);
                        }
                        stats.on_iq(samples.len());
                        push_iq(
                            rx,
                            &interleaved,
                            &mut stats,
                            shared.rx_paused.load(Ordering::Relaxed),
                        );
                        shared
                            .last_rx_ms
                            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    }
                }
                Err(TransferError::Cancelled) => {}
                Err(TransferError::Disconnected) => {
                    return Err(Error::NotFound("the Airspy HF+ was unplugged".into()));
                }
                Err(TransferError::Stall) => {
                    tracing::warn!("Airspy HF+ endpoint stalled; clearing and resynchronising");
                    trace.note("bulk endpoint stalled; cleared");
                    let _ = ep.clear_halt().wait();
                    // The stream restarts from a packet boundary, so bytes
                    // carried from before the stall belong to nothing.
                    deconstruct.reset();
                    stats.on_stall();
                }
                Err(e) => {
                    tracing::debug!("Airspy HF+ transfer error: {e}");
                    stats.on_error();
                }
            }

            // Reuse the buffer rather than allocating a new one every 18 ms.
            let mut buf = completion.buffer;
            buf.clear();
            ep.submit(buf);

            drained += 1;
            // Go back and serve control rather than spinning here forever if
            // the receiver is outrunning us.
            if drained >= IN_FLIGHT {
                break;
            }
        }

        publish_balance(shared, &dsp);
        stats.tick(trace);
    }

    trace.note(format!("stream ended: {}", stats.summary()));

    // Cancel outstanding transfers and let them come back before the interface
    // is dropped.
    ep.cancel_all();
    let deadline = Instant::now() + Duration::from_millis(500);
    while ep.pending() > 0 && Instant::now() < deadline {
        if ep.wait_next_complete(Duration::from_millis(50)).is_none() {
            break;
        }
    }
    Ok(())
}

/// Publish the image balancer's state so the diagnostics report can name it.
///
/// A converged pair of numbers is what says whether the port of the estimator
/// works at all, and it is the first thing to look at if the image on a zero-IF
/// rate is worse than expected.
fn publish_balance(shared: &Arc<Shared>, dsp: &HostDsp) {
    let b = dsp.balancer();
    if b.estimates() == 0 {
        return;
    }
    let (p, a) = b.state();
    shared.balance_phase_ppm.store((p as f64 * 1e6) as i64, Ordering::Relaxed);
    shared.balance_amplitude_ppm.store((a as f64 * 1e6) as i64, Ordering::Relaxed);
    shared.balance_estimates.store(b.estimates(), Ordering::Relaxed);
}

/// Apply coalesced control changes, in dependency order.
///
/// The calibration and the host-DSP switch first, because both change what a
/// frequency *means*; then the centre, so the dial has the last word; then the
/// gains, with the AGC before the attenuator it overrides.
///
/// The sample rate is deliberately not here. It is fixed for the life of a
/// session: the engine builds its whole downconversion chain around
/// `IqSource::sample_rate`, so changing it goes through a reopen — which is why
/// the settings panel marks it "takes effect on Apply".
fn apply(dev: &mut Device, p: &Pending) -> Result<()> {
    if let Some(ppb) = p.calibration_ppb {
        dev.set_calibration_ppb(ppb)?;
    }
    if let Some(on) = p.lib_dsp {
        dev.set_lib_dsp(on)?;
    }
    if let Some(hz) = p.center {
        dev.set_center_hz(hz)?;
    }
    if let Some(on) = p.agc {
        dev.set_agc(on)?;
    }
    if let Some(high) = p.agc_threshold {
        dev.set_agc_threshold(high)?;
    }
    if let Some(db) = p.attenuator {
        dev.set_attenuator_db(db)?;
    }
    if let Some(on) = p.lna {
        dev.set_lna(on)?;
    }
    if let Some(on) = p.bias_tee {
        dev.set_bias_tee(on);
    }
    Ok(())
}
