//! Threading wrapper around [`crate::bpsk::acquire`], mirroring
//! `sdroxide_skimmer::SkimmerController`: the realtime engine thread ships IQ
//! blocks to a worker over a bounded channel and drains status updates
//! non-blocking via [`Qo100Controller::poll`]. All the DSP runs on the worker
//! thread.
//!
//! Two things it does *not* copy from `SkimmerController`, because that one's
//! unit of work is a single block's FFT and this one's is a whole
//! [`bpsk::acquire`] sweep that can run for seconds:
//!
//! * dropping an IQ block on backpressure would punch a hole into a buffer a
//!   10.36 s frame has to sit inside contiguously — the same rule the DeepCW
//!   window follows. So a dropped block instead *restarts* the rolling
//!   buffer: fewer search windows under sustained backpressure, but every one
//!   of them is a contiguous span of air.
//! * `Drop` cannot simply join the worker — a sweep in progress would hold
//!   the engine thread for as long as the sweep takes. A shared cancel flag,
//!   polled between candidates inside `acquire`, brings that back to at most
//!   one candidate's work.
//!
//! Settings ride a separate unbounded channel: rare, and must never be
//! dropped even while the IQ queue is backed up.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use sdroxide_dsp::Complex32;
use sdroxide_types::Qo100Settings;

use crate::bpsk::{self, FRAME_SECONDS};

/// Realtime data, dropped on backpressure.
struct Iq(Vec<Complex32>);

/// Control traffic, never dropped.
enum Ctl {
    Config(Qo100Settings),
    Stop,
}

/// How long a rolling buffer is kept before each search — comfortably more
/// than two frame times, so a frame beginning anywhere in the buffer is
/// always captured whole at least once, regardless of where the buffer
/// happens to be cut relative to the beacon's own, unrelated, transmit
/// timing.
fn window_seconds() -> f64 {
    FRAME_SECONDS * 2.3
}

/// How much of the window survives each search, so consecutive windows
/// overlap by more than one frame time — the reason a frame can never land
/// exactly on a cut.
fn keep_seconds() -> f64 {
    FRAME_SECONDS * 1.15
}

/// Coarse frequency-grid step the search tries, in Hz. The delay-and-multiply
/// chip detector `bpsk::acquire` runs at each candidate tolerates a residual
/// carrier error of well over 100 Hz (its own tests decode an uncorrected
/// 100 Hz offset), and `bpsk::refine_offset_hz` then measures the true offset
/// to about a hertz — so the grid only has to land *inside* that capture
/// range, not resolve it. 150 Hz keeps the nearest candidate within 75 Hz of
/// any real signal while keeping the candidate count — and so the sweep
/// time — an order of magnitude below a 10 Hz grid's.
const FREQ_STEP_HZ: f64 = 150.0;

/// The rate every candidate is mixed down to before the chip search, whatever
/// the capture rate above it. The beacon is 400 baud; 16 kHz is heavily
/// oversampled for it and is the floor `Engine::qo100_target_rate_hz` uses
/// too. Fixing it here is what stops the sweep cost growing with the square
/// of the search width — see [`bpsk::acquire`].
pub(crate) const DEMOD_RATE_HZ: f64 = 16_000.0;

/// Bounded IQ queue depth. Roughly a second and a half of channel-rate audio
/// at a typical device read, enough that an ordinary scheduling hiccup does
/// not cost a window; sustained backpressure past this restarts the buffer
/// rather than splicing it (see the module doc).
const IQ_QUEUE_DEPTH: usize = 256;

pub struct Qo100Controller {
    iq_tx: Sender<Iq>,
    ctl_tx: Sender<Ctl>,
    res_rx: Receiver<sdroxide_types::Qo100Status>,
    /// Set true when the realtime side had to drop a block: the worker sees
    /// it, throws away the spliced buffer and starts a fresh contiguous one.
    spliced: Arc<AtomicBool>,
    /// Set true by `Drop` so a sweep in progress returns at the next
    /// candidate instead of running to completion under the engine thread's
    /// `join`.
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Qo100Controller {
    pub fn new(rate_hz: f64, cfg: Qo100Settings) -> Self {
        let (iq_tx, iq_rx) = bounded::<Iq>(IQ_QUEUE_DEPTH);
        let (ctl_tx, ctl_rx) = unbounded::<Ctl>();
        let (res_tx, res_rx) = unbounded::<sdroxide_types::Qo100Status>();
        let spliced = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let window_len = (rate_hz * window_seconds()).round() as usize;
        let keep_len = (rate_hz * keep_seconds()).round() as usize;
        let worker = std::thread::Builder::new()
            .name("sdroxide-qo100".into())
            .spawn({
                let spliced = Arc::clone(&spliced);
                let cancel = Arc::clone(&cancel);
                move || {
                    let mut cfg = cfg;
                    let mut buf: Vec<Complex32> = Vec::with_capacity(window_len);
                    let (mut tried, mut locked) = (0u64, 0u64);
                    let mut last: Option<(f64, String, i64)> = None; // offset, text, unix
                    loop {
                        select! {
                            recv(ctl_rx) -> msg => match msg {
                                Ok(Ctl::Config(next)) => cfg = next,
                                Ok(Ctl::Stop) | Err(_) => break,
                            },
                            recv(iq_rx) -> msg => match msg {
                                Ok(Iq(block)) => {
                                    // A dropped block since the last read means
                                    // `buf` now spans a discontinuity. Nothing
                                    // coherent can come out of that, so start
                                    // over from this block, which *is*
                                    // contiguous with what follows it.
                                    if spliced.swap(false, Ordering::Relaxed) {
                                        buf.clear();
                                    }
                                    buf.extend_from_slice(&block);
                                    if buf.len() < window_len {
                                        continue;
                                    }
                                    tried += 1;
                                    let lock = bpsk::acquire(
                                        &buf,
                                        rate_hz,
                                        cfg.search_half_width_hz,
                                        FREQ_STEP_HZ,
                                        DEMOD_RATE_HZ,
                                        &cancel,
                                    );
                                    if let Some(l) = lock {
                                        locked += 1;
                                        let unix = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_secs() as i64)
                                            .unwrap_or(0);
                                        last = Some((l.offset_hz, l.text, unix));
                                    }
                                    // Keep the newest slice — enough overlap
                                    // that no frame can fall in the gap —
                                    // rather than clearing outright, so a
                                    // frame that straddles this cut is still
                                    // whole in the *next* window instead of
                                    // being thrown away twice.
                                    let start = buf.len().saturating_sub(keep_len);
                                    buf.drain(..start);
                                    let (offset_hz, text, locked_unix) =
                                        last.clone().unwrap_or_default();
                                    let _ = res_tx.send(sdroxide_types::Qo100Status {
                                        running: true,
                                        locked: lock_is_fresh(locked_unix),
                                        offset_hz,
                                        text,
                                        locked_unix,
                                        blocks_tried: tried,
                                        blocks_locked: locked,
                                    });
                                }
                                Err(_) => break,
                            },
                        }
                    }
                }
            })
            .expect("spawn qo100 worker");
        Qo100Controller {
            iq_tx,
            ctl_tx,
            res_rx,
            spliced,
            cancel,
            worker: Some(worker),
        }
    }

    /// Realtime path: hand a block of channel-rate IQ to the worker.
    /// Non-blocking; a block that will not fit is dropped and the worker is
    /// told the buffer is now spliced so it restarts rather than searching a
    /// buffer with a hole in it.
    pub fn on_rx_iq(&self, iq: &[Complex32]) {
        if self.iq_tx.try_send(Iq(iq.to_vec())).is_err() {
            self.spliced.store(true, Ordering::Relaxed);
        }
    }

    /// Apply new settings (currently just the search width) to the running
    /// worker.
    pub fn set_config(&self, cfg: Qo100Settings) {
        let _ = self.ctl_tx.send(Ctl::Config(cfg));
    }

    /// Drain the latest status, if a search finished since the last poll.
    /// Non-blocking. Only the newest matters — a status is a full snapshot,
    /// like `IsmStatus`.
    pub fn poll(&self) -> Option<sdroxide_types::Qo100Status> {
        let mut out = None;
        while let Ok(s) = self.res_rx.try_recv() {
            out = Some(s);
        }
        out
    }
}

/// Whether a lock reported at `locked_unix` is still worth showing as
/// "locked" — the beacon alternates an uncoded frame (this decoder) with a
/// coded one (not attempted) roughly every 10.36 s, so a gap of a bit over
/// twice that is expected and not evidence the beacon went away.
fn lock_is_fresh(locked_unix: i64) -> bool {
    if locked_unix == 0 {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (now - locked_unix) as f64 <= FRAME_SECONDS * 3.0
}

impl Drop for Qo100Controller {
    fn drop(&mut self) {
        // Cancel first: a sweep already running inside `acquire` polls this
        // between candidates, so the `join` below waits out at most one
        // candidate rather than a whole search.
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.ctl_tx.send(Ctl::Stop);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}
