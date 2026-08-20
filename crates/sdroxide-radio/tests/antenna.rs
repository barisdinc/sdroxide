//! Antenna selection: a multi-port front end (a LimeSDR receives on
//! LNAH/LNAL/LNAW and transmits on BAND1/BAND2) must end up on the port the
//! operator asked for — at start, when they switch it, and again after a
//! reconnect, which reopens the device on whatever its driver defaults to.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, ControlUpdate, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, Direction, RadioEvent};

const RATE: f64 = 48_000.0;
const CENTER: f64 = 14_100_000.0;

/// The ports a front end is on, shared with the test so it can see what the
/// engine actually asked the hardware for — as opposed to what the published
/// state says, which is only the readback of it.
#[derive(Clone)]
struct Ports {
    rx: Arc<Mutex<String>>,
    tx: Arc<Mutex<String>>,
    /// Every selection the engine made, in order, tagged with its direction.
    asked: Arc<Mutex<Vec<(Direction, String)>>>,
}

impl Ports {
    /// A device that powers up on its first port, as a driver does.
    fn fresh() -> Self {
        Ports {
            rx: Arc::new(Mutex::new("LNAH".into())),
            tx: Arc::new(Mutex::new("BAND1".into())),
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn rx(&self) -> String {
        self.rx.lock().unwrap().clone()
    }

    fn tx(&self) -> String {
        self.tx.lock().unwrap().clone()
    }

    fn asked(&self) -> Vec<(Direction, String)> {
        self.asked.lock().unwrap().clone()
    }
}

struct Rig {
    ports: Ports,
}

impl IqSource for Rig {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        CENTER
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(256);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "multi-port rig".into()
    }
    fn set_antenna(&mut self, name: &str) -> Result<()> {
        self.ports.asked.lock().unwrap().push((Direction::Rx, name.into()));
        *self.ports.rx.lock().unwrap() = name.into();
        Ok(())
    }
    fn current_antenna(&self) -> String {
        self.ports.rx()
    }
    fn set_tx_antenna(&mut self, name: &str) -> Result<()> {
        self.ports.asked.lock().unwrap().push((Direction::Tx, name.into()));
        *self.ports.tx.lock().unwrap() = name.into();
        Ok(())
    }
    fn current_tx_antenna(&self) -> String {
        self.ports.tx()
    }
}

/// A front end whose antenna is a setting in the *radio* rather than a switch
/// in the driver — an ELAD FDM-DUO's `AN`, which lives in the transceiver, keeps
/// its value across a power cycle, and is read back when the control link opens.
/// Such a rig announces its port instead of being asked for one.
struct ReportingRig {
    ports: Ports,
    /// The port the radio reports, once, the first time it is polled.
    report: Option<&'static str>,
}

impl IqSource for ReportingRig {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        CENTER
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(256);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "rig that reports its own port".into()
    }
    fn poll_control(&mut self) -> Vec<ControlUpdate> {
        match self.report.take() {
            // Recorded as where the hardware is, but not as something that was
            // *asked* of it: the operator set this at the front panel, and all
            // that has happened here is finding out.
            //
            // Unless something *has* been asked of it since, in which case this
            // is a stale answer that crossed that command on the wire — the
            // radio has moved on, and the report says where it used to be.
            Some(name) => {
                if self.ports.asked.lock().unwrap().is_empty() {
                    *self.ports.rx.lock().unwrap() = name.into();
                }
                vec![ControlUpdate::Antenna(name)]
            }
            None => Vec::new(),
        }
    }
    fn set_antenna(&mut self, name: &str) -> Result<()> {
        self.ports.asked.lock().unwrap().push((Direction::Rx, name.into()));
        *self.ports.rx.lock().unwrap() = name.into();
        Ok(())
    }
    fn current_antenna(&self) -> String {
        self.ports.rx()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "lime".into(),
        label: "LimeSDR".into(),
        rx_channels: 1,
        tx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(0.0, 60_000_000.0)],
        freq_ranges_tx: vec![(0.0, 60_000_000.0)],
        antennas_rx: vec!["LNAH".into(), "LNAL".into(), "LNAW".into()],
        antennas_tx: vec!["BAND1".into(), "BAND2".into()],
        ..DeviceCaps::default()
    }
}

/// Wait for a published state that satisfies `want`, so the assertions are
/// about what every UI sees rather than about internal timing.
fn wait_for_state(
    rx: &crossbeam_channel::Receiver<RadioEvent>,
    want: impl Fn(&sdroxide_types::RadioState) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(RadioEvent::State(s)) if want(&s) => return true,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    false
}

/// `--antenna` / `--tx-antenna` on a headless server: nobody is at the machine
/// to pick a port, so the one named on the command line has to be selected as
/// the engine comes up.
#[test]
fn the_startup_antenna_is_selected_on_the_front_end() {
    let ports = Ports::fresh();
    let cfg = EngineConfig {
        initial_antenna: (Some("LNAW".into()), Some("BAND2".into())),
        ..Default::default()
    };
    let mut h = start_engine(Box::new(Rig { ports: ports.clone() }), caps(), cfg);
    let thread = h.thread.take();

    assert!(
        wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAW" && s.antenna_tx == "BAND2"),
        "the opening state must show the ports that were asked for, not the driver's defaults"
    );
    assert_eq!(ports.rx(), "LNAW", "the RX port reached the hardware");
    assert_eq!(ports.tx(), "BAND2", "the TX port reached the hardware");

    drop(h);
    let _ = thread.map(|t| t.join());
}

/// A port the front end does not have is somebody else's radio — the preference
/// outlived an interface switch. Asking for it would only make the driver log an
/// error, so it is skipped and the device is left where it opened.
#[test]
fn an_antenna_this_device_does_not_have_is_left_alone() {
    let ports = Ports::fresh();
    let cfg = EngineConfig {
        initial_antenna: (Some("VERTICAL".into()), Some("ATU".into())),
        ..Default::default()
    };
    let mut h = start_engine(Box::new(Rig { ports: ports.clone() }), caps(), cfg);
    let thread = h.thread.take();

    assert!(wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAH"));
    assert!(ports.asked().is_empty(), "nothing should have been asked of the device");
    assert_eq!(ports.rx(), "LNAH");
    assert_eq!(ports.tx(), "BAND1");

    drop(h);
    let _ = thread.map(|t| t.join());
}

/// The TX port used to be recorded in the state and never sent anywhere. A
/// LimeSDR that transmits on the wrong band port puts the power into a filter
/// that isn't for this band, so the command has to reach the hardware.
#[test]
fn selecting_the_tx_antenna_reaches_the_hardware() {
    let ports = Ports::fresh();
    let mut h =
        start_engine(Box::new(Rig { ports: ports.clone() }), caps(), EngineConfig::default());
    let thread = h.thread.take();

    h.cmd_tx.send(Command::SetAntenna { dir: Direction::Tx, name: "BAND2".into() }).unwrap();
    assert!(wait_for_state(&h.event_rx, |s| s.antenna_tx == "BAND2"));
    assert_eq!(ports.tx(), "BAND2");
    assert_eq!(ports.asked(), vec![(Direction::Tx, "BAND2".to_string())]);

    drop(h);
    let _ = thread.map(|t| t.join());
}

/// A rig that drops out and comes back is reopened by the engine, and a freshly
/// opened device is on its driver's default port. The antenna belongs to the
/// station's coax rather than to the front end, so it has to be restored — an
/// operator who came back to a working waterfall would have no reason to suspect
/// they were listening on the wrong feedline.
#[test]
fn a_reconnect_returns_to_the_selected_antenna() {
    // The replacement the factory hands back: a fresh device on LNAH/BAND1.
    let after = Ports::fresh();
    let handed = after.clone();
    let reopen: sdroxide_radio::ReopenFn = Box::new(move |_center: f64| {
        Ok((Box::new(Rig { ports: handed.clone() }) as Box<dyn IqSource>, caps()))
    });

    let before = Ports::fresh();
    let cfg = EngineConfig {
        initial_antenna: (Some("LNAL".into()), None),
        reopen: Some(reopen),
        ..Default::default()
    };
    let mut h = start_engine(Box::new(Rig { ports: before.clone() }), caps(), cfg);
    let thread = h.thread.take();
    assert!(wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAL"));

    // The operator moves to a third port, then the interface is rebuilt under
    // them: what they chose by hand is what must come back, not what they
    // started on.
    h.cmd_tx.send(Command::SetAntenna { dir: Direction::Rx, name: "LNAW".into() }).unwrap();
    assert!(wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAW"));
    assert_eq!(before.rx(), "LNAW");

    h.swap_tx.send(sdroxide_radio::EngineSwap::ReopenSource).unwrap();
    assert!(
        wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAW"),
        "the reopened front end must be put back on the operator's port"
    );
    assert_eq!(after.rx(), "LNAW", "and the new device is the one that was asked");

    drop(h);
    let _ = thread.map(|t| t.join());
}

/// A radio that carries its own antenna switch is *asked* which port it is on,
/// not told — the setting is in the rig and survived its last power cycle. What
/// it answers has to end up on screen, and has to be remembered: the next time
/// this front end is opened, putting a session file's port back on top of the
/// one the operator left the radio on would move an antenna relay nobody
/// touched.
#[test]
fn the_port_the_radio_reports_is_adopted_and_remembered() {
    // The replacement a reconnect hands back: a fresh device, on its default.
    let after = Ports::fresh();
    let handed = after.clone();
    let reopen: sdroxide_radio::ReopenFn = Box::new(move |_center: f64| {
        Ok((
            Box::new(ReportingRig { ports: handed.clone(), report: None }) as Box<dyn IqSource>,
            caps(),
        ))
    });

    let before = Ports::fresh();
    let cfg = EngineConfig { reopen: Some(reopen), ..Default::default() };
    let mut h = start_engine(
        Box::new(ReportingRig { ports: before.clone(), report: Some("LNAW") }),
        caps(),
        cfg,
    );
    let thread = h.thread.take();

    assert!(
        wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAW"),
        "the port the radio reported must be the one on screen"
    );
    assert!(
        before.asked().is_empty(),
        "adopting a port must not command the radio back to anything"
    );

    // Rebuild the interface: the new device comes up on its driver default, and
    // the port the *radio* reported is the one that has to be put back.
    h.swap_tx.send(sdroxide_radio::EngineSwap::ReopenSource).unwrap();
    assert!(wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAW"));
    assert_eq!(after.rx(), "LNAW", "the reported port outlived the device that reported it");

    drop(h);
    let _ = thread.map(|t| t.join());
}

/// The other half of the rule above: a port this end has already *asserted*
/// outranks what the radio says about itself.
///
/// Every open re-asserts the remembered port, and the radio is read at the same
/// moment, so the two cross on the wire as a matter of course: the answer
/// describes the socket the rig was on a fraction of a second before it was told
/// to move. Adopting that would show — and then remember — the port the operator
/// had just left, on a radio that is no longer on it.
#[test]
fn a_port_the_operator_asked_for_outranks_what_the_radio_reports() {
    let ports = Ports::fresh();
    let cfg = EngineConfig {
        // What the operator wants, from the command line or the session.
        initial_antenna: (Some("LNAW".into()), None),
        ..Default::default()
    };
    // What the radio answers: where it was before the command reached it.
    let mut h = start_engine(
        Box::new(ReportingRig { ports: ports.clone(), report: Some("LNAH") }),
        caps(),
        cfg,
    );
    let thread = h.thread.take();

    assert!(wait_for_state(&h.event_rx, |s| s.antenna_rx == "LNAW"));
    assert_eq!(ports.rx(), "LNAW", "the radio was told, and that is where it is");

    // Every state published from here on has to still say LNAW. The report is
    // polled within a loop or two of the engine starting, and adopting it would
    // publish a state saying otherwise — which is precisely what an operator
    // would see on screen.
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        if let Ok(RadioEvent::State(s)) = h.event_rx.recv_timeout(Duration::from_millis(50)) {
            assert_eq!(
                s.antenna_rx, "LNAW",
                "a stale report must not move the panel off the port that was asked for"
            );
        }
    }
    assert_eq!(ports.rx(), "LNAW", "and the radio must be left where it was told");
    assert_eq!(
        ports.asked(),
        vec![(Direction::Rx, "LNAW".to_string())],
        "nothing may be re-commanded on the strength of the report either"
    );

    drop(h);
    let _ = thread.map(|t| t.join());
}
