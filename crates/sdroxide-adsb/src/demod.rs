//! The 1090 MHz pulse-position demodulator: complex baseband in, candidate
//! Mode S messages out.
//!
//! # The waveform
//!
//! Mode S downlink (ICAO Annex 10 Volume IV, RTCA DO-260B) is on-off keying at
//! one megabit a second, and every reply has the same two parts:
//!
//! * an **8 µs preamble** — 0.5 µs pulses beginning at 0.0, 1.0, 3.5 and
//!   4.5 µs, with everything between and after them dark until the data
//!   starts. The odd spacing is the point: it is a pattern noise does not make.
//! * a **data block** of either 56 or 112 bits, each bit 1 µs long and split
//!   into two half-microsecond chips. Energy in the first chip is a `1`, energy
//!   in the second is a `0`. Which length it is comes from the first five bits,
//!   the downlink format: 16 and above are the long ones.
//!
//! So a short reply is 8 + 56 = 64 µs and a long one 120 µs, and everything
//! this module does is measured in microseconds rather than in samples.
//!
//! # Why it is written in microseconds
//!
//! There is no resampler in front of this. The engine hands over whatever its
//! downconverter settled on — 2.4 Msps from an RTL-SDR, 2.5 from an Airspy,
//! 2.0 from an SDRplay — and the decoder indexes by time:
//! [`Scan::at`] turns a microsecond offset into a sample index at whatever rate
//! it was built for. Nothing here assumes two samples per bit, which is what
//! lets one implementation cover every front end in the tree without a
//! fractional resampler in the hot path.
//!
//! # Power, not magnitude
//!
//! Every comparison this module makes is between two envelope levels, and
//! `a > b` has the same answer for `|z|` as for `|z|²`. So the hot path squares
//! and never takes a root; the one square root per accepted message is in
//! [`Candidate::rssi_dbfs`], where a decibel is actually wanted.
//!
//! # Blocks are not messages
//!
//! A long reply is 120 µs — about 288 samples at 2.4 Msps — and the engine's
//! blocks are not aligned to anything. [`Demod::push`] therefore keeps the tail
//! of the previous block in front of the new one, so a message that straddles a
//! boundary is still seen whole. Dropping those would not look like a bug; it
//! would look like a receiver a few decibels deaf.

use sdroxide_dsp::Complex32;

/// Bit period, microseconds.
const BIT_US: f64 = 1.0;
/// Preamble length, microseconds — where the data starts.
const PREAMBLE_US: f64 = 8.0;
/// The four preamble pulse positions, microseconds from the start.
const PULSES_US: [f64; 4] = [0.0, 1.0, 3.5, 4.5];
/// The gaps between and after them that must be dark.
const SPACES_US: [f64; 6] = [0.5, 1.5, 2.0, 3.0, 4.0, 5.0];
/// Long-message length in bits (DF >= 16); the short one is 56.
const LONG_BITS: usize = 112;
const SHORT_BITS: usize = 56;

/// Total span a long message occupies, microseconds.
const MSG_US: f64 = PREAMBLE_US + LONG_BITS as f64 * BIT_US;

/// How much of the previous block to carry forward, microseconds.
///
/// One whole long message plus a little, so a preamble that begins in the last
/// microsecond of a block still has its data available on the next pass.
const TAIL_US: f64 = MSG_US + 4.0;

/// How far above the noise floor the weakest preamble pulse must sit.
///
/// The preamble's own shape does nearly all the work — four pulses at
/// irregular spacing, each of which has to beat every one of the six dark
/// chips — so this is not the sensitivity limit. It is what stops the
/// correlator spending 112 bit comparisons on a stretch of pure noise whose
/// samples happen to fall in the right order, which at 2.4 million samples a
/// second is often.
const PULSE_OVER_NOISE: f32 = 3.0;

/// How much stronger the weakest pulse must be than the strongest dark chip.
///
/// A factor of two in power, i.e. 3 dB. Requiring more rejects real aircraft at
/// the edge of range, where the pulse tops are within a few dB of the ringing
/// between them; requiring less lets a strong signal's own tail masquerade as a
/// second preamble one microsecond later.
const PULSE_OVER_SPACE: f32 = 2.0;

/// A message the slicer produced, before anything has checked it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 7 or 14 bytes, most significant bit of byte 0 first.
    pub bytes: Vec<u8>,
    /// Mean `|a - b| / (a + b)` over the sliced bits: 1.0 when every bit was
    /// unambiguous, near 0 when the slicer was guessing. Reported rather than
    /// acted on — the CRC is the gate, and a low-confidence message that passes
    /// a 24-bit check is a real message.
    pub confidence: f32,
    /// Peak envelope of the preamble, dBFS. Negative on any real signal.
    pub rssi_dbfs: f32,
}

/// Rolling demodulator over one receiver window.
pub struct Demod {
    rate_hz: f64,
    /// Samples per microsecond — the only rate-dependent number in the module.
    sps_us: f64,
    /// Envelope power, with [`TAIL_US`] of the previous block still in front.
    power: Vec<f32>,
    /// How many samples of `power` are carried over from last time.
    carried: usize,
    /// Slowly-tracked noise floor in power units.
    noise: f32,
    /// Preambles accepted, and messages sliced out of them.
    pub preambles: u64,
}

impl Demod {
    /// Build a demodulator for a stream at `rate_hz`.
    ///
    /// Any rate is accepted; [`sdroxide_types::ADSB_MIN_RATE_HZ`] is the caller's
    /// business, because "this receiver cannot do ADS-B" is a thing to tell the
    /// operator rather than a panic.
    pub fn new(rate_hz: f64) -> Demod {
        Demod {
            rate_hz,
            sps_us: rate_hz / 1e6,
            power: Vec::new(),
            carried: 0,
            // Starts pessimistic and tracks down: a floor that begins at zero
            // would accept everything for the first few blocks.
            noise: 1e-3,
            preambles: 0,
        }
    }

    pub fn rate_hz(&self) -> f64 {
        self.rate_hz
    }

    /// Feed one block and collect every message found in it.
    ///
    /// The tail of the previous block is searched again as far as one message
    /// length in, which is what makes a straddling reply decodable; the scan
    /// stops one message length before the end of the new data so the same
    /// message is not found twice from the two sides of a boundary.
    pub fn push(&mut self, iq: &[Complex32], out: &mut Vec<Candidate>) {
        if iq.is_empty() {
            return;
        }
        // Keep the tail, drop everything older.
        let tail = (TAIL_US * self.sps_us).ceil() as usize;
        if self.power.len() > tail {
            self.power.drain(..self.power.len() - tail);
        }
        self.carried = self.power.len();
        self.power.reserve(iq.len());
        for z in iq {
            self.power.push(z.re * z.re + z.im * z.im);
        }
        self.track_noise();

        let span = (MSG_US * self.sps_us).ceil() as usize;
        if self.power.len() <= span {
            return;
        }
        let last = self.power.len() - span;
        let mut i = 0usize;
        while i < last {
            match self.try_at(i) {
                Some(c) => {
                    self.preambles += 1;
                    // Step past the whole message: its own pulses would
                    // otherwise re-trigger the correlator on the way through.
                    i += span;
                    out.push(c);
                }
                None => i += 1,
            }
        }
        // Anything from `last` on stays for the next block to look at.
        self.carried = 0;
    }

    /// Sample index of a time offset from a preamble start.
    #[inline]
    fn at(&self, base: usize, us: f64) -> usize {
        base + (us * self.sps_us).round() as usize
    }

    /// Envelope power over the half-microsecond chip beginning at `us`.
    ///
    /// The mean over the chip rather than one sample from the middle of it: at
    /// 2 Msps a chip is one sample and the two are the same thing, but at 8 or
    /// 20 Msps a single sample throws away most of what was received and costs
    /// several dB of sensitivity for nothing.
    #[inline]
    fn chip(&self, base: usize, us: f64) -> f32 {
        let a = self.at(base, us);
        let b = self.at(base, us + 0.5).max(a + 1);
        let b = b.min(self.power.len());
        if a >= b {
            return 0.0;
        }
        let mut sum = 0.0f32;
        for &p in &self.power[a..b] {
            sum += p;
        }
        sum / (b - a) as f32
    }

    /// Try to read a message whose preamble starts at sample `i`.
    fn try_at(&self, i: usize) -> Option<Candidate> {
        // The four pulses, and the six chips that have to be dark.
        let mut weakest = f32::MAX;
        let mut peak = 0.0f32;
        for us in PULSES_US {
            let p = self.chip(i, us);
            weakest = weakest.min(p);
            peak = peak.max(p);
        }
        if weakest < self.noise * PULSE_OVER_NOISE {
            return None;
        }
        let mut loudest_space = 0.0f32;
        for us in SPACES_US {
            loudest_space = loudest_space.max(self.chip(i, us));
        }
        if weakest < loudest_space * PULSE_OVER_SPACE {
            return None;
        }

        // Slice the first five bits to learn the length, then the rest.
        let mut bytes = [0u8; 14];
        let mut conf_sum = 0.0f32;
        let mut nbits = LONG_BITS;
        let mut k = 0usize;
        while k < nbits {
            let t = PREAMBLE_US + k as f64 * BIT_US;
            let a = self.chip(i, t);
            let b = self.chip(i, t + 0.5);
            if a > b {
                bytes[k / 8] |= 0x80 >> (k % 8);
            }
            let sum = a + b;
            conf_sum += if sum > 0.0 { (a - b).abs() / sum } else { 0.0 };
            k += 1;
            if k == 5 {
                // Downlink format: bit 0 of the five is worth 16.
                let df = bytes[0] >> 3;
                nbits = if df >= 16 { LONG_BITS } else { SHORT_BITS };
            }
        }

        // A message of all zeroes or all ones is what an unmodulated carrier
        // and a dead channel look like; neither is worth a CRC.
        let len = nbits / 8;
        let msg = &bytes[..len];
        if msg.iter().all(|&b| b == 0) || msg.iter().all(|&b| b == 0xff) {
            return None;
        }

        // dBFS against a full-scale complex sample, which is what every source
        // in this tree normalises to.
        let rssi_dbfs = 10.0 * peak.max(1e-12).log10();
        Some(Candidate {
            bytes: msg.to_vec(),
            confidence: conf_sum / nbits as f32,
            rssi_dbfs: rssi_dbfs.min(0.0),
        })
    }

    /// Track the channel's noise floor from the block just pushed.
    ///
    /// The floor is taken as a low quantile of a coarse sample of the block
    /// rather than as its mean: half of a busy second at 1090 MHz is other
    /// people's transmissions, and a mean over those is not a noise floor at
    /// all. Tracking is asymmetric for the reason the ISM gate's is — down
    /// quickly, up slowly — so a burst of traffic cannot walk the threshold up
    /// behind itself and deafen the receiver for the next second.
    fn track_noise(&mut self) {
        let fresh = &self.power[self.carried..];
        if fresh.len() < 64 {
            return;
        }
        // Every 37th sample: coprime with any plausible period in the signal,
        // so the sample is not synchronised to the traffic it is measuring.
        let mut sample: Vec<f32> = fresh.iter().step_by(37).copied().collect();
        sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = sample[sample.len() / 4];
        if q < self.noise {
            self.noise = 0.7 * self.noise + 0.3 * q;
        } else {
            self.noise = 0.99 * self.noise + 0.01 * q;
        }
        self.noise = self.noise.max(1e-12);
    }
}

/// Modulate a Mode S reply the way a transponder does: the preamble, then one
/// on-off keyed chip per half-microsecond, into `nf` of complex noise.
///
/// Public because two callers outside this module need a transmitter and it has
/// to be *the same* transmitter — the unit tests here, and the `adsb_iq`
/// example that synthesises a sky to a file. A generator that placed its pulses
/// differently from the decoder's expectations would prove nothing and hide
/// exactly the errors a test is for.
///
/// `seed` makes the noise deterministic; there is no `rand` in this tree, and a
/// decoder test that fails one run in fifty is worse than no test at all.
pub fn modulate(msg: &[u8], rate_hz: f64, amp: f32, nf: f32, seed: u64) -> Vec<Complex32> {
    let sps_us = rate_hz / 1e6;
    let bits = msg.len() * 8;
    // 10 µs of quiet in front so the noise tracker has something to measure,
    // and a whole long-message span behind: the scan deliberately stops one
    // message short of the end of what it has, so a burst any closer to the end
    // than that waits for a block that a caller may never send.
    let lead = 10.0;
    let total_us = lead + PREAMBLE_US + bits as f64 + MSG_US + 8.0;
    let n = (total_us * sps_us).ceil() as usize;
    let mut on = vec![false; n];
    let mut mark = |us: f64| {
        let a = (us * sps_us).round() as usize;
        let b = ((us + 0.5) * sps_us).round() as usize;
        for s in on.iter_mut().take(b.min(n)).skip(a.min(n)) {
            *s = true;
        }
    };
    for us in PULSES_US {
        mark(lead + us);
    }
    for k in 0..bits {
        let bit = msg[k / 8] & (0x80 >> (k % 8)) != 0;
        let t = lead + PREAMBLE_US + k as f64;
        mark(if bit { t } else { t + 0.5 });
    }
    let mut st = seed | 1;
    let mut rnd = || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        ((st >> 40) as f32 / 8_388_608.0) - 1.0
    };
    on.iter()
        .map(|&o| {
            let a = if o { amp } else { 0.0 };
            Complex32::new(a + nf * rnd(), nf * rnd())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real DF17 position squitter, from the published test vectors.
    const DF17: [u8; 14] =
        [0x8D, 0x40, 0x62, 0x1D, 0x58, 0xC3, 0x82, 0xD6, 0x90, 0xC8, 0xAC, 0x28, 0x63, 0xA7];

    fn decode_at(rate: f64) -> Vec<Candidate> {
        let iq = modulate(&DF17, rate, 1.0, 0.02, 0x9E37_79B9);
        let mut d = Demod::new(rate);
        let mut out = Vec::new();
        // Two passes so the noise tracker has run before the message arrives —
        // exactly what happens on air.
        d.push(&vec![Complex32::new(0.0, 0.0); 4096], &mut out);
        out.clear();
        d.push(&iq, &mut out);
        out
    }

    /// The demodulator is written in microseconds, so the same burst has to
    /// come back at every rate a front end in this tree might deliver.
    #[test]
    fn one_burst_decodes_at_every_rate_a_front_end_delivers() {
        for rate in [2_000_000.0, 2_400_000.0, 2_500_000.0, 3_200_000.0, 8_000_000.0] {
            let out = decode_at(rate);
            assert!(!out.is_empty(), "nothing found at {rate}");
            assert!(
                out.iter().any(|c| c.bytes == DF17),
                "the message came back wrong at {rate}: {:02X?}",
                out[0].bytes
            );
        }
    }

    /// A message split across two blocks must still decode. Without the carried
    /// tail this silently costs a few percent of every aircraft's frames, which
    /// looks like a deaf receiver rather than like a bug.
    #[test]
    fn a_burst_straddling_a_block_boundary_still_decodes() {
        let rate = 2_400_000.0;
        let iq = modulate(&DF17, rate, 1.0, 0.02, 0x1234_5678);
        // Cut in the middle of the data block.
        let cut = iq.len() / 2;
        let mut d = Demod::new(rate);
        let mut out = Vec::new();
        d.push(&vec![Complex32::new(0.0, 0.0); 4096], &mut out);
        out.clear();
        d.push(&iq[..cut], &mut out);
        d.push(&iq[cut..], &mut out);
        assert!(
            out.iter().any(|c| c.bytes == DF17),
            "the split message was lost: {} candidates",
            out.len()
        );
    }

    /// Noise alone must not produce messages. The CRC is the real gate, but a
    /// correlator that fires on every other sample would hand it millions of
    /// candidates a second and cost more than the whole receive chain.
    #[test]
    fn noise_alone_yields_almost_no_candidates() {
        let rate = 2_400_000.0;
        let mut st = 0xDEAD_BEEFu64;
        let mut rnd = || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            ((st >> 40) as f32 / 8_388_608.0) - 1.0
        };
        // One second of noise.
        let iq: Vec<Complex32> =
            (0..2_400_000).map(|_| Complex32::new(0.05 * rnd(), 0.05 * rnd())).collect();
        let mut d = Demod::new(rate);
        let mut out = Vec::new();
        for chunk in iq.chunks(16_384) {
            d.push(chunk, &mut out);
        }
        assert!(
            out.len() < 2_000,
            "the correlator fired {} times on a second of pure noise",
            out.len()
        );
    }

    /// A short reply is 56 bits, and the length comes from the first five —
    /// reading a DF4 as 112 bits would run the CRC over eight bytes of the next
    /// aircraft's silence.
    #[test]
    fn the_downlink_format_chooses_the_length() {
        let rate = 2_400_000.0;
        // DF4 (00100...) surveillance altitude reply, seven bytes.
        let short = [0x20u8, 0x00, 0x11, 0x91, 0xAB, 0xCD, 0xEF];
        let iq = modulate(&short, rate, 1.0, 0.02, 0xABCD_0123);
        let mut d = Demod::new(rate);
        let mut out = Vec::new();
        d.push(&vec![Complex32::new(0.0, 0.0); 4096], &mut out);
        out.clear();
        d.push(&iq, &mut out);
        assert!(
            out.iter().any(|c| c.bytes == short),
            "a short reply did not come back as seven bytes: {:02X?}",
            out.first().map(|c| &c.bytes)
        );
    }
}
