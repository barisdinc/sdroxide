//! The decoder on its own thread.
//!
//! Dream's receive chain blocks on its input and then does an OFDM
//! demodulation, a Viterbi decode and an AAC frame in one go, at intervals set
//! by the transmission rather than by the sound card. None of that can happen
//! on the audio path, so it happens here and the two sides meet at [`Ring`].

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sdroxide_types::{DrmChannel, DrmStatus};
use tracing::{debug, warn};

use crate::{Decoder, DrmError, Ring};

/// How often the thread takes a status snapshot. Dream's own display updates
/// about this often, and every read locks the receiver's parameter block.
const STATUS_INTERVAL: Duration = Duration::from_millis(200);

/// Two seconds of I/Q in and of audio out, as interleaved pairs.
///
/// Generous on purpose: with two seconds of time interleaving, DRM's own
/// latency is already over a second, and a decoder that has just been handed a
/// burst of samples should be free to work through them rather than lose the
/// front of the block.
const RING_SAMPLES: usize = 48_000 * 2 * 2;

pub struct DrmWorker {
    ring: Arc<Ring>,
    status: Arc<Mutex<DrmStatus>>,
    stop: Arc<AtomicBool>,
    /// Service the operator has asked for, or -1 for "leave it alone".
    select: Arc<AtomicI32>,
    /// Logical channel whose constellation to read back, or -1 for none —
    /// which is the case whenever nobody has one on screen.
    constellation: Arc<AtomicI32>,
    restart: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DrmWorker {
    /// Start a decoder. `iq_input` feeds zero-IF I/Q pairs; `false` feeds a
    /// real signal in both channels, which is what a recording off a receiver's
    /// IF looks like.
    pub fn new(iq_input: bool, flip_spectrum: bool) -> Result<Self, DrmError> {
        let ring = Arc::new(Ring::new(RING_SAMPLES, RING_SAMPLES));
        let status = Arc::new(Mutex::new(DrmStatus::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let select = Arc::new(AtomicI32::new(-1));
        let restart = Arc::new(AtomicBool::new(false));
        let constellation = Arc::new(AtomicI32::new(-1));

        // The receiver has to be built on the thread that will drive it, so the
        // outcome comes back over a channel rather than as a return value.
        let (tx, rx) = sync_channel::<Result<(), DrmError>>(1);

        let thread = {
            let ring = Arc::clone(&ring);
            let status = Arc::clone(&status);
            let stop = Arc::clone(&stop);
            let select = Arc::clone(&select);
            let restart = Arc::clone(&restart);
            let constellation = Arc::clone(&constellation);
            std::thread::Builder::new()
                .name("drm".into())
                .spawn(move || {
                    let mut decoder = match Decoder::new(&ring, iq_input, flip_spectrum) {
                        Ok(d) => {
                            let _ = tx.send(Ok(()));
                            d
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    debug!(codec = %crate::codec_version(), "DRM decoder started");

                    let mut next_status = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        if restart.swap(false, Ordering::Relaxed) {
                            decoder.restart();
                        }
                        let want = select.swap(-1, Ordering::Relaxed);
                        if want >= 0 {
                            decoder.select_service(want as u8);
                        }
                        if !decoder.process() {
                            warn!(
                                reason = %crate::last_error(),
                                "the DRM receive chain failed; decoding stopped"
                            );
                            break;
                        }
                        let now = Instant::now();
                        if now >= next_status {
                            next_status = now + STATUS_INTERVAL;
                            let mut snapshot = decoder.status();
                            // Reading the constellation copies a frame's worth
                            // of cells under the decoder's own lock, so it only
                            // happens while a plot is actually open.
                            let want = constellation.load(Ordering::Relaxed);
                            if let Some(ch) = channel_from_raw(want) {
                                snapshot.constellation = decoder.constellation(ch);
                            }
                            if let Ok(mut slot) = status.lock() {
                                *slot = snapshot;
                            }
                        }
                    }
                })
                .expect("spawn the DRM decoder thread")
        };

        // Propagate a failed open rather than leaving a thread that already exited.
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(DrmError::OpenFailed);
            }
        }

        Ok(DrmWorker { ring, status, stop, select, constellation, restart, thread: Some(thread) })
    }

    /// Queue interleaved I/Q. Returns how many samples were dropped for want of
    /// room, which is how the caller learns the decoder is not keeping up.
    pub fn push(&self, interleaved: &[i16]) -> usize {
        self.ring.push(interleaved)
    }

    /// Take decoded interleaved stereo audio. Returns how many samples.
    pub fn pop(&self, out: &mut [i16]) -> usize {
        self.ring.pop(out)
    }

    pub fn audio_available(&self) -> usize {
        self.ring.audio_available()
    }

    pub fn status(&self) -> DrmStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Ask for re-acquisition, after a retune.
    pub fn restart(&self) {
        self.restart.store(true, Ordering::Relaxed);
    }

    /// Ask for a different service of the multiplex.
    pub fn select_service(&self, service: u8) {
        self.select.store(i32::from(service), Ordering::Relaxed);
    }

    /// Start or stop reading back a logical channel's constellation. `None`
    /// stops, and is the state whenever no plot is on screen.
    pub fn set_constellation(&self, channel: Option<DrmChannel>) {
        self.constellation.store(channel.map(|c| c.as_raw()).unwrap_or(-1), Ordering::Relaxed);
    }
}

fn channel_from_raw(v: i32) -> Option<DrmChannel> {
    match v {
        0 => Some(DrmChannel::Fac),
        1 => Some(DrmChannel::Sdc),
        2 => Some(DrmChannel::Msc),
        _ => None,
    }
}

impl Drop for DrmWorker {
    fn drop(&mut self) {
        // Order matters: the flag has to be visible before the ring releases a
        // thread blocked in Read, or it would go straight back to waiting.
        self.stop.store(true, Ordering::Relaxed);
        self.ring.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
