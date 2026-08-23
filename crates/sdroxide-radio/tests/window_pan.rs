//! Panning the panadapter past the end of the captured window (issue #133).
//!
//! Fully zoomed out the view *is* the window, so there is nothing left for a
//! drag to slide: the picture stood still however far the operator dragged it,
//! until the dial had crept 45% of the span away and [`Command::SetVfo`]'s span
//! guard jumped the window after it. The client's answer is to hand the part of
//! the pan the window could not absorb to the front end as
//! [`Command::SetCenter`], once per frame, so the window slides with the drag.
//!
//! Two things have to hold here for that to be safe. The centre a client is
//! told about has to say whether it can be commanded at all — on a rig whose
//! I/Q output is its own dial there is one synthesiser and the drag is already
//! turning it — and a centre that is asked for twice must only be commanded
//! once, because a pan held against the end of a band asks for the same
//! unreachable place every frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use sdroxide_radio::{Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, RadioEvent};

const RATE: f64 = 2_000_000.0;
const CENTER: f64 = 14_200_000.0;

/// An SDR: a centre of its own, which the dial tunes inside.
struct Sdr {
    center_hz: f64,
    /// How many times the hardware was actually told to move.
    tunes: Arc<AtomicUsize>,
    dial_centred: bool,
}

impl IqSource for Sdr {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        self.center_hz
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center_hz = hz;
        self.tunes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn center_is_dial(&self) -> bool {
        self.dial_centred
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(256);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "test front end".into()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "test".into(),
        label: "test".into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(0.0, 60_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Collect events for `secs`: the capabilities announced, and the last centre
/// and dial reported.
fn drain(rx: &Receiver<RadioEvent>, secs: f64) -> (Option<DeviceCaps>, f64, f64) {
    let (mut announced, mut center, mut dial) = (None, f64::NAN, f64::NAN);
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < deadline {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                RadioEvent::Capabilities(c) => announced = Some(c),
                RadioEvent::State(s) => {
                    center = s.center_hz;
                    dial = s.active_freq_hz();
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (announced, center, dial)
}

fn engine(dial_centred: bool, tunes: &Arc<AtomicUsize>) -> sdroxide_radio::EngineHandles {
    let source = Sdr { center_hz: CENTER, tunes: Arc::clone(tunes), dial_centred };
    start_engine(Box::new(source), caps(), EngineConfig::default())
}

/// The capabilities a client is handed say what kind of centre this is, and
/// they say it whatever the backend put in the struct — the engine fills it in
/// from the source itself, so no backend can forget to.
#[test]
fn the_capabilities_report_whether_the_centre_is_the_dial() {
    for dial_centred in [false, true] {
        let tunes = Arc::new(AtomicUsize::new(0));
        let mut h = engine(dial_centred, &tunes);
        let thread = h.thread.take();
        let (announced, _, _) = drain(&h.event_rx, 0.5);
        assert_eq!(
            announced.expect("capabilities are announced at start-up").center_is_dial,
            dial_centred,
            "the source's own answer must reach the client"
        );
        drop(h.cmd_tx);
        if let Some(t) = thread {
            let _ = t.join();
        }
    }
}

/// What a pan asks for: the window moves and the dial is left exactly where it
/// was. The client moves the dial itself, by the same amount, so that the
/// marker keeps its place on screen — the engine must not do it here, or a pan
/// would tune twice.
#[test]
fn a_commanded_centre_moves_the_window_and_not_the_dial() {
    let tunes = Arc::new(AtomicUsize::new(0));
    let mut h = engine(false, &tunes);
    let thread = h.thread.take();
    drain(&h.event_rx, 0.3);

    h.cmd_tx.send(Command::SetCenter(CENTER - 400_000.0)).unwrap();
    let (_, center, dial) = drain(&h.event_rx, 0.5);
    assert!(
        (center - (CENTER - 400_000.0)).abs() < 1.0,
        "the window should have moved, got {center}"
    );
    assert!(
        (dial - CENTER).abs() < 1.0,
        "the dial is the client's to move, not the engine's: {dial}"
    );

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// A pan pressed against the end of a band asks for the same centre every
/// frame — the front end clamped the last one and reported where it really
/// landed. Commanding it again would cost a retune, a skimmer restart and a
/// waterfall remap sixty times a second.
#[test]
fn the_same_centre_asked_for_twice_is_commanded_once() {
    let tunes = Arc::new(AtomicUsize::new(0));
    let mut h = engine(false, &tunes);
    let thread = h.thread.take();
    drain(&h.event_rx, 0.3);
    let before = tunes.load(Ordering::SeqCst);

    for _ in 0..20 {
        h.cmd_tx.send(Command::SetCenter(CENTER + 250_000.0)).unwrap();
    }
    drain(&h.event_rx, 0.5);
    assert_eq!(
        tunes.load(Ordering::SeqCst) - before,
        1,
        "twenty asks for one centre should be one tune"
    );

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}
