//! Threading wrapper around [`crate::bpsk::acquire`], mirroring
//! `sdroxide_skimmer::SkimmerController`: the realtime engine thread ships IQ
//! blocks to a worker over a bounded channel (dropping on backpressure — a
//! frame lost to a dropped block just gets tried again in the next window)
//! and drains status updates non-blocking via [`Qo100Controller::poll`]. All
//! the DSP runs on the worker thread.
//!
//! Settings ride a separate unbounded channel: rare, and must never be
//! dropped even while the IQ queue is backed up.

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

/// Frequency grid step the search tries, in Hz. Coarser than this and a
/// real LNB's drift could sit between two candidates that both miss; finer
/// buys nothing a 400 baud link's own frequency tolerance needs.
const FREQ_STEP_HZ: f64 = 10.0;

pub struct Qo100Controller {
    iq_tx: Sender<Iq>,
    ctl_tx: Sender<Ctl>,
    res_rx: Receiver<sdroxide_types::Qo100Status>,
    worker: Option<JoinHandle<()>>,
}

impl Qo100Controller {
    pub fn new(rate_hz: f64, cfg: Qo100Settings) -> Self {
        let (iq_tx, iq_rx) = bounded::<Iq>(64);
        let (ctl_tx, ctl_rx) = unbounded::<Ctl>();
        let (res_tx, res_rx) = unbounded::<sdroxide_types::Qo100Status>();
        let window_len = (rate_hz * window_seconds()).round() as usize;
        let keep_len = (rate_hz * keep_seconds()).round() as usize;
        let worker = std::thread::Builder::new()
            .name("sdroxide-qo100".into())
            .spawn(move || {
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
                                );
                                if let Some(l) = lock {
                                    locked += 1;
                                    let unix = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() as i64)
                                        .unwrap_or(0);
                                    last = Some((l.offset_hz, l.text, unix));
                                }
                                // Keep the newest slice — enough overlap that
                                // no frame can fall in the gap — rather than
                                // clearing outright, so a frame that straddles
                                // this cut is still whole in the *next*
                                // window instead of being thrown away twice.
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
            })
            .expect("spawn qo100 worker");
        Qo100Controller { iq_tx, ctl_tx, res_rx, worker: Some(worker) }
    }

    /// Realtime path: hand a block of channel-rate IQ to the worker.
    /// Non-blocking; drops the block if the worker is behind.
    pub fn on_rx_iq(&self, iq: &[Complex32]) {
        let _ = self.iq_tx.try_send(Iq(iq.to_vec()));
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
        let _ = self.ctl_tx.send(Ctl::Stop);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}
