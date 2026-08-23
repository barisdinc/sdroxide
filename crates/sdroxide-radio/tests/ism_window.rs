//! The ISM decoder's window across a band change (issue #142).
//!
//! The window is not just a place, it is a width: the 868 MHz plan wants nearly
//! two megahertz where 433.92 wants a quarter of one. Both come out of the same
//! front end, so changing band changes the decimation the engine has to take —
//! and a chain built for the previous band cannot be retuned into the new one,
//! it has to be rebuilt.
//!
//! The reported symptom was the whole decoder going silent after a band change,
//! with the panel saying nothing was inside the window, and both lanes coming
//! back the moment the operator switched decoding off and on again. That is the
//! signature of a stale window rather than a wrong one, which is what this test
//! pins: the engine is walked from one band to the other and the lane has to be
//! running on the far side, without the decoder being restarted.
//!
//! rtl_433 only, because it is the lane that can be pointed at more than one
//! band: the native decoders are fixed on the 868 MHz channel plan, so their
//! window slides with the dial but is never asked to change width.

#![cfg(feature = "rtl433")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_radio::{AudioParams, Complex32, EngineConfig, IqSource, Result, rtrb, start_engine};
use sdroxide_types::{Command, DeviceCaps, IsmSettings, IsmStatus, RadioEvent, Vfo};

/// The bits of `Rtl433Settings::bands`, from `RTL433_BAND_LABELS`.
const BAND_433: u32 = 1 << 0;
const BAND_868: u32 = 1 << 1;

/// `Rtl433Settings::bandwidth_hz` meaning "whatever the band asks for".
const AUTO: u32 = sdroxide_types::RTL433_BANDWIDTH_AUTO;

const DIAL_433: f64 = 433_920_000.0;
const DIAL_868: f64 = 868_650_000.0;

/// Wide enough for the 868 MHz band to fit undecimated (1.024 MHz inside three
/// quarters of 1.4), and narrow enough that 433.92 MHz is decimated by four —
/// so the two bands genuinely ask for different chains, which is the whole
/// point of the test.
const RATE: f64 = 1_400_000.0;

/// A front end that tunes anywhere and hears nothing but its own noise floor.
/// This test is about which window the engine builds, not about what decodes
/// inside it.
struct Quiet {
    center: Arc<Mutex<f64>>,
    seed: u64,
}

impl IqSource for Quiet {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        *self.center.lock().unwrap()
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        *self.center.lock().unwrap() = hz;
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        // Not real time: the lane's status is emitted on a wall clock, so a
        // trickle of blocks is enough to keep it talking without spending a core
        // on 1.4 Msps of silence.
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(4096);
        for z in buf[..n].iter_mut() {
            let mut r = || {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 7;
                self.seed ^= self.seed << 17;
                ((self.seed >> 11) as f64 / (1u64 << 53) as f64) as f32 - 0.5
            };
            *z = Complex32::new(r() * 0.01, r() * 0.01);
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "quiet stand-in".into()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "mock".into(),
        label: "mock".into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(1_000_000.0, 2_000_000_000.0)],
        ..DeviceCaps::default()
    }
}

fn ism_on(bands: u32, bandwidth_hz: u32) -> IsmSettings {
    IsmSettings {
        enabled: true,
        // The native decoders switched off: they listen on 868 MHz whatever the
        // dial says, and this is about the lane that follows it.
        families: 0,
        rtl433: sdroxide_types::Rtl433Settings { bands, bandwidth_hz },
        ..IsmSettings::default()
    }
}

/// Wait for an ISM status that satisfies `f`, or say what the last one was.
fn ism_status(
    h: &sdroxide_radio::EngineHandles,
    what: &str,
    mut f: impl FnMut(&IsmStatus) -> bool,
) -> IsmStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last: Option<IsmStatus> = None;
    while Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::IsmStatus(s) = ev {
                if f(&s) {
                    return s;
                }
                last = Some(s);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the ISM decoder never reported {what}; last status: {last:#?}");
}

fn lane_running_on(band: &'static str) -> impl Fn(&IsmStatus) -> bool {
    move |s: &IsmStatus| s.rtl433.as_ref().is_some_and(|r| r.running && r.band == band)
}

#[test]
fn a_band_change_re_sizes_the_window_without_a_restart() {
    let root = std::env::temp_dir().join(format!("sdroxide-ism-window-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &root) };

    // The lanes below the speaker — the skimmers, the ISM window, the IQ
    // recorder — are fed from the same pass as the main receiver, so an engine
    // with nowhere to play audio never reaches them. Hence a ring nothing reads.
    let (producer, _consumer) = rtrb::RingBuffer::<f32>::new(48_000);
    let center = Arc::new(Mutex::new(DIAL_433));
    let mut h = start_engine(
        Box::new(Quiet { center, seed: 0x1234_5678_9ABC_DEF0 }),
        caps(),
        EngineConfig {
            remember_session: false,
            audio: Some(AudioParams { producer, out_rate: 48_000.0 }),
            ..Default::default()
        },
    );
    let thread = h.thread.take();
    let send = |c: Command| h.cmd_tx.send(c).unwrap();

    // ---- Listening on 433.92 MHz, which is decimated by four ----
    send(Command::SetVfo { vfo: Vfo::A, hz: DIAL_433 });
    send(Command::SetIsmConfig(ism_on(BAND_433, AUTO)));
    let narrow = ism_status(&h, "the 433.92 MHz lane starting", lane_running_on("433.92 MHz"));
    assert!(
        narrow.window_rate_hz < 500_000.0,
        "433.92 MHz should be reached through a decimated window, not {:.0} Hz",
        narrow.window_rate_hz
    );

    // ---- The operator picks 868 MHz and tunes there ----
    // Nothing else happens: the decoder is never stopped, which is precisely the
    // workaround this test exists to make unnecessary.
    send(Command::SetIsmConfig(ism_on(BAND_868, AUTO)));
    send(Command::SetVfo { vfo: Vfo::A, hz: DIAL_868 });
    let wide = ism_status(&h, "the 868 MHz lane starting", lane_running_on("868 MHz EU"));
    assert!(
        wide.window_rate_hz > 1_024_000.0,
        "868 MHz EU needs 1.024 MHz of window and got {:.0} Hz",
        wide.window_rate_hz
    );
    assert!(
        (wide.window_center_hz - DIAL_868).abs() < 100_000.0,
        "the window sat on {:.4} MHz with the dial on {:.4}",
        wide.window_center_hz / 1e6,
        DIAL_868 / 1e6
    );
    assert_eq!(wide.unavailable, None, "the decoder called itself unavailable while running");

    // ---- The operator narrows the window by hand (issue #141) ----
    // Same band, same dial: only the chosen bandwidth moves, and it has to move
    // the window with it exactly as a band change does.
    send(Command::SetIsmConfig(ism_on(BAND_868, 250_000)));
    let hand = ism_status(&h, "the 868 MHz lane on a 250 kHz window", |s| {
        s.rtl433.as_ref().is_some_and(|r| r.running && r.band == "868 MHz EU" && r.rate_hz > 0.0)
            && s.window_rate_hz < 500_000.0
    });
    let lane_rate = hand.rtl433.as_ref().unwrap().rate_hz;
    assert!(
        (250_000.0..=400_000.0).contains(&lane_rate),
        "a 250 kHz request settled on {lane_rate:.0} Hz"
    );

    // ---- And back to AUTO, so the stretch is covered as well as the shrink ----
    send(Command::SetIsmConfig(ism_on(BAND_868, AUTO)));
    let back = ism_status(&h, "the 868 MHz lane back on its own width", |s| {
        s.rtl433.as_ref().is_some_and(|r| r.running && r.band == "868 MHz EU")
            && s.window_rate_hz > 1_024_000.0
    });
    assert!(
        back.rtl433.as_ref().unwrap().rate_hz >= 1_024_000.0,
        "AUTO should give 868 MHz its own megahertz back"
    );

    // ---- And back to the other band, which is the shrink the ticket was ----
    send(Command::SetIsmConfig(ism_on(BAND_433, AUTO)));
    send(Command::SetVfo { vfo: Vfo::A, hz: DIAL_433 });
    ism_status(&h, "the 433.92 MHz lane starting again", lane_running_on("433.92 MHz"));

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    unsafe { std::env::remove_var("SDROXIDE_CONFIG_DIR") };
}
