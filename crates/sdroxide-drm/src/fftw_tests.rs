//! The FFTW stand-in against a direct transform.
//!
//! A unit test rather than one under `tests/`: the transform is reached by its
//! C symbol, and a build script's link flags apply to this crate's own targets,
//! not to a separate integration-test binary.
//!
//! `src/fftw_compat.c` replaces a library Dream would otherwise take from the
//! system, so it has to agree with that library exactly — a demodulator built
//! on a subtly wrong FFT does not fail, it just never decodes anything, which
//! is the hardest kind of bug to find later.
//!
//! The reference here is the defining sum itself, at the lengths Dream actually
//! asks for: the four OFDM symbol sizes (mode A–D at 48 kHz), the spectrum
//! sizes, and a few awkward ones to exercise the Bluestein path that covers
//! every length that is not a power of two.

use std::f64::consts::PI;
use std::os::raw::{c_double, c_int, c_uint};

#[repr(C)]
struct FftwPlan {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn fftw_plan_dft_1d(
        n: c_int,
        input: *mut [c_double; 2],
        output: *mut [c_double; 2],
        sign: c_int,
        flags: c_uint,
    ) -> *mut FftwPlan;
    fn fftw_plan_r2r_1d(
        n: c_int,
        input: *mut c_double,
        output: *mut c_double,
        kind: c_int,
        flags: c_uint,
    ) -> *mut FftwPlan;
    fn fftw_execute(plan: *mut FftwPlan);
    fn fftw_destroy_plan(plan: *mut FftwPlan);
}

const FFTW_FORWARD: c_int = -1;
const FFTW_BACKWARD: c_int = 1;
const FFTW_R2HC: c_int = 0;
const FFTW_HC2R: c_int = 1;

/// Dream's four OFDM sizes at 48 kHz, its spectrum sizes, and lengths chosen to
/// hit every shape the transform has: powers of two, small primes, and a large
/// prime that can only go through Bluestein.
const SIZES: &[usize] =
    &[448, 704, 1024, 1152, 256, 512, 8192, 1, 2, 3, 7, 11, 96, 320, 1000, 1009];

/// A deterministic, reproducible input — a fixed LCG rather than a random one,
/// so a failure can be reproduced exactly from the size alone.
fn samples(n: usize, seed: u64) -> Vec<f64> {
    let mut state = seed;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        })
        .collect()
}

/// X[k] = sum_j x[j] * exp(sign * 2*pi*i*j*k/n), straight from the definition.
fn direct_dft(input: &[[f64; 2]], sign: f64) -> Vec<[f64; 2]> {
    let n = input.len();
    (0..n)
        .map(|k| {
            let mut re = 0.0;
            let mut im = 0.0;
            for (j, x) in input.iter().enumerate() {
                let ang = sign * 2.0 * PI * (j as f64) * (k as f64) / n as f64;
                let (s, c) = ang.sin_cos();
                re += x[0] * c - x[1] * s;
                im += x[0] * s + x[1] * c;
            }
            [re, im]
        })
        .collect()
}

/// Relative error against the reference's own magnitude, so the tolerance means
/// the same thing at every length.
fn rel_error(got: &[f64], want: &[f64]) -> f64 {
    let num: f64 = got.iter().zip(want).map(|(a, b)| (a - b) * (a - b)).sum();
    let den: f64 = want.iter().map(|b| b * b).sum::<f64>().max(1e-300);
    (num / den).sqrt()
}

/// Loose against double precision, and deliberately so: the reference is the
/// naive O(n^2) sum, which at 8192 points accumulates more rounding than the
/// fast transform it is checking. It still discriminates perfectly — a
/// transform that is actually wrong (a mis-signed twiddle, a bad halfcomplex
/// index, the Bluestein kernel conjugated the wrong way) lands at order 1, nine
/// decades above this.
const TOL: f64 = 1e-9;

#[test]
fn complex_transforms_match_the_defining_sum() {
    for &n in SIZES {
        for (sign, label) in [(FFTW_FORWARD, "forward"), (FFTW_BACKWARD, "backward")] {
            let src = samples(2 * n, 0x5EED);
            let mut input: Vec<[f64; 2]> = src.chunks_exact(2).map(|p| [p[0], p[1]]).collect();
            let mut output = vec![[0.0f64; 2]; n];

            // SAFETY: both buffers are `n` elements and outlive the plan.
            unsafe {
                let plan =
                    fftw_plan_dft_1d(n as c_int, input.as_mut_ptr(), output.as_mut_ptr(), sign, 0);
                assert!(!plan.is_null(), "no plan for n={n}");
                fftw_execute(plan);
                fftw_destroy_plan(plan);
            }

            let want = direct_dft(&input, if sign == FFTW_FORWARD { -1.0 } else { 1.0 });
            let flat_got: Vec<f64> = output.iter().flatten().copied().collect();
            let flat_want: Vec<f64> = want.iter().flatten().copied().collect();
            let err = rel_error(&flat_got, &flat_want);
            assert!(err < TOL, "n={n} {label}: relative error {err:.3e}");
        }
    }
}

#[test]
fn halfcomplex_forward_matches_the_defining_sum() {
    for &n in SIZES {
        let input = samples(n, 0xC0FFEE);
        let mut buf = input.clone();
        let mut output = vec![0.0f64; n];

        // SAFETY: both buffers are `n` elements and outlive the plan.
        unsafe {
            let plan =
                fftw_plan_r2r_1d(n as c_int, buf.as_mut_ptr(), output.as_mut_ptr(), FFTW_R2HC, 0);
            assert!(!plan.is_null(), "no plan for n={n}");
            fftw_execute(plan);
            fftw_destroy_plan(plan);
        }

        // FFTW's halfcomplex layout: the real parts counting up from DC, then
        // the imaginary parts counting back down from the top.
        let full = direct_dft(&input.iter().map(|&x| [x, 0.0]).collect::<Vec<_>>(), -1.0);
        let mut want = vec![0.0f64; n];
        want[0] = full[0][0];
        for k in 1..=n / 2 {
            want[k] = full[k][0];
        }
        for k in 1..=(n - 1) / 2 {
            want[n - k] = full[k][1];
        }
        let err = rel_error(&output, &want);
        assert!(err < TOL, "n={n} r2hc: relative error {err:.3e}");
    }
}

#[test]
fn halfcomplex_round_trip_scales_by_n() {
    // FFTW's transforms are unnormalised in both directions, and Dream divides
    // by n itself on the way back. A shim that normalised one side would decode
    // nothing at all, so the convention is worth pinning.
    for &n in SIZES {
        let input = samples(n, 0xBEEF);
        let mut buf = input.clone();
        let mut spectrum = vec![0.0f64; n];
        let mut back = vec![0.0f64; n];

        // SAFETY: every buffer is `n` elements and outlives its plan.
        unsafe {
            let fwd =
                fftw_plan_r2r_1d(n as c_int, buf.as_mut_ptr(), spectrum.as_mut_ptr(), FFTW_R2HC, 0);
            let inv = fftw_plan_r2r_1d(
                n as c_int,
                spectrum.as_mut_ptr(),
                back.as_mut_ptr(),
                FFTW_HC2R,
                0,
            );
            assert!(!fwd.is_null() && !inv.is_null(), "no plan for n={n}");
            fftw_execute(fwd);
            fftw_execute(inv);
            fftw_destroy_plan(fwd);
            fftw_destroy_plan(inv);
        }

        let want: Vec<f64> = input.iter().map(|x| x * n as f64).collect();
        let err = rel_error(&back, &want);
        assert!(err < TOL, "n={n} round trip: relative error {err:.3e}");
    }
}
