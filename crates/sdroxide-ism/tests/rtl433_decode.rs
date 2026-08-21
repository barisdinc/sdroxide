//! End-to-end checks on the embedded rtl_433: a synthesised transmission goes in
//! as IQ, a decode comes back out as key/value pairs.
//!
//! Everything here exercises the real C library — the config, the decoder
//! registry, the demodulator and the output sink — so a break in the FFI shows
//! up as a failing test rather than as silence on the band.
//!
//! The signal is generated rather than recorded so the test needs no capture
//! files and no radio: what it proves is that the plumbing carries a decode, not
//! that any particular device works.

#![cfg(feature = "rtl433")]

use sdroxide_ism::rtl433::{flex, map, sys};

const RATE: u32 = 250_000;
const CENTER: u32 = 433_920_000;

/// Samples for a given number of microseconds.
fn us(n: f64) -> usize {
    (n * RATE as f64 / 1e6).round() as usize
}

/// Append an on or off period to an interleaved cs16 buffer.
///
/// The carrier is offset from the centre so the signal is not sitting on DC,
/// where a receiver's own artefacts live. Amplitude is well below full scale:
/// rtl_433 tracks its own noise floor, and a signal pinned at the rails tells us
/// less than a realistic one.
fn push(buf: &mut Vec<i16>, samples: usize, on: bool, phase: &mut f64) {
    const OFFSET_HZ: f64 = 25_000.0;
    const AMPLITUDE: f64 = 6000.0;
    let step = std::f64::consts::TAU * OFFSET_HZ / RATE as f64;
    for _ in 0..samples {
        let a = if on { AMPLITUDE } else { 0.0 };
        buf.push((a * phase.cos()) as i16);
        buf.push((a * phase.sin()) as i16);
        *phase += step;
        if *phase > std::f64::consts::TAU {
            *phase -= std::f64::consts::TAU;
        }
    }
}

/// One OOK-PWM transmission of `bits`, at 400/800 µs on a 1200 µs period.
///
/// Pulse-width modulation in the plainest form rtl_433 supports: every symbol is
/// one pulse whose length carries the bit, padded to a constant period, with a
/// long quiet gap at the end to close the package.
fn transmission(bits: &[u8]) -> Vec<i16> {
    let mut buf = Vec::new();
    let mut phase = 0.0;

    // Quiet lead-in, so the noise-floor tracker has something to measure before
    // the first edge.
    push(&mut buf, us(5000.0), false, &mut phase);

    for &bit in bits {
        let (on, off) = if bit == 1 { (400.0, 800.0) } else { (800.0, 400.0) };
        push(&mut buf, us(on), true, &mut phase);
        push(&mut buf, us(off), false, &mut phase);
    }

    // Longer than the decoder's reset limit, which is what ends the package.
    push(&mut buf, us(9000.0), false, &mut phase);
    buf
}

fn bits_of(byte_str: &str) -> Vec<u8> {
    byte_str.chars().filter(|c| *c == '0' || *c == '1').map(|c| c as u8 - b'0').collect()
}

/// The whole chain: create, register a user decoder, feed IQ, get an event.
#[test]
fn a_flex_decoder_decodes_a_synthesised_transmission() {
    let spec = "n=sdroxide-test,m=OOK_PWM,s=400,l=800,g=1500,r=7000,bits=24";
    flex::validate(spec).expect("the test's own spec must be valid");

    let mut inst = sys::Instance::new(RATE, CENTER).expect("rtl_433 instance");
    assert!(inst.decoder_count() > 100, "expected the built-in decoders to register");
    inst.register_flex(spec).expect("register flex decoder");

    // 24 bits, chosen so the pattern is not all one symbol width.
    let pattern = "101010011000011110001100";
    let iq = transmission(&bits_of(pattern));

    let mut events = inst.feed(&iq);
    events.extend(inst.flush());

    assert!(!inst.poisoned(), "a callback panicked");
    assert!(!events.is_empty(), "no decode came back from {} samples", iq.len() / 2);

    let ours = events
        .iter()
        .find(|kvs| {
            kvs.iter().any(|kv| kv.key == "model" && kv.value.to_display() == "sdroxide-test")
        })
        .unwrap_or_else(|| panic!("no event from our decoder; got {events:#?}"));

    // report_meta is on, so every decode is stamped with where it was heard.
    let freq = ours.iter().find(|kv| kv.key == "freq").expect("freq from report_meta");
    let mhz = freq.value.as_f64().expect("freq is a number");
    assert!((mhz - 433.92).abs() < 0.1, "freq {mhz} MHz is not near the configured centre");
}

/// Decode with a given spec and return the event our decoder produced.
fn decode_with(spec: &str) -> Vec<sys::Kv> {
    flex::validate(spec).unwrap_or_else(|e| panic!("the test's own spec must be valid: {e}"));
    let mut inst = sys::Instance::new(RATE, CENTER).expect("rtl_433 instance");
    inst.register_flex(spec).expect("register");

    let iq = transmission(&bits_of("101010011000011110001100"));
    let mut events = inst.feed(&iq);
    events.extend(inst.flush());
    assert!(!inst.poisoned(), "a callback panicked");

    events
        .into_iter()
        .find(|kvs| {
            kvs.iter().any(|kv| kv.key == "model" && kv.value.to_display() == "sdroxide-test")
        })
        .expect("no event from our decoder")
}

/// The mapper turns a decode into a device-table row, getters and all.
#[test]
fn a_decode_maps_into_a_report() {
    let spec = "n=sdroxide-test,m=OOK_PWM,s=400,l=800,g=1500,r=7000,bits=24,\
                get=@0:{8}:humidity,unique";
    let ours = decode_with(spec);

    let mapped = map::map_event(&ours, CENTER as f64).expect("event has a model");
    assert_eq!(mapped.decoded.model.as_deref(), Some("sdroxide-test"));
    assert!(
        (mapped.freq_hz - CENTER as f64).abs() < 100_000.0,
        "mapped freq {} is not near the centre",
        mapped.freq_hz
    );
    // The getter named a quantity the table knows, so it became a reading
    // rather than a key/value line.
    assert!(
        mapped
            .decoded
            .readings
            .iter()
            .any(|r| r.quantity == sdroxide_types::IsmQuantity::HumidityPct),
        "expected the humidity getter to map to a reading, got {:?} / {:?}",
        mapped.decoded.readings,
        mapped.decoded.extra
    );
}

/// A spec without `unique` reports its getters one level down, inside a "rows"
/// array. Fifteen of the seventy decoders upstream ships are written that way,
/// so those readings have to survive the walk too.
#[test]
fn getters_survive_a_spec_without_unique() {
    let spec = "n=sdroxide-test,m=OOK_PWM,s=400,l=800,g=1500,r=7000,bits=24,\
                get=@0:{8}:humidity";
    let ours = decode_with(spec);

    let mapped = map::map_event(&ours, CENTER as f64).expect("event has a model");
    assert!(
        mapped
            .decoded
            .readings
            .iter()
            .any(|r| r.quantity == sdroxide_types::IsmQuantity::HumidityPct),
        "nested getter was dropped; readings {:?} extra {:?}",
        mapped.decoded.readings,
        mapped.decoded.extra
    );
    // "codes" carries the frame as {bits}hex, which is what the raw column shows.
    assert!(!mapped.decoded.raw_hex.is_empty(), "no raw frame captured");
}

/// The built-in decoders are registered and the version is readable — the two
/// things the status line reports.
#[test]
fn the_library_reports_itself() {
    let inst = sys::Instance::new(RATE, CENTER).expect("rtl_433 instance");
    assert!(
        inst.decoder_count() > 100,
        "only {} decoders registered — is register_all_protocols working?",
        inst.decoder_count()
    );
    let v = sys::version();
    assert!(!v.is_empty(), "no version string");
}

/// Feeding silence must not invent devices.
#[test]
fn quiet_air_decodes_nothing() {
    let mut inst = sys::Instance::new(RATE, CENTER).expect("rtl_433 instance");
    let mut phase = 0.0;
    let mut iq = Vec::new();
    push(&mut iq, us(50_000.0), false, &mut phase);
    let events = inst.feed(&iq);
    assert!(events.is_empty(), "silence produced {events:#?}");
}
