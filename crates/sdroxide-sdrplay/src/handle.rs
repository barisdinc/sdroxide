//! The handle the rest of the program holds, and the accounting behind it.
//!
//! Same shape as the RTL-SDR and RX-888 backends: a control thread owns the
//! device session, control goes in over a crossbeam channel, samples come
//! back out through an `rtrb` ring of interleaved `f32`. The one structural
//! difference is that samples are not pulled off a USB endpoint here — the
//! vendor service pushes them into a callback on a thread it owns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_types::SdrPlayModel;

/// How often the callback emits a throughput line.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// A control message for the session thread.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ctrl {
    Center(f64),
    /// IF gain reduction in dB (the RSP's native unit, 20..=59).
    IfGr(i32),
    Lna(u8),
    /// [`sdroxide_types::SdrPlayAgc::code`] value.
    Agc(i32),
    AgcSetpoint(i32),
    Ppm(f64),
    BiasTee(bool),
    RfNotch(bool),
    DabNotch(bool),
    Hdr(bool),
    Antenna(String),
    Shutdown,
}

/// Control messages accumulated over one pass of the session loop. Dragging
/// the dial or a slider emits far more messages than `sdrplay_api_Update`
/// round-trips per second; last value wins, which is the right semantics for
/// every one of these.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Pending {
    pub center: Option<f64>,
    pub if_gr: Option<i32>,
    pub lna: Option<u8>,
    pub agc: Option<i32>,
    pub agc_setpoint: Option<i32>,
    pub ppm: Option<f64>,
    pub bias_tee: Option<bool>,
    pub rf_notch: Option<bool>,
    pub dab_notch: Option<bool>,
    pub hdr: Option<bool>,
    pub antenna: Option<String>,
    pub shutdown: bool,
}

impl Pending {
    pub(crate) fn absorb(&mut self, c: Ctrl) {
        match c {
            Ctrl::Center(v) => self.center = Some(v),
            Ctrl::IfGr(v) => self.if_gr = Some(v),
            Ctrl::Lna(v) => self.lna = Some(v),
            Ctrl::Agc(v) => self.agc = Some(v),
            Ctrl::AgcSetpoint(v) => self.agc_setpoint = Some(v),
            Ctrl::Ppm(v) => self.ppm = Some(v),
            Ctrl::BiasTee(v) => self.bias_tee = Some(v),
            Ctrl::RfNotch(v) => self.rf_notch = Some(v),
            Ctrl::DabNotch(v) => self.dab_notch = Some(v),
            Ctrl::Hdr(v) => self.hdr = Some(v),
            Ctrl::Antenna(v) => self.antenna = Some(v),
            Ctrl::Shutdown => self.shutdown = true,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        *self == Pending::default()
    }
}

/// State the session thread and the API's callbacks publish for the handle.
pub(crate) struct Shared {
    pub alive: AtomicBool,
    /// Timestamp of the newest samples, in ms since the session epoch — a
    /// timestamp, not an age; see [`SdrPlayHandle::silent_for`].
    pub last_rx_ms: AtomicU64,
    /// Samples the ring could not take because the consumer fell behind.
    pub dropped: AtomicU64,
    /// The service said the ADC is overloaded (and has not yet said corrected).
    pub overload: AtomicBool,
    /// An overload message is waiting for the mandatory acknowledgement,
    /// which must come from the session thread rather than the callback.
    pub overload_ack_pending: AtomicBool,
    /// The device disappeared mid-session (unplug or service failure).
    pub removed: AtomicBool,
    /// What the GainChange event last reported, `i64::MIN` for "not yet".
    /// Written by the event callback on every gain update — including the
    /// AGC's own — which is what keeps `current_gains()` honest while the
    /// hardware, not the operator, is moving the gain.
    pub ev_gr_db: AtomicI64,
    pub ev_lna_gr_db: AtomicI64,
    /// System gain from the same event, in milli-dB.
    pub ev_curr_gain_milli_db: AtomicI64,
    /// The LNA state actually programmed, after any per-band clamp.
    pub lna_state: AtomicU8,
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
            dropped: AtomicU64::new(0),
            overload: AtomicBool::new(false),
            overload_ack_pending: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            ev_gr_db: AtomicI64::new(i64::MIN),
            ev_lna_gr_db: AtomicI64::new(i64::MIN),
            ev_curr_gain_milli_db: AtomicI64::new(i64::MIN),
            lna_state: AtomicU8::new(0),
            rx_paused: AtomicBool::new(false),
        }
    }
}

/// Throughput and health accounting, ticked from the stream callback.
pub(crate) struct RxStats {
    nominal_hz: f64,
    since: Instant,
    win_samples: u64,
    win_dropped: u64,
    /// Discarded while the engine was not reading this receiver because the
    /// station was transmitting. Counted apart from `win_dropped` because it
    /// is not a fault: see [`RxStats::on_dropped_keyed`].
    win_keyed: u64,
}

impl RxStats {
    pub(crate) fn new(nominal_hz: f64) -> RxStats {
        RxStats { nominal_hz, since: Instant::now(), win_samples: 0, win_dropped: 0, win_keyed: 0 }
    }

    pub(crate) fn on_iq(&mut self, samples: usize) {
        self.win_samples += samples as u64;
    }

    pub(crate) fn on_dropped(&mut self, samples: usize) {
        self.win_dropped += samples as u64;
    }

    /// Record `samples` complex samples discarded because the ring was full
    /// while the station was transmitting.
    ///
    /// This receiver does not transmit, so this is the case where it is
    /// somebody else's panadapter: the engine stops reading a half-duplex
    /// source for the length of an over and empties the ring on unkey, while
    /// this one carries on streaming throughout. The ring fills within its own
    /// depth of key-down and everything after that is discarded — expected, at
    /// exactly the sample rate, for as long as the operator transmits. Counting
    /// it as an overrun turns an ordinary over into a warning that blames the
    /// DSP thread. See `IqSource::set_rx_paused`.
    pub(crate) fn on_dropped_keyed(&mut self, samples: usize) {
        self.win_keyed += samples as u64;
    }

    /// What this receiver threw away while the station was transmitting,
    /// phrased so it cannot be read as a fault. Empty when the operator did not
    /// key up, so it costs nothing on a receive-only session.
    fn keyed_note(&self) -> String {
        if self.win_keyed == 0 {
            return String::new();
        }
        format!(
            ", {} discarded while keyed (expected — this receiver is not read during \
             an over)",
            self.win_keyed,
        )
    }

    pub(crate) fn tick(&mut self) {
        let elapsed = self.since.elapsed();
        if elapsed < STATS_INTERVAL {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let rate = self.win_samples as f64 / secs;
        if self.win_dropped > 0 {
            tracing::warn!(
                "SDRplay: {:.3} Msps ({:.1}% of nominal), {} samples dropped{}",
                rate / 1e6,
                100.0 * rate / self.nominal_hz,
                self.win_dropped,
                self.keyed_note(),
            );
        } else {
            tracing::debug!(
                "SDRplay: {:.3} Msps ({:.1}% of nominal)",
                rate / 1e6,
                100.0 * rate / self.nominal_hz
            );
        }
        self.since = Instant::now();
        self.win_samples = 0;
        self.win_dropped = 0;
        self.win_keyed = 0;
    }
}

/// Push interleaved complex samples into the ring, counting what will not
/// fit. Dropping beats blocking: the thread being served here belongs to the
/// vendor service, and stalling it stalls the hardware.
pub(crate) fn push_iq(
    ring: &mut Producer<f32>,
    data: &[f32],
    stats: &mut RxStats,
    paused: bool,
) -> usize {
    let mut written = 0;
    // Rounded down to a whole number of I/Q pairs. `slots()` is a count of
    // floats and is under no obligation to be even, so taking it at face value
    // would eventually commit a lone I with its Q dropped — and from then on
    // every pair in the ring is one float out of step, which swaps I with Q for
    // the rest of the session and mirrors the spectrum. That reads like a
    // driver bug rather than the overrun it is.
    let room = ring.slots() & !1;
    if let Ok(mut chunk) = ring.write_chunk_uninit(data.len().min(room)) {
        let (a, b) = chunk.as_mut_slices();
        let split = a.len();
        for (dst, src) in a.iter_mut().zip(&data[..split.min(data.len())]) {
            dst.write(*src);
        }
        if data.len() > split {
            for (dst, src) in b.iter_mut().zip(&data[split..]) {
                dst.write(*src);
            }
        }
        written = a.len() + b.len();
        unsafe { chunk.commit_all() };
    }
    stats.on_iq(written / 2);
    let short = (data.len() - written) / 2;
    if short == 0 {
        return 0;
    }
    // While the engine is transmitting it is not reading this receiver at all,
    // so a full ring is the ordinary cost of an over rather than a host that
    // fell behind — and the caller's fault counter must not collect it either.
    // See `IqSource::set_rx_paused`.
    if paused {
        stats.on_dropped_keyed(short);
        return 0;
    }
    stats.on_dropped(short);
    short
}

/// Ring sized for half a second of interleaved `f32` at `rate`, rounded up to
/// a power of two.
pub(crate) fn ring_for(rate: f64) -> (Producer<f32>, Consumer<f32>) {
    let slots = ((rate * 2.0 * 0.5) as usize).next_power_of_two().clamp(1 << 14, 1 << 24);
    RingBuffer::new(slots)
}

/// A running RSP.
pub struct SdrPlayHandle {
    rx: Consumer<f32>,
    ctrl: Sender<Ctrl>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    label: String,
    serial: String,
    model: SdrPlayModel,
    out_rate_hz: f64,
    analog_bw_hz: f64,
    opened_at: Instant,
    released: bool,
}

impl SdrPlayHandle {
    /// Tell the stream thread that the engine has stopped reading for an over,
    /// and then that it has started again — see `IqSource::set_rx_paused`. The
    /// receiver itself is left running: the samples keep arriving and keep
    /// being offered, this only decides whether the ones that no longer fit are
    /// reported as a fault.
    pub fn set_rx_paused(&self, paused: bool) {
        self.shared.rx_paused.store(paused, Ordering::Relaxed);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        rx: Consumer<f32>,
        ctrl: Sender<Ctrl>,
        shared: Arc<Shared>,
        join: JoinHandle<()>,
        label: String,
        serial: String,
        model: SdrPlayModel,
        out_rate_hz: f64,
        analog_bw_hz: f64,
    ) -> SdrPlayHandle {
        SdrPlayHandle {
            rx,
            ctrl,
            shared,
            join: Some(join),
            label,
            serial,
            model,
            out_rate_hz,
            analog_bw_hz,
            opened_at: Instant::now(),
            released: false,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn model(&self) -> SdrPlayModel {
        self.model
    }

    /// The effective complex rate delivered, after any decimation.
    pub fn out_rate_hz(&self) -> f64 {
        self.out_rate_hz
    }

    /// The programmed analog IF bandwidth, for the LO-offset calculation.
    pub fn analog_bw_hz(&self) -> f64 {
        self.analog_bw_hz
    }

    /// Whether the session thread is still running.
    pub fn is_alive(&self) -> bool {
        !self.released && self.shared.alive.load(Ordering::Relaxed)
    }

    /// The service reported the device gone (unplugged, or the service died).
    pub fn removed(&self) -> bool {
        self.shared.removed.load(Ordering::Relaxed)
    }

    /// The ADC is currently overloaded — gain is set too high for the input.
    pub fn overloaded(&self) -> bool {
        self.shared.overload.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// How long the receiver has delivered nothing. `last_rx_ms` is a
    /// timestamp from the session epoch, not an age — subtract, don't read.
    pub fn silent_for(&self) -> Duration {
        let since_open = self.opened_at.elapsed();
        let last = Duration::from_millis(self.shared.last_rx_ms.load(Ordering::Relaxed));
        since_open.saturating_sub(last)
    }

    /// IF gain reduction the hardware last reported, if it has reported one.
    /// Under AGC this moves on its own, and it is the honest answer.
    pub fn effective_if_gr_db(&self) -> Option<i32> {
        match self.shared.ev_gr_db.load(Ordering::Relaxed) {
            i64::MIN => None,
            v => Some(v as i32),
        }
    }

    /// The LNA state actually programmed, after any per-band clamp.
    pub fn lna_state(&self) -> u8 {
        self.shared.lna_state.load(Ordering::Relaxed)
    }

    /// Read interleaved complex samples. Returns how many `f32` were taken.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let n = out.len().min(self.rx.slots());
        // Keep I and Q together: an odd count would swap them for good.
        let n = n - (n % 2);
        if n == 0 {
            return 0;
        }
        match self.rx.read_chunk(n) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                out[..a.len()].copy_from_slice(a);
                out[a.len()..a.len() + b.len()].copy_from_slice(b);
                chunk.commit_all();
                n
            }
            Err(_) => 0,
        }
    }

    pub fn set_center_hz(&self, hz: f64) {
        let _ = self.ctrl.send(Ctrl::Center(hz));
    }

    pub fn set_if_gr_db(&self, gr: i32) {
        let _ = self.ctrl.send(Ctrl::IfGr(gr));
    }

    pub fn set_lna_state(&self, state: u8) {
        let _ = self.ctrl.send(Ctrl::Lna(state));
    }

    pub fn set_agc(&self, code: i32) {
        let _ = self.ctrl.send(Ctrl::Agc(code));
    }

    pub fn set_agc_setpoint(&self, dbfs: i32) {
        let _ = self.ctrl.send(Ctrl::AgcSetpoint(dbfs));
    }

    pub fn set_ppm(&self, ppm: f64) {
        let _ = self.ctrl.send(Ctrl::Ppm(ppm));
    }

    pub fn set_bias_tee(&self, on: bool) {
        let _ = self.ctrl.send(Ctrl::BiasTee(on));
    }

    pub fn set_rf_notch(&self, on: bool) {
        let _ = self.ctrl.send(Ctrl::RfNotch(on));
    }

    pub fn set_dab_notch(&self, on: bool) {
        let _ = self.ctrl.send(Ctrl::DabNotch(on));
    }

    pub fn set_hdr(&self, on: bool) {
        let _ = self.ctrl.send(Ctrl::Hdr(on));
    }

    pub fn set_antenna(&self, name: &str) {
        let _ = self.ctrl.send(Ctrl::Antenna(name.to_string()));
    }

    /// Stop the session and hand the device back to the service.
    ///
    /// Idempotent, and leaves the handle callable but inert — the engine's
    /// reopen path requires exactly that, because a replacement is built only
    /// after the outgoing source has stood down.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.shared.alive.store(false, Ordering::Relaxed);
    }
}

impl Drop for SdrPlayHandle {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_keeps_only_the_last_value_per_field() {
        let mut p = Pending::default();
        assert!(p.is_empty());
        p.absorb(Ctrl::Center(7_000_000.0));
        p.absorb(Ctrl::Center(14_000_000.0));
        p.absorb(Ctrl::IfGr(30));
        p.absorb(Ctrl::Antenna("Antenna A".into()));
        p.absorb(Ctrl::Antenna("Antenna B".into()));
        assert_eq!(p.center, Some(14_000_000.0));
        assert_eq!(p.if_gr, Some(30));
        assert_eq!(p.antenna.as_deref(), Some("Antenna B"));
        assert_eq!(p.lna, None);
        assert!(!p.is_empty());
    }

    #[test]
    fn shutdown_survives_later_messages() {
        let mut p = Pending::default();
        p.absorb(Ctrl::Shutdown);
        p.absorb(Ctrl::Center(1.0));
        assert!(p.shutdown, "a shutdown must not be forgotten by a later retune");
    }

    #[test]
    fn the_ring_holds_about_half_a_second() {
        let (p, _c) = ring_for(2_000_000.0);
        assert!(p.slots() >= 2_000_000);
        assert!(p.slots().is_power_of_two());
    }

    fn test_handle(cons: Consumer<f32>) -> SdrPlayHandle {
        let (tx, _rx) = crossbeam_channel::unbounded();
        SdrPlayHandle::from_parts(
            cons,
            tx,
            Arc::new(Shared::new()),
            std::thread::spawn(|| {}),
            "test".into(),
            "0000000000".into(),
            SdrPlayModel::Rsp1b,
            2_000_000.0,
            1_536_000.0,
        )
    }

    #[test]
    fn reads_never_split_an_iq_pair() {
        let (mut prod, cons) = RingBuffer::<f32>::new(64);
        for i in 0..9 {
            prod.push(i as f32).unwrap();
        }
        let mut h = test_handle(cons);
        let mut buf = [0f32; 32];
        // Nine samples are available; an even count must come back.
        let n = h.read(&mut buf);
        assert_eq!(n % 2, 0, "read {n} f32, which splits an I/Q pair");
        assert_eq!(n, 8);
        h.released = true; // do not try to join in Drop
    }

    #[test]
    fn silence_is_an_age_not_a_timestamp() {
        let (_p, cons) = RingBuffer::<f32>::new(16);
        let mut h = test_handle(cons);
        let age = h.opened_at.elapsed();
        h.shared.last_rx_ms.store(age.as_millis() as u64, Ordering::Relaxed);
        assert!(h.silent_for() < Duration::from_millis(50), "{:?}", h.silent_for());
        h.released = true;
    }

    #[test]
    fn effective_gain_is_unknown_until_an_event_reports_one() {
        let (_p, cons) = RingBuffer::<f32>::new(16);
        let mut h = test_handle(cons);
        assert_eq!(h.effective_if_gr_db(), None);
        h.shared.ev_gr_db.store(42, Ordering::Relaxed);
        assert_eq!(h.effective_if_gr_db(), Some(42));
        h.released = true;
    }

    /// The partial writer must never commit a lone I with its Q left behind.
    /// `slots()` is a count of floats and is under no obligation to be even, so
    /// taking it at face value eventually puts the ring one float out of step —
    /// and from then on every pair is swapped and the spectrum is mirrored for
    /// the rest of the session.
    #[test]
    fn a_partial_write_still_lands_on_a_pair_boundary() {
        let (mut prod, cons) = RingBuffer::<f32>::new(8);
        let mut stats = RxStats::new(2.0e6);
        // Leave an odd three slots free, then offer four floats.
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            prod.push(v).expect("room");
        }
        let dropped = push_iq(&mut prod, &[6.0, 7.0, 8.0, 9.0], &mut stats, false);
        assert_eq!(cons.slots(), 7, "only one whole pair fitted the three free slots");
        assert_eq!(dropped, 1, "the pair that did not fit is one complex sample");
    }

    /// A ring that fills because the engine stopped reading for an over is not
    /// the DSP thread falling behind, and must not reach the fault counters —
    /// nor the count this hands back for the handle's `dropped()` total.
    #[test]
    fn a_full_ring_while_paused_is_not_an_overrun() {
        let (mut prod, cons) = RingBuffer::<f32>::new(4);
        let mut stats = RxStats::new(2.0e6);

        push_iq(&mut prod, &[1.0, 2.0, 3.0, 4.0], &mut stats, true);
        let dropped = push_iq(&mut prod, &[5.0, 6.0], &mut stats, true);
        assert_eq!(dropped, 0, "a paused receiver reports no overruns to its caller");
        assert_eq!(stats.win_dropped, 0);
        assert_eq!(stats.win_keyed, 1, "the discarded pair is accounted for as keyed");

        // Unpaused, the very same full ring is a genuine overrun again.
        let dropped = push_iq(&mut prod, &[7.0, 8.0], &mut stats, false);
        assert_eq!(dropped, 1);
        assert_eq!(stats.win_dropped, 1);
        assert_eq!(stats.win_keyed, 1);
        assert_eq!(cons.slots() % 2, 0);
    }
}
