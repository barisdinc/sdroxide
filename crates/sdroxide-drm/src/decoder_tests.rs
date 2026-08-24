//! The decoder across the C boundary.
//!
//! Unit tests rather than tests under `tests/`, for the reason given in
//! [`crate::fftw_tests`].
//!
//! What runs everywhere is the lifecycle: that a receiver can be built, fed,
//! read and shut down without deadlocking. That is worth a test on its own —
//! the decoder blocks on its input by design, so a mistake in the stop sequence
//! hangs the radio on every mode change rather than failing visibly.
//!
//! Decoding a real broadcast needs a recording, which cannot live in the
//! repository: they are minutes of somebody's copyrighted programme material,
//! and megabytes of it. Point `SDROXIDE_DRM_SAMPLE` at one to run that test —
//! see the harness in `examples/drm_harness.rs`, which is the fuller tool.

use crate::{AUDIO_RATE, DrmWorker, SIGNAL_RATE};

/// A deterministic pseudo-random fill, so a failure reproduces exactly.
fn noise(n: usize) -> Vec<i16> {
    let mut state = 0x1234_5678_9abc_def0u64;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            ((state >> 40) as i32 - 8192) as i16
        })
        .collect()
}

#[test]
fn a_decoder_starts_takes_samples_and_stops() {
    let worker = DrmWorker::new(true, false).expect("start the decoder");

    // A second of noise, as interleaved I/Q pairs.
    let block = noise(2 * SIGNAL_RATE as usize);
    for chunk in block.chunks(4800) {
        worker.push(chunk);
    }

    // Nothing to find in noise, so the interesting assertion is that asking is
    // safe and answers something coherent rather than that it locks.
    let status = worker.status();
    assert!(!status.locked, "the decoder claimed a lock on noise");
    assert!(status.service.label.is_empty(), "noise produced a service label");

    // The real assertion: dropping the worker while its thread is blocked
    // waiting for more input still joins. A regression here hangs forever
    // rather than failing, which the test harness reports as a timeout.
    drop(worker);
}

#[test]
fn a_decoder_that_was_never_fed_still_shuts_down() {
    // The thread spends its whole life blocked in the read, which is the case
    // the stop flag and the ring's own release have to cover between them.
    let worker = DrmWorker::new(true, false).expect("start the decoder");
    drop(worker);
}

#[test]
fn two_decoders_do_not_share_a_ring() {
    // Each decoder finds its queues through a thread-local set on its own
    // worker thread. If that ever became one global, a second receiver would
    // silently steal the first's samples — which is exactly what a split-view
    // or multi-radio session would do.
    let a = DrmWorker::new(true, false).expect("start the first decoder");
    let b = DrmWorker::new(true, false).expect("start the second decoder");
    a.push(&noise(4800));
    assert_eq!(b.audio_available(), 0, "samples pushed to one decoder reached the other");
    drop(a);
    drop(b);
}

/// Two decoders may be built at the same time on two threads.
///
/// Dream shares its audio codecs through a `static` list with a plain `int`
/// reference count and no lock, which is safe for the single-threaded console
/// receiver it was written for and not for a host that runs one receiver per
/// radio. Two constructors racing double-free the list's storage; worse, both
/// receivers would then be handed the *same* `AacCodec`, so one's `DecClose`
/// frees the faad2 handle the other is decoding through. The vendored list is
/// per-thread now (see `vendor/PROVENANCE.md`).
///
/// This is not a hypothetical pair: a split view or a second radio makes it
/// permanent, and `RxChain::build_for_mode` used to make it momentarily every
/// time the mode was set — which a CAT rig reporting its own mode back does on
/// its own schedule.
///
/// Like the exception test, a failure here does not report an assertion. The
/// process dies, or ASan does the reporting.
#[test]
fn two_decoders_can_start_at_the_same_time() {
    use std::sync::{Arc, Barrier};

    // Several rounds: the window is the few milliseconds inside the two
    // constructors, so one attempt proves very little.
    for _ in 0..8 {
        let gate = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    let w = DrmWorker::new(true, false).expect("start a decoder");
                    w.push(&noise(4800));
                    drop(w);
                })
            })
            .collect();
        for t in threads {
            t.join().expect("a decoder thread panicked");
        }
    }
}

/// Decode a real off-air recording, when one is available.
///
/// Recordings are 48 kHz *real* signals off a receiver's I.F., which is not the
/// zero-IF baseband the receive chain produces — so this drives the decoder's
/// real-signal input rather than [`crate::DrmDemod`]. The harness example
/// covers the baseband path as well.
#[test]
fn a_recording_decodes() {
    let Ok(path) = std::env::var("SDROXIDE_DRM_SAMPLE") else {
        eprintln!("set SDROXIDE_DRM_SAMPLE to a 48 kHz DRM recording to run this");
        return;
    };

    let mut reader = hound::WavReader::open(&path).expect("open the recording");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate as f64, SIGNAL_RATE, "the recording must be 48 kHz");
    let channels = spec.channels as usize;
    let mono: Vec<i16> = reader
        .samples::<i16>()
        .map(|s| s.expect("read sample"))
        .collect::<Vec<_>>()
        .chunks(channels)
        .map(|c| c[0])
        .collect();

    let worker = DrmWorker::new(false, false).expect("start the decoder");
    let mut interleaved = Vec::with_capacity(mono.len() * 2);
    for &s in &mono {
        interleaved.push(s);
        interleaved.push(s);
    }

    let mut sink = vec![0i16; 8192];
    let mut audio_frames = 0usize;
    for chunk in interleaved.chunks(4800) {
        while worker.push(chunk) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        loop {
            let n = worker.pop(&mut sink);
            if n == 0 {
                break;
            }
            audio_frames += n / 2;
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }

    let status = worker.status();
    assert!(status.locked, "the decoder did not lock onto {path}");
    assert!(status.fac.is_ok(), "the FAC never decoded, so nothing else can be believed");
    assert!(!status.service.label.is_empty(), "no service label was decoded");
    assert!(status.snr_db > 0.0, "a locked decode reported {} dB SNR", status.snr_db);
    // Well under real time would mean the audio chain stalled even though the
    // demodulator was working.
    let seconds = audio_frames as f64 / AUDIO_RATE;
    let expected = mono.len() as f64 / SIGNAL_RATE;
    assert!(
        seconds > expected * 0.5,
        "only {seconds:.1} s of audio came out of a {expected:.1} s recording"
    );
}

/// Nothing thrown inside the shim may unwind into Rust.
///
/// This is the property that matters most for a radio that has to stay up: the
/// FFI declarations are plain `extern "C"`, and Rust cannot catch a foreign
/// exception crossing one — it aborts the whole process, `catch_unwind` and
/// all. Dream's deliberate throws are `CGenErr` and `std::string`, which the
/// shim always caught; the ones that actually reach the boundary from a real
/// broadcast are implicit — `std::bad_alloc` and `std::length_error` out of the
/// `resize()` calls its over-the-air parsers make with lengths the transmission
/// supplied.
///
/// A failure here is not a failed assertion. The test binary dies.
#[test]
fn no_exception_escapes_the_c_boundary() {
    for kind in 0..5 {
        // SAFETY: the hook exists to be called; it throws and catches inside C++.
        let rc = unsafe { crate::sys::sdrx_drm_test_throw(kind) };
        assert_eq!(rc, -1, "kind {kind} was not reported as a failure");
        assert!(
            crate::last_error().contains("test throw"),
            "kind {kind} left no reason behind: {:?}",
            crate::last_error()
        );
    }
}
