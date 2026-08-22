//! What level a digital over reaches the radio at.
//!
//! Field report ([issue #131]): a Kenwood on its I/Q output made 100 % of its
//! power under TUNE and a quarter of it on FT8, with the Drive slider moving
//! nothing. The rig modulates the audio we send it, and the modems synthesise
//! their modulating signal at half scale — 6 dB of headroom that belongs to the
//! modem's own arithmetic, not to the transmitter. Nothing divided it out
//! again, so the radio was asked for a quarter of the power a TUNE at the same
//! slider setting asks for, and no power command could make up the difference:
//! the output was riding on the audio, not on the power register.
//!
//! So the two levels are compared against each other rather than against a
//! constant. Whatever a tune tone is worth to a rig, a digital over has to be
//! worth the same.
//!
//! [issue #131]: https://github.com/dividebysandwich/sdroxide/issues/131

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, Mode, RxId};

const DIAL_HZ: f64 = 14_090_000.0;

/// The loudest sample the rig's sound card was given, and how many blocks it
/// took.
#[derive(Default)]
struct Heard {
    peak: f32,
    blocks: usize,
}

/// A CAT rig on a sound card: it modulates the audio we hand it and has its own
/// power control, which is what nearly every rig this backend drives looks
/// like — a Kenwood among them.
struct SoundCardRig {
    heard: Arc<Mutex<Heard>>,
}

impl IqSource for SoundCardRig {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_hz(&self) -> f64 {
        DIAL_HZ
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn center_is_dial(&self) -> bool {
        true
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(1024);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock CAT rig on a sound card".into()
    }
    /// The rig's power register is the level control; the audio is only the
    /// modulating signal. Exactly the case the report came from.
    fn commands_tx_power(&self) -> bool {
        true
    }
    fn tx_begin(&mut self, _center_hz: f64, rate: f64) -> Result<f64> {
        Ok(rate)
    }
    fn tx_write_audio(&mut self, audio: &[f32]) -> Result<()> {
        let mut h = self.heard.lock().unwrap();
        h.blocks += 1;
        for &a in audio {
            h.peak = h.peak.max(a.abs());
        }
        Ok(())
    }
    fn tx_end(&mut self) -> Result<()> {
        Ok(())
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "mock-cat".into(),
        label: "mock CAT rig".into(),
        rx_channels: 1,
        tx_channels: 1,
        // Quadrature receive, audio transmit — the shape of the rig in the
        // report.
        audio_mode: false,
        tx_audio: true,
        freq_ranges_rx: vec![(10_000.0, 148_000_000.0)],
        freq_ranges_tx: vec![(1_800_000.0, 54_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Run an over built by `cmds` and report the loudest audio the rig was given.
/// Gives up once the transmission is plainly under way, since the level is
/// settled by then and a keyboard mode would otherwise send until stopped.
fn peak_of(cmds: Vec<Command>) -> f32 {
    let heard = Arc::new(Mutex::new(Heard::default()));
    let src = SoundCardRig { heard: Arc::clone(&heard) };
    let cfg = EngineConfig { tx_ham_only: false, ..Default::default() };
    let mut h = start_engine(Box::new(src), caps(), cfg);
    let thread = h.thread.take();
    std::thread::sleep(Duration::from_millis(200));
    for c in cmds {
        h.cmd_tx.send(c).expect("engine gone");
        std::thread::sleep(Duration::from_millis(60));
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        while h.event_rx.try_recv().is_ok() {}
        if heard.lock().unwrap().blocks > 20 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = std::mem::take(&mut *heard.lock().unwrap());

    drop(h.cmd_tx);
    drop(h.event_rx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    assert!(out.blocks > 0, "nothing ever reached the rig's sound card");
    out.peak
}

/// The fix. A keyboard-mode over and a tune tone are both modulating signals
/// for the same transmitter, and they arrive at the same level; before this the
/// over arrived 6 dB down, which is the quarter of the power the report
/// measured.
#[test]
fn a_digital_over_reaches_the_rig_at_the_level_a_tune_does() {
    let tune = peak_of(vec![Command::SetTune(true)]);
    let over = peak_of(vec![
        Command::SetMode { rx: RxId::Main, mode: Mode::Rtty },
        Command::DigiTxActive(true),
        Command::DigiTxText("cq cq de w1aw w1aw k".into()),
    ]);
    assert!(tune > 0.9, "the tune tone is full scale: {tune}");
    assert!(
        (over - tune).abs() < 0.1,
        "a digital over is {over} against the tune tone's {tune} — the rig is being asked for \
         {:.0}% of the power",
        (over / tune).powi(2) * 100.0
    );
}

// ── the same over on a radio we modulate ourselves ───────────────────────────

/// An I/Q transmitter: no sound card, no power register of its own, so Drive is
/// the level and the modulated baseband is what goes on the air.
struct IqRadio {
    heard: Arc<Mutex<Heard>>,
}

impl IqSource for IqRadio {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_hz(&self) -> f64 {
        DIAL_HZ
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(1024);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock I/Q transmitter".into()
    }
    fn tx_begin(&mut self, _center_hz: f64, rate: f64) -> Result<f64> {
        Ok(rate)
    }
    fn tx_write(&mut self, iq: &[Complex32]) -> Result<()> {
        let mut h = self.heard.lock().unwrap();
        h.blocks += 1;
        for z in iq {
            h.peak = h.peak.max(z.norm());
        }
        Ok(())
    }
    fn tx_end(&mut self) -> Result<()> {
        Ok(())
    }
}

/// As [`peak_of`], for the modulated-I/Q path: the strongest baseband magnitude
/// the converter was handed.
fn iq_peak_of(cmds: Vec<Command>) -> f32 {
    let heard = Arc::new(Mutex::new(Heard::default()));
    let src = IqRadio { heard: Arc::clone(&heard) };
    let cfg = EngineConfig { tx_ham_only: false, ..Default::default() };
    let mut h = start_engine(Box::new(src), DeviceCaps { tx_audio: false, ..caps() }, cfg);
    let thread = h.thread.take();
    std::thread::sleep(Duration::from_millis(200));
    for c in cmds {
        h.cmd_tx.send(c).expect("engine gone");
        std::thread::sleep(Duration::from_millis(60));
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        while h.event_rx.try_recv().is_ok() {}
        if heard.lock().unwrap().blocks > 20 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = std::mem::take(&mut *heard.lock().unwrap());

    drop(h.cmd_tx);
    drop(h.event_rx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    assert!(out.blocks > 0, "nothing was ever transmitted");
    out.peak
}

/// On a radio sdroxide modulates itself, Drive is the level — so a hundred
/// percent of it has to be a hundred percent of the transmitter, the same as a
/// tune at a hundred percent. The modem's headroom used to eat two thirds of
/// the power the slider was promising, with nothing to show it.
#[test]
fn drive_spends_the_whole_transmitter_on_a_digital_over() {
    let tune = iq_peak_of(vec![Command::SetTuneDrive(1.0), Command::SetTune(true)]);
    let over = iq_peak_of(vec![
        Command::SetTxDrive(1.0),
        Command::SetMode { rx: RxId::Main, mode: Mode::Rtty },
        Command::DigiTxActive(true),
        Command::DigiTxText("cq cq de w1aw w1aw k".into()),
    ]);
    assert!(tune > 0.9, "a full tune is a full-scale carrier: {tune}");
    assert!(
        (over - tune).abs() < 0.1,
        "an over at full Drive is {over} against the full tune's {tune} — {:.0}% of the power",
        (over / tune).powi(2) * 100.0
    );
}
