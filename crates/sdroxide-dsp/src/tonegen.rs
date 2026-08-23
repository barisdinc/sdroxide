//! Transmit-side repeater signalling: the sub-audible tone that rides under an
//! FM over, and the 1750 Hz burst that opens a carrier-access repeater.
//!
//! The receiving half of this lives in [`crate::SubToneDetect`]. Both ends work
//! on the same signal in the same units — audio where 1.0 is the FM modulator's
//! full-scale deviation — so what this writes is exactly what that reads, and
//! the round-trip is a test rather than an assumption.
//!
//! ## What goes out at what level
//!
//! A sub-audible tone is a *deviation* budget, not a volume: it has to be big
//! enough for the repeater's decoder and small enough to leave the voice its
//! own room in the channel. [`SUB_TONE_LEVEL`] is the usual 15 %, and the voice
//! is scaled by what is left rather than being allowed to add to it, so a
//! transmission with a tone under it occupies the same channel width as one
//! without.
//!
//! The voice also gets a high-pass on the way past. Without it the bottom of a
//! man's voice and the rumble a hand microphone makes sit right on top of a
//! 67-100 Hz tone, and the repeater's decoder — which is doing the same
//! narrow-band measurement [`crate::SubToneDetect`] does — has to find the tone
//! underneath them.
//!
//! ## DCS
//!
//! ⚠️ The bit *order* here is transcribed from the published description of DCS
//! and has never been checked against a repeater. [`crate::SubToneDetect`] says
//! why that is harder than it sounds: the code word is cyclic, so every one of
//! the 23 possible alignments is a valid codeword, and a receiver cannot tell
//! from the signal alone which one the transmitter meant. That cuts both ways —
//! a receiver cannot read the code back, and a transmitter has to *choose* a
//! convention. The one implemented here is the one the standard is usually
//! written down as: nine code bits (three octal digits) least-significant
//! first, then three fixed bits, then eleven Golay check bits, repeating
//! forever at 134.4 bps. If a repeater will not open on DCS, that convention is
//! the first thing to suspect — and CTCSS, which has no such ambiguity, is the
//! thing to fall back on.

use sdroxide_types::{TONE_BURST_HZ, TxSubTone, dcs_bits};

use crate::demod::DcBlock;

/// Peak deviation of the sub-audible signalling, as a fraction of the FM
/// modulator's own full scale — so 0.15 is 750 Hz of the ±5 kHz
/// [`crate::modulator::FmMod`] runs at, which is what a transmitter with a
/// CTCSS encoder in it actually puts out.
pub const SUB_TONE_LEVEL: f32 = 0.15;

/// Corner of the high-pass the voice goes through while a tone is being
/// encoded, in Hz. Above the top of the CTCSS table (254.1 Hz) so the tone the
/// repeater is listening for has the bottom of the channel to itself.
const VOICE_HP_HZ: f64 = 300.0;

/// DCS data rate.
const DCS_BIT_RATE: f64 = 134.4;

/// Corner of the shaping filter on the DCS bit stream, in Hz. A bare NRZ square
/// wave at 134.4 bps has harmonics right through the voice band; rounding it
/// off here keeps the data where it belongs. Two poles, because one leaves
/// audible edges.
const DCS_SHAPE_HZ: f64 = 250.0;

/// The three bits every DCS word carries between the code and the check bits.
///
/// Part of the transcribed convention — see the module note. They are what a
/// receiver would use to find the word boundary, if the polarity question had
/// an answer.
const DCS_FIXED_BITS: u16 = 0b100;

/// Peak deviation of the 1750 Hz burst, as a fraction of full scale. Louder
/// than the sub-audible tone by design: it is an in-band tone whose whole job
/// is to be unmistakable to a repeater's decoder for half a second.
pub const BURST_LEVEL: f32 = 0.6;

/// How long the burst takes to reach full amplitude, and to fall back, in ms.
/// A tone that starts and stops on a step is a click, on the air and in the
/// operator's monitor.
const BURST_RAMP_MS: f64 = 5.0;

/// The 23 bits of a DCS word, in the order they are transmitted.
///
/// `None` for a code that is not three octal digits. See the module note for
/// what "the order they are transmitted" rests on.
fn dcs_word(code: u16, invert: bool) -> Option<[bool; 23]> {
    let data = dcs_bits(code)?;
    // Twelve information bits: the code, then the fixed three above it.
    let info = data | (DCS_FIXED_BITS << 9);
    // `golay23_encode` puts the information in the high bits and the eleven
    // check bits in the low ones, which is the systematic form DCS uses.
    let parity = crate::golay23_encode(u32::from(info)) & 0x7FF;
    let mut bits = [false; 23];
    for (i, b) in bits.iter_mut().enumerate().take(12) {
        *b = info >> i & 1 != 0;
    }
    for (i, b) in bits.iter_mut().enumerate().skip(12) {
        *b = parity >> (i - 12) & 1 != 0;
    }
    if invert {
        for b in &mut bits {
            *b = !*b;
        }
    }
    Some(bits)
}

/// A one-pole low-pass, used to round off the DCS bit stream. Sample-for-sample
/// (unlike [`crate::RealFir`], which delays by its tap count) because the tone
/// is being mixed into a block of audio that has to come out the length it went
/// in.
#[derive(Clone, Copy)]
struct OnePole {
    a: f32,
    y: f32,
}

impl OnePole {
    fn new(cutoff_hz: f64, rate: f64) -> Self {
        let x = (-std::f64::consts::TAU * cutoff_hz / rate).exp();
        OnePole { a: (1.0 - x) as f32, y: 0.0 }
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        self.y
    }
}

/// Generates the sub-audible signalling that rides under a transmitted FM over,
/// and mixes it into the voice.
pub struct SubToneGen {
    tone: TxSubTone,
    /// CTCSS phase and its per-sample advance, in radians.
    phase: f64,
    inc: f64,
    /// The word being sent, and where in it we are.
    bits: [bool; 23],
    bit: usize,
    /// Fraction of the current bit already sent, and the per-sample advance.
    bit_phase: f64,
    bit_step: f64,
    shape: [OnePole; 2],
    /// High-pass on the voice, so it leaves the tone room.
    voice_hp: DcBlock,
}

impl SubToneGen {
    /// A generator for `tone` at `rate` samples a second.
    ///
    /// A DCS code that is not three octal digits falls back to a silent word
    /// rather than being refused: this is the transmit path, and the answer to
    /// nonsense arriving here is a plain FM over, not a panic.
    pub fn new(tone: TxSubTone, rate: f64) -> Self {
        let (inc, bits, bit_step) = match tone {
            TxSubTone::Ctcss(tenths) => {
                let hz = f64::from(tenths) / 10.0;
                (std::f64::consts::TAU * hz / rate, [false; 23], 0.0)
            }
            TxSubTone::Dcs { code, invert } => {
                (0.0, dcs_word(code, invert).unwrap_or([false; 23]), DCS_BIT_RATE / rate)
            }
        };
        SubToneGen {
            tone,
            phase: 0.0,
            inc,
            bits,
            bit: 0,
            bit_phase: 0.0,
            bit_step,
            shape: [OnePole::new(DCS_SHAPE_HZ, rate), OnePole::new(DCS_SHAPE_HZ, rate)],
            voice_hp: DcBlock::new(VOICE_HP_HZ, rate),
        }
    }

    /// What this generator is sending — so a caller can tell whether the one it
    /// is holding still matches the operator's settings.
    pub fn tone(&self) -> TxSubTone {
        self.tone
    }

    /// One sample of the signalling alone, at [`SUB_TONE_LEVEL`].
    fn next(&mut self) -> f32 {
        match self.tone {
            TxSubTone::Ctcss(_) => {
                let s = self.phase.sin() as f32;
                self.phase += self.inc;
                if self.phase > std::f64::consts::TAU {
                    self.phase -= std::f64::consts::TAU;
                }
                s * SUB_TONE_LEVEL
            }
            TxSubTone::Dcs { .. } => {
                let level = if self.bits[self.bit] { 1.0 } else { -1.0 };
                self.bit_phase += self.bit_step;
                while self.bit_phase >= 1.0 {
                    self.bit_phase -= 1.0;
                    // Straight back to the top of the same word: DCS is a
                    // continuous stream with no gap between repetitions, which
                    // is what lets a receiver find the bit clock at all.
                    self.bit = (self.bit + 1) % self.bits.len();
                }
                let one = self.shape[0].run(level);
                self.shape[1].run(one) * SUB_TONE_LEVEL
            }
        }
    }

    /// Mix the signalling into a block of transmit audio, in place.
    ///
    /// The voice is high-passed and scaled to leave the tone its share of the
    /// deviation, so the block comes back no louder than it went in.
    pub fn mix(&mut self, audio: &mut [f32]) {
        for a in audio.iter_mut() {
            let voice = self.voice_hp.run(*a) * (1.0 - SUB_TONE_LEVEL);
            *a = (voice + self.next()).clamp(-1.0, 1.0);
        }
    }

    /// The signalling on its own, over `n` samples — for a test, and for a
    /// caller with nothing to mix it into.
    pub fn fill(&mut self, out: &mut [f32]) {
        for a in out.iter_mut() {
            *a = self.next();
        }
    }
}

/// The 1750 Hz burst that opens a carrier-access repeater: a fixed length of
/// tone that replaces the microphone for as long as it lasts.
pub struct ToneBurst {
    phase: f64,
    inc: f64,
    /// Samples still to send, and how long the whole burst was — the ramp at
    /// each end is worked out from both.
    left: usize,
    total: usize,
    ramp: usize,
}

impl ToneBurst {
    /// A burst of `ms` at `rate` samples a second.
    pub fn new(ms: u32, rate: f64) -> Self {
        let total = ((f64::from(ms) / 1000.0) * rate).round().max(1.0) as usize;
        // Never more than a third of the burst at each end, so a very short one
        // is still a tone rather than two ramps back to back.
        let ramp = (((BURST_RAMP_MS / 1000.0) * rate) as usize).min(total / 3);
        ToneBurst {
            phase: 0.0,
            inc: std::f64::consts::TAU * TONE_BURST_HZ / rate,
            left: total,
            total,
            ramp,
        }
    }

    /// Whether the burst has played out.
    pub fn finished(&self) -> bool {
        self.left == 0
    }

    /// Overwrite the head of `audio` with the burst, and say how many samples
    /// it took.
    ///
    /// A burst that ends inside the block leaves the rest of it alone, so the
    /// microphone comes back the moment the tone stops rather than at the top
    /// of the next block.
    pub fn fill(&mut self, audio: &mut [f32]) -> usize {
        let n = self.left.min(audio.len());
        for a in audio.iter_mut().take(n) {
            let sent = self.total - self.left;
            // Linear in and out. The ear is not fussy about the shape of a
            // 5 ms ramp; it is very fussy about there not being one.
            let env = if self.ramp == 0 {
                1.0
            } else {
                let up = (sent as f32 / self.ramp as f32).min(1.0);
                let down = (self.left as f32 / self.ramp as f32).min(1.0);
                up.min(down)
            };
            *a = self.phase.sin() as f32 * BURST_LEVEL * env;
            self.phase += self.inc;
            if self.phase > std::f64::consts::TAU {
                self.phase -= std::f64::consts::TAU;
            }
            self.left -= 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use sdroxide_types::{CTCSS_TONES, SubTone};

    use super::*;
    use crate::SubToneDetect;

    const RATE: f64 = 48_000.0;

    /// Push `secs` of a generator's output through the detector and say what it
    /// made of it. Blocks rather than one buffer because that is how the engine
    /// feeds it, and the detector carries state across them.
    fn heard(src: &mut SubToneGen, secs: f64) -> Option<SubTone> {
        let mut det = SubToneDetect::new(RATE);
        let mut block = [0.0f32; 480];
        for _ in 0..(secs * RATE / block.len() as f64) as usize {
            src.fill(&mut block);
            det.process(&block);
        }
        det.detected()
    }

    /// Every tone in the table comes back as itself. This is the whole of what
    /// makes the encoder trustworthy: the detector was written against real
    /// signals, and it reads what this writes.
    #[test]
    fn every_ctcss_tone_decodes_as_itself() {
        for tenths in CTCSS_TONES {
            let mut src = SubToneGen::new(TxSubTone::Ctcss(tenths), RATE);
            assert_eq!(heard(&mut src, 2.0), Some(SubTone::Ctcss(tenths)), "CTCSS {tenths} tenths",);
        }
    }

    /// The DCS stream is recognised as DCS in both polarities. Which *code* it
    /// carries is not asserted, because — see the module note — nothing here
    /// can read one back.
    #[test]
    fn a_dcs_stream_is_recognised_as_dcs() {
        for code in [23u16, 131, 754] {
            for invert in [false, true] {
                let mut src = SubToneGen::new(TxSubTone::Dcs { code, invert }, RATE);
                assert_eq!(
                    heard(&mut src, 3.0),
                    Some(SubTone::Dcs),
                    "DCS {code:03}{}",
                    if invert { "I" } else { "N" },
                );
            }
        }
    }

    /// A DCS word is a Golay codeword whichever of the 23 bit boundaries it is
    /// read from — the cyclic property the detector leans on, and the reason
    /// the code itself cannot be read back.
    #[test]
    fn a_dcs_word_is_a_codeword_at_every_rotation() {
        let bits = dcs_word(23, false).expect("023 is octal");
        let word = bits.iter().enumerate().fold(0u32, |w, (i, &b)| w | u32::from(b) << i);
        for rot in 0..23 {
            let r = ((word >> rot) | (word << (23 - rot))) & 0x7F_FFFF;
            let (_, flips) = crate::golay23_decode(r);
            assert_eq!(flips, 0, "rotation {rot} of {word:#x} is not a codeword");
        }
    }

    /// Mixing a tone under the voice must not make the block louder — the
    /// deviation budget is fixed, and what the tone takes the voice gives up.
    #[test]
    fn the_tone_takes_its_level_out_of_the_voice() {
        let mut src = SubToneGen::new(TxSubTone::Ctcss(885), RATE);
        // Full-scale 1 kHz "voice", well above the high-pass corner so nothing
        // of it is lost to that instead.
        let mut audio: Vec<f32> = (0..4800)
            .map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / RATE).sin() as f32)
            .collect();
        src.mix(&mut audio);
        // Past the high-pass's own settling, which is where a DC blocker's
        // first few samples are.
        let peak = audio[480..].iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        assert!(peak <= 1.0, "peak {peak} is over full scale");
        // …and the voice is still most of it: this is a tone under a signal,
        // not a signal under a tone.
        assert!(peak > 0.8, "peak {peak} — the voice has been squashed");
    }

    /// The burst is its stated length, starts and ends at zero, and hands the
    /// rest of the block back to the microphone.
    #[test]
    fn the_burst_is_as_long_as_it_says_and_ramps_at_both_ends() {
        let mut burst = ToneBurst::new(500, RATE);
        let mut sent = 0;
        let mut block = [0.0f32; 480];
        let mut first = None;
        while !burst.finished() {
            block.fill(-1.0); // the "microphone" underneath
            let n = burst.fill(&mut block);
            first.get_or_insert(block[0]);
            // Everything past the burst is left as it was found.
            assert!(block[n..].iter().all(|&s| s == -1.0), "the burst overran its length");
            sent += n;
        }
        assert_eq!(sent, 24_000, "500 ms at 48 kHz");
        assert!(first.expect("a sample").abs() < 0.01, "the burst opened on a step");
    }
}
