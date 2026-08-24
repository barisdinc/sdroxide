//! The handle the rest of the program holds, and the accounting behind it.
//!
//! Same shape as [`sdroxide_rtlsdr::RtlSdrHandle`]: one blocking thread owns
//! the connection, control goes in over a crossbeam channel, samples come back
//! out through an `rtrb` ring of interleaved `f32`.
//!
//! What is new here is a *second* output lane. This protocol can send a
//! low-rate FFT of the whole band alongside the I/Q, and that lane wants
//! different handling: a couple of kilobytes a dozen times a second, of which
//! only the newest matters. So it rides a mutex-guarded slot rather than a
//! ring — see [`Shared::fft`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_types::SpyServerConfig;

use crate::proto::DeviceInfo;

/// How often the stream thread emits a throughput line.
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// One finished full-band frame from the server's FFT stream.
///
/// The centre and span travel *with* the bins rather than in separate
/// accessors. They have to: the FFT window moves, and a frame decoded just
/// before a recentre would otherwise be labelled with the centre from just
/// after it — a picture drawn at the wrong frequency, silently.
#[derive(Debug, Clone, PartialEq)]
pub struct FftFrame {
    pub center_hz: f64,
    pub span_hz: f64,
    /// dBFS, ascending from the low edge.
    pub bins: Vec<f32>,
}

/// A control message for the stream thread.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ctrl {
    Center(f64),
    /// The server's gain *index*, not a number of dB.
    Gain(u32),
    AutoDigitalGain(bool),
    DigitalGain(f64),
    FftEnabled(bool),
    FftDecimation(u32),
    FftDbOffset(f64),
    FftDbRange(f64),
    Shutdown,
}

/// Control messages accumulated over one pass of the thread loop.
///
/// Dragging the panadapter emits hundreds of `Center` messages a second and
/// the far end will accept eight; applying each in turn would put the thread
/// permanently behind the operator's hand. The whole channel is collapsed into
/// this and each field applied once, last value wins — the right semantics for
/// a dial.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Pending {
    pub center: Option<f64>,
    pub gain: Option<u32>,
    pub auto_digital_gain: Option<bool>,
    pub digital_gain: Option<f64>,
    pub fft_enabled: Option<bool>,
    pub fft_decimation: Option<u32>,
    pub fft_db_offset: Option<f64>,
    pub fft_db_range: Option<f64>,
    pub shutdown: bool,
}

impl Pending {
    pub(crate) fn absorb(&mut self, c: Ctrl) {
        match c {
            Ctrl::Center(v) => self.center = Some(v),
            Ctrl::Gain(v) => self.gain = Some(v),
            Ctrl::AutoDigitalGain(v) => self.auto_digital_gain = Some(v),
            Ctrl::DigitalGain(v) => self.digital_gain = Some(v),
            Ctrl::FftEnabled(v) => self.fft_enabled = Some(v),
            Ctrl::FftDecimation(v) => self.fft_decimation = Some(v),
            Ctrl::FftDbOffset(v) => self.fft_db_offset = Some(v),
            Ctrl::FftDbRange(v) => self.fft_db_range = Some(v),
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
    since: Instant,
    win_samples: u64,
    win_dropped: u64,
    /// Discarded while the engine was not reading this receiver because the
    /// station was transmitting. Counted apart from `win_dropped` because it
    /// is not a fault: see [`RxStats::on_dropped_keyed`].
    win_keyed: u64,
    win_gaps: u64,
    total_samples: u64,
    total_dropped: u64,
    total_keyed: u64,
    total_gaps: u64,
}

impl RxStats {
    /// Accounting for samples that arrived over a socket.
    ///
    /// Deliberately without the clock-error estimate the USB backends print.
    /// Counting samples against elapsed time over a network measures the
    /// *buffering*, not the crystal: the difference is however many samples sit
    /// in the server's queue, the kernel's and the ring at the moment of the
    /// reading, and that moves by tens of milliseconds between readings.
    /// Measured against `rtl_tcp` on loopback — the most favourable link there
    /// is — successive readings ranged over three thousand ppm. A number that
    /// looks like a measurement and is off by three orders of magnitude is
    /// worse than no number.
    pub(crate) fn network(nominal_hz: f64) -> RxStats {
        RxStats {
            nominal_hz,
            since: Instant::now(),
            win_samples: 0,
            win_dropped: 0,
            win_keyed: 0,
            win_gaps: 0,
            total_samples: 0,
            total_dropped: 0,
            total_keyed: 0,
            total_gaps: 0,
        }
    }

    pub(crate) fn on_iq(&mut self, pairs: usize) {
        self.win_samples += pairs as u64;
        self.total_samples += pairs as u64;
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

    /// The server skipped I/Q messages because this client fell behind. Unlike
    /// a ring overrun this happened on the *far* side of the link, and the
    /// protocol's sequence numbers are the only reason we can know at all.
    pub(crate) fn on_gap(&mut self, messages: u64) {
        self.win_gaps += messages;
        self.total_gaps += messages;
    }

    pub(crate) fn tick(&mut self) {
        let dt = self.since.elapsed();
        if dt < STATS_INTERVAL {
            return;
        }
        let ksps = self.win_samples as f64 / dt.as_secs_f64() / 1000.0;
        if self.win_dropped > 0 || self.win_gaps > 0 {
            tracing::warn!(
                "SpyServer RX: {} samples ({ksps:.1} ksps of {:.1} nominal) over {:.2}s; \
                 {} sample(s) dropped here (RX ring full — the DSP thread is not keeping up), \
                 {} message(s) skipped by the server (the link is not carrying this rate); \
                 totals {} dropped / {} skipped{}",
                self.win_samples,
                self.nominal_hz / 1000.0,
                dt.as_secs_f64(),
                self.win_dropped,
                self.win_gaps,
                self.total_dropped,
                self.total_gaps,
                self.keyed_note(),
            );
        } else {
            tracing::debug!(
                "SpyServer RX: {} samples ({ksps:.1} ksps) over {:.2}s; total {}{}",
                self.win_samples,
                dt.as_secs_f64(),
                self.total_samples,
                self.keyed_note(),
            );
        }
        self.since = Instant::now();
        self.win_samples = 0;
        self.win_dropped = 0;
        self.win_keyed = 0;
        self.win_gaps = 0;
    }
}

/// Push interleaved I/Q into the RX ring, keeping I and Q paired.
///
/// All-or-nothing: a block the ring cannot take whole is dropped whole.
/// Pushing what fits would leave the ring one float out of step, swapping I
/// with Q for the rest of the session — a mirrored, unusable spectrum that
/// reads like a driver bug rather than the overrun it is.
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
    /// The newest full-band frame, or none since it was last taken.
    ///
    /// Newest wins: an unread frame is *overwritten* rather than queued,
    /// because a six-frame-old picture of the band is worth nothing beside a
    /// current one and a queue here would turn a slow consumer into growing
    /// latency. The thread only ever swaps a `Vec` in under this lock; the
    /// decoding happens outside it.
    pub fft: std::sync::Mutex<Option<FftFrame>>,
    /// Whether this client may move the device itself, from the last
    /// `CLIENT_SYNC`. False means another client owns the receiver and this one
    /// may only slide its own window inside what that client is receiving.
    pub can_control: AtomicBool,
    /// Where the server says the *device* is, in millihertz so the atomic can
    /// carry the fraction a decimated rate can produce.
    pub device_center_milli_hz: AtomicI64,
    /// Where the server says this client's I/Q window is.
    pub iq_center_milli_hz: AtomicI64,
    /// The gain index the server reports, which on a shared server is whatever
    /// its owner set rather than what we asked for.
    pub gain_index: AtomicU32,
    /// Bumped on every `CLIENT_SYNC`, so the source can tell a fresh statement
    /// from a stale reading without comparing every field.
    pub sync_seq: AtomicU64,
    /// Set while the engine is transmitting and therefore not reading this
    /// receiver — see `IqSource::set_rx_paused`. Read by the stream thread on
    /// every block so a ring that fills during an over is accounted for as the
    /// cost of transmitting rather than as an overrun.
    pub rx_paused: AtomicBool,
}

/// An open connection to a SpyServer.
pub struct SpyServerHandle {
    rx: Consumer<f32>,
    ctrl: Sender<Ctrl>,
    shared: Arc<Shared>,
    opened_at: Instant,
    join: Option<JoinHandle<()>>,
    /// What the server said about itself, read once at connect.
    pub info: DeviceInfo,
    /// Description for logs and the UI.
    pub label: String,
    /// The I/Q rate this stream is running at, in Hz.
    pub sample_rate_hz: f64,
    /// The decimation stage that rate came from.
    pub iq_decimation: u32,
    /// The span the FFT stream covers, in Hz, or 0 when it was not asked for.
    pub fft_span_hz: f64,
}

impl SpyServerHandle {
    /// Tell the stream thread that the engine has stopped reading for an over,
    /// and then that it has started again — see `IqSource::set_rx_paused`. The
    /// receiver itself is left running: the samples keep arriving and keep
    /// being offered, this only decides whether the ones that no longer fit are
    /// reported as a fault.
    pub fn set_rx_paused(&self, paused: bool) {
        self.shared.rx_paused.store(paused, Ordering::Relaxed);
    }

    pub(crate) fn from_parts(
        rx: Consumer<f32>,
        ctrl: Sender<Ctrl>,
        shared: Arc<Shared>,
        join: JoinHandle<()>,
        info: DeviceInfo,
        label: String,
        sample_rate_hz: f64,
        iq_decimation: u32,
        fft_span_hz: f64,
    ) -> SpyServerHandle {
        SpyServerHandle {
            rx,
            ctrl,
            shared,
            opened_at: Instant::now(),
            join: Some(join),
            info,
            label,
            sample_rate_hz,
            iq_decimation,
            fft_span_hz,
        }
    }

    /// Whether the stream thread is still running.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// How long the server has gone without delivering samples, measured from
    /// the last block or — if none ever arrived — from when it was opened. A
    /// stream that never starts matters as much as one that stops, so it ages
    /// the same way.
    pub fn silent_for(&self) -> Duration {
        let since_open = self.opened_at.elapsed();
        let last = Duration::from_millis(self.shared.last_rx_ms.load(Ordering::Relaxed));
        since_open.saturating_sub(last)
    }

    /// Take the newest full-band frame, if one has arrived since the last call.
    pub fn take_fft(&self) -> Option<FftFrame> {
        self.shared.fft.lock().ok().and_then(|mut g| g.take())
    }

    pub fn can_control(&self) -> bool {
        self.shared.can_control.load(Ordering::Relaxed)
    }

    pub fn device_center_hz(&self) -> f64 {
        self.shared.device_center_milli_hz.load(Ordering::Relaxed) as f64 / 1000.0
    }

    pub fn iq_center_hz(&self) -> f64 {
        self.shared.iq_center_milli_hz.load(Ordering::Relaxed) as f64 / 1000.0
    }

    pub fn gain_index(&self) -> u32 {
        self.shared.gain_index.load(Ordering::Relaxed)
    }

    /// A counter that moves whenever the server restates its state, so a
    /// caller can notice a change without polling every field.
    pub fn sync_seq(&self) -> u64 {
        self.shared.sync_seq.load(Ordering::Relaxed)
    }

    /// The window this client can actually tune within, in Hz.
    ///
    /// With control of the device that is the whole receiver. Without it, the
    /// device centre belongs to another client and all this end may do is
    /// slide its own narrower window inside what that client is already
    /// receiving.
    pub fn tuning_window(&self) -> (f64, f64) {
        if self.can_control() {
            (f64::from(self.info.minimum_frequency), f64::from(self.info.maximum_frequency))
        } else {
            let slack = self.info.iq_slack_hz(self.sample_rate_hz);
            let dc = self.device_center_hz();
            (dc - slack, dc + slack)
        }
    }

    /// Where a requested centre would actually land. Equal to `want` when the
    /// request is reachable, which is how the caller tells a refusal from a
    /// success.
    pub fn reachable_center(&self, want: f64) -> f64 {
        let (lo, hi) = self.tuning_window();
        want.clamp(lo, hi.max(lo))
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
        // A closed channel means the thread has exited; `needs_reopen` picks
        // that up from `is_alive`, so there is nothing useful to do here.
        let _ = self.ctrl.send(c);
    }

    pub fn set_center_hz(&self, hz: f64) {
        self.send(Ctrl::Center(hz));
    }

    pub fn set_gain_index(&self, index: u32) {
        self.send(Ctrl::Gain(index));
    }

    pub fn set_auto_digital_gain(&self, on: bool) {
        self.send(Ctrl::AutoDigitalGain(on));
    }

    pub fn set_digital_gain_db(&self, db: f64) {
        self.send(Ctrl::DigitalGain(db));
    }

    pub fn set_fft_enabled(&self, on: bool) {
        self.send(Ctrl::FftEnabled(on));
    }

    pub fn set_fft_decimation(&self, stage: u32) {
        self.send(Ctrl::FftDecimation(stage));
    }

    pub fn set_fft_db_offset(&self, db: f64) {
        self.send(Ctrl::FftDbOffset(db));
    }

    pub fn set_fft_db_range(&self, db: f64) {
        self.send(Ctrl::FftDbRange(db));
    }

    /// Stop the stream thread and close the connection, without dropping the
    /// handle. Afterwards the handle is inert rather than invalid:
    /// [`Self::rx_read`] drains what is left and then returns nothing, control
    /// messages go nowhere, and [`Self::is_alive`] is false. Idempotent.
    pub fn release(&mut self) {
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for SpyServerHandle {
    fn drop(&mut self) {
        self.release();
    }
}

/// Hand-written: the ring ends and the channel inside have no `Debug`, and
/// none of them would say anything useful if they did. What a reader wants
/// here is which server, at what rate, and whether it is still up.
impl std::fmt::Debug for SpyServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpyServerHandle")
            .field("label", &self.label)
            .field("sample_rate_hz", &self.sample_rate_hz)
            .field("iq_decimation", &self.iq_decimation)
            .field("fft_span_hz", &self.fft_span_hz)
            .field("alive", &self.is_alive())
            .field("can_control", &self.can_control())
            .finish()
    }
}

/// Size the RX ring for a sample rate — half a second of interleaved floats,
/// rounded up to a power of two. Same formula as the RTL-SDR, HPSDR and TCI
/// backends.
pub(crate) fn ring_for(rate_hz: f64) -> (Producer<f32>, Consumer<f32>) {
    let cap = ((rate_hz * 2.0 * 0.5) as usize).next_power_of_two().max(1 << 16);
    RingBuffer::<f32>::new(cap)
}

/// The decimation stage to open at, resolving
/// [`SpyServerConfig::AUTO_DECIMATION`] against what the server offers.
pub(crate) fn resolve_stage(cfg: &SpyServerConfig, info: &DeviceInfo, vfo: bool) -> u32 {
    let rates = info.iq_rates(vfo);
    if cfg.iq_decimation < 0 {
        let target = if vfo {
            SpyServerConfig::VFO_TARGET_RATE_HZ
        } else {
            SpyServerConfig::WIDEBAND_TARGET_RATE_HZ
        };
        return info.stage_for_rate(target, vfo);
    }
    // A stage this server does not offer — a config carried over from a
    // different receiver — falls back to the nearest end of its ladder rather
    // than being sent as-is, which a server may answer by streaming nothing.
    let want = cfg.iq_decimation as u32;
    match rates.iter().find(|&&(i, _)| i == want) {
        Some(_) => want,
        None => {
            let fallback = rates
                .iter()
                .min_by_key(|&&(i, _)| i.abs_diff(want))
                .map(|&(i, _)| i)
                .unwrap_or(info.minimum_iq_decimation);
            tracing::warn!(
                "SpyServer: this server has no I/Q decimation stage {want} \
                 (it offers {}..={}); using stage {fallback}",
                rates.first().map(|&(i, _)| i).unwrap_or(0),
                rates.last().map(|&(i, _)| i).unwrap_or(0),
            );
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> DeviceInfo {
        DeviceInfo {
            device_type: 1,
            maximum_sample_rate: 10_000_000,
            maximum_bandwidth: 10_000_000,
            decimation_stage_count: 8,
            minimum_iq_decimation: 2,
            maximum_gain_index: 21,
            ..DeviceInfo::default()
        }
    }

    #[test]
    fn pending_keeps_only_the_last_value_per_field() {
        let mut p = Pending::default();
        for hz in [100e6, 100.1e6, 100.2e6, 100.3e6] {
            p.absorb(Ctrl::Center(hz));
        }
        p.absorb(Ctrl::Gain(4));
        p.absorb(Ctrl::Gain(9));
        assert_eq!(p.center, Some(100.3e6));
        assert_eq!(p.gain, Some(9));
        assert!(!p.shutdown);
    }

    #[test]
    fn pending_shutdown_is_sticky() {
        let mut p = Pending::default();
        p.absorb(Ctrl::Shutdown);
        p.absorb(Ctrl::Center(100e6));
        assert!(p.shutdown, "a later message must not cancel a shutdown");
    }

    #[test]
    fn empty_pending_is_detected() {
        let mut p = Pending::default();
        assert!(p.is_empty());
        p.absorb(Ctrl::FftEnabled(false));
        assert!(!p.is_empty(), "even a false request is a request");
    }

    /// The ring must hold an even number of floats, or the very first wrap
    /// would swap I and Q.
    #[test]
    fn ring_capacity_is_even_and_at_least_half_a_second() {
        for rate in [12_000.0, 96_000.0, 625_000.0, 2_500_000.0] {
            let (p, _c) = ring_for(rate);
            let cap = p.buffer().capacity();
            assert_eq!(cap % 2, 0, "odd ring capacity at {rate}");
            assert!(cap as f64 >= rate, "ring holds {cap} floats, less than 0.5 s of {rate} sps");
        }
    }

    #[test]
    fn push_iq_drops_whole_blocks_rather_than_splitting_pairs() {
        let (mut prod, cons) = RingBuffer::<f32>::new(8);
        let mut stats = RxStats::network(1000.0);

        push_iq(&mut prod, &[1.0, 2.0, 3.0, 4.0], &mut stats, false);
        assert_eq!(cons.slots(), 4);

        push_iq(&mut prod, &[5.0, 6.0, 7.0, 8.0, 9.0, 10.0], &mut stats, false);
        assert_eq!(cons.slots(), 4, "a partial write would desynchronise I and Q");
        assert_eq!(stats.total_dropped, 3);
        assert_eq!(cons.slots() % 2, 0);
    }

    #[test]
    fn automatic_decimation_differs_between_the_two_interfaces() {
        let cfg = SpyServerConfig::default();
        assert_eq!(cfg.iq_decimation, SpyServerConfig::AUTO_DECIMATION);
        let info = info();
        // 10 Msps ladder from stage 2: 2.5M 1.25M 625k 312k 156k 78k 39k.
        assert_eq!(resolve_stage(&cfg, &info, false), 4, "625 kHz for the wideband target");
        assert_eq!(resolve_stage(&cfg, &info, true), 7, "78 kHz for the VFO target");
    }

    /// A stage stored against one server and pointed at another must not be
    /// sent as-is: this server's floor is 2, and asking for 0 gets silence.
    #[test]
    fn a_stage_this_server_does_not_offer_falls_back_to_its_ladder() {
        let info = info();
        let below = SpyServerConfig { iq_decimation: 0, ..Default::default() };
        assert_eq!(resolve_stage(&below, &info, false), 2);
        let above = SpyServerConfig { iq_decimation: 20, ..Default::default() };
        assert_eq!(resolve_stage(&above, &info, false), 8);
        let exact = SpyServerConfig { iq_decimation: 5, ..Default::default() };
        assert_eq!(resolve_stage(&exact, &info, false), 5);
    }

    /// A ring that fills because the engine stopped reading for an over is not
    /// the DSP thread falling behind, and must not reach the fault counters:
    /// that is what turned a healthy station into a warning per two seconds of
    /// transmit and a running total that only measured time on the air.
    #[test]
    fn a_full_ring_while_paused_is_not_an_overrun() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(4);
        let mut stats = RxStats::network(1000.0);

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
