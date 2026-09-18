//! A port of `modem.py` — a real OFDM modulator/demodulator.
//!
//! This is NOT a "sonification" that plays made-up tones: it modulates and
//! demodulates with a real IFFT/FFT and is subject to real bit errors. It is
//! meant to be **bit-for-bit faithful** to the Python version — the cross-
//! vector tests (`tests/`) prove that.
//!
//! PHY: 8000 Hz sample rate, 256-point FFT (~31.25 Hz subcarrier spacing),
//! a 64-sample (8 ms) guard interval, 77 data subcarriers inside a ~2.7 kHz
//! band (~312–2688 Hz). Synchronisation is Schmidl-Cox-style autocorrelation;
//! coding is frequency-domain differential BPSK/QPSK.
//!
//! Deliberate simplifications (the same as `modem.py`): no adaptive bit
//! loading, no channel estimation/equaliser, no LDPC/RS — integrity is CRC32
//! only, and error correction is left to the upper layer's ARQ.

use std::sync::Arc;

use num_complex::Complex64;
use rand::Rng;
use rustfft::{Fft, FftPlanner};

pub use crate::netproto::Mode;

pub const SAMPLE_RATE: u32 = 8000;
pub const N: usize = 256; // FFT size
pub const CP_LEN: usize = 64; // guard interval / cyclic prefix (8 ms)
pub const SYMBOL_LEN: usize = N + CP_LEN; // 320

const HEADER_BITS: usize = 17; // 16-bit length + 1-bit mode flag
const HEADER_REPEAT: usize = 4;
pub(crate) const SYNC_THRESHOLD: f64 = 0.25;

/// Carrier-offset search range, in subcarrier bins (~31.25 Hz each) — see
/// [`carrier_diffs`]. Bounded by where the data carriers (bins 10..87) can
/// slide before they fall on DC or spill past the header/data band's
/// headroom toward Nyquist (bin 128).
pub(crate) const MAX_SHIFT_UP: i32 = 40; // +1250 Hz: 86 + 40 = 126, still short of bin 128
pub(crate) const MAX_SHIFT_DOWN: i32 = -9; // -281 Hz: 10 - 9 = 1, still short of DC

/// `list(range(10, 87))` — 77 data subcarriers.
#[inline]
fn data_carriers() -> impl Iterator<Item = usize> {
    10..87
}
const N_DATA_CARRIERS: usize = 77;
/// The number of subcarriers that carry information under differential coding
/// (the first subcarrier is the reference).
const N_INFO: usize = N_DATA_CARRIERS - 1; // 76

/// `list(range(2, N // 2, 2))` — Schmidl-Cox: the even indices (63 of them).
#[inline]
fn preamble_carriers() -> impl Iterator<Item = usize> {
    (2..N / 2).step_by(2)
}

/// `np.random.RandomState(1234).choice([1.0, -1.0], size=63)` — the FIXED
/// preamble both TX and RX know. Rather than imitating the NumPy RNG, the
/// sequence itself is embedded (see the plan: generated with `python3 -c "..."`).
#[rustfmt::skip]
const PREAMBLE_SYMBOLS: [f64; 63] = [
    -1.0,-1.0, 1.0,-1.0, 1.0, 1.0, 1.0,-1.0,-1.0,-1.0,-1.0,-1.0, 1.0, 1.0,-1.0, 1.0,
     1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0,-1.0, 1.0,-1.0,-1.0, 1.0, 1.0,-1.0, 1.0,
     1.0,-1.0, 1.0,-1.0, 1.0, 1.0, 1.0,-1.0,-1.0,-1.0, 1.0,-1.0,-1.0, 1.0,-1.0, 1.0,
    -1.0, 1.0,-1.0,-1.0,-1.0,-1.0, 1.0,-1.0, 1.0,-1.0,-1.0, 1.0, 1.0,-1.0, 1.0,
];

// ------------------------------------------------------------------ //
// Shared helpers
// ------------------------------------------------------------------ //

/// `{carrier_index: complex_value}` -> an N-sample REAL time-domain signal
/// (with Hermitian symmetry: `X[N-k] = conj(X[k])`). `np.fft.ifft(...).real`.
fn spectrum_to_time(inv_fft: &dyn Fft<f64>, pairs: &[(usize, Complex64)]) -> Vec<f64> {
    let mut x = vec![Complex64::new(0.0, 0.0); N];
    for &(k, v) in pairs {
        x[k] = v;
        x[N - k] = v.conj();
    }
    inv_fft.process(&mut x);
    // rustfft's inverse is unscaled; np.fft.ifft is scaled by 1/N.
    x.iter().map(|c| c.re / N as f64).collect()
}

/// `_add_cp`: copy the last `CP_LEN` samples to the front -> 320 samples.
fn add_cp(symbol: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(SYMBOL_LEN);
    out.extend_from_slice(&symbol[N - CP_LEN..]);
    out.extend_from_slice(symbol);
    out
}

fn make_preamble(inv_fft: &dyn Fft<f64>) -> Vec<f64> {
    let pairs: Vec<(usize, Complex64)> = preamble_carriers()
        .zip(PREAMBLE_SYMBOLS.iter())
        .map(|(k, &s)| (k, Complex64::new(s, 0.0)))
        .collect();
    add_cp(&spectrum_to_time(inv_fft, &pairs))
}

/// `np.unpackbits` — MSB-first within a byte.
fn bits_from_bytes(data: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(data.len() * 8);
    for &b in data {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1);
        }
    }
    bits
}

/// `np.packbits` — MSB-first; leftover bits that do not fill a whole byte are
/// discarded.
fn bytes_from_bits(bits: &[u8]) -> Vec<u8> {
    let n = (bits.len() / 8) * 8;
    let mut out = Vec::with_capacity(n / 8);
    for chunk in bits[..n].chunks(8) {
        let mut byte = 0u8;
        for &bit in chunk {
            byte = (byte << 1) | (bit & 1);
        }
        out.push(byte);
    }
    out
}

/// Differential accumulator: `seq[0] = 1+0j` (the reference), then `cur *= step`.
/// N_INFO steps -> N_DATA_CARRIERS values.
fn differential_sequence(steps: &[Complex64]) -> Vec<Complex64> {
    let mut seq = Vec::with_capacity(steps.len() + 1);
    let mut cur = Complex64::new(1.0, 0.0);
    seq.push(cur);
    for &v in steps {
        cur *= v;
        seq.push(cur);
    }
    seq
}

fn carrier_spectrum(seq: &[Complex64]) -> Vec<(usize, Complex64)> {
    data_carriers().zip(seq.iter().copied()).collect()
}

// ------------------------------------------------------------------ //
// The header symbol (BPSK, frequency-domain DIFFERENTIAL coding)
// ------------------------------------------------------------------ //

fn make_header_symbol(inv_fft: &dyn Fft<f64>, payload_len: usize, mode: Mode) -> Vec<f64> {
    let mut header_bits = Vec::with_capacity(HEADER_BITS);
    for i in 0..16 {
        header_bits.push(((payload_len >> (15 - i)) & 1) as u8);
    }
    header_bits.push(if mode == Mode::Bpsk { 1 } else { 0 });

    // tile(header_bits, 4) -> 68, then zero-pad to 76.
    let mut padded = [0u8; N_INFO];
    let repeated_len = HEADER_BITS * HEADER_REPEAT; // 68
    for i in 0..repeated_len.min(N_INFO) {
        padded[i] = header_bits[i % HEADER_BITS];
    }

    let steps: Vec<Complex64> = padded
        .iter()
        .map(|&b| if b == 0 { Complex64::new(1.0, 0.0) } else { Complex64::new(-1.0, 0.0) })
        .collect();

    let seq = differential_sequence(&steps);
    add_cp(&spectrum_to_time(inv_fft, &carrier_spectrum(&seq)))
}

fn decode_header_symbol(
    fwd_fft: &dyn Fft<f64>,
    symbol_no_cp: &[f64],
    shift: i32,
) -> Option<(usize, Mode)> {
    let diffs = carrier_diffs(fwd_fft, symbol_no_cp, shift);
    let bits: Vec<u8> = diffs.iter().map(|d| (d.re < 0.0) as u8).collect(); // 76

    let reps = HEADER_REPEAT.min(N_INFO / HEADER_BITS); // 4
    if reps < 1 {
        return None;
    }
    let mut votes = [0u32; HEADER_BITS];
    for r in 0..reps {
        for j in 0..HEADER_BITS {
            votes[j] += bits[r * HEADER_BITS + j] as u32;
        }
    }
    // Python: decoded = votes > reps/2  (float division) -> "> 2" for reps=4.
    let decoded: Vec<u8> = votes.iter().map(|&v| (2 * v > reps as u32) as u8).collect();

    let mut val = 0usize;
    for &b in &decoded[..16] {
        val = (val << 1) | b as usize;
    }
    let mode = if decoded[16] == 1 { Mode::Bpsk } else { Mode::Qpsk };
    Some((val, mode))
}

// ------------------------------------------------------------------ //
// Data symbols (QPSK by default, BPSK optional)
// ------------------------------------------------------------------ //

#[inline]
fn qpsk_symbol(b0: u8, b1: u8) -> Complex64 {
    // (0,0)->(1+1j)/√2, (0,1)->(1-1j)/√2, (1,0)->(-1+1j)/√2, (1,1)->(-1-1j)/√2
    let s = std::f64::consts::FRAC_1_SQRT_2;
    let re = if b0 == 0 { s } else { -s };
    let im = if b1 == 0 { s } else { -s };
    Complex64::new(re, im)
}

fn make_data_symbols(inv_fft: &dyn Fft<f64>, bits: &[u8], mode: Mode) -> Vec<Vec<f64>> {
    let bits_per_carrier = if mode == Mode::Bpsk { 1 } else { 2 };
    let bits_per_symbol = bits_per_carrier * N_INFO;

    let mut buf = bits.to_vec();
    let pad = (bits_per_symbol - buf.len() % bits_per_symbol) % bits_per_symbol;
    buf.extend(std::iter::repeat_n(0u8, pad));

    let n_symbols = buf.len() / bits_per_symbol;
    let mut out = Vec::with_capacity(n_symbols);
    for s in 0..n_symbols {
        let chunk = &buf[s * bits_per_symbol..(s + 1) * bits_per_symbol];
        let steps: Vec<Complex64> = if mode == Mode::Bpsk {
            chunk
                .iter()
                .map(|&b| if b == 0 { Complex64::new(1.0, 0.0) } else { Complex64::new(-1.0, 0.0) })
                .collect()
        } else {
            chunk.chunks(2).map(|p| qpsk_symbol(p[0], p[1])).collect()
        };
        let seq = differential_sequence(&steps);
        out.push(add_cp(&spectrum_to_time(inv_fft, &carrier_spectrum(&seq))));
    }
    out
}

/// FFT -> `X[DATA_CARRIERS + shift]` -> `diffs = vals[1:] * conj(vals[:-1])` (76
/// of them).
///
/// `shift` reads every carrier `shift` bins away from where a zero-offset
/// transmission would put it (~31.25 Hz/bin). Two stations' radios rarely
/// share the same dial frequency down to the hertz — a plain LO/TCXO
/// mismatch of a few hundred Hz between two independent SDRs is normal, and
/// the mode has no tone offset to absorb it — so a fixed carrier error just
/// slides every subcarrier sideways by the same number of bins. Because the
/// coding is differential *within* a symbol (across adjacent carriers, not
/// across time), reading the whole symbol `shift` bins over costs nothing
/// once the right shift is found: see the search in [`Modem::demodulate`].
fn carrier_diffs(fwd_fft: &dyn Fft<f64>, symbol_no_cp: &[f64], shift: i32) -> Vec<Complex64> {
    let mut buf: Vec<Complex64> = symbol_no_cp.iter().map(|&s| Complex64::new(s, 0.0)).collect();
    fwd_fft.process(&mut buf);
    let vals: Vec<Complex64> = data_carriers().map(|k| buf[(k as i32 + shift) as usize]).collect();
    vals.windows(2).map(|w| w[1] * w[0].conj()).collect()
}

fn decode_data_symbol(
    fwd_fft: &dyn Fft<f64>,
    symbol_no_cp: &[f64],
    mode: Mode,
    shift: i32,
) -> Vec<u8> {
    let diffs = carrier_diffs(fwd_fft, symbol_no_cp, shift);
    let mut bits = Vec::new();
    for d in diffs {
        bits.push((d.re < 0.0) as u8);
        if mode != Mode::Bpsk {
            bits.push((d.im < 0.0) as u8);
        }
    }
    bits
}

// ------------------------------------------------------------------ //
// Modem
// ------------------------------------------------------------------ //

/// The OFDM modulator/demodulator. Stateless: every call handles one whole
/// transmission (preamble + header + data) from start to finish. The FFT plans
/// are cached.
#[derive(Clone)]
pub struct Modem {
    fwd: Arc<dyn Fft<f64>>,
    inv: Arc<dyn Fft<f64>>,
}

impl Default for Modem {
    fn default() -> Self {
        Self::new()
    }
}

impl Modem {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f64>::new();
        Self { fwd: planner.plan_fft_forward(N), inv: planner.plan_fft_inverse(N) }
    }

    /// JSON/raw payload -> int16 audio samples (one whole transmission).
    pub fn modulate(&self, payload: &[u8], mode: Mode) -> Vec<i16> {
        self.modulate_with_leadin(payload, mode, rand::thread_rng().gen_range(20..300))
    }

    /// For tests: the lead-in length can be given deterministically.
    pub fn modulate_with_leadin(&self, payload: &[u8], mode: Mode, lead_in: usize) -> Vec<i16> {
        let crc = crate::netproto::crc32(payload);
        let mut full = payload.to_vec();
        full.extend_from_slice(&crc.to_be_bytes());
        let bits = bits_from_bytes(&full);

        let mut waveform: Vec<f64> = Vec::new();
        waveform.extend(std::iter::repeat_n(0.0, lead_in));
        waveform.extend(make_preamble(&*self.inv));
        waveform.extend(make_header_symbol(&*self.inv, full.len(), mode));
        for sym in make_data_symbols(&*self.inv, &bits, mode) {
            waveform.extend(sym);
        }
        // Padding so the last symbol does not run past the buffer if it slips a
        // few samples.
        waveform.extend(std::iter::repeat_n(0.0, CP_LEN));

        let peak = waveform.iter().map(|v| v.abs()).fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
        let scale = 0.7 * 32767.0 / peak;
        waveform
            .iter()
            .map(|v| {
                let s = v * scale;
                // np.astype(np.int16): truncation toward zero.
                s.trunc().clamp(i16::MIN as f64, i16::MAX as f64) as i16
            })
            .collect()
    }

    /// int16 audio samples -> payload bytes. `None` if it cannot be decoded
    /// (on a real radio this is treated as "heard nothing").
    pub fn demodulate(&self, samples: &[i16]) -> Option<Vec<u8>> {
        self.demodulate_verbose(samples).payload
    }

    /// Same decode, with the intermediate results a "why didn't that decode?"
    /// question needs: whether timing sync found anything and how strong,
    /// which carrier-offset shift (if any) produced a plausible header, and
    /// whether a shift got as far as a full data decode before failing CRC.
    /// [`demodulate`](Self::demodulate) is this minus the bookkeeping.
    pub fn demodulate_verbose(&self, samples: &[i16]) -> DemodAttempt {
        let mut attempt = DemodAttempt { input_samples: samples.len(), ..Default::default() };

        let x: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
        if x.len() < SYMBOL_LEN * 2 {
            return attempt;
        }
        let half = N / 2; // 128
        let search_end = x.len().saturating_sub(SYMBOL_LEN * 2).min(400);
        if search_end == 0 {
            return attempt;
        }

        let mut raw = vec![0.0_f64; search_end];
        for ps in 0..search_end {
            let a = &x[ps..ps + half];
            let b = &x[ps + half..ps + 2 * half];
            let mut dot = 0.0;
            let mut na = 0.0;
            let mut nb = 0.0;
            for i in 0..half {
                dot += a[i] * b[i];
                na += a[i] * a[i];
                nb += b[i] * b[i];
            }
            let denom = (na.sqrt() * nb.sqrt()).max(f64::MIN_POSITIVE);
            raw[ps] = dot.abs() / denom;
        }
        if raw.len() < CP_LEN {
            return attempt;
        }

        // Because the CP is a copy of the periodic preamble, the raw score has
        // a "plateau" that starts CP_LEN before the true start. A moving
        // average CP_LEN wide turns it into a single, noise-robust peak (the
        // classic Schmidl-Cox practice).
        let window = CP_LEN;
        let mut csum = vec![0.0_f64; raw.len() + 1];
        for i in 0..raw.len() {
            csum[i + 1] = csum[i] + raw[i];
        }
        let smoothed_len = raw.len() - window + 1;
        let mut best_idx = 0usize;
        let mut best_score = f64::NEG_INFINITY;
        for i in 0..smoothed_len {
            let s = (csum[i + window] - csum[i]) / window as f64;
            if s > best_score {
                best_score = s;
                best_idx = i;
            }
        }
        attempt.sync_score = best_score;
        if best_score < SYNC_THRESHOLD {
            return attempt;
        }
        attempt.synced = true;
        let preamble_cp_start = best_idx; // best_ps - CP_LEN

        let symbol_at = |index: usize| -> Option<&[f64]> {
            let start = preamble_cp_start + index * SYMBOL_LEN + CP_LEN;
            let end = start + N;
            if end > x.len() { None } else { Some(&x[start..end]) }
        };

        let Some(header_sym) = symbol_at(1) else { return attempt };

        // Try the no-offset read first — the common case, and the only one
        // any existing (offset-free) test vector needs — then walk outward
        // through plausible carrier offsets. Each candidate is cheap to rule
        // out: the header's majority-voted length has to land in range
        // before a shift is worth a full data decode.
        let mut shifts = vec![0i32];
        for s in 1..=MAX_SHIFT_UP.max(-MAX_SHIFT_DOWN) {
            if s <= MAX_SHIFT_UP {
                shifts.push(s);
            }
            if -s >= MAX_SHIFT_DOWN {
                shifts.push(-s);
            }
        }

        for shift in shifts {
            let Some((payload_len, mode)) = decode_header_symbol(&*self.fwd, header_sym, shift)
            else {
                continue;
            };
            if !(4..=20000).contains(&payload_len) {
                continue;
            }
            if attempt.header_shift.is_none() {
                attempt.header_shift = Some(shift);
                attempt.header_payload_len = Some(payload_len);
            }

            let bits_needed = payload_len * 8;
            let bits_per_carrier = if mode == Mode::Bpsk { 1 } else { 2 };
            let bits_per_symbol = bits_per_carrier * N_INFO;
            let n_data_symbols = bits_needed.div_ceil(bits_per_symbol);

            let mut all_bits: Vec<u8> = Vec::with_capacity(n_data_symbols * bits_per_symbol);
            let mut symbols_ok = true;
            for i in 0..n_data_symbols {
                let Some(sym) = symbol_at(2 + i) else {
                    symbols_ok = false;
                    break;
                };
                all_bits.extend(decode_data_symbol(&*self.fwd, sym, mode, shift));
            }
            if !symbols_ok {
                continue;
            }

            let take = bits_needed.min(all_bits.len());
            let full = bytes_from_bits(&all_bits[..take]);
            if full.len() < payload_len {
                continue;
            }

            let payload = &full[..payload_len - 4];
            let crc_recv = u32::from_be_bytes([
                full[payload_len - 4],
                full[payload_len - 3],
                full[payload_len - 2],
                full[payload_len - 1],
            ]);
            if crate::netproto::crc32(payload) == crc_recv {
                attempt.crc_shift = Some(shift);
                attempt.payload = Some(payload.to_vec());
                return attempt;
            }
        }
        attempt
    }
}

/// The intermediate results behind one [`Modem::demodulate_verbose`] call —
/// enough to tell a sync miss (bad SNR, wrong moment) from a sync hit that
/// never found a valid carrier offset (CFO past the search range) from one
/// that found a plausible header but failed data/CRC (residual sub-bin
/// offset, real-channel distortion, or genuine noise).
#[derive(Debug, Clone, Default)]
pub struct DemodAttempt {
    pub input_samples: usize,
    /// The Schmidl-Cox timing-sync score; compare against `SYNC_THRESHOLD`
    /// (0.25) to see how close a miss was, not just that it missed.
    pub sync_score: f64,
    pub synced: bool,
    /// The first carrier-offset shift (bins, ~31.25 Hz each) whose header
    /// decoded to a plausible length, if any.
    pub header_shift: Option<i32>,
    pub header_payload_len: Option<usize>,
    /// The shift that produced a CRC-valid payload, if decode succeeded.
    pub crc_shift: Option<i32>,
    pub payload: Option<Vec<u8>>,
}

/// How many seconds a payload actually stays "on the air" (lead-in excluded).
/// A port of `modem.airtime_seconds` — for comparing against the PHY table.
pub fn airtime_seconds(payload_len_bytes: usize, mode: Mode) -> f64 {
    let full_len = payload_len_bytes + 4;
    let bits_needed = full_len * 8;
    let bits_per_carrier = if mode == Mode::Bpsk { 1 } else { 2 };
    let bits_per_symbol = bits_per_carrier * N_INFO;
    let n_data_symbols = bits_needed.div_ceil(bits_per_symbol);
    let n_symbols = 2 + n_data_symbols; // preamble + header + data
    (n_symbols * SYMBOL_LEN) as f64 / SAMPLE_RATE as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_roundtrip_msb_first() {
        let data = [0b1010_0001u8, 0x00, 0xFF];
        let bits = bits_from_bytes(&data);
        assert_eq!(&bits[..8], &[1, 0, 1, 0, 0, 0, 0, 1]);
        assert_eq!(bytes_from_bits(&bits), data);
    }

    #[test]
    fn noiseless_roundtrip_all_sizes() {
        let m = Modem::new();
        for &size in &[1usize, 2, 16, 100, 220, 512, 1000, 2500, 5000] {
            for &mode in &[Mode::Bpsk, Mode::Qpsk] {
                let payload: Vec<u8> = (0..size).map(|i| (i * 7 + 13) as u8).collect();
                let wave = m.modulate_with_leadin(&payload, mode, 137);
                let got = m.demodulate(&wave).expect("demod failed");
                assert_eq!(got, payload, "size={size} mode={:?}", mode);
            }
        }
    }

    #[test]
    fn pure_silence_is_none() {
        let m = Modem::new();
        assert!(m.demodulate(&vec![0i16; 4000]).is_none());
    }

    #[test]
    fn airtime_matches_python_shape() {
        // 250 B QPSK: full=254 -> bits=2032 -> bps=152 -> 14 sym -> +2 = 16
        // 16 * 320 / 8000 = 0.64 s
        assert!((airtime_seconds(250, Mode::Qpsk) - 0.64).abs() < 1e-9);
    }

    /// Frequency-shift a real signal by `hz`, the way two stations' radios
    /// disagreeing on the dial actually would: build the analytic signal
    /// (zero the negative-frequency half of its spectrum, double the
    /// positive half), mix it up by `hz`, and keep the real part. Test-only —
    /// this is what a receiver a few hundred Hz off the transmitter's LO
    /// actually hands the modem, unlike a synthetic bin-index nudge.
    fn shift_real_signal_hz(samples: &[f64], hz: f64, fs: f64) -> Vec<f64> {
        let n = samples.len();
        let mut planner = FftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);

        let mut buf: Vec<Complex64> = samples.iter().map(|&s| Complex64::new(s, 0.0)).collect();
        fwd.process(&mut buf);

        let half = n / 2;
        for c in buf.iter_mut().take(half).skip(1) {
            *c *= 2.0;
        }
        for c in buf.iter_mut().take(n).skip(half + 1) {
            *c = Complex64::new(0.0, 0.0);
        }

        inv.process(&mut buf);
        let scale = 1.0 / n as f64;
        let step = Complex64::from_polar(1.0, 2.0 * std::f64::consts::PI * hz / fs);
        let mut rot = Complex64::new(1.0, 0.0);
        buf.iter()
            .map(|c| {
                let re = (c * scale * rot).re;
                rot *= step;
                re
            })
            .collect()
    }

    /// A dial mismatch between two independent radios — a few hundred Hz is
    /// ordinary for two different SDRs' LOs — must not stop a decode: the
    /// carrier-offset search in `demodulate` exists exactly for this.
    #[test]
    fn demodulate_tolerates_a_realistic_dial_offset() {
        let m = Modem::new();
        let payload = b"CQ CQ DE TA1XYZ AtCHAT NET test".to_vec();
        let wave_i16 = m.modulate_with_leadin(&payload, Mode::Qpsk, 137);
        let wave_f64: Vec<f64> = wave_i16.iter().map(|&s| s as f64).collect();

        // +500 Hz and -250 Hz: comfortably inside MAX_SHIFT_UP / MAX_SHIFT_DOWN
        // (~1250 Hz / -281 Hz), and not multiples of the 31.25 Hz bin spacing,
        // so this also exercises the residual sub-bin offset the search
        // leaves behind.
        for hz in [500.0_f64, -250.0] {
            let shifted = shift_real_signal_hz(&wave_f64, hz, SAMPLE_RATE as f64);
            let shifted_i16: Vec<i16> = shifted
                .iter()
                .map(|&v| v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16)
                .collect();
            let got =
                m.demodulate(&shifted_i16).unwrap_or_else(|| panic!("demod failed at {hz} Hz"));
            assert_eq!(got, payload, "offset={hz} Hz");
        }
    }

    /// An offset past the search range is a station too far off to hear —
    /// this must fail cleanly (`None`), not panic or return garbage.
    #[test]
    fn demodulate_past_the_search_range_is_none_not_garbage() {
        let m = Modem::new();
        let payload = b"out of range".to_vec();
        let wave_i16 = m.modulate_with_leadin(&payload, Mode::Qpsk, 137);
        let wave_f64: Vec<f64> = wave_i16.iter().map(|&s| s as f64).collect();

        let shifted = shift_real_signal_hz(&wave_f64, 3000.0, SAMPLE_RATE as f64);
        let shifted_i16: Vec<i16> = shifted
            .iter()
            .map(|&v| v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16)
            .collect();
        assert!(m.demodulate(&shifted_i16).is_none());
    }
}
