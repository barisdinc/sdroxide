//! Telling a receiver that nobody is reading it for the length of an over.
//!
//! The engine does not read a half-duplex source while it transmits, but a
//! receiver that is not the transmitter carries on streaming — a network rig's
//! DDC, a separate SDR lent to a rig as a panadapter, an FDM-DUO whose USB
//! receiver knows nothing about the PTT line its CAT port just asserted. Its
//! buffer fills within its own depth of key-down and stays full until the
//! backlog is discarded, and every sample delivered in between is thrown away.
//! Backends count that, and counted as overruns it reads as a fault: a warning
//! per two seconds of transmit, blaming the DSP thread and advising a lower
//! sample rate, with a running total that only ever measured time on the air.
//!
//! [`IqSource::set_rx_paused`] is what tells the backend which of the two it is
//! looking at. Two things about it have to hold, and neither is visible from
//! inside a backend:
//!
//! * a half-duplex source is told, and a full-duplex one is not — the latter is
//!   still being read through the over, so anything it drops it really dropped;
//! * on the way out, the backlog is discarded **before** the receiver is told to
//!   resume counting. That ordering is what makes a "still draining" latch
//!   unnecessary: the buffer is empty again before the first sample that could
//!   be blamed on anyone arrives.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sdroxide_radio::{Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, Mode, RxId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ev {
    TxBegin,
    TxEnd,
    DiscardRx,
    Paused(bool),
}

type Log = Arc<Mutex<Vec<Ev>>>;

/// A transmit-capable stand-in that records the receive-side hooks.
struct Recorder {
    rate: f64,
    log: Log,
}

impl Recorder {
    fn mark(&self, ev: Ev) {
        self.log.lock().unwrap().push(ev);
    }
}

impl IqSource for Recorder {
    fn sample_rate(&self) -> f64 {
        self.rate
    }
    fn center_hz(&self) -> f64 {
        14_074_000.0
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        // Paced like real hardware: the read blocks for as long as the samples
        // take to arrive, which is what makes the engine's loop tick.
        let n = buf.len().min(4096);
        std::thread::sleep(Duration::from_secs_f64(n as f64 / self.rate));
        for b in buf.iter_mut().take(n) {
            *b = Complex32::new(0.0, 0.0);
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "pause recorder".into()
    }
    fn tx_begin(&mut self, _center_hz: f64, rate: f64) -> Result<f64> {
        self.mark(Ev::TxBegin);
        Ok(rate)
    }
    fn tx_write(&mut self, _samples: &[Complex32]) -> Result<()> {
        Ok(())
    }
    fn tx_end(&mut self) -> Result<()> {
        self.mark(Ev::TxEnd);
        Ok(())
    }
    fn discard_pending_rx(&mut self) {
        self.mark(Ev::DiscardRx);
    }
    fn set_rx_paused(&mut self, paused: bool) {
        self.mark(Ev::Paused(paused));
    }
}

fn caps(rate: f64, full_duplex: bool) -> DeviceCaps {
    DeviceCaps {
        driver: "recorder".into(),
        label: "recorder".into(),
        rx_channels: 1,
        tx_channels: 1,
        full_duplex,
        sample_rates: vec![rate],
        freq_ranges_rx: vec![(0.0, 1_000_000_000.0)],
        freq_ranges_tx: vec![(0.0, 1_000_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Key up, hold, key down; hand back what the source was asked to do.
fn one_over(full_duplex: bool) -> Vec<Ev> {
    let rate = 768_000.0;
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let src = Recorder { rate, log: Arc::clone(&log) };
    let cfg = EngineConfig { tx_ham_only: false, ..Default::default() };
    let mut h = start_engine(Box::new(src), caps(rate, full_duplex), cfg);
    let thread = h.thread.take();

    h.cmd_tx.send(Command::SetMode { rx: RxId::Main, mode: Mode::Usb }).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    h.cmd_tx.send(Command::SetPtt(true)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    h.cmd_tx.send(Command::SetPtt(false)).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    log.lock().unwrap().clone()
}

#[test]
fn a_half_duplex_over_pauses_and_resumes_the_receiver() {
    let log = one_over(false);
    assert!(log.contains(&Ev::TxBegin), "the engine never transmitted: {log:?}");

    let paused = log.iter().position(|e| *e == Ev::Paused(true));
    let discard = log.iter().position(|e| *e == Ev::DiscardRx);
    let resumed = log.iter().position(|e| *e == Ev::Paused(false));

    let paused =
        paused.unwrap_or_else(|| panic!("the receiver was never told about the over: {log:?}"));
    let discard = discard.unwrap_or_else(|| panic!("the backlog was never discarded: {log:?}"));
    let resumed =
        resumed.unwrap_or_else(|| panic!("the receiver was never told it was over: {log:?}"));

    let begin = log.iter().position(|e| *e == Ev::TxBegin).unwrap();
    assert!(paused > begin, "the pause must belong to the over, not precede it: {log:?}");
    // The ordering the whole design rests on: empty the buffer first, *then*
    // start counting faults again. The other way round and the discards still
    // in flight from the over land on the fault counter — which is the bug this
    // is here to prevent coming back as a latch nobody remembers to clear.
    assert!(
        discard < resumed,
        "the backlog must be discarded before the receiver resumes counting: {log:?}"
    );
}

/// Full duplex is still being read through the over, so it is never told
/// otherwise — anything it drops there it really did drop, and hiding it would
/// throw away the one signal that says the host cannot carry both directions.
#[test]
fn a_full_duplex_source_is_never_told_to_pause() {
    let log = one_over(true);
    assert!(log.contains(&Ev::TxBegin), "the engine never transmitted: {log:?}");
    assert!(
        !log.iter().any(|e| matches!(e, Ev::Paused(_))),
        "a full-duplex source must not be paused: {log:?}"
    );
}
