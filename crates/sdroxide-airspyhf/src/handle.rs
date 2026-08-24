//! The handle the rest of the program holds, and the accounting behind it.
//!
//! Same shape as [`sdroxide_rtlsdr::RtlSdrHandle`]: one blocking thread owns
//! the receiver, control goes in over a crossbeam channel, samples come back
//! out through an `rtrb` ring of interleaved `f32`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_types::{AirspyHfConfig, AirspyHfModel};

use crate::error::Result;
use crate::trace::Trace;

/// How often the stream thread emits a throughput line.
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// A control message for the stream thread.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ctrl {
    Center(f64),
    Agc(bool),
    AgcThreshold(bool),
    Attenuator(f64),
    Lna(bool),
    BiasTee(bool),
    CalibrationPpb(i32),
    LibDsp(bool),
    Shutdown,
}

/// Control messages accumulated over one pass of the thread loop.
///
/// A retune is two control transfers and dragging the panadapter emits hundreds
/// of `Center` messages a second. Applying each in turn would put the thread
/// permanently behind the operator's hand *and* starve the completion drain, so
/// the whole channel is collapsed into this and each field applied once, last
/// value wins — which is exactly the right semantics for a dial.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Pending {
    pub center: Option<f64>,
    pub agc: Option<bool>,
    pub agc_threshold: Option<bool>,
    pub attenuator: Option<f64>,
    pub lna: Option<bool>,
    pub bias_tee: Option<bool>,
    pub calibration_ppb: Option<i32>,
    pub lib_dsp: Option<bool>,
    pub shutdown: bool,
}

impl Pending {
    pub(crate) fn absorb(&mut self, c: Ctrl) {
        match c {
            Ctrl::Center(v) => self.center = Some(v),
            Ctrl::Agc(v) => self.agc = Some(v),
            Ctrl::AgcThreshold(v) => self.agc_threshold = Some(v),
            Ctrl::Attenuator(v) => self.attenuator = Some(v),
            Ctrl::Lna(v) => self.lna = Some(v),
            Ctrl::BiasTee(v) => self.bias_tee = Some(v),
            Ctrl::CalibrationPpb(v) => self.calibration_ppb = Some(v),
            Ctrl::LibDsp(v) => self.lib_dsp = Some(v),
            Ctrl::Shutdown => self.shutdown = true,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        *self == Pending::default()
    }
}

/// Throughput and health accounting, mirroring the RTL-SDR backend's.
pub(crate) struct RxStats {
    nominal_hz: f64,
    /// When the first sample arrived, and the sample count since then.
    ///
    /// Deliberately *not* the thread start: identifying and configuring the
    /// receiver is a dozen control transfers before the first sample appears,
    /// and counting that dead time against the sample total biases the clock
    /// estimate low.
    first_iq: Option<Instant>,
    since: Instant,
    win_samples: u64,
    win_dropped: u64,
    /// Discarded while the engine was not reading this receiver because the
    /// station was transmitting. Counted apart from `win_dropped` because it
    /// is not a fault: see [`RxStats::on_dropped_keyed`].
    win_keyed: u64,
    win_errors: u64,
    total_samples: u64,
    total_dropped: u64,
    total_keyed: u64,
    total_errors: u64,
    stalls: u64,
}

impl RxStats {
    pub(crate) fn new(nominal_hz: f64) -> RxStats {
        RxStats {
            nominal_hz,
            first_iq: None,
            since: Instant::now(),
            win_samples: 0,
            win_dropped: 0,
            win_keyed: 0,
            win_errors: 0,
            total_samples: 0,
            total_dropped: 0,
            total_keyed: 0,
            total_errors: 0,
            stalls: 0,
        }
    }

    pub(crate) fn on_iq(&mut self, pairs: usize) {
        self.win_samples += pairs as u64;
        match self.first_iq {
            // Start the clock at the first block and do not count that block:
            // it spans an unknown interval reaching back into device setup.
            None => self.first_iq = Some(Instant::now()),
            Some(_) => self.total_samples += pairs as u64,
        }
    }

    pub(crate) fn on_dropped(&mut self, pairs: usize) {
        self.win_dropped += pairs as u64;
        self.total_dropped += pairs as u64;
    }

    /// Record `pairs` complex samples discarded because the ring was full
    /// while the station was transmitting.
    ///
    /// The engine does not read a half-duplex source for the length of an over
    /// and empties the ring on unkey, but this receiver need not be the
    /// transmitter — it may be a separate SDR lent to a rig as a panadapter —
    /// and it carries on streaming throughout. So the ring fills within its own
    /// depth of key-down and everything after that is discarded: expected, at
    /// exactly the sample rate, for as long as the operator transmits. Counting
    /// it as an overrun turns an ordinary over into a warning that blames the
    /// DSP thread and advises a lower sample rate, and leaves the running total
    /// reading as transmit time. See `IqSource::set_rx_paused`, which is what
    /// tells this side which of the two it is looking at.
    pub(crate) fn on_dropped_keyed(&mut self, pairs: usize) {
        self.win_keyed += pairs as u64;
        self.total_keyed += pairs as u64;
    }

    /// What this receiver threw away while the station was transmitting,
    /// phrased so it cannot be read as a fault. Empty when the operator did not
    /// key up, so it costs nothing on a receive-only session.
    fn keyed_note(&self) -> String {
        if self.win_keyed == 0 {
            return String::new();
        }
        format!(
            "; {} sample(s) discarded while keyed (expected — this receiver is not read \
             during an over); {} discarded while keyed in total",
            self.win_keyed, self.total_keyed,
        )
    }

    pub(crate) fn on_error(&mut self) {
        self.win_errors += 1;
        self.total_errors += 1;
    }

    pub(crate) fn on_stall(&mut self) {
        self.stalls += 1;
        self.on_error();
    }

    /// The receiver's sample clock measured against the host's, in ppm.
    ///
    /// **Unverified.** The RTL-SDR backend prints the same figure and tells the
    /// operator to type it into its ppm field; here the receiver's own unit is
    /// parts per *billion* and its sign convention has not been checked against
    /// hardware, so this reports the measurement and stops short of telling
    /// anybody what to do with it.
    fn clock_error(&self) -> String {
        let dt = self.first_iq.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        // Below ~20 s the host's own scheduling jitter dominates.
        if dt < 20.0 || self.total_samples == 0 || self.nominal_hz <= 0.0 {
            return "clock: measuring".to_string();
        }
        let measured = self.total_samples as f64 / dt;
        let ppm = (measured / self.nominal_hz - 1.0) * 1e6;
        format!(
            "clock: {measured:.0} sps, {ppm:+.1} ppm ({:+.0} ppb) — unverified: check it \
             against a known carrier before using it as a calibration",
            ppm * 1000.0
        )
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "{} samples, {} dropped, {} transfer errors, {} endpoint stalls; {}",
            self.total_samples,
            self.total_dropped,
            self.total_errors,
            self.stalls,
            self.clock_error()
        )
    }

    pub(crate) fn tick(&mut self, trace: &Trace) {
        let dt = self.since.elapsed();
        if dt < STATS_INTERVAL {
            return;
        }
        let ksps = self.win_samples as f64 / dt.as_secs_f64() / 1000.0;
        if self.win_dropped > 0 || self.win_errors > 0 {
            let line = format!(
                "Airspy HF+ RX: {} samples ({ksps:.1} ksps) over {:.2}s; \
                 {} sample(s) DROPPED (RX ring full — the DSP thread is not keeping up; \
                 try a lower sample rate), {} transfer error(s); \
                 totals {} dropped / {} errors{}",
                self.win_samples,
                dt.as_secs_f64(),
                self.win_dropped,
                self.win_errors,
                self.total_dropped,
                self.total_errors,
                self.keyed_note(),
            );
            tracing::warn!("{line}");
            trace.note(line);
        } else {
            tracing::debug!(
                "Airspy HF+ RX: {} samples ({ksps:.1} ksps) over {:.2}s; total {}; {}{}",
                self.win_samples,
                dt.as_secs_f64(),
                self.total_samples,
                self.clock_error(),
                self.keyed_note(),
            );
        }
        self.since = Instant::now();
        self.win_samples = 0;
        self.win_dropped = 0;
        self.win_keyed = 0;
        self.win_errors = 0;
    }
}

/// Push interleaved I/Q into the RX ring, keeping I and Q paired.
///
/// All-or-nothing: if the ring cannot take the whole block it is dropped whole.
/// Pushing what fits would leave the ring one float out of step, swapping I with
/// Q for the rest of the session — a mirrored, unusable spectrum that reads like
/// a driver bug rather than the overrun it is.
///
/// `paused` says whether the engine has stopped reading for an over, which
/// decides how a full ring is accounted for — a fault, or the normal cost of
/// transmitting. It is deliberately not a reason to skip the push: the samples
/// are still offered, and it is the reader's business whether it wants them.
pub(crate) fn push_iq(rx: &mut Producer<f32>, iq: &[f32], stats: &mut RxStats, paused: bool) {
    let Ok(mut chunk) = rx.write_chunk(iq.len()) else {
        if paused {
            stats.on_dropped_keyed(iq.len() / 2);
        } else {
            stats.on_dropped(iq.len() / 2);
        }
        return;
    };
    let (head, tail) = chunk.as_mut_slices();
    head.copy_from_slice(&iq[..head.len()]);
    tail.copy_from_slice(&iq[head.len()..]);
    chunk.commit_all();
}

/// Shared state the stream thread publishes and the handle reads.
pub(crate) struct Shared {
    pub alive: AtomicBool,
    /// Milliseconds since the thread started, at the last sample delivered.
    pub last_rx_ms: AtomicU64,
    /// The image balancer's converged phase and amplitude, scaled by 1e6 so
    /// they fit an integer. `i64::MIN` while nothing has converged yet.
    pub balance_phase_ppm: AtomicI64,
    pub balance_amplitude_ppm: AtomicI64,
    /// How many times the balancer has updated since the last retune.
    pub balance_estimates: AtomicU64,
    /// Set while the engine is transmitting and therefore not reading this
    /// receiver — see `IqSource::set_rx_paused`. Read by the stream thread on
    /// every block so a ring that fills during an over is accounted for as the
    /// cost of transmitting rather than as an overrun.
    pub rx_paused: AtomicBool,
}

impl Shared {
    pub(crate) fn new() -> Shared {
        Shared {
            alive: AtomicBool::new(true),
            last_rx_ms: AtomicU64::new(0),
            balance_phase_ppm: AtomicI64::new(i64::MIN),
            balance_amplitude_ppm: AtomicI64::new(i64::MIN),
            balance_estimates: AtomicU64::new(0),
            rx_paused: AtomicBool::new(false),
        }
    }
}

/// An open Airspy HF+.
pub struct AirspyHfHandle {
    rx: Consumer<f32>,
    ctrl: Sender<Ctrl>,
    shared: Arc<Shared>,
    opened_at: Instant,
    join: Option<JoinHandle<()>>,
    trace: Trace,

    /// Description for logs and the UI, filled in by the thread at open time.
    pub label: String,
    pub model: AirspyHfModel,
    pub firmware: String,
    pub board_serial: u64,
    /// Rates this receiver actually has, and which of them are low-IF.
    pub rates_hz: Vec<f64>,
    pub low_if: Vec<bool>,
    pub sample_rate_hz: f64,
    /// Set when the configured rate was not on offer. Surfaced to the operator
    /// through `IqSource::open_status` rather than logged and forgotten.
    pub snapped_from: Option<f64>,
    /// Attenuator range and step the receiver reported, in dB.
    pub att_max_db: f64,
    pub att_step_db: f64,
    pub bias_tee_supported: bool,
    /// The calibration in use and where it came from.
    pub calibration_ppb: i32,
    pub calibration_from_flash: bool,
}

impl AirspyHfHandle {
    /// Tell the stream thread that the engine has stopped reading for an over,
    /// and then that it has started again — see `IqSource::set_rx_paused`. The
    /// receiver itself is left running: the samples keep arriving and keep
    /// being offered, this only decides whether the ones that no longer fit are
    /// reported as a fault.
    pub fn set_rx_paused(&self, paused: bool) {
        self.shared.rx_paused.store(paused, Ordering::Relaxed);
    }

    /// Open a receiver and start streaming.
    ///
    /// The device is opened and configured on the stream thread, not here, so
    /// that every control transfer in the process happens on one thread — see
    /// the invariant in [`crate::usb`]. This call blocks until that has either
    /// succeeded or failed.
    pub fn open(cfg: &AirspyHfConfig, center_hz: f64) -> Result<AirspyHfHandle> {
        crate::stream::spawn(cfg, center_hz)
    }

    /// Whether the stream thread is still running.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// How long the receiver has gone without delivering samples, measured from
    /// the last block or — if none ever arrived — from when it was opened.
    ///
    /// A stream that never starts matters as much as one that stops, so it has
    /// to age the same way.
    pub fn silent_for(&self) -> Duration {
        let since_open = self.opened_at.elapsed();
        let last = Duration::from_millis(self.shared.last_rx_ms.load(Ordering::Relaxed));
        since_open.saturating_sub(last)
    }

    /// The image balancer's state, for the diagnostics report: the correction
    /// it has settled on and how many updates went into it. `None` until the
    /// first update, and always `None` on a low-IF rate, where it does not run.
    pub fn balance_state(&self) -> Option<(f32, f32, u64)> {
        let p = self.shared.balance_phase_ppm.load(Ordering::Relaxed);
        let a = self.shared.balance_amplitude_ppm.load(Ordering::Relaxed);
        if p == i64::MIN || a == i64::MIN {
            return None;
        }
        Some((
            p as f32 / 1e6,
            a as f32 / 1e6,
            self.shared.balance_estimates.load(Ordering::Relaxed),
        ))
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// Drain interleaved I,Q floats into `out`. Always returns an even count,
    /// so the stream can never come out of alignment. Zero means nothing is
    /// available yet.
    pub fn rx_read(&mut self, out: &mut [f32]) -> usize {
        let take = self.rx.slots().min(out.len()) & !1;
        let mut n = 0;
        while n < take {
            match self.rx.pop() {
                Ok(v) => {
                    out[n] = v;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        n
    }

    fn send(&self, c: Ctrl) {
        // A closed channel means the thread has exited; `needs_reopen` will
        // pick that up from `is_alive`, so there is nothing useful to do here.
        let _ = self.ctrl.send(c);
    }

    pub fn set_center_hz(&self, hz: f64) {
        self.send(Ctrl::Center(hz));
    }

    pub fn set_agc(&self, on: bool) {
        self.send(Ctrl::Agc(on));
    }

    pub fn set_agc_threshold(&self, high: bool) {
        self.send(Ctrl::AgcThreshold(high));
    }

    /// `db` is a gain: 0 is no attenuation, negative is attenuation.
    pub fn set_attenuator_db(&self, db: f64) {
        self.send(Ctrl::Attenuator(db));
    }

    pub fn set_lna(&self, on: bool) {
        self.send(Ctrl::Lna(on));
    }

    pub fn set_bias_tee(&self, on: bool) {
        self.send(Ctrl::BiasTee(on));
    }

    pub fn set_calibration_ppb(&self, ppb: i32) {
        self.send(Ctrl::CalibrationPpb(ppb));
    }

    pub fn set_lib_dsp(&self, on: bool) {
        self.send(Ctrl::LibDsp(on));
    }

    /// Stop the stream thread and let the receiver go, without dropping the
    /// handle.
    ///
    /// The engine needs this before it can build a replacement front-end: the
    /// USB interface is claimed exclusively and a second claim is refused even
    /// from this same process, so a receiver that has not let go is one that
    /// cannot be reopened. Blocks until the thread has closed the device.
    ///
    /// Afterwards the handle is inert rather than invalid: [`Self::rx_read`]
    /// drains what is left in the ring and then returns nothing, control
    /// messages go nowhere, and [`Self::is_alive`] is false — which is what
    /// makes `AirspyHfSource::needs_reopen` true. Idempotent.
    pub fn release(&mut self) {
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        rx: Consumer<f32>,
        ctrl: Sender<Ctrl>,
        shared: Arc<Shared>,
        join: JoinHandle<()>,
        trace: Trace,
        info: crate::stream::DeviceInfo,
    ) -> AirspyHfHandle {
        AirspyHfHandle {
            rx,
            ctrl,
            shared,
            opened_at: Instant::now(),
            join: Some(join),
            trace,
            label: info.label,
            model: info.model,
            firmware: info.firmware,
            board_serial: info.board_serial,
            rates_hz: info.rates_hz,
            low_if: info.low_if,
            sample_rate_hz: info.sample_rate_hz,
            snapped_from: info.snapped_from,
            att_max_db: info.att_max_db,
            att_step_db: info.att_step_db,
            bias_tee_supported: info.bias_tee_supported,
            calibration_ppb: info.calibration_ppb,
            calibration_from_flash: info.calibration_from_flash,
        }
    }
}

impl Drop for AirspyHfHandle {
    fn drop(&mut self) {
        self.release();
    }
}

/// Size the RX ring for a sample rate — half a second of interleaved floats,
/// rounded up to a power of two. Same formula as the RTL-SDR and HPSDR
/// backends.
pub(crate) fn ring_for(rate_hz: f64) -> (Producer<f32>, Consumer<f32>) {
    let cap = ((rate_hz * 2.0 * 0.5) as usize).next_power_of_two().max(1 << 16);
    RingBuffer::<f32>::new(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dragging the dial emits hundreds of messages a second and a retune costs
    /// two control transfers. Only the last value can matter.
    #[test]
    fn pending_keeps_only_the_last_value_of_each_field() {
        let mut p = Pending::default();
        assert!(p.is_empty());
        for hz in [7_000_000.0, 7_050_000.0, 7_074_000.0] {
            p.absorb(Ctrl::Center(hz));
        }
        p.absorb(Ctrl::Agc(true));
        p.absorb(Ctrl::Agc(false));
        p.absorb(Ctrl::Attenuator(-6.0));
        assert_eq!(p.center, Some(7_074_000.0));
        assert_eq!(p.agc, Some(false));
        assert_eq!(p.attenuator, Some(-6.0));
        assert!(!p.is_empty());
        // Fields nobody set stay unset, so `apply` does not touch the hardware
        // for settings that did not change.
        assert_eq!(p.lna, None);
        assert_eq!(p.bias_tee, None);
        assert_eq!(p.lib_dsp, None);
    }

    /// Shutdown must survive anything that arrives after it in the same batch,
    /// or a busy dial could keep the thread alive past a release.
    #[test]
    fn shutdown_is_sticky() {
        let mut p = Pending::default();
        p.absorb(Ctrl::Shutdown);
        p.absorb(Ctrl::Center(7_074_000.0));
        p.absorb(Ctrl::Lna(true));
        assert!(p.shutdown);
    }

    /// An odd ring capacity would eventually split an I/Q pair across the wrap.
    #[test]
    fn the_ring_holds_at_least_half_a_second_and_an_even_number_of_floats() {
        for rate in [192_000.0, 384_000.0, 768_000.0, 912_000.0] {
            let (p, _c) = ring_for(rate);
            let cap = p.buffer().capacity();
            assert_eq!(cap % 2, 0, "{rate}");
            assert!(cap as f64 >= rate * 2.0 * 0.5, "{rate}: {cap} floats");
        }
    }

    /// A partial push would leave the ring one float out of step and swap I
    /// with Q for the rest of the session.
    #[test]
    fn push_iq_drops_whole_blocks_rather_than_splitting_a_pair() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(8);
        let mut stats = RxStats::new(768_000.0);
        push_iq(&mut prod, &[1.0, 2.0, 3.0, 4.0], &mut stats, false);
        assert_eq!(cons.slots(), 4);
        // Six more floats into four free slots: nothing goes in.
        push_iq(&mut prod, &[0.0; 6], &mut stats, false);
        assert_eq!(cons.slots(), 4);
        assert_eq!(stats.total_dropped, 3);
        assert_eq!(cons.pop(), Ok(1.0));
    }

    /// A ring that fills because the engine stopped reading for an over is not
    /// the DSP thread falling behind, and must not reach the fault counters:
    /// that is what turned a healthy station into a warning per two seconds of
    /// transmit and a running total that only measured time on the air.
    #[test]
    fn a_full_ring_while_paused_is_not_an_overrun() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(4);
        let mut stats = RxStats::new(768_000.0);

        push_iq(&mut prod, &[1.0, 2.0, 3.0, 4.0], &mut stats, true);
        // The over: nobody is draining, so everything after this is discarded.
        push_iq(&mut prod, &[5.0, 6.0], &mut stats, true);
        assert_eq!(stats.total_dropped, 0, "a paused receiver reports no overruns");
        assert_eq!(stats.total_keyed, 1, "the discarded pair is accounted for as keyed");

        // Unpaused, the very same full ring is a genuine overrun again.
        push_iq(&mut prod, &[7.0, 8.0], &mut stats, false);
        assert_eq!(stats.total_dropped, 1);
        assert_eq!(stats.total_keyed, 1);

        // And nothing was let into the ring out of pair alignment on the way.
        while cons.pop().is_ok() {}
        push_iq(&mut prod, &[9.0, 10.0], &mut stats, false);
        assert_eq!(cons.slots() % 2, 0);
    }
}
