//! The handle the rest of the program holds, and the accounting behind it.
//!
//! Same shape as `sdroxide-airspy` and `sdroxide-rtlsdr`: one blocking thread
//! owns the receiver, control goes in over a crossbeam channel, samples come
//! back out through an `rtrb` ring of interleaved `f32`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_types::HydraSdrConfig;

use crate::error::Result;
use crate::protocol::{GainCurve, RfPort};
use crate::trace::Trace;

/// How often the stream thread emits a throughput line.
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// A control message for the stream thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Ctrl {
    Center(f64),
    Rate(f64),
    GainStep(u8),
    Curve(GainCurve),
    LnaAgc(bool),
    MixerAgc(bool),
    RfPort(RfPort),
    BiasTee(bool),
    DcBlock(bool),
    Shutdown,
}

/// Control messages accumulated over one pass of the thread loop.
///
/// A retune is one control transfer and dragging the panadapter emits hundreds
/// of `Center` messages a second. Applying each in turn would put the thread
/// permanently behind the operator's hand *and* starve the completion drain, so
/// the whole channel is collapsed into this and each field applied once, last
/// value wins — which is exactly the right semantics for a dial.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Pending {
    pub center: Option<f64>,
    pub rate: Option<f64>,
    pub gain_step: Option<u8>,
    pub curve: Option<GainCurve>,
    pub lna_agc: Option<bool>,
    pub mixer_agc: Option<bool>,
    pub rf_port: Option<RfPort>,
    pub bias_tee: Option<bool>,
    pub dc_block: Option<bool>,
    pub shutdown: bool,
}

impl Pending {
    pub(crate) fn absorb(&mut self, c: Ctrl) {
        match c {
            Ctrl::Center(v) => self.center = Some(v),
            Ctrl::Rate(v) => self.rate = Some(v),
            Ctrl::GainStep(v) => self.gain_step = Some(v),
            Ctrl::Curve(v) => self.curve = Some(v),
            Ctrl::LnaAgc(v) => self.lna_agc = Some(v),
            Ctrl::MixerAgc(v) => self.mixer_agc = Some(v),
            Ctrl::RfPort(v) => self.rf_port = Some(v),
            Ctrl::BiasTee(v) => self.bias_tee = Some(v),
            Ctrl::DcBlock(v) => self.dc_block = Some(v),
            Ctrl::Shutdown => self.shutdown = true,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        *self == Pending::default()
    }
}

/// Throughput and health accounting.
pub(crate) struct RxStats {
    nominal_hz: f64,
    /// When the first sample arrived, and the count since then. Deliberately
    /// *not* the thread start: identifying and configuring the receiver is a
    /// dozen control transfers before the first sample appears, and counting
    /// that dead time against the total biases the clock estimate low.
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
    /// The first transfer error's text, kept because it is the one fact a
    /// field report cannot be reconstructed without — and because tracing every
    /// error would push the opening sequence out of a bounded trace.
    first_error: Option<String>,
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
            first_error: None,
        }
    }

    pub(crate) fn on_iq(&mut self, pairs: usize) {
        self.win_samples += pairs as u64;
        match self.first_iq {
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

    /// Count a transfer error, and put the *first* one's text in the trace.
    ///
    /// Named once rather than every time: a wedged endpoint can produce one
    /// error per transfer, and a line apiece would flush the opening sequence
    /// out of the trace ring — which is the part of a field report that says
    /// how the receiver was configured. The count carries the rest.
    pub(crate) fn on_error(&mut self, what: &str, trace: &Trace) {
        self.win_errors += 1;
        self.total_errors += 1;
        if self.first_error.is_none() {
            self.first_error = Some(what.to_string());
            trace.note(format!("first transfer error: {what}"));
        }
    }

    pub(crate) fn on_stall(&mut self) {
        self.stalls += 1;
    }

    /// Restart the clock measurement after a discontinuity — a rate change
    /// stops and restarts the receiver, and counting the gap as slow samples
    /// would report a wildly wrong figure.
    pub(crate) fn restart_clock(&mut self, nominal_hz: f64) {
        self.first_iq = None;
        self.total_samples = 0;
        self.nominal_hz = nominal_hz;
    }

    /// The receiver's sample clock measured against the host's, in ppm.
    ///
    /// **Unverified.** This backend has not been run against hardware, and the
    /// figure is reported rather than offered as a calibration to type in
    /// anywhere.
    fn clock_error(&self) -> String {
        let dt = self.first_iq.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        // Below ~20 s the host's own scheduling jitter dominates.
        if dt < 20.0 || self.total_samples == 0 || self.nominal_hz <= 0.0 {
            return "clock: measuring".to_string();
        }
        let measured = self.total_samples as f64 / dt;
        let ppm = (measured / self.nominal_hz - 1.0) * 1e6;
        format!("clock: {measured:.0} sps, {ppm:+.1} ppm (unverified)")
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "{} samples, {} dropped, {} transfer errors{}, {} endpoint stalls; {}",
            self.total_samples,
            self.total_dropped,
            self.total_errors,
            match &self.first_error {
                Some(e) => format!(" (first: {e})"),
                None => String::new(),
            },
            self.stalls,
            self.clock_error()
        )
    }

    /// Report on the window just ended — but only when there is something worth
    /// reporting, and only in the terms that actually apply.
    ///
    /// **A silent stream is a report.** Nothing arriving at all is the most
    /// important thing this can say, and a receiver that is not sending and a
    /// host that cannot keep up are opposite faults: the trace has to tell them
    /// apart, or the next hour goes in the wrong direction.
    pub(crate) fn tick(&mut self, trace: &Trace) {
        let dt = self.since.elapsed();
        if dt < STATS_INTERVAL {
            return;
        }
        let ksps = self.win_samples as f64 / dt.as_secs_f64() / 1000.0;
        let silent = self.win_samples == 0;
        if self.win_dropped > 0 || self.win_errors > 0 || silent {
            let mut line = format!(
                "HydraSDR RX: {} samples ({ksps:.1} ksps) over {:.2}s",
                self.win_samples,
                dt.as_secs_f64(),
            );
            if silent {
                line.push_str(
                    "; NOTHING ARRIVED — the receiver is not sending, which is a \
                     different fault from not keeping up",
                );
            }
            if self.win_dropped > 0 {
                line.push_str(&format!(
                    "; {} sample(s) DROPPED (RX ring full — the DSP thread is not \
                     keeping up; try a lower sample rate)",
                    self.win_dropped
                ));
            }
            if self.win_errors > 0 {
                line.push_str(&format!(
                    "; {} transfer error(s){}",
                    self.win_errors,
                    match &self.first_error {
                        Some(e) => format!(", first was {e}"),
                        None => String::new(),
                    }
                ));
            }
            line.push_str(&format!(
                "; totals {} dropped / {} errors",
                self.total_dropped, self.total_errors
            ));
            line.push_str(&self.keyed_note());
            tracing::warn!("{line}");
            trace.note(line);
        } else {
            tracing::debug!(
                "HydraSDR RX: {} samples ({ksps:.1} ksps) over {:.2}s; total {}; {}{}",
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
    /// The complex rate in use, in milli-hertz so a rate change is visible to
    /// the handle without a lock.
    pub rate_milli_hz: AtomicU64,
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
            rate_milli_hz: AtomicU64::new(0),
            rx_paused: AtomicBool::new(false),
        }
    }
}

/// What the thread learned about the receiver while opening it.
pub(crate) struct DeviceInfo {
    pub label: String,
    pub firmware: String,
    pub board: String,
    pub serial: Option<String>,
    pub legacy_usb_id: bool,
    pub sample_rate_hz: f64,
    pub rates_hz: Vec<f64>,
    pub snapped_from: Option<f64>,
    pub packing_active: bool,
    pub rf_port_active: bool,
}

/// An open HydraSDR RFOne.
pub struct HydraSdrHandle {
    rx: Consumer<f32>,
    ctrl: Sender<Ctrl>,
    shared: Arc<Shared>,
    opened_at: Instant,
    join: Option<JoinHandle<()>>,
    trace: Trace,

    /// Description for logs and the UI, filled in by the thread at open time.
    pub label: String,
    pub firmware: String,
    /// The board the firmware named — which build of the RFOne this is, not
    /// which radio: a prototype and an Airspy R2 both call themselves board 0.
    pub board: String,
    pub serial: Option<String>,
    /// Whether this board came up on `1d50:60a1`, the pair it shares with the
    /// Airspy R2. Worth surfacing: it is the one thing about this receiver that
    /// can send somebody to the wrong interface.
    pub legacy_usb_id: bool,
    /// The **complex** rate the host produces.
    pub sample_rate_hz: f64,
    /// Every complex rate this receiver offers.
    pub rates_hz: Vec<f64>,
    /// Set when the configured rate was not on offer. Surfaced through
    /// `IqSource::open_status` rather than logged and forgotten.
    pub snapped_from: Option<f64>,
    /// Whether 12-bit packing is really in use — older firmware has no such
    /// request and streams unpacked.
    pub packing_active: bool,
    /// Whether the RF input could be commanded. A firmware without the request
    /// leaves the receiver on `ANT`.
    pub rf_port_active: bool,
}

impl HydraSdrHandle {
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
    pub fn open(cfg: &HydraSdrConfig, center_hz: f64) -> Result<HydraSdrHandle> {
        crate::stream::spawn(cfg, center_hz)
    }

    /// Whether the stream thread is still running.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// The complex rate currently in use, which a rate change moves under the
    /// caller.
    pub fn current_rate_hz(&self) -> f64 {
        let m = self.shared.rate_milli_hz.load(Ordering::Relaxed);
        if m == 0 { self.sample_rate_hz } else { m as f64 / 1000.0 }
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

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// Drain interleaved I,Q floats into `out`. Always returns an even count,
    /// so the stream can never come out of alignment. Zero means nothing is
    /// available yet.
    pub fn rx_read(&mut self, out: &mut [f32]) -> usize {
        let take = self.rx.slots().min(out.len()) & !1;
        if take == 0 {
            return 0;
        }
        let Ok(chunk) = self.rx.read_chunk(take) else {
            return 0;
        };
        let (head, tail) = chunk.as_slices();
        out[..head.len()].copy_from_slice(head);
        out[head.len()..take].copy_from_slice(&tail[..take - head.len()]);
        chunk.commit_all();
        take
    }

    fn send(&self, c: Ctrl) {
        // A closed channel means the thread has exited; `needs_reopen` will
        // pick that up from `is_alive`, so there is nothing useful to do here.
        let _ = self.ctrl.send(c);
    }

    pub fn set_center_hz(&self, hz: f64) {
        self.send(Ctrl::Center(hz));
    }

    pub fn set_rate_hz(&self, hz: f64) {
        self.send(Ctrl::Rate(hz));
    }

    pub fn set_gain_step(&self, step: u8) {
        self.send(Ctrl::GainStep(step));
    }

    pub fn set_curve(&self, curve: GainCurve) {
        self.send(Ctrl::Curve(curve));
    }

    pub fn set_lna_agc(&self, on: bool) {
        self.send(Ctrl::LnaAgc(on));
    }

    pub fn set_mixer_agc(&self, on: bool) {
        self.send(Ctrl::MixerAgc(on));
    }

    pub fn set_rf_port(&self, port: RfPort) {
        self.send(Ctrl::RfPort(port));
    }

    pub fn set_bias_tee(&self, on: bool) {
        self.send(Ctrl::BiasTee(on));
    }

    pub fn set_dc_block(&self, on: bool) {
        self.send(Ctrl::DcBlock(on));
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
    /// messages go nowhere, and [`Self::is_alive`] is false. Idempotent.
    pub fn release(&mut self) {
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    pub(crate) fn from_parts(
        rx: Consumer<f32>,
        ctrl: Sender<Ctrl>,
        shared: Arc<Shared>,
        join: JoinHandle<()>,
        trace: Trace,
        info: DeviceInfo,
    ) -> HydraSdrHandle {
        HydraSdrHandle {
            rx,
            ctrl,
            shared,
            opened_at: Instant::now(),
            join: Some(join),
            trace,
            label: info.label,
            firmware: info.firmware,
            board: info.board,
            serial: info.serial,
            legacy_usb_id: info.legacy_usb_id,
            sample_rate_hz: info.sample_rate_hz,
            rates_hz: info.rates_hz,
            snapped_from: info.snapped_from,
            packing_active: info.packing_active,
            rf_port_active: info.rf_port_active,
        }
    }
}

impl Drop for HydraSdrHandle {
    fn drop(&mut self) {
        self.release();
    }
}

/// Size the RX ring for a complex rate — half a second of interleaved floats,
/// rounded up to a power of two.
///
/// Half a second is affordable here: the top complex rate is 12 Msps, so this is
/// 48 MB at worst — and the rate cannot be raised beyond what the firmware has,
/// so the figure is bounded by the hardware rather than by a config field.
pub(crate) fn ring_for(rate_hz: f64) -> (Producer<f32>, Consumer<f32>) {
    let cap = ((rate_hz * 2.0 * 0.5) as usize).next_power_of_two().max(1 << 16);
    RingBuffer::<f32>::new(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dragging the dial emits hundreds of messages a second and a retune costs
    /// a control transfer. Only the last value can matter.
    #[test]
    fn pending_keeps_only_the_last_value_of_each_field() {
        let mut p = Pending::default();
        assert!(p.is_empty());
        for hz in [100.0e6, 100.5e6, 101.0e6] {
            p.absorb(Ctrl::Center(hz));
        }
        p.absorb(Ctrl::GainStep(3));
        p.absorb(Ctrl::GainStep(9));
        p.absorb(Ctrl::Curve(GainCurve::Sensitivity));
        p.absorb(Ctrl::RfPort(RfPort::Cable2));
        assert_eq!(p.center, Some(101.0e6));
        assert_eq!(p.gain_step, Some(9));
        assert_eq!(p.curve, Some(GainCurve::Sensitivity));
        assert_eq!(p.rf_port, Some(RfPort::Cable2));
        assert!(!p.is_empty());
        // Fields nobody set stay unset, so `apply` does not touch the hardware
        // for settings that did not change.
        assert_eq!(p.rate, None);
        assert_eq!(p.bias_tee, None);
        assert_eq!(p.lna_agc, None);
    }

    /// Shutdown must survive anything that arrives after it in the same batch,
    /// or a busy dial could keep the thread alive past a release.
    #[test]
    fn shutdown_is_sticky() {
        let mut p = Pending::default();
        p.absorb(Ctrl::Shutdown);
        p.absorb(Ctrl::Center(100.0e6));
        p.absorb(Ctrl::Rate(10.0e6));
        assert!(p.shutdown);
    }

    /// An odd ring capacity would eventually split an I/Q pair across the wrap.
    #[test]
    fn the_ring_holds_at_least_half_a_second_and_an_even_number_of_floats() {
        for rate in crate::protocol::ALL_RATES {
            let (p, _c) = ring_for(rate);
            let cap = p.buffer().capacity();
            assert_eq!(cap % 2, 0, "{rate}");
            assert!(cap as f64 >= rate * 2.0 * 0.5, "{rate}: {cap} floats");
        }
    }

    /// A receiver that sends nothing has to say so, in its own words — the
    /// opposite fault from one the host cannot keep up with.
    #[test]
    fn a_silent_window_says_nothing_arrived_and_does_not_blame_the_ring() {
        let trace = Trace::new();
        let mut stats = RxStats::new(3.0e6);
        stats.since = Instant::now() - STATS_INTERVAL - Duration::from_millis(10);
        stats.tick(&trace);

        let dump = trace.dump();
        assert!(dump.contains("NOTHING ARRIVED"), "{dump}");
        assert!(!dump.contains("DROPPED"), "nothing was dropped: {dump}");

        // The first error is named once, and only once, however many follow.
        let mut stats = RxStats::new(3.0e6);
        let trace = Trace::new();
        stats.on_error("Unknown(0xe00002ed)", &trace);
        stats.on_error("Unknown(0xe00002ed)", &trace);
        stats.on_error("Stall", &trace);
        assert_eq!(stats.total_errors, 3);
        assert_eq!(dump_lines(&trace, "first transfer error"), 1);
        assert!(trace.dump().contains("0xe00002ed"));

        // And a window with samples and no trouble stays quiet.
        let trace = Trace::new();
        let mut stats = RxStats::new(3.0e6);
        stats.on_iq(4096);
        stats.since = Instant::now() - STATS_INTERVAL - Duration::from_millis(10);
        stats.tick(&trace);
        assert!(!trace.dump().contains("HydraSDR RX:"), "{}", trace.dump());
    }

    fn dump_lines(trace: &Trace, needle: &str) -> usize {
        trace.dump().lines().filter(|l| l.contains(needle)).count()
    }

    /// A partial push would leave the ring one float out of step and swap I
    /// with Q for the rest of the session.
    #[test]
    fn push_iq_drops_whole_blocks_rather_than_splitting_a_pair() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(8);
        let mut stats = RxStats::new(2.5e6);
        push_iq(&mut prod, &[1.0, 2.0, 3.0, 4.0], &mut stats, false);
        assert_eq!(cons.slots(), 4);
        // Six more floats into four free slots: nothing goes in.
        push_iq(&mut prod, &[0.0; 6], &mut stats, false);
        assert_eq!(cons.slots(), 4);
        assert_eq!(stats.total_dropped, 3);
        assert_eq!(cons.pop(), Ok(1.0));
    }

    /// A rate change stops and restarts the receiver; counting the gap as slow
    /// samples would report a wildly wrong clock error.
    #[test]
    fn the_clock_measurement_restarts_across_a_rate_change() {
        let mut s = RxStats::new(2.5e6);
        s.on_iq(1000);
        s.on_iq(1000);
        assert!(s.total_samples > 0);
        s.restart_clock(10.0e6);
        assert_eq!(s.total_samples, 0);
        assert!(s.summary().contains("measuring"), "{}", s.summary());
    }

    /// A ring that fills because the engine stopped reading for an over is not
    /// the DSP thread falling behind, and must not reach the fault counters:
    /// that is what turned a healthy station into a warning per two seconds of
    /// transmit and a running total that only measured time on the air.
    #[test]
    fn a_full_ring_while_paused_is_not_an_overrun() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(4);
        let mut stats = RxStats::new(3.0e6);

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
