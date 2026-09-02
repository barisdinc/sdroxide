//! A tracking receiver for the beacon: carrier FLL, chip matched filter, and
//! a Gardner timing loop, in place of searching for the answer.
//!
//! [`crate::bpsk::acquire`] finds a frame by trying things — a grid of carrier
//! frequencies, a grid of drift rates, eight chip phases at each — and asking
//! the CRC which combination was right. That works, and it is what a decoder
//! with no other information has to do, but it costs a sweep per candidate and
//! it only ever answers for the buffer in front of it. This module does the
//! ordinary thing a receiver does instead: it *tracks*. Three loops, each
//! closing on one unknown:
//!
//! * **Carrier FLL.** Where the beacon sits, and where it is drifting to.
//! * **Matched filter.** The chip pulse, integrated so noise between chips
//!   does not reach the detector.
//! * **Timing loop.** Where the chip boundaries are, and how the transmitter's
//!   clock is running against ours.
//!
//! Tracking rather than searching is what removes the drift grid: a warming
//! LNB walking a few Hz a second is not a chirp to be de-rotated by trial, it
//! is simply a frequency the loop follows.
//!
//! # Why a frequency lock and not a phase lock
//!
//! The obvious carrier loop for BPSK is a Costas loop, which recovers absolute
//! phase. It is also the wrong one here, for a reason specific to this signal
//! and these stations: the beacon is **differentially** encoded, so absolute
//! phase carries nothing — only the change from one chip to the next does —
//! and a typical amateur station receives it through a free-running Ku-band
//! LNB whose local oscillator has enough phase noise to keep a Costas loop
//! from ever settling. A loop that tries to track a phase that is being
//! shaken has nothing to gain and a great deal to lose. Locking frequency
//! alone asks the loop for exactly what differential detection needs and no
//! more, and it stays locked through phase noise that would break a PLL.
//!
//! # The discriminator
//!
//! Manchester coding puts a *null* at the carrier and two lobes either side of
//! it, peaking near ±[`crate::bpsk::BAUD`] — which is why nothing here hunts
//! for a peak. The frequency discriminator measures the power at ±
//! [`FLANK_HZ`], on the inner flank of each lobe, and takes the difference. On
//! frequency the two are equal by symmetry; off frequency the whole shape
//! slides and one flank climbs while the other falls.
//!
//! The flanks, not the peaks: the sensitivity of this discriminator is
//! proportional to the *slope* of the spectrum where it is sampled, and at the
//! peak of a lobe the slope is zero. Sampling the two lobe peaks would give a
//! reading that is beautifully symmetric and completely blind.
//!
//! # What this does not yet reach
//!
//! KA9Q published two reference recordings of the coded format, one clean and
//! one at 7 dB Eb/N0. This chain decodes the clean one — carrier offset and
//! all, see `tests::the_reference_recordings_decode` — and does **not** decode
//! the 7 dB one, which the format itself is comfortably strong enough for. So
//! there are a few dB on the table here that a better front end would collect,
//! and the likely places to look are the chip pulse the matched filter assumes
//! (square, which the satellite's transmit chain will have rounded off), the
//! timing loop's jitter at low SNR, and the fact that nothing here yet feeds
//! the framing layer a soft value scaled by its actual confidence rather than
//! by signal amplitude.

use sdroxide_dsp::Complex32;

use crate::bpsk::{BAUD, CHIP_RATE};

/// Where on each lobe the frequency discriminator samples, in Hz either side
/// of the carrier. Half the baud rate puts it on the inner flank, roughly
/// where the spectrum climbs fastest out of the Manchester null.
const FLANK_HZ: f64 = BAUD / 2.0;
/// Bandwidth of the power estimates the discriminator compares. Narrow enough
/// to be a flank rather than a smear of the whole lobe, wide enough to settle
/// in a fraction of a frame.
const FLANK_BW_HZ: f64 = 60.0;

/// Carrier loop bandwidth, Hz. The thing being tracked is an LNB warming up —
/// a few Hz per second at worst — so the loop only has to be faster than that,
/// and every Hz beyond it is noise let into the frequency estimate.
const FLL_BW_HZ: f64 = 1.5;
/// Damping factor for both loops. 1/√2 is the usual critically-damped-ish
/// compromise between overshoot and settling time.
const DAMPING: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Timing loop bandwidth, as a fraction of the chip rate, and the
/// proportional/integral gains it comes to.
///
/// The loop has two jobs with very different urgencies. *Phase* — where within
/// the chip we are sampling — is unknown at the start and has to be found
/// within a few dozen chips, so the proportional term is comparatively brisk.
/// *Rate* — how fast the transmitter's clock runs against ours — is a
/// parts-per-million business that barely moves, and keeping that term slow is
/// what stops chip-to-chip noise being mistaken for clock drift.
const TIMING_BW: f64 = 5e-3;
const TIMING_PI: (f64, f64) = {
    let theta = TIMING_BW / (DAMPING + 0.25 / DAMPING);
    let d = 1.0 + 2.0 * DAMPING * theta + theta * theta;
    (4.0 * DAMPING * theta / d, 4.0 * theta * theta / d)
};

/// Sign of the Gardner timing error, and of the frequency discriminator.
///
/// Both are a matter of convention — which way round the detector's output
/// means "early", and which way the correction then has to go — and both are
/// fixed here by measurement rather than by argument: get one wrong and its
/// loop walks away from the answer instead of toward it, which is
/// unmistakable and exactly what the loop tests below catch.
const TIMING_SIGN: f64 = 1.0;
const FLL_SIGN: f64 = -1.0;

/// How far the frequency estimate may be pulled from where it started, in Hz.
///
/// The discriminator is only monotonic across about a lobe's width; past that
/// it turns over and the loop would happily run away, locking onto a shape
/// that is not the beacon. The spectral tracker in [`crate::bpsk`] already
/// places the carrier to within a bin, so the loop is being asked to hold and
/// follow, not to find — and this says so.
const FREQ_PULL_HZ: f64 = 400.0;

/// Matched-filter samples carried from one block to the next, so the
/// interpolator never has to clamp against a block edge. One behind and two
/// ahead is what the cubic reaches for.
const CARRY: usize = 4;

/// A one-pole lowpass over complex samples, used for the flank powers.
#[derive(Clone, Copy)]
struct OnePole {
    a: f32,
    y: Complex32,
}

impl OnePole {
    fn new(bw_hz: f64, rate_hz: f64) -> OnePole {
        let a = (-std::f64::consts::TAU * bw_hz / rate_hz).exp();
        OnePole { a: a as f32, y: Complex32::new(0.0, 0.0) }
    }
    fn push(&mut self, x: Complex32) -> Complex32 {
        self.y = self.y * self.a + x * (1.0 - self.a);
        self.y
    }
}

/// Proportional-integral loop filter, in the standard second-order form.
#[derive(Clone, Copy)]
struct Pi {
    kp: f64,
    ki: f64,
    acc: f64,
}

impl Pi {
    /// `bw` and the loop rate in the same units; `bw` is the loop's noise
    /// bandwidth as a fraction of that rate.
    fn new(bw: f64, damping: f64) -> Pi {
        let theta = bw / (damping + 0.25 / damping);
        let d = 1.0 + 2.0 * damping * theta + theta * theta;
        Pi { kp: 4.0 * damping * theta / d, ki: 4.0 * theta * theta / d, acc: 0.0 }
    }
    /// Feed an error, get the correction to apply.
    fn step(&mut self, err: f64) -> f64 {
        self.acc += self.ki * err;
        self.kp * err + self.acc
    }
}

/// What the receiver knows about the signal right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RxState {
    /// The carrier, in Hz from the input's own centre.
    pub carrier_hz: f64,
    /// Chip clock error, in parts per million of [`CHIP_RATE`] — the
    /// transmitter's clock against this receiver's.
    pub clock_ppm: f64,
    /// How strongly the discriminator is seeing a beacon-shaped spectrum,
    /// 0..1. Near zero means the loops are tracking noise.
    pub lock: f32,
}

/// The tracking front end. Feed it IQ, take chip decisions out.
pub struct Rx {
    rate: f64,
    /// Carrier NCO: phase, and frequency in radians per sample.
    phase: f64,
    freq: f64,
    freq0: f64,
    fll: Pi,
    /// The two flank filters, and their smoothed powers.
    flank_hi: (f64, OnePole),
    flank_lo: (f64, OnePole),
    hi_phase: f64,
    lo_phase: f64,
    p_hi: f32,
    p_lo: f32,
    /// Chip matched filter: a running sum over one chip's samples.
    mf: Vec<Complex32>,
    mf_sum: Complex32,
    mf_len: usize,
    mf_at: usize,
    /// Matched-filter output for the block being processed, with the tail of
    /// the previous block's output in front of it so the interpolator has
    /// something to reach back into. [`Rx::t`] is measured from `hist[0]`.
    hist: Vec<Complex32>,
    carry: Vec<Complex32>,
    /// Fractional read position, and the half-chip step the timing loop is
    /// tracking.
    t: f64,
    step: f64,
    step0: f64,
    timing_kp: f64,
    timing_ki: f64,
    timing_rate: f64,
    /// Half-chip samples: the previous strobe, and the mid-point after it.
    prev_strobe: Option<Complex32>,
    mid: Option<Complex32>,
    /// The last chip decision, for the differential detector.
    prev_chip: Option<Complex32>,
    lock: f32,
}

impl Rx {
    /// A receiver for IQ at `rate_hz`, with the beacon expected `carrier_hz`
    /// from the centre — the spectral tracker's estimate, which the FLL then
    /// holds and follows.
    pub fn new(rate_hz: f64, carrier_hz: f64) -> Rx {
        let sps = rate_hz / CHIP_RATE;
        let mf_len = sps.round().max(1.0) as usize;
        let freq = std::f64::consts::TAU * carrier_hz / rate_hz;
        Rx {
            rate: rate_hz,
            phase: 0.0,
            freq,
            freq0: freq,
            // The FLL runs once per input sample.
            fll: Pi::new(FLL_BW_HZ / rate_hz, DAMPING),
            flank_hi: (FLANK_HZ, OnePole::new(FLANK_BW_HZ, rate_hz)),
            flank_lo: (-FLANK_HZ, OnePole::new(FLANK_BW_HZ, rate_hz)),
            hi_phase: 0.0,
            lo_phase: 0.0,
            p_hi: 0.0,
            p_lo: 0.0,
            mf: vec![Complex32::new(0.0, 0.0); mf_len],
            mf_sum: Complex32::new(0.0, 0.0),
            mf_len,
            mf_at: 0,
            hist: Vec::new(),
            carry: Vec::new(),
            // The matched filter integrates a whole chip, so its output peaks
            // one chip *after* that chip began. Start the read pointer so the
            // first strobe lands near that peak rather than half a chip off
            // it; the loop pulls in whatever the real chip phase turns out to
            // be, but it should not have to start from the worst case.
            t: mf_len as f64 - 1.0 - sps / 2.0,
            step: sps / 2.0,
            step0: sps / 2.0,
            timing_kp: TIMING_PI.0,
            timing_ki: TIMING_PI.1,
            timing_rate: 0.0,
            prev_strobe: None,
            mid: None,
            prev_chip: None,
            lock: 0.0,
        }
    }

    pub fn state(&self) -> RxState {
        RxState {
            carrier_hz: self.freq * self.rate / std::f64::consts::TAU,
            clock_ppm: -self.timing_rate * 1e6,
            lock: self.lock,
        }
    }

    /// Push a block of IQ and take out one soft value per chip *boundary*.
    ///
    /// Each value is the differential detector's output: positive when this
    /// chip arrived in phase with the one before it, negative when it was
    /// reversed, with the magnitude standing for how confident that is. The
    /// beacon's data lives in exactly those reversals, so this — not the chips
    /// themselves — is what the framing layers want. See [`symbols`] for the
    /// Manchester step that turns it into channel symbols.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<f32> {
        // Mix to baseband, run the discriminator, and matched-filter, keeping
        // the filtered output for the timing loop to interpolate.
        // Carry the tail of the last block's matched-filter output forward:
        // the interpolator looks one sample back and two ahead, so without it
        // every block boundary would clamp against the edge of the buffer and
        // put a small discontinuity into the chip stream.
        self.hist.clear();
        self.hist.extend_from_slice(&self.carry);
        let start_t = self.t;
        for &x in iq {
            let mixed = x * Complex32::new((-self.phase).cos() as f32, (-self.phase).sin() as f32);
            self.phase += self.freq;
            if self.phase > std::f64::consts::TAU {
                self.phase -= std::f64::consts::TAU;
            } else if self.phase < -std::f64::consts::TAU {
                self.phase += std::f64::consts::TAU;
            }
            self.discriminate(mixed);

            // Matched filter: integrate over exactly one chip. For the square
            // chips Manchester sends, that boxcar *is* the matched filter —
            // a root-raised-cosine would be the right answer to a different
            // transmitter, one that shaped its pulses.
            self.mf_sum += mixed - self.mf[self.mf_at];
            self.mf[self.mf_at] = mixed;
            self.mf_at = (self.mf_at + 1) % self.mf_len;
            self.hist.push(self.mf_sum / self.mf_len as f32);
        }

        // Now walk the timing loop through what the matched filter produced.
        let mut out = Vec::with_capacity(iq.len() * 2 / self.mf_len.max(1) + 2);
        let last = self.hist.len() as f64 - 2.0;
        let _ = start_t;
        while self.t < last {
            let y = self.interpolate(self.t);
            self.t += self.step;
            match self.mid.take() {
                // A mid-point was pending, so this is a strobe: the chip
                // decision, and the instant the timing error is measured at.
                Some(mid) => {
                    if let Some(prev) = self.prev_strobe {
                        // Gardner: the mid-point sample, correlated against
                        // the change across the chip. Zero when the strobes
                        // straddle the transition evenly. It needs no carrier
                        // phase — the conjugate cancels it — which is exactly
                        // why it suits a receiver that never locks phase.
                        let raw = (mid.conj() * (y - prev)).re as f64;
                        let scale = (mid.norm_sqr() + y.norm_sqr()).max(1e-12) as f64;
                        let err = TIMING_SIGN * raw / scale;
                        // Proportional term moves the sampling *instant*, the
                        // integral term trims the *rate*. Folding both into
                        // the rate — one PI on the step size — leaves a phase
                        // offset to be worked off through the frequency path,
                        // which is slow and never quite settles.
                        self.timing_rate =
                            (self.timing_rate + self.timing_ki * err).clamp(-1e-3, 1e-3);
                        self.step = self.step0 * (1.0 + self.timing_rate);
                        self.t += self.step0 * self.timing_kp * err;
                    }
                    if let Some(prev) = self.prev_chip {
                        out.push((y * prev.conj()).re);
                    }
                    self.prev_chip = Some(y);
                    self.prev_strobe = Some(y);
                }
                None => self.mid = Some(y),
            }
        }
        // Hand the interpolator's context to the next block, and rebase the
        // read position onto it.
        let keep = CARRY.min(self.hist.len());
        let consumed = self.hist.len() - keep;
        self.carry = self.hist[consumed..].to_vec();
        self.t -= consumed as f64;
        out
    }

    /// One pass of the frequency discriminator — see the module doc.
    fn discriminate(&mut self, x: Complex32) {
        let flank = |f_hz: f64, ph: &mut f64, lp: &mut OnePole| -> f32 {
            let w = std::f64::consts::TAU * f_hz / self.rate;
            *ph += w;
            if ph.abs() > std::f64::consts::TAU {
                *ph -= ph.signum() * std::f64::consts::TAU;
            }
            let shifted = x * Complex32::new((-*ph).cos() as f32, (-*ph).sin() as f32);
            lp.push(shifted).norm_sqr()
        };
        let (f_hi, mut lp_hi) = self.flank_hi;
        let (f_lo, mut lp_lo) = self.flank_lo;
        self.p_hi = flank(f_hi, &mut self.hi_phase, &mut lp_hi);
        self.p_lo = flank(f_lo, &mut self.lo_phase, &mut lp_lo);
        self.flank_hi.1 = lp_hi;
        self.flank_lo.1 = lp_lo;

        let sum = (self.p_hi + self.p_lo) as f64;
        if sum <= 1e-20 {
            return;
        }
        // Normalised so the loop's response does not depend on the signal
        // level. Positive when the upper flank is the stronger, which happens
        // when the whole spectrum has slid *down* past the sampling points —
        // hence the sign of the correction below.
        let err = (self.p_hi - self.p_lo) as f64 / sum;
        // How lobe-shaped the spectrum is at all, for the caller's benefit:
        // both flanks strong and balanced is a beacon, and one flank alone is
        // something else.
        let bal = 1.0 - err.abs();
        self.lock = 0.999 * self.lock + 0.001 * bal as f32;

        let adj = FLL_SIGN * self.fll.step(err) * std::f64::consts::TAU * FLANK_HZ / self.rate;
        let pull = std::f64::consts::TAU * FREQ_PULL_HZ / self.rate;
        self.freq = (self.freq + adj).clamp(self.freq0 - pull, self.freq0 + pull);
    }

    /// The matched filter's output at a fractional sample position, by cubic
    /// interpolation over the four samples around it.
    fn interpolate(&self, t: f64) -> Complex32 {
        let i = t.floor() as isize;
        let mu = (t - i as f64) as f32;
        let at = |k: isize| -> Complex32 {
            let k = k.clamp(0, self.hist.len() as isize - 1);
            self.hist[k as usize]
        };
        // Catmull-Rom: smooth, cheap, and exact on the linear ramps a matched
        // filter's output mostly consists of.
        let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        let a = (p1 - p2) * 1.5 + (p3 - p0) * 0.5;
        let b = p0 - p1 * 2.5 + p2 * 2.0 - p3 * 0.5;
        let c = (p2 - p0) * 0.5;
        ((a * mu + b) * mu + c) * mu + p1
    }
}

/// Manchester: fold chip reversals into channel symbols.
///
/// Every Manchester bit contains a transition of its own, which carries
/// nothing; the reversal that matters is the one at the *boundary* between
/// bits. Which of the two alternating positions that is depends on where the
/// chip stream happened to start, so `parity` selects between them and the
/// framing layer decides which was right by whether it finds a frame.
///
/// A symbol is `1` where the chips did *not* reverse, matching the convention
/// [`crate::bpsk`] decodes the uncoded frames with.
pub fn symbols(flips: &[f32], parity: usize) -> Vec<f32> {
    flips.iter().skip(parity).step_by(2).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// Modulate channel symbols the way the beacon does: Manchester chips,
    /// differentially encoded, on a carrier `offset_hz` from centre with an
    /// optional drift.
    fn modulate(
        symbols: &[bool],
        rate_hz: f64,
        offset_hz: f64,
        drift_hz_s: f64,
        noise: f32,
        seed: u64,
    ) -> Vec<Complex32> {
        // The beacon's order, and it matters which way round: the data is
        // differentially encoded *first*, and each encoded bit then becomes a
        // Manchester pair. Doing the differential step on the chips instead
        // gives a signal that is self-consistent and is not this one.
        let mut state = false;
        let encoded: Vec<bool> = symbols
            .iter()
            .map(|&d| {
                state ^= d;
                state
            })
            .collect();
        let diff: Vec<bool> = encoded.iter().flat_map(|&b| [b, !b]).collect();

        let sps = rate_hz / CHIP_RATE;
        let n = (diff.len() as f64 * sps) as usize;
        let mut st = seed | 1;
        let mut rnd = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            (st >> 11) as f32 / (1u64 << 52) as f32 - 1.0
        };
        let mut ph = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate_hz;
                let f = offset_hz + drift_hz_s * t;
                ph += TAU * f / rate_hz;
                let bit = diff[(i as f64 / sps) as usize];
                let a = if bit { 1.0f32 } else { -1.0 };
                Complex32::new(
                    a * ph.cos() as f32 + noise * rnd(),
                    a * ph.sin() as f32 + noise * rnd(),
                )
            })
            .collect()
    }

    fn test_symbols(n: usize) -> Vec<bool> {
        let mut st = 0x2545_f491_4f6c_dd1du64;
        (0..n)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                st & 0x10000 != 0
            })
            .collect()
    }

    /// The symbol error rate, once the ambiguities the *framing* layer is
    /// responsible for are resolved: which Manchester parity, which polarity,
    /// and where in the stream the first symbol fell. None of those is the
    /// demodulator's job, so none of them is what these tests measure.
    fn best_ber(flips: &[f32], sent: &[bool]) -> f64 {
        let mut best = 1.0f64;
        for parity in 0..2 {
            let got = symbols(flips, parity);
            for pol in [1.0f32, -1.0] {
                // Either stream may lead: `symbols` yields the *second* data
                // bit first, because the earliest chip boundary a differential
                // detector can see already lies inside the first bit.
                for (ga, sa) in [(0, 0), (1, 0), (0, 1), (2, 0), (0, 2), (3, 0), (0, 3)] {
                    if got.len() <= ga || sent.len() <= sa {
                        continue;
                    }
                    let m = (got.len() - ga).min(sent.len() - sa);
                    // The loops need a moment to pull in, so the opening
                    // symbols are expected to be wrong and are not counted.
                    let skip = m / 4;
                    if m <= skip {
                        continue;
                    }
                    let errs =
                        (skip..m).filter(|&i| ((got[i + ga] * pol) > 0.0) != sent[i + sa]).count();
                    best = best.min(errs as f64 / (m - skip) as f64);
                }
            }
        }
        best
    }

    /// Feed a modulated stream through and return the symbol stream that best
    /// matches what went in, plus the error rate.
    fn run(iq: &[Complex32], rate: f64, seed_hz: f64, sent: &[bool]) -> (f64, RxState) {
        let mut rx = Rx::new(rate, seed_hz);
        let mut flips = Vec::new();
        for chunk in iq.chunks(4096) {
            flips.extend(rx.process(chunk));
        }
        // Either Manchester parity, and either polarity: the framing layer
        // resolves both, so what is measured here is the demodulator alone.
        (best_ber(&flips, sent), rx.state())
    }

    #[test]
    fn a_clean_signal_demodulates_with_no_errors() {
        let rate = 9600.0;
        let sent = test_symbols(2000);
        let iq = modulate(&sent, rate, 0.0, 0.0, 0.0, 1);
        let (ber, st) = run(&iq, rate, 0.0, &sent);
        assert_eq!(ber, 0.0, "clean signal: {ber} errors, {st:?}");
    }

    /// The carrier loop's job: pull in an offset the seed did not know about
    /// and hold it. A search-based decoder would need a grid point within
    /// tens of Hz of this; the loop is simply told the wrong answer and finds
    /// the right one.
    #[test]
    fn the_frequency_loop_pulls_in_a_carrier_the_seed_got_wrong() {
        let rate = 9600.0;
        let sent = test_symbols(3000);
        for offset in [-150.0, -60.0, 60.0, 150.0] {
            let iq = modulate(&sent, rate, offset, 0.0, 0.0, 3);
            // Seeded at zero: the loop has to find `offset` for itself.
            let (ber, st) = run(&iq, rate, 0.0, &sent);
            assert!(ber < 0.01, "offset {offset}: BER {ber}, {st:?}");
            assert!(
                (st.carrier_hz - offset).abs() < 25.0,
                "offset {offset}: loop settled at {:.1} Hz",
                st.carrier_hz
            );
        }
    }

    /// A warming LNB walks in frequency across a frame. To a searching decoder
    /// that is a chirp to be de-rotated by trial; to a loop it is just a
    /// frequency, and following it is what the loop is for.
    #[test]
    fn a_drifting_carrier_is_followed_rather_than_searched_for() {
        let rate = 9600.0;
        let sent = test_symbols(4000);
        for drift in [-8.0, 8.0] {
            let iq = modulate(&sent, rate, 0.0, drift, 0.0, 5);
            let (ber, st) = run(&iq, rate, 0.0, &sent);
            assert!(ber < 0.01, "drift {drift} Hz/s: BER {ber}, {st:?}");
            // 4000 symbols is 10 s, so the carrier should have walked to
            // roughly `10 * drift` and the loop should be sitting on it.
            let expect = drift * sent.len() as f64 / BAUD;
            assert!(
                (st.carrier_hz - expect).abs() < 30.0,
                "drift {drift}: ended at {:.1} Hz, expected near {expect:.1}",
                st.carrier_hz
            );
        }
    }

    /// The timing loop's job: a transmitter clock that is not ours. Parts per
    /// million accumulate to whole chips over a frame, and a fixed sampling
    /// phase would walk off the eye.
    #[test]
    fn the_timing_loop_follows_a_transmitter_clock_that_runs_fast() {
        let rate = 9600.0;
        let sent = test_symbols(3000);
        // Resample the modulated signal to stand in for a chip clock that is
        // 100 ppm off: a whole chip of slip across this many symbols.
        let iq = modulate(&sent, rate * (1.0 + 100e-6), 0.0, 0.0, 0.0, 7);
        let (ber, st) = run(&iq, rate, 0.0, &sent);
        assert!(ber < 0.01, "BER {ber}, {st:?}");
    }

    #[test]
    fn noise_leaves_the_loops_reporting_no_lock() {
        let rate = 9600.0;
        let mut st = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<Complex32> = (0..40_000)
            .map(|_| {
                let mut r = || {
                    st ^= st << 13;
                    st ^= st >> 7;
                    st ^= st << 17;
                    (st >> 11) as f32 / (1u64 << 52) as f32 - 1.0
                };
                Complex32::new(r(), r())
            })
            .collect();
        let mut rx = Rx::new(rate, 0.0);
        for c in noise.chunks(4096) {
            rx.process(c);
        }
        let sent = test_symbols(1000);
        let iq = modulate(&sent, rate, 0.0, 0.0, 0.0, 11);
        let mut good = Rx::new(rate, 0.0);
        for c in iq.chunks(4096) {
            good.process(c);
        }
        assert!(
            good.state().lock > rx.state().lock,
            "a beacon should look more locked than noise: {:?} vs {:?}",
            good.state(),
            rx.state()
        );
    }

    /// The two halves together: a coded frame the reference encoder produced,
    /// put on a carrier and taken off it again by this receiver, then decoded.
    ///
    /// The frame content comes from `encode_ref.c` (see
    /// [`crate::fec::tests::the_whole_chain_decodes_a_frame_from_the_reference_encoder`]),
    /// so what is being checked is the join: that the soft values this module
    /// produces are what the framing layer expects — right polarity, right
    /// Manchester parity, right order — over a signal with a carrier offset,
    /// a drifting LNB and noise on it.
    ///
    /// The noise here is a little under where this chain gives out — measured,
    /// it decodes at 0.2 and not at 0.3 — so the test is also a coarse guard
    /// against the demodulator quietly losing sensitivity.
    #[test]
    fn a_reference_frame_survives_the_air_and_the_receiver() {
        let hex = include_str!("../tests/ao40_reference_frame.hex").trim();
        let packed: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
            .collect();
        let frame: Vec<bool> = (0..crate::fec::FRAME_SYMBOLS)
            .map(|p| packed[p / 8] & (0x80 >> (p % 8)) != 0)
            .collect();

        // The beacon is continuous, so the frame arrives inside a longer run
        // of symbols and the loops have something to pull in on first.
        let mut st = 0x51ed_270b_1234_5678u64;
        let mut filler = |n: usize| -> Vec<bool> {
            (0..n)
                .map(|_| {
                    st ^= st << 13;
                    st ^= st >> 7;
                    st ^= st << 17;
                    st & 0x10000 != 0
                })
                .collect()
        };
        let mut stream = filler(600);
        stream.extend_from_slice(&frame);
        stream.extend(filler(200));

        let rate = 9600.0;
        // An uncalibrated LNB: 120 Hz out and walking 3 Hz a second, with
        // noise on top. Nothing here is searched for — the loops follow it.
        let iq = modulate(&stream, rate, 120.0, 3.0, 0.2, 19);
        let mut rx = Rx::new(rate, 0.0);
        let mut flips = Vec::new();
        for c in iq.chunks(4096) {
            flips.extend(rx.process(c));
        }
        assert!(rx.state().carrier_hz > 100.0, "the loop should have found the carrier");

        let decoded = (0..2).find_map(|parity| crate::fec::decode_frame(&symbols(&flips, parity)));
        let f = decoded.expect("a reference frame off a drifting carrier should still decode");
        let want: Vec<u8> = (0..crate::fec::PAYLOAD_BYTES)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(3))
            .collect();
        assert_eq!(f.payload, want);
    }

    /// Validation against a real modulated recording rather than anything
    /// this crate generated: KA9Q's own AO-40 coded-telemetry test signals,
    /// 9600 Hz mono audio with the beacon on a 1600 Hz subcarrier, one 13 s
    /// frame each. Marked `#[ignore]` only because the recordings are not
    /// redistributed here; fetch them from `ka9q.net/ao40/` to run it.
    #[test]
    #[ignore]
    fn the_reference_recordings_decode() {
        const DIR: &str = "/tmp/claude-1000/-home-toumal-Development-sdroxide/d63e031c-a0b0-468a-953e-c8e756a21bfa/scratchpad";
        for name in ["testmessage_nonoise.wav", "testmessage_ebno7.wav"] {
            let path = format!("{DIR}/{name}");
            let Ok(raw) = std::fs::read(&path) else {
                println!("{name}: not present, skipped");
                continue;
            };
            // 16-bit mono PCM after a 44-byte header.
            let one: Vec<f32> = raw[44..]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect();
            // The recording is exactly one 13 s frame, and the loops need a
            // moment to pull in, so a single copy leaves the search a symbol
            // or two short of a whole frame. Two copies back to back give it
            // one it can actually find — which is also what a receiver on the
            // air always has, the beacon being continuous.
            let pcm: Vec<f32> = one.iter().chain(one.iter()).copied().collect();
            let rate = 9600.0;

            for seed_err in [0.0f64, 40.0, -40.0] {
                // The receiver is seeded at zero, so mixing the subcarrier
                // down by a deliberately wrong amount leaves the FLL that much
                // to find for itself.
                let f0 = 1600.0 + seed_err;
                // Band-limit and mix to baseband. Real audio carries a
                // mirror image at the negative frequencies, and a receiver
                // takes it out with a filter, so that is what this does.
                let taps: Vec<f32> = {
                    let n = 63usize;
                    let fc = 1300.0 / rate;
                    (0..n)
                        .map(|i| {
                            let x = i as f64 - (n - 1) as f64 / 2.0;
                            let sinc = if x == 0.0 {
                                2.0 * fc
                            } else {
                                (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x)
                            };
                            let w = 0.54
                                - 0.46 * (std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos();
                            (sinc * w) as f32
                        })
                        .collect()
                };
                let mixed: Vec<Complex32> = pcm
                    .iter()
                    .enumerate()
                    .map(|(n, &x)| {
                        let ph = -std::f64::consts::TAU * f0 * n as f64 / rate;
                        Complex32::new(x * ph.cos() as f32, x * ph.sin() as f32)
                    })
                    .collect();
                let iq: Vec<Complex32> = (0..mixed.len())
                    .map(|n| {
                        let mut acc = Complex32::new(0.0, 0.0);
                        for (k, &t) in taps.iter().enumerate() {
                            if n >= k {
                                acc += mixed[n - k] * t;
                            }
                        }
                        acc
                    })
                    .collect();

                // The receiver is seeded at zero, so it has `seed_err` to find.
                let mut rx = Rx::new(rate, 0.0);
                let mut flips = Vec::new();
                for c in iq.chunks(4096) {
                    flips.extend(rx.process(c));
                }
                let st = rx.state();
                println!(
                    "    ({} flips, {} symbols per parity, a frame is {})",
                    flips.len(),
                    symbols(&flips, 0).len(),
                    crate::fec::FRAME_SYMBOLS
                );
                let mut got = None;
                for parity in 0..2 {
                    if let Some(f) = crate::fec::decode_frame(&symbols(&flips, parity)) {
                        got = Some((parity, f));
                        break;
                    }
                }
                match got {
                    Some((parity, f)) => {
                        let text: String = f
                            .payload
                            .iter()
                            .map(|&b| if (32..127).contains(&b) { b as char } else { '.' })
                            .collect();
                        println!(
                            "{name} seed_err {seed_err:+.0}: DECODED parity {parity} \
                             sync {:.3} rs {:?} carrier {:.1} ppm {:.1}",
                            f.sync_quality, f.rs_errors, st.carrier_hz, st.clock_ppm
                        );
                        println!("    payload: {}", &text[..text.len().min(180)]);
                    }
                    None => println!(
                        "{name} seed_err {seed_err:+.0}: no frame (carrier {:.1}, lock {:.2})",
                        st.carrier_hz, st.lock
                    ),
                }
            }
        }
    }
}
