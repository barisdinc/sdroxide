//! What an APRS over actually sounds like by the time it leaves the engine.
//!
//! Field report (issue #150): a beacon and a message went out on an IC-705 —
//! the transmission was there, and briefer than it should have been — and no
//! receiver could decode it. The controller's own tests could not see it: they
//! stop at `DigiEngine::fill_tx_block`, and what reaches a radio is what the
//! engine does with those blocks afterwards.
//!
//! So this stands the whole engine up on an audio-modulated rig — the shape an
//! IC-705 on its LAN or USB connector presents — captures every sample handed
//! to `tx_write_audio`, and puts it back through a real Bell 202 modem and
//! deframer. Anything between the modem and the sound card that loses,
//! duplicates, reorders or truncates a sample shows up here as a frame that
//! will not decode, which is exactly what the operator on the far end sees.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_ax25::{Deframer, Packet, PacketType};
use sdroxide_dsp::{AfskProfile, AfskRx};
use sdroxide_radio::{AudioParams, Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, DigiConfig, Mode, RxId};

/// The APRS channel for Region 1, which is where the engine tunes itself.
const DIAL_HZ: f64 = 144_800_000.0;

/// Every sample the rig's sound card was given, in order.
#[derive(Default)]
struct Heard {
    audio: Vec<f32>,
    blocks: usize,
}

/// A transceiver that modulates the audio we send it: quadrature receive, audio
/// transmit, its own power control. An IC-705 over its LAN port is this shape,
/// and so is any CAT rig on a sound card.
struct SoundCardRig {
    heard: Arc<Mutex<Heard>>,
    /// A little noise on the receive side. Not decoration: the packet channel
    /// counts its CSMA slots on the receive sample clock, so a source that
    /// hands back nothing at all is a station that can never decide to key.
    rng: u32,
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
        // Audio in the I component, which is what a demod-audio rig hands
        // back. Noise rather than silence: the packet channel counts its CSMA
        // slots on the receive sample clock, so a source that hands back
        // nothing is a station that can never decide to key.
        for z in &mut buf[..n] {
            self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let a = (self.rng >> 16) as f32 / 32_768.0 - 1.0;
            *z = Complex32::new(a * 0.02, 0.0);
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock CAT rig on a sound card".into()
    }
    fn display_bandwidth(&self) -> Option<f64> {
        Some(4000.0)
    }
    fn commands_tx_power(&self) -> bool {
        true
    }
    fn tx_begin(&mut self, _center_hz: f64, rate: f64) -> Result<f64> {
        Ok(rate)
    }
    fn tx_write_audio(&mut self, audio: &[f32]) -> Result<()> {
        let mut h = self.heard.lock().unwrap();
        h.blocks += 1;
        h.audio.extend_from_slice(audio);
        Ok(())
    }
    fn tx_end(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The two shapes a transceiver presents, which are two different paths
/// through the engine's receive side and have to be tested apart.
///
/// `audio_mode` is a radio that demodulates for us and hands back audio — an
/// IC-705 on its LAN port, a CAT rig on a sound card. The other is a radio
/// that hands back I/Q and takes audio to modulate, which is every SDR with a
/// transmitter. The digital-mode tap is fed from a different place in each,
/// and a packet station counts its channel-access slots on that tap — so a
/// fault in either is a station that never keys at all.
fn caps(audio_mode: bool) -> DeviceCaps {
    DeviceCaps {
        driver: "mock-cat".into(),
        label: "mock CAT rig".into(),
        rx_channels: 1,
        tx_channels: 1,
        // The shape an IC-705 on its LAN port presents: the rig demodulates
        // and hands back audio, and modulates the audio we hand it.
        audio_mode,
        tx_audio: true,
        freq_ranges_rx: vec![(10_000.0, 500_000_000.0)],
        freq_ranges_tx: vec![(144_000_000.0, 148_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// A station that will actually transmit: a callsign and somewhere to say it is.
fn station() -> DigiConfig {
    DigiConfig {
        my_call: "OE3JJS".into(),
        my_grid: "JN88ec".into(),
        // Take the channel as soon as a slot has passed; the dice are not what
        // is under test.
        packet_persist: 255,
        packet_slottime_ms: 10,
        ..DigiConfig::default()
    }
}

/// Run `cmds` on an APRS station and return every sample the rig was given.
fn over_for(audio_mode: bool, cmds: Vec<Command>) -> Vec<f32> {
    let heard = Arc::new(Mutex::new(Heard::default()));
    let src = SoundCardRig { heard: Arc::clone(&heard), rng: 0x1234_5678 };
    // A ring nothing reads. Without somewhere to play audio the engine never
    // builds the main receive chain, and the digital-mode tap it feeds is what
    // a packet station counts its channel-access slots on — so an engine with
    // no audio sink is a station that can never decide to key.
    let (producer, _consumer) = rtrb::RingBuffer::<f32>::new(48_000);
    let cfg = EngineConfig {
        tx_ham_only: false,
        remember_session: false,
        audio: Some(AudioParams { producer, out_rate: 48_000.0 }),
        ..Default::default()
    };
    let mut h = start_engine(Box::new(src), caps(audio_mode), cfg);
    let thread = h.thread.take();
    std::thread::sleep(Duration::from_millis(200));
    for c in cmds {
        h.cmd_tx.send(c).expect("engine gone");
        std::thread::sleep(Duration::from_millis(80));
    }

    // Wait for the over to finish rather than for a fixed time: a packet burst
    // is about a second, and cutting the capture short would fake the very
    // symptom under test.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut quiet_since: Option<Instant> = None;
    while Instant::now() < deadline {
        while h.event_rx.try_recv().is_ok() {}
        let blocks = heard.lock().unwrap().blocks;
        std::thread::sleep(Duration::from_millis(50));
        let after = heard.lock().unwrap().blocks;
        if after > 0 && after == blocks {
            // Nothing new for a beat once something has gone out: the over is
            // over.
            match quiet_since {
                Some(t) if t.elapsed() > Duration::from_millis(250) => break,
                Some(_) => {}
                None => quiet_since = Some(Instant::now()),
            }
        } else {
            quiet_since = None;
        }
    }
    let out = std::mem::take(&mut *heard.lock().unwrap());

    drop(h.cmd_tx);
    drop(h.event_rx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    assert!(out.blocks > 0, "nothing ever reached the rig's sound card");
    out.audio
}

/// Demodulate what the rig was given and return every frame that survived its
/// check sequence.
fn frames_in(audio: &[f32]) -> Vec<Vec<u8>> {
    let mut modem = AfskRx::new(48_000.0, AfskProfile::Vhf1200);
    let mut deframer = Deframer::new();
    let (mut levels, mut frames) = (Vec::new(), Vec::new());
    for chunk in audio.chunks(480) {
        levels.clear();
        modem.process(chunk, &mut levels);
        for lvl in levels.drain(..) {
            deframer.push_level(lvl, &mut frames);
        }
    }
    frames
}

/// A beacon, all the way from the button to the sound card and back through a
/// receiver.
#[test]
fn a_beacon_reaches_the_rig_as_a_decodable_frame() {
    for audio_mode in [true, false] {
        beacon_on(audio_mode);
    }
}

fn beacon_on(audio_mode: bool) {
    let audio = over_for(
        audio_mode,
        vec![
            Command::SetMode { rx: RxId::Main, mode: Mode::Aprs },
            Command::SetDigiConfig(station()),
            Command::AprsBeacon,
        ],
    );
    let secs = audio.len() as f32 / 48_000.0;
    // 500 ms of TXDELAY flags plus a position report; materially less than that
    // means the over was cut off.
    assert!(secs > 0.5, "audio_mode={audio_mode}: the over was only {secs:.3} s");

    let frames = frames_in(&audio);
    assert_eq!(
        frames.len(),
        1,
        "audio_mode={audio_mode}: {secs:.3} s of audio reached the rig and {} frames came back \
         out of it",
        frames.len()
    );
    let p = Packet::parse(&frames[0], None).expect("the frame does not parse");
    assert_eq!(p.src().call(), "OE3JJS");
    let PacketType::Ui(ui) = p.packet_type() else { panic!("a beacon must be a UI frame") };
    let data = sdroxide_aprs::parse(p.dst().call(), &ui.payload).expect("not APRS");
    let pos = data.position().expect("no position in the beacon");
    assert!((pos.pos.lat - 48.2).abs() < 0.2, "{}", pos.pos.lat);
    assert!((pos.pos.lon - 16.4).abs() < 0.4, "{}", pos.pos.lon);
}

/// ...and a message, which is what the report was actually about.
#[test]
fn a_message_reaches_the_rig_as_a_decodable_frame() {
    let audio = over_for(
        true,
        vec![
            Command::SetMode { rx: RxId::Main, mode: Mode::Aprs },
            Command::SetDigiConfig(station()),
            Command::AprsSendMessage { to: "VK2ABC".into(), text: "hello from sdroxide".into() },
        ],
    );
    let secs = audio.len() as f32 / 48_000.0;
    assert!(secs > 0.5, "the over reached the rig as only {secs:.3} s of audio");

    let frames = frames_in(&audio);
    assert!(!frames.is_empty(), "{secs:.3} s of audio reached the rig and nothing decoded");
    let p = Packet::parse(&frames[0], None).expect("the frame does not parse");
    let PacketType::Ui(ui) = p.packet_type() else { panic!("must be a UI frame") };
    let data = sdroxide_aprs::parse(p.dst().call(), &ui.payload).expect("not APRS");
    match data {
        sdroxide_aprs::AprsData::Message(m) => {
            assert_eq!(m.addressee, "VK2ABC");
            assert_eq!(m.text, "hello from sdroxide");
        }
        other => panic!("{other:?}"),
    }
}

/// Control: the same harness driving the *packet* mode, which shares the modem
/// and the channel-access rules. Tells an APRS-specific fault from one in the
/// AX.25 layer under it.
#[test]
fn a_packet_beacon_reaches_the_rig() {
    let cfg = DigiConfig {
        packet_mycall: "OE3JJS".into(),
        packet_beacon_text: "test".into(),
        packet_persist: 255,
        packet_slottime_ms: 10,
        ..DigiConfig::default()
    };
    let audio = over_for(
        false,
        vec![
            Command::SetMode { rx: RxId::Main, mode: Mode::Packet },
            Command::SetDigiConfig(cfg),
            Command::PacketBeacon,
        ],
    );
    let secs = audio.len() as f32 / 48_000.0;
    assert!(secs > 0.5, "the over reached the rig as only {secs:.3} s of audio");
}
