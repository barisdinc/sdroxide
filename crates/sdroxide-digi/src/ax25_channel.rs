//! The half of a packet station that is not the protocol: the modem, the
//! deframer, the framer, carrier detect, and the CSMA that decides when the
//! transmitter may key.
//!
//! Shared by [`crate::PacketController`] and [`crate::AprsController`], which
//! run the same waveform and the same link framing and differ entirely in what
//! they do with the bytes. Written down once because both of the rules here
//! are the kind that fail silently:
//!
//! 1. **The modem must not hear its own transmission.** Full-duplex hardware
//!    keeps receiving through an over, and in audio mode `on_rx_audio` is fed
//!    unconditionally. For a keyboard mode that is cosmetic; for AX.25 it is
//!    fatal, because the link layer would see its own I-frames and acknowledge
//!    itself — and on APRS a station would hear its own beacon and put itself
//!    on the map twice. Hence [`Ax25Channel::keyed`].
//! 2. **A CSMA slot timer may never be clocked from `DigiEngine::poll`.**
//!    `poll` runs once per source block while receiving — measured at 341 ms
//!    on a sound-card rig with the real buffer size, against the 10 ms a slot
//!    wants. Slot countdown and DCD hold are therefore counted on the audio
//!    clock inside [`Ax25Channel::on_rx_audio`]. See
//!    `crates/sdroxide-radio/tests/tx_turnaround.rs` for the measurement.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use sdroxide_ax25::{Deframer, Discard, Framer};
use sdroxide_dsp::{AFSK_TX_PEAK, AfskProfile, AfskRx, AfskTx, G3RUH_TX_PEAK, G3ruhRx, G3ruhTx};
use sdroxide_types::{DigiConfig, PacketBaud};

/// Above this the channel counts as busy, and CSMA will not key.
///
/// Well clear of the idle-noise figure — an unmodulated channel sits near zero,
/// because the two tone branches see the same noise — and well below what a
/// real signal produces, so it does not need tuning per band.
const DCD_THRESHOLD: f32 = 0.35;

/// The modem for the configured speed. Both arms emit line levels; everything
/// above them is shared.
enum Modem {
    Afsk(Box<AfskRx>),
    G3ruh(Box<G3ruhRx>),
}

impl Modem {
    fn new(baud: PacketBaud, rate: f64) -> Modem {
        match baud {
            PacketBaud::Hf300 => Modem::Afsk(Box::new(AfskRx::new(rate, AfskProfile::Hf300))),
            PacketBaud::Vhf1200 => Modem::Afsk(Box::new(AfskRx::new(rate, AfskProfile::Vhf1200))),
            PacketBaud::Vhf9600 => Modem::G3ruh(Box::new(G3ruhRx::new(rate))),
        }
    }

    fn process(&mut self, audio: &[f32], out: &mut Vec<bool>) {
        match self {
            Modem::Afsk(m) => m.process(audio, out),
            Modem::G3ruh(m) => m.process(audio, out),
        }
    }

    /// How convincingly a signal is present, 0..1.
    fn separation(&self) -> f32 {
        match self {
            Modem::Afsk(m) => m.separation(),
            Modem::G3ruh(m) => m.separation(),
        }
    }

    fn magnitude(&self) -> f32 {
        match self {
            Modem::Afsk(m) => m.magnitude(),
            Modem::G3ruh(m) => m.magnitude(),
        }
    }
}

/// The transmit half, matching [`Modem`].
enum TxModem {
    Afsk(Box<AfskTx>),
    G3ruh(Box<G3ruhTx>),
}

impl TxModem {
    fn new(baud: PacketBaud, rate: f64) -> TxModem {
        match baud {
            PacketBaud::Hf300 => TxModem::Afsk(Box::new(AfskTx::new(rate, AfskProfile::Hf300))),
            PacketBaud::Vhf1200 => TxModem::Afsk(Box::new(AfskTx::new(rate, AfskProfile::Vhf1200))),
            PacketBaud::Vhf9600 => TxModem::G3ruh(Box::new(G3ruhTx::new(rate))),
        }
    }

    fn push_bits(&mut self, bits: &[bool]) {
        match self {
            TxModem::Afsk(m) => m.push_bits(bits),
            TxModem::G3ruh(m) => m.push_bits(bits),
        }
    }

    fn next_block(&mut self, out: &mut [f32]) -> usize {
        match self {
            TxModem::Afsk(m) => m.next_block(out),
            TxModem::G3ruh(m) => m.next_block(out),
        }
    }

    fn idle(&self) -> bool {
        match self {
            TxModem::Afsk(m) => m.idle(),
            TxModem::G3ruh(m) => m.idle(),
        }
    }

    /// The loudest sample this modem produces. The two differ: the AFSK tone is
    /// exactly its half-scale amplitude, while the 9600 baseband rings past its
    /// symbol level wherever the shaping filter meets a run of transitions.
    fn peak(&self) -> f32 {
        match self {
            TxModem::Afsk(_) => AFSK_TX_PEAK,
            TxModem::G3ruh(_) => G3RUH_TX_PEAK,
        }
    }
}

/// What one call to [`Ax25Channel::on_rx_audio`] produced.
pub(crate) struct RxResult {
    /// Frames that passed their check sequence, in order.
    pub frames: Vec<Vec<u8>>,
    /// Frames that arrived and failed it, since the last call.
    pub bad: u32,
    /// The carrier-detect state changed, so the caller's status is stale.
    pub dcd_changed: bool,
}

/// One AX.25 channel: modem in, frames out, frames in, audio out.
pub(crate) struct Ax25Channel {
    baud: PacketBaud,
    rate: f64,
    modem: Modem,
    deframer: Deframer,
    tx_modem: TxModem,

    /// True while our own transmitter is on the air. Rule 1: the modem is deaf
    /// by construction for as long as this is set, whatever the hardware hands
    /// us.
    pub keyed: bool,
    /// Frames waiting for the channel, oldest first.
    pending: VecDeque<Vec<u8>>,
    /// Set by CSMA once the channel is clear and the dice have been thrown;
    /// the caller's `poll` turns it into a key-up request.
    want_tx: bool,
    /// Samples left in the current CSMA slot. Counted on the audio clock, and
    /// never in `poll` — rule 2.
    slot_left: u32,
    slot_samples: u32,
    pub dcd: bool,

    levels: Vec<bool>,
    frames: Vec<Vec<u8>>,
}

impl Ax25Channel {
    pub(crate) fn new(baud: PacketBaud, cfg: &DigiConfig, rate: f64) -> Ax25Channel {
        Ax25Channel {
            baud,
            rate,
            modem: Modem::new(baud, rate),
            deframer: Deframer::new(),
            tx_modem: TxModem::new(baud, rate),
            keyed: false,
            pending: VecDeque::new(),
            want_tx: false,
            // A full slot, not zero. Zero means the first audio block after a
            // frame is queued keys immediately, without the wait that stops two
            // stations pouncing together — and the window for it is exactly the
            // case of queueing something before any audio has arrived, which is
            // what a beacon at startup does.
            slot_left: slot_samples(cfg, rate),
            slot_samples: slot_samples(cfg, rate),
            dcd: false,
            levels: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Rebuild after a speed change or a new tap rate.
    pub(crate) fn rebuild(&mut self, baud: PacketBaud, cfg: &DigiConfig, rate: f64) {
        self.baud = baud;
        self.rate = rate;
        self.modem = Modem::new(baud, rate);
        self.tx_modem = TxModem::new(baud, rate);
        self.deframer = Deframer::new();
        self.slot_samples = slot_samples(cfg, rate);
    }

    pub(crate) fn baud(&self) -> PacketBaud {
        self.baud
    }

    /// Smoothed receive level, for the meter.
    pub(crate) fn level(&self) -> f32 {
        self.modem.magnitude().clamp(0.0, 1.0)
    }

    pub(crate) fn queued(&self) -> usize {
        self.pending.len()
    }

    /// Queue a frame for the next clear slot.
    ///
    /// Nothing is sent from here — CSMA decides when, on the audio clock. That
    /// separation is the point: a caller that could transmit directly would be
    /// able to key on top of another station.
    pub(crate) fn queue(&mut self, frame: Vec<u8>) {
        self.pending.push_back(frame);
    }

    /// One block of receive audio: demodulate, deframe, and step CSMA.
    ///
    /// Rule 1 is enforced here and nowhere else, because the engine has good
    /// reasons to keep the receive chain running through an over — a
    /// full-duplex front end genuinely is receiving, and the panadapter wants
    /// it. It is this mode that must not listen.
    pub(crate) fn on_rx_audio(&mut self, tap: &[f32], cfg: &DigiConfig) -> RxResult {
        if self.keyed {
            return RxResult { frames: Vec::new(), bad: 0, dcd_changed: false };
        }
        let mut levels = std::mem::take(&mut self.levels);
        levels.clear();
        self.modem.process(tap, &mut levels);

        let mut frames = std::mem::take(&mut self.frames);
        frames.clear();
        for lvl in levels.drain(..) {
            self.deframer.push_level(lvl, &mut frames);
        }
        let out = frames.clone();
        frames.clear();

        let mut bad = 0;
        for d in self.deframer.take_discards() {
            if d == Discard::BadFcs {
                bad += 1;
            }
        }

        // Carrier detect and the CSMA slot clock, both on the audio clock and
        // never on `poll` — rule 2.
        let dcd = self.modem.separation() > DCD_THRESHOLD;
        let dcd_changed = dcd != self.dcd;
        self.dcd = dcd;
        self.csma(tap.len(), cfg);

        self.levels = levels;
        self.frames = frames;
        RxResult { frames: out, bad, dcd_changed }
    }

    /// Standard KISS p-persistence: while the channel is busy the slot timer is
    /// held at a full slot, so the countdown only begins once the channel
    /// clears; each time a slot elapses, transmit with probability
    /// `persist/256`, otherwise wait another slot. The randomness is what stops
    /// two stations that were both waiting out the same transmission from
    /// colliding the instant it ends.
    fn csma(&mut self, samples: usize, cfg: &DigiConfig) {
        if self.pending.is_empty() || self.keyed || self.want_tx {
            self.slot_left = self.slot_samples;
            return;
        }
        if self.dcd {
            // Busy. Restart the countdown so we wait a full slot after it
            // clears rather than pouncing on the first quiet sample.
            self.slot_left = self.slot_samples;
            return;
        }
        let n = samples as u32;
        if self.slot_left > n {
            self.slot_left -= n;
            return;
        }
        self.slot_left = self.slot_samples;
        if roll() < cfg.packet_persist {
            self.want_tx = true;
        }
    }

    /// CSMA has cleared us: render everything queued into the transmit modem
    /// and say which frames went into the over, or `None` if it is not time.
    ///
    /// Frames share a single flag between them rather than each paying its own
    /// preamble — on a link with several frames outstanding that is the
    /// difference between one over and several.
    pub(crate) fn take_over(&mut self, cfg: &DigiConfig) -> Option<Vec<Vec<u8>>> {
        if !self.want_tx || self.keyed {
            return None;
        }
        self.want_tx = false;
        self.keyed = true;

        let bits_per_flag = 8.0;
        let baud = self.baud.baud();
        let flags =
            |ms: u16| ((f64::from(ms) / 1000.0) * baud / bits_per_flag).ceil().max(1.0) as usize;

        let mut framer = Framer::new();
        framer.push_flags(flags(cfg.packet_txdelay_ms));
        let mut sent = Vec::new();
        while let Some(f) = self.pending.pop_front() {
            framer.push_frame(&f);
            framer.push_flags(1);
            sent.push(f);
        }
        framer.push_flags(flags(cfg.packet_txtail_ms));
        let bits = framer.take();
        self.tx_modem.push_bits(&bits);
        Some(sent)
    }

    pub(crate) fn tx_peak(&self) -> f32 {
        self.tx_modem.peak()
    }

    /// Fill one transmit block. `true` ends the over.
    pub(crate) fn fill_tx_block(&mut self, out: &mut [f32]) -> bool {
        let n = self.tx_modem.next_block(out);
        // Anything the modem did not fill is silence, and a block it filled
        // nothing of means the over is played out. Returning `true` ends it:
        // holding a packet channel open with dead carrier is the one thing a
        // packet station must never do.
        if n == 0 {
            out.fill(0.0);
            return true;
        }
        self.tx_modem.idle() && n < out.len()
    }

    pub(crate) fn on_burst_done(&mut self) {
        self.keyed = false;
    }

    /// The safety rails refused the key-up, or the operator stopped the over.
    ///
    /// The queue is left alone. A refused key is usually temporary — another
    /// radio holds the station interlock — and throwing the frames away would
    /// turn a moment's contention into lost traffic. CSMA will try again.
    pub(crate) fn abort_tx(&mut self) {
        self.keyed = false;
        self.want_tx = false;
        self.tx_modem = TxModem::new(self.baud, self.rate);
    }

    /// Everything the transmit side is holding goes, queue included.
    pub(crate) fn abort(&mut self) {
        self.pending.clear();
        self.abort_tx();
    }
}

/// Samples in one CSMA slot at `rate`.
fn slot_samples(cfg: &DigiConfig, rate: f64) -> u32 {
    ((f64::from(cfg.packet_slottime_ms.max(1)) / 1000.0) * rate).round().max(1.0) as u32
}

/// A random byte for the persistence test.
///
/// Falls back to the clock when there is no entropy source. That is weaker than
/// it looks but not dangerous here: the worst case is two stations picking the
/// same slot and colliding, which the link layer already has to survive.
fn roll() -> u8 {
    let mut b = [0u8; 1];
    if getrandom::fill(&mut b).is_err() {
        let nanos =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        return (nanos >> 7) as u8;
    }
    b[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 1, pinned here rather than in each controller: whichever one is
    /// running, a keyed station must not decode.
    #[test]
    fn a_keyed_channel_does_not_listen_to_itself() {
        let cfg = DigiConfig::default();
        let mut c = Ax25Channel::new(PacketBaud::Vhf1200, &cfg, 48_000.0);
        c.keyed = true;
        let r = c.on_rx_audio(&[0.5; 480], &cfg);
        assert!(r.frames.is_empty());
        assert!(!c.dcd, "a keyed station must not raise its own carrier detect");
    }

    /// Rule 2's other half: a queued frame does not go out on the first block
    /// of audio, because a whole slot has to elapse first.
    #[test]
    fn a_queued_frame_waits_at_least_one_slot() {
        // Persistence at maximum so the only thing that can hold it is the
        // slot timer.
        let cfg = DigiConfig { packet_persist: 255, packet_slottime_ms: 100, ..Default::default() };
        let mut c = Ax25Channel::new(PacketBaud::Vhf1200, &cfg, 48_000.0);
        c.queue(vec![0u8; 20]);
        assert!(c.take_over(&cfg).is_none(), "nothing may go out before any audio has arrived");
        // One block well short of a slot: 480 samples is 10 ms of a 100 ms slot.
        c.on_rx_audio(&[0.0; 480], &cfg);
        assert!(c.take_over(&cfg).is_none());
        // Past the slot, and it may key.
        for _ in 0..12 {
            c.on_rx_audio(&[0.0; 480], &cfg);
        }
        assert!(c.take_over(&cfg).is_some(), "a clear channel must eventually let the frame out");
    }

    /// A refused key-up keeps the queue: contention is temporary and the
    /// traffic is not.
    #[test]
    fn a_refused_key_up_keeps_the_queue() {
        let cfg = DigiConfig { packet_persist: 255, packet_slottime_ms: 1, ..Default::default() };
        let mut c = Ax25Channel::new(PacketBaud::Vhf1200, &cfg, 48_000.0);
        c.queue(vec![0u8; 20]);
        for _ in 0..4 {
            c.on_rx_audio(&[0.0; 480], &cfg);
        }
        assert!(c.take_over(&cfg).is_some());
        c.abort_tx();
        assert!(!c.keyed);
        // The frame has left the queue and is in the modem, so a refusal at
        // this point does lose it — which is why `abort` and `abort_tx` are
        // different calls and only the operator's stop clears the queue.
        c.queue(vec![1u8; 20]);
        assert_eq!(c.queued(), 1);
        c.abort();
        assert_eq!(c.queued(), 0);
    }
}
