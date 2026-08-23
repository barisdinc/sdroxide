//! The board's **second** receive chain, and the arithmetic that pairs its
//! samples with the first one's.
//!
//! A LimeSDR-USB has two receive chains sharing one synthesiser and one sample
//! clock. They cannot be tuned apart — which is why this is not a second radio
//! — but they *are* two aerials on the same span with a fixed relative phase,
//! and that is what a diversity canceller needs (issue #98).
//!
//! # Why the pairing is the hard part
//!
//! LimeSuite's C API has one `lms_stream_t` per channel and no way to read a
//! pair at once, so the two are read independently out of two FIFOs that were
//! started a moment apart and drain at whatever rate the caller happens to ask
//! them. Nothing about that guarantees that the *n*th sample out of one is the
//! *n*th sample out of the other, and a canceller fed a pair that is off by
//! even a handful of samples fits a delay that is not there: it converges on
//! nothing, or worse, on the wanted signal.
//!
//! What makes it tractable is that LimeSuite stamps every received block with
//! the hardware sample counter of its first sample. So the main stream's read
//! decides the wanted span — `[ts, ts + n)` — and this module's job is to have
//! exactly those samples of the second chain ready, discarding what is older
//! and waiting for what has not arrived.
//!
//! **The timestamps are doc-derived**, like everything else in this crate: no
//! LimeSDR has been attached to this code. If a build of LimeSuite turns out
//! not to fill them in, [`AuxQueue`] notices that the counter is not advancing
//! and falls back to pairing by arrival order alone, which is what the FIFOs
//! give anyway when nothing has been dropped.

use num_complex::Complex32;

use crate::ffi;

/// What a pairing attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pull {
    /// The wanted span was copied out.
    Filled,
    /// Not enough has arrived yet; read the chain again and retry.
    NeedMore,
    /// The queue holds samples *newer* than the ones asked for — the main
    /// stream is behind. Nothing is consumed: the main stream will catch up,
    /// and until it does this block goes through uncombined.
    Ahead,
}

/// Samples read from the second chain and not yet paired.
pub(crate) struct AuxQueue {
    buf: Vec<Complex32>,
    /// Index of the oldest sample still wanted; everything before it has been
    /// handed out or discarded.
    head: usize,
    /// The hardware timestamp of `buf[head]`.
    ts: u64,
    /// Whether the timestamps are believed. Cleared the first time a block
    /// arrives stamped the same as the last one, which is what a build that
    /// does not fill them in looks like.
    stamped: bool,
    /// How many times the pairing had to be abandoned and restarted — a gap in
    /// one FIFO and not the other. Reported, because a canceller that keeps
    /// resetting is one whose sample rate the host cannot keep up with.
    pub(crate) slips: u64,
}

impl AuxQueue {
    pub(crate) fn new() -> AuxQueue {
        AuxQueue { buf: Vec::new(), head: 0, ts: 0, stamped: true, slips: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    pub(crate) fn believes_timestamps(&self) -> bool {
        self.stamped
    }

    /// Abandon everything held and start pairing again.
    pub(crate) fn restart(&mut self) {
        self.buf.clear();
        self.head = 0;
        self.slips += 1;
    }

    /// Take one block that LimeSuite handed back, stamped `ts`.
    ///
    /// A block that does not continue where the last one ended means the FIFO
    /// dropped something, so what is held is no longer contiguous and is thrown
    /// away rather than spliced — a splice would be a delay the filter cannot
    /// see and would fit against.
    pub(crate) fn push(&mut self, samples: &[Complex32], ts: u64) {
        if samples.is_empty() {
            return;
        }
        if self.len() == 0 {
            self.buf.clear();
            self.head = 0;
            self.ts = ts;
            self.buf.extend_from_slice(samples);
            return;
        }
        let expected = self.ts + self.len() as u64;
        if self.stamped && ts != expected {
            // Either a gap, or a build that stamps everything the same. Told
            // apart by which way it went: a repeated stamp is not a gap.
            if ts <= self.ts {
                self.stamped = false;
            }
            self.restart();
            self.ts = ts;
            self.buf.extend_from_slice(samples);
            return;
        }
        // Compact before growing, so a long session does not grow the buffer
        // without bound.
        if self.head > 0 {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        self.buf.extend_from_slice(samples);
    }

    /// Copy the `out.len()` samples starting at `want_ts` into `out`, if they
    /// are held.
    pub(crate) fn take(&mut self, want_ts: u64, out: &mut [Complex32]) -> Pull {
        let n = out.len();
        if n == 0 {
            return Pull::Filled;
        }
        if !self.stamped {
            // Order alone. Everything held is by definition the next thing the
            // second chain heard.
            if self.len() < n {
                return Pull::NeedMore;
            }
            out.copy_from_slice(&self.buf[self.head..self.head + n]);
            self.head += n;
            return Pull::Filled;
        }
        if self.len() == 0 {
            self.ts = want_ts;
            return Pull::NeedMore;
        }
        if self.ts > want_ts {
            return Pull::Ahead;
        }
        let skip = (want_ts - self.ts) as usize;
        if skip >= self.len() {
            // All of it is older than what is wanted.
            self.buf.clear();
            self.head = 0;
            self.ts = want_ts;
            return Pull::NeedMore;
        }
        self.head += skip;
        self.ts = want_ts;
        if self.len() < n {
            return Pull::NeedMore;
        }
        out.copy_from_slice(&self.buf[self.head..self.head + n]);
        self.head += n;
        self.ts += n as u64;
        Pull::Filled
    }
}

/// The second chain: a LimeSuite stream, the queue in front of it, and what the
/// chain itself is set to.
pub(crate) struct AuxRx {
    pub(crate) stream: ffi::StreamT,
    pub(crate) running: bool,
    pub(crate) channel: usize,
    pub(crate) antenna: String,
    pub(crate) gain_db: f64,
    queue: AuxQueue,
    scratch: Vec<Complex32>,
    /// Set when the chain stopped delivering; the caller passes the main
    /// stream through alone rather than refusing to receive at all.
    pub(crate) stalled: bool,
}

/// How long a single auxiliary read may wait, and how many it may take, when
/// the caller is one that is allowed to block.
///
/// Both deliberately small. The two chains produce samples at the same rate
/// from the same clock, so the second one's block is either in its FIFO
/// already or something has hiccupped — and every millisecond spent here is
/// spent inside the engine's own receive call. Four times five is 20 ms
/// against a 200 ms receive, which is a stall the loop absorbs.
const AUX_TIMEOUT_MS: u32 = 5;
const AUX_TRIES: usize = 4;

/// The same for a caller that must not block: `IqSource::read_available`,
/// which the engine calls every tick *during an over*, sharing the thread with
/// the transmit feed. Waiting there would come out of the transmitter's
/// budget, so this takes what has arrived and no more.
const AUX_TIMEOUT_QUICK_MS: u32 = 0;
const AUX_TRIES_QUICK: usize = 2;

impl AuxRx {
    pub(crate) fn new(
        stream: ffi::StreamT,
        channel: usize,
        antenna: String,
        gain_db: f64,
    ) -> AuxRx {
        AuxRx {
            stream,
            running: false,
            channel,
            antenna,
            gain_db,
            queue: AuxQueue::new(),
            scratch: Vec::new(),
            stalled: false,
        }
    }

    pub(crate) fn slips(&self) -> u64 {
        self.queue.slips
    }

    /// Whether this LimeSuite fills in the receive timestamps at all — see
    /// [`AuxQueue::believes_timestamps`].
    pub(crate) fn stamped(&self) -> bool {
        self.queue.believes_timestamps()
    }

    /// Take whatever the second chain has, in order, without pairing it with
    /// anything.
    ///
    /// What the transmit-feedback role wants: the predistortion loop finds its
    /// own alignment by correlating envelopes, because the reference it is
    /// aligning against is a block of samples handed to the transmitter and
    /// carries no hardware timestamp at all. Also what a chain that is *not*
    /// being used for anything wants, so its FIFO does not sit there
    /// overflowing.
    pub(crate) fn read_raw(&mut self, api: &ffi::Api, out: &mut [Complex32]) -> usize {
        if !self.running || out.is_empty() {
            return 0;
        }
        let mut meta = ffi::StreamMetaT::default();
        let n = unsafe {
            (api.recv_stream)(
                &mut self.stream,
                out.as_mut_ptr().cast(),
                out.len(),
                &mut meta,
                AUX_TIMEOUT_QUICK_MS,
            )
        };
        if n <= 0 { 0 } else { n as usize }
    }

    /// Fill `out` with the second chain's samples for `[want_ts, want_ts +
    /// out.len())`.
    ///
    /// Returns how many were paired — `out.len()` or nothing. Nothing is not a
    /// failure: it means this block goes through uncombined, which is the right
    /// answer whenever the alternative is combining against the wrong samples.
    ///
    /// `patient` is the caller saying whether it may block at all; see
    /// [`AUX_TIMEOUT_QUICK_MS`].
    pub(crate) fn read_aligned(
        &mut self,
        api: &ffi::Api,
        want_ts: u64,
        out: &mut [Complex32],
        patient: bool,
    ) -> usize {
        if !self.running || out.is_empty() {
            return 0;
        }
        let (timeout, tries) = if patient {
            (AUX_TIMEOUT_MS, AUX_TRIES)
        } else {
            (AUX_TIMEOUT_QUICK_MS, AUX_TRIES_QUICK)
        };
        for _ in 0..tries {
            match self.queue.take(want_ts, out) {
                Pull::Filled => {
                    self.stalled = false;
                    return out.len();
                }
                Pull::Ahead => return 0,
                Pull::NeedMore => {}
            }
            // Ask for a whole block at a time: LimeSuite hands back what it
            // has, and asking for the shortfall alone would cost a call per
            // packet.
            if self.scratch.len() < out.len() {
                self.scratch.resize(out.len(), Complex32::new(0.0, 0.0));
            }
            let mut meta = ffi::StreamMetaT::default();
            let n = unsafe {
                (api.recv_stream)(
                    &mut self.stream,
                    self.scratch.as_mut_ptr().cast(),
                    self.scratch.len(),
                    &mut meta,
                    timeout,
                )
            };
            if n <= 0 {
                break;
            }
            self.queue.push(&self.scratch[..n as usize], meta.timestamp);
        }
        self.stalled = true;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(from: u64, n: usize) -> Vec<Complex32> {
        (0..n).map(|i| Complex32::new((from + i as u64) as f32, 0.0)).collect()
    }

    /// The ordinary case: the second chain runs a little ahead, and the block
    /// asked for is cut out of what is held.
    #[test]
    fn the_wanted_span_is_cut_out_of_what_is_held() {
        let mut q = AuxQueue::new();
        q.push(&ramp(1000, 64), 1000);
        let mut out = vec![Complex32::new(0.0, 0.0); 16];
        assert_eq!(q.take(1008, &mut out), Pull::Filled);
        assert_eq!(out[0], Complex32::new(1008.0, 0.0));
        assert_eq!(out[15], Complex32::new(1023.0, 0.0));
        // And the next request continues where that one left off.
        assert_eq!(q.take(1024, &mut out), Pull::Filled);
        assert_eq!(out[0], Complex32::new(1024.0, 0.0));
    }

    /// Not enough held yet: read again rather than pair against silence.
    #[test]
    fn a_short_queue_asks_for_more() {
        let mut q = AuxQueue::new();
        q.push(&ramp(0, 8), 0);
        let mut out = vec![Complex32::new(0.0, 0.0); 16];
        assert_eq!(q.take(0, &mut out), Pull::NeedMore);
        q.push(&ramp(8, 8), 8);
        assert_eq!(q.take(0, &mut out), Pull::Filled);
        assert_eq!(out[15], Complex32::new(15.0, 0.0));
    }

    /// Samples older than the ones wanted are dropped, not handed over: that
    /// is precisely the misalignment the timestamps exist to prevent.
    #[test]
    fn stale_samples_are_discarded_rather_than_paired() {
        let mut q = AuxQueue::new();
        q.push(&ramp(0, 32), 0);
        let mut out = vec![Complex32::new(0.0, 0.0); 8];
        assert_eq!(q.take(1000, &mut out), Pull::NeedMore);
        assert_eq!(q.len(), 0, "the stale block should be gone");
        q.push(&ramp(1000, 8), 1000);
        assert_eq!(q.take(1000, &mut out), Pull::Filled);
        assert_eq!(out[0], Complex32::new(1000.0, 0.0));
    }

    /// The second chain running ahead of the main one is not an error and must
    /// not consume anything — the main stream catches up.
    #[test]
    fn a_chain_that_is_ahead_keeps_what_it_has() {
        let mut q = AuxQueue::new();
        q.push(&ramp(500, 32), 500);
        let mut out = vec![Complex32::new(0.0, 0.0); 8];
        assert_eq!(q.take(100, &mut out), Pull::Ahead);
        assert_eq!(q.len(), 32);
        assert_eq!(q.take(500, &mut out), Pull::Filled);
    }

    /// A gap in the second chain's FIFO throws away what was held rather than
    /// splicing across it, and says so.
    #[test]
    fn a_gap_restarts_the_pairing_instead_of_splicing() {
        let mut q = AuxQueue::new();
        q.push(&ramp(0, 16), 0);
        q.push(&ramp(64, 16), 64); // 48 samples went missing
        assert_eq!(q.slips, 1);
        assert_eq!(q.len(), 16);
        let mut out = vec![Complex32::new(0.0, 0.0); 8];
        assert_eq!(q.take(64, &mut out), Pull::Filled);
        assert_eq!(out[0], Complex32::new(64.0, 0.0));
    }

    /// A LimeSuite that does not stamp its receive blocks: the counter never
    /// moves, which is noticed once and then pairing falls back to arrival
    /// order.
    #[test]
    fn an_unstamped_build_falls_back_to_arrival_order() {
        let mut q = AuxQueue::new();
        q.push(&ramp(0, 16), 0);
        q.push(&ramp(16, 16), 0);
        assert!(!q.believes_timestamps());
        let mut out = vec![Complex32::new(0.0, 0.0); 8];
        // Whatever timestamp is asked for, the next samples held are the
        // answer.
        assert_eq!(q.take(123_456, &mut out), Pull::Filled);
        assert_eq!(out[0], Complex32::new(16.0, 0.0));
        assert_eq!(q.take(999, &mut out), Pull::Filled);
        assert_eq!(out[0], Complex32::new(24.0, 0.0));
    }
}
