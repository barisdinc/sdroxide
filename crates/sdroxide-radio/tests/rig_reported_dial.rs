//! A rig that reports its own dial *and* is the front end.
//!
//! A transceiver whose I/Q output goes into a sound card (a KX3, a K3 with the
//! I/Q tap) is not an SDR with a rig beside it: its synthesiser is the centre of
//! the baseband we capture. Turning its knob moves the spectrum as surely as it
//! moves the readout, so the engine has to adopt the new centre when the rig
//! reports one — otherwise the window keeps the old axis, the waterfall shows
//! content that no longer matches its labels, and the DDC, left on the offset
//! between a stale centre and the new VFO, demodulates somewhere else again.
//!
//! It runs the other way too. Setting the dial here — the readout, a memory, an
//! external controller, a click on the picture — has to move the radio, or its
//! readout and ours show different frequencies with nothing to reconcile them
//! until the next thing it reports snaps ours back to its. The click used to be
//! exempt (the signal is already in the baseband, so only our receiver moved)
//! until a field report showed where the exemption transmits: CW keyed as text
//! through the rig's own keyer, and a mic keyed at the radio, both go out on
//! the dial the click left behind. So [`Command::TuneInSpan`] follows the dial
//! exactly as `SetVfo` does — and on a radio whose window is its own, both
//! still leave the hardware alone while the VFO stays inside the span.
//!
//! All of which needs a dial that answers. A transceiver sending I/Q down a
//! sound card with no control cable on it has one synthesiser and no way to
//! command it, so the last two tests here hold the other end of the same
//! contract: with no link the engine tunes inside the span the radio is
//! already sending (issue #155 — commanding a dial nothing hears relabelled
//! the picture and left the receiver unable to change station at all), and a
//! link that comes up later takes the dial straight back.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, ControlUpdate, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, Mode, RadioEvent, RxId, Vfo};

const DIAL: f64 = 14_074_000.0;
const RATE: f64 = 48_000.0;

/// The rig's one frequency control, shared with the test.
#[derive(Default)]
struct Rig {
    /// Where the rig's synthesiser is — the centre of the I/Q it sends us.
    dial: f64,
    /// Set by the test to stand in for a hand on the knob; the source reports it
    /// on its next poll exactly as the CAT thread would.
    knob: Option<f64>,
    /// Frequencies the engine commanded, in order.
    commanded: Vec<f64>,
    /// Whether anything answers on the rig's control port. False stands in for
    /// a transceiver sending I/Q down a sound card with no CAT cable on it —
    /// one synthesiser still, but not one this end can say anything to.
    dial_reachable: bool,
    /// Modes the engine commanded, in order.
    modes: Vec<sdroxide_types::Mode>,
}

/// A stand-in for a CAT rig whose I/Q output is the capture device: the dial is
/// the centre, and the rig reports the operator's knob out-of-band.
struct MockIqRig {
    rig: Arc<Mutex<Rig>>,
}

impl IqSource for MockIqRig {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        self.rig.lock().unwrap().dial
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        let mut r = self.rig.lock().unwrap();
        r.dial = hz;
        r.commanded.push(hz);
        Ok(())
    }
    /// The point of this whole fixture: one synthesiser for both jobs — for as
    /// long as there is a control link to command it through.
    fn center_is_dial(&self) -> bool {
        self.rig.lock().unwrap().dial_reachable
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(1024);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock rig with an I/Q output".into()
    }
    /// The radio in front of us owns its mode as well as its dial: sdroxide
    /// demodulates the I/Q, but the mode is what the rig's own IF filter, its
    /// front panel and its transmitter are all set by, so a mode chosen here has
    /// to reach it. Not `tracks_rx_mode` — nothing is imposed at connect.
    fn commands_rx_mode(&self) -> bool {
        true
    }
    fn set_control_mode(&mut self, mode: Mode) -> Result<()> {
        self.rig.lock().unwrap().modes.push(mode);
        Ok(())
    }
    fn poll_control(&mut self) -> Vec<ControlUpdate> {
        let mut r = self.rig.lock().unwrap();
        match r.knob.take() {
            // The knob moved the synthesiser; the baseband we capture moved
            // with it, and the rig tells us where it ended up.
            Some(hz) => {
                r.dial = hz;
                vec![ControlUpdate::Freq(hz)]
            }
            None => Vec::new(),
        }
    }
}

/// I/Q from a CAT rig takes the ordinary DDC path — `audio_mode` is for the
/// demodulated-audio format, and this is the case that used to be missed.
fn iq_rig_caps() -> DeviceCaps {
    DeviceCaps {
        driver: "mock-cat".into(),
        label: "mock CAT rig (I/Q)".into(),
        rx_channels: 1,
        tx_channels: 1,
        audio_mode: false,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(10_000.0, 60_000_000.0)],
        freq_ranges_tx: vec![(1_800_000.0, 54_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// An engine on the mock rig, parked on `DIAL`, plus the rig the test drives.
fn engine() -> (sdroxide_radio::EngineHandles, Arc<Mutex<Rig>>) {
    engine_with_link(true)
}

/// [`engine`], choosing whether the rig's control port answers at all.
fn engine_with_link(dial_reachable: bool) -> (sdroxide_radio::EngineHandles, Arc<Mutex<Rig>>) {
    let rig = Arc::new(Mutex::new(Rig { dial: DIAL, dial_reachable, ..Rig::default() }));
    let src = MockIqRig { rig: Arc::clone(&rig) };
    let cfg = EngineConfig { tx_ham_only: false, ..Default::default() };
    let h = start_engine(Box::new(src), iq_rig_caps(), cfg);
    h.cmd_tx.send(Command::SetVfo { vfo: Vfo::A, hz: DIAL }).unwrap();
    (h, rig)
}

/// Wait for a published state that satisfies `f`, returning the last state seen.
fn settle(
    h: &sdroxide_radio::EngineHandles,
    mut f: impl FnMut(&sdroxide_types::RadioState) -> bool,
) -> sdroxide_types::RadioState {
    let mut last = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::State(s) = ev {
                let done = f(&s);
                last = Some(s);
                if done {
                    return last.unwrap();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    last.expect("the engine should publish state")
}

fn shutdown(mut h: sdroxide_radio::EngineHandles) {
    let thread = h.thread.take();
    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// The knob turns, the rig reports it, and the display window has to follow the
/// synthesiser it belongs to — down to the smallest step the rig can report.
#[test]
fn the_window_follows_a_dial_the_rig_moved_itself() {
    let (h, rig) = engine();
    let state = settle(&h, |s| s.vfo_a_hz == DIAL);
    assert_eq!(state.center_hz, DIAL, "the engine starts on the rig's dial");

    // A hand on the knob: two kilohertz, then ten hertz — well inside the
    // captured span, which is exactly the case that used to stay put.
    for step in [2_000.0, 10.0] {
        let want = rig.lock().unwrap().dial + step;
        rig.lock().unwrap().knob = Some(want);
        let state = settle(&h, |s| s.center_hz == want);
        assert_eq!(state.vfo_a_hz, want, "the readout follows the rig");
        assert_eq!(
            state.center_hz, want,
            "the spectrum window has to follow the synthesiser that moved it \
             (a {step} Hz step left it on the old centre)"
        );
    }

    // Adoption, not a correction: nothing was commanded back at the rig, which
    // is what would fight the operator's hand on the knob.
    assert!(
        rig.lock().unwrap().commanded.iter().all(|&hz| hz == DIAL),
        "a dial the rig reported must not be commanded back at it"
    );
    shutdown(h);
}

/// The other half of the contract: a click on the picture moves the rig too,
/// however far inside the captured span the clicked signal already is.
///
/// Field report (a Kenwood on its I/Q output): the click used to tune only the
/// DDC, and the rig's readout parted company with ours. That is more than a
/// display quarrel — an over the engine does not key itself (CW sent as text
/// to the rig's own keyer, a mic keyed at the radio) never borrows the dial
/// through `tx_begin`, so it went on air at the frequency the click had left
/// behind.
#[test]
fn clicking_inside_the_span_moves_the_rig() {
    let (h, rig) = engine();
    settle(&h, |s| s.vfo_a_hz == DIAL);
    rig.lock().unwrap().commanded.clear();

    let clicked = DIAL + 5_000.0;
    h.cmd_tx.send(Command::TuneInSpan { vfo: Vfo::A, hz: clicked }).unwrap();
    let state = settle(&h, |s| s.center_hz == clicked);
    assert_eq!(state.vfo_a_hz, clicked);
    assert_eq!(state.center_hz, clicked, "the window follows the radio the click just moved");
    assert_eq!(
        rig.lock().unwrap().commanded.last().copied(),
        Some(clicked),
        "the clicked frequency has to reach the radio — its keyer and its mic transmit there"
    );
    shutdown(h);
}

/// Setting the dial is the opposite gesture, and the step it moves by is not
/// what decides: a nudge well inside the captured span has to reach the radio
/// exactly as a jump to another band does. Anything less leaves the two
/// readouts disagreeing.
#[test]
fn setting_the_dial_moves_the_rig_however_small_the_step() {
    let (h, rig) = engine();
    settle(&h, |s| s.vfo_a_hz == DIAL);
    rig.lock().unwrap().commanded.clear();

    // One detent on the 10 kHz digit, then on the 100 Hz digit — both far
    // inside the ±21 kHz this 48 kHz capture can reach, which is exactly why
    // neither used to be commanded at the rig.
    for step in [10_000.0, 100.0] {
        let want = rig.lock().unwrap().dial + step;
        h.cmd_tx.send(Command::SetVfo { vfo: Vfo::A, hz: want }).unwrap();
        let state = settle(&h, |s| s.center_hz == want);
        assert_eq!(
            rig.lock().unwrap().commanded.last().copied(),
            Some(want),
            "a {step} Hz dial step has to reach the radio"
        );
        assert_eq!(state.vfo_a_hz, want);
        assert_eq!(state.center_hz, want, "and the window follows the radio it just moved");
    }
    shutdown(h);
}

/// The same commands on an ordinary SDR must not touch the front end while the
/// VFO is inside the span: its window is a resource worth keeping, and retuning
/// on every dial nudge or click would throw the picture away. Both commands,
/// because they now share one engine arm and this is the case that arm must
/// not have changed.
#[test]
fn an_sdr_still_keeps_its_window_when_the_dial_moves() {
    let rig = Arc::new(Mutex::new(Rig { dial: DIAL, ..Rig::default() }));
    // Same fixture, minus the one thing that makes a rig a rig.
    struct Sdr(Arc<Mutex<Rig>>);
    impl IqSource for Sdr {
        fn sample_rate(&self) -> f64 {
            RATE
        }
        fn center_hz(&self) -> f64 {
            self.0.lock().unwrap().dial
        }
        fn set_center_hz(&mut self, hz: f64) -> Result<()> {
            let mut r = self.0.lock().unwrap();
            r.dial = hz;
            r.commanded.push(hz);
            Ok(())
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            std::thread::sleep(Duration::from_millis(5));
            let n = buf.len().min(1024);
            buf[..n].fill(Complex32::new(0.0, 0.0));
            Ok(n)
        }
        fn describe(&self) -> String {
            "mock SDR".into()
        }
    }
    let cfg = EngineConfig { tx_ham_only: false, ..Default::default() };
    let h = start_engine(Box::new(Sdr(Arc::clone(&rig))), iq_rig_caps(), cfg);
    settle(&h, |s| s.center_hz == DIAL);
    rig.lock().unwrap().commanded.clear();

    let want = DIAL + 10_000.0;
    h.cmd_tx.send(Command::SetVfo { vfo: Vfo::A, hz: want }).unwrap();
    let state = settle(&h, |s| s.vfo_a_hz == want);
    assert_eq!(state.center_hz, DIAL, "the SDR's window stays where it was");
    assert!(rig.lock().unwrap().commanded.is_empty(), "and its hardware is not retuned");

    let clicked = DIAL - 8_000.0;
    h.cmd_tx.send(Command::TuneInSpan { vfo: Vfo::A, hz: clicked }).unwrap();
    let state = settle(&h, |s| s.vfo_a_hz == clicked);
    assert_eq!(state.center_hz, DIAL, "a click inside the span keeps the window too");
    assert!(rig.lock().unwrap().commanded.is_empty(), "with no retune for the click either");
    shutdown(h);
}

/// A rig sending I/Q with nothing on its control port. Its synthesiser is
/// still the centre of the baseband — but only its own knob can move it, so
/// the engine has to tune inside the span it is being sent, exactly as it does
/// on an SDR.
///
/// Field report (issue #155, a Xiegu G90 on I/Q with no CAT cable): commanding
/// the dial anyway moved sdroxide's idea of the centre and nothing else, so
/// every click relabelled the span around spectrum the sound card was not
/// sending and walked the clicked station out of the receiver. What the
/// operator saw was a frequency that would not change without a hand on the
/// radio.
#[test]
fn a_rig_with_no_control_link_is_tuned_inside_its_span() {
    let (h, rig) = engine_with_link(false);
    settle(&h, |s| s.vfo_a_hz == DIAL);
    rig.lock().unwrap().commanded.clear();

    let clicked = DIAL + 5_000.0;
    h.cmd_tx.send(Command::TuneInSpan { vfo: Vfo::A, hz: clicked }).unwrap();
    let state = settle(&h, |s| s.vfo_a_hz == clicked);
    assert_eq!(state.vfo_a_hz, clicked, "the receiver goes to the clicked signal");
    assert_eq!(
        state.center_hz, DIAL,
        "and the window stays on the spectrum the radio is actually sending"
    );
    let rig = rig.lock().unwrap();
    assert!(rig.commanded.is_empty(), "there is nothing on the control port to command");
    assert_eq!(rig.dial, DIAL, "so the rig's own synthesiser never moved");
    shutdown(h);
}

/// The link comes up after sdroxide did — the operator switched the radio on,
/// or plugged the cable in. The dial is ours again from that moment, and the
/// capabilities are re-announced so every attached UI hears about it.
#[test]
fn a_control_link_that_comes_up_late_takes_the_dial_back() {
    let (h, rig) = engine_with_link(false);
    settle(&h, |s| s.vfo_a_hz == DIAL);
    rig.lock().unwrap().commanded.clear();
    rig.lock().unwrap().dial_reachable = true;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut announced = false;
    while !announced && Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::Capabilities(c) = ev {
                announced |= c.center_is_dial;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(announced, "a front end that has got its dial back has to say so");

    let clicked = DIAL + 5_000.0;
    h.cmd_tx.send(Command::TuneInSpan { vfo: Vfo::A, hz: clicked }).unwrap();
    let state = settle(&h, |s| s.center_hz == clicked);
    assert_eq!(state.vfo_a_hz, clicked);
    assert_eq!(
        rig.lock().unwrap().commanded.last().copied(),
        Some(clicked),
        "with a link to command through, the click moves the radio again"
    );
    shutdown(h);
}

/// The mode goes the other way too — and only because the source says so.
///
/// The engine asks its front end whether the radio in front of it owns the
/// receive mode, and where the answer is no, an operator's mode change reaches
/// nothing until the next key-down asserts it. That is not a hypothetical: it is
/// what an ELAD FDM-DUO on its own USB receiver did (issue #146) and what a CAT
/// rig sending I/Q down a sound card did before it, in the same shape both
/// times — mode followed rig→app on the poll perfectly, and never travelled
/// app→rig at all, so the two readouts sat there disagreeing.
#[test]
fn a_mode_chosen_here_reaches_a_rig_that_owns_its_mode() {
    let (h, rig) = engine();
    settle(&h, |s| s.vfo_a_hz == DIAL);
    rig.lock().unwrap().modes.clear();

    h.cmd_tx.send(Command::SetMode { rx: RxId::Main, mode: Mode::Cw }).unwrap();
    settle(&h, |s| s.rx[0].mode == Mode::Cw);
    assert_eq!(
        rig.lock().unwrap().modes.last().copied(),
        Some(Mode::Cw),
        "the operator's mode has to be commanded at the radio that is doing the receiving"
    );
    shutdown(h);
}
