//! The signal levels a real receiver actually delivers.
//!
//! This exists because of a bug that made the whole lane useless on air while
//! every other test passed: rtl_433's default minimum detection level is about
//! -12 dBFS, which suits an RTL-SDR setting its own gain and does not suit a
//! decimated window from a receiver that has none. On an RX-888 at 868 MHz a
//! sensor burst is 35 to 65 dB below full scale — under that threshold, so the
//! pulse detector never opened and nothing decoded at all.
//!
//! The synthetic transmissions elsewhere in this crate were written at a
//! comfortable level and so never saw it. These are deliberately weak.

#![cfg(feature = "rtl433")]

use sdroxide_ism::rtl433::{flex, sys};

const RATE: u32 = 1_012_500;
/// 868 MHz matters: above rtl_433's `FSK_PULSE_DETECTOR_LIMIT` it selects a
/// different FSK pulse detector, so a test at 433 does not cover this path.
const CENTER: u32 = 868_650_000;

/// A 2-FSK burst, the shape the 868 MHz sensors transmit: the carrier steps
/// between two tones, one per bit, on top of a constant noise floor.
fn fsk_burst(bits: &[u8], sig: f64, noise_amp: f64) -> Vec<i16> {
    const BAUD: f64 = 8200.0; // as a Bresser 6-in-1
    const DEV_HZ: f64 = 30_000.0;
    let sps = (RATE as f64 / BAUD) as usize;

    let mut buf: Vec<i16> = Vec::with_capacity(1 << 20);
    let mut phase = 0.0f64;
    let mut rng = 12345u32;
    let mut noise = |rng: &mut u32| {
        *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
        ((*rng >> 16) as f64 - 32768.0) / 32768.0 * noise_amp
    };

    // A second of bare noise first. The level tracker falls an eighth of the
    // way per block, so it needs a moment to find the floor — which a receiver
    // that has been running always gives it.
    for _ in 0..RATE as usize {
        buf.push(noise(&mut rng) as i16);
        buf.push(noise(&mut rng) as i16);
    }

    for &b in bits {
        // Offset from DC, where a real receiver's own artefacts live.
        let f = 120_000.0 + if b == 1 { DEV_HZ } else { -DEV_HZ };
        let step = std::f64::consts::TAU * f / RATE as f64;
        for _ in 0..sps {
            buf.push((sig * phase.cos() + noise(&mut rng)).clamp(-32768.0, 32767.0) as i16);
            buf.push((sig * phase.sin() + noise(&mut rng)).clamp(-32768.0, 32767.0) as i16);
            phase += step;
        }
    }

    for _ in 0..(RATE / 100) as usize {
        buf.push(noise(&mut rng) as i16);
        buf.push(noise(&mut rng) as i16);
    }
    buf
}

/// A preamble long enough to lock to, then a sync word and a few bytes.
fn frame_bits() -> Vec<u8> {
    let mut bits: Vec<u8> = (0..32).map(|i| (i % 2) as u8).collect();
    for byte in [0x2du8, 0xd4, 0x9a, 0x5c, 0x3f, 0x81] {
        for i in (0..8).rev() {
            bits.push((byte >> i) & 1);
        }
    }
    bits
}

fn decodes_at(sig: f64) -> bool {
    let spec = "n=weak-test,m=FSK_PCM,s=122,l=122,r=2000,bits>=48,unique";
    flex::validate(spec).expect("the test's own spec must be valid");

    let mut inst = sys::Instance::new(RATE, CENTER).expect("rtl_433 instance");
    inst.register_flex(spec).expect("register");

    // A 20 dB signal-to-noise ratio throughout: only the absolute level moves,
    // which is exactly the axis the bug lived on.
    let iq = fsk_burst(&frame_bits(), sig, sig / 10.0);
    let mut events = inst.feed(&iq);
    events.extend(inst.flush());
    assert!(!inst.poisoned(), "a callback panicked");

    events
        .iter()
        .any(|kvs| kvs.iter().any(|kv| kv.key == "model" && kv.value.to_display() == "weak-test"))
}

/// The levels an actual receiver hands over, well below full scale.
///
/// -34.7 dBFS is a strong sensor on an RX-888; -44 dBFS is an ordinary one.
/// Before the auto-level fix everything below about -24 dBFS was silently
/// undetectable, so these are the cases that matter.
#[test]
fn a_weak_burst_still_decodes() {
    for sig in [600.0, 200.0] {
        assert!(
            decodes_at(sig),
            "a burst at {:.1} dBFS did not decode — is demod->auto_level still set in shim.c?",
            20.0 * (sig / 32767.0f64).log10()
        );
    }
}

/// A strong burst has to keep working too: the fix moves a threshold down, and
/// a threshold that has moved too far would show up here.
#[test]
fn a_strong_burst_still_decodes() {
    assert!(decodes_at(6000.0), "a burst at -14.7 dBFS did not decode");
}
