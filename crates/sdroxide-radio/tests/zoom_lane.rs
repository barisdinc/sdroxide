//! What the panadapter resolves has to follow the window on screen, not the
//! width the front end happens to be streaming.
//!
//! The device-wide analyser is a fixed number of bins across the whole stream,
//! so the further in the operator zooms the fewer of them land on screen. On a
//! narrow front end that never bites; on a wide one it bites almost at once. An
//! RX-888 asked for 8.1 MHz gives 247 Hz a bin through the largest FFT the
//! display will ask for, so a 68 kHz window on screen was drawn out of 275
//! numbers and stair-stepped visibly. Front-end decimation was the only cure,
//! and it buys the resolution by throwing the rest of the band away.
//!
//! So a viewport the device-wide analyser can no longer fill is served from a
//! zoom lane instead: the window mixed down and decimated to its own width,
//! analysed there. These tests pin the two halves of that — the resolution it
//! is for, and the frequency accuracy that makes the resolution worth having.

use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, EngineConfig, EngineHandles, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, SpectrumConfig, SpectrumFrame};

/// A front end as wide as an RX-888 asked for a third of its half-spectrum.
const RATE: f64 = 8_100_000.0;
const CENTER: f64 = 14_200_000.0;

/// Where the pair sits. Well off the front end's own centre, so the DC-spike
/// suppression cannot be what makes them hard to see — and so the lane's NCO
/// has a real offset to get the sign of.
const TONE_MID_HZ: f64 = CENTER + 100_000.0;
/// Two tones this far apart. A fifth of one bin of the device-wide analyser
/// (8.1 MHz over 4096 points is 1978 Hz), and 52 bins apart through the lane.
const TONE_GAP_HZ: f64 = 400.0;
const TONE_A_HZ: f64 = TONE_MID_HZ - TONE_GAP_HZ / 2.0;
const TONE_B_HZ: f64 = TONE_MID_HZ + TONE_GAP_HZ / 2.0;

/// The window the client asks to see: 10 kHz, which is 5 bins of the
/// device-wide analyser and 1300 of the lane's.
const VIEW_HZ: f64 = 10_000.0;

/// How far below the weaker peak the trough between two tones has to sit before
/// they count as resolved, in the u8 units a frame carries — about 11 dB over
/// the 140 dB window these tests map.
const TROUGH_UNITS: i32 = 20;

/// A front end streaming two closely spaced tones at the device rate.
struct TwoTones {
    center_hz: f64,
    phase_a: f64,
    phase_b: f64,
}

impl IqSource for TwoTones {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        self.center_hz
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center_hz = hz;
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        // Paced so the engine's loop runs at a realistic rate without the test
        // waiting real time for 8.1 Msps.
        std::thread::sleep(Duration::from_millis(2));
        let (sa, sb) =
            (TAU * (TONE_A_HZ - self.center_hz) / RATE, TAU * (TONE_B_HZ - self.center_hz) / RATE);
        for s in buf.iter_mut() {
            *s = Complex32::new(
                (self.phase_a.cos() + self.phase_b.cos()) as f32 * 0.4,
                (self.phase_a.sin() + self.phase_b.sin()) as f32 * 0.4,
            );
            self.phase_a = (self.phase_a + sa) % TAU;
            self.phase_b = (self.phase_b + sb) % TAU;
        }
        Ok(buf.len())
    }
    fn describe(&self) -> String {
        "two-tone generator".into()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "test".into(),
        label: "test".into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(1_000_000.0, 60_000_000.0)],
        ..DeviceCaps::default()
    }
}

fn engine() -> EngineHandles {
    start_engine(
        Box::new(TwoTones { center_hz: CENTER, phase_a: 0.0, phase_b: 0.0 }),
        caps(),
        EngineConfig { remember_session: false, ..Default::default() },
    )
}

/// The spectrum config a client sends for a `span_hz` window on the centre —
/// `None` for the fully zoomed-out view, which is what the device-wide
/// analyser already is. `bins` is the panadapter width the client is drawing,
/// which is what the lane's threshold and its own FFT are both measured
/// against.
fn cfg_at(span_hz: Option<f64>, bins: u32) -> Command {
    Command::SetSpectrumCfg(SpectrumConfig {
        fft_size: 4096,
        display_bins: bins,
        db_floor: -140.0,
        db_ceil: 0.0,
        viewport: span_hz.map(|s| (TONE_MID_HZ - s / 2.0, TONE_MID_HZ + s / 2.0)),
        fps: 30,
        avg_tc: 0.0,
    })
}

/// The same, on the historic 2048-column panadapter.
fn cfg(span_hz: Option<f64>) -> Command {
    cfg_at(span_hz, 2048)
}

/// Collect frames for `secs` and return the last one whose span matches what
/// was asked for, so a frame still in flight from the previous config cannot be
/// mistaken for the answer.
fn frame_of_span(h: &mut EngineHandles, want_span: f64, secs: f64) -> SpectrumFrame {
    let mut got: Option<SpectrumFrame> = None;
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < deadline {
        if h.spectrum_out.update() {
            let f = h.spectrum_out.output_buffer();
            if (f.span_hz - want_span).abs() < want_span * 0.05 && !f.bins.is_empty() {
                got = Some(f.clone());
            }
        }
        while h.event_rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(5));
    }
    got.unwrap_or_else(|| panic!("no {want_span} Hz frame arrived"))
}

/// Whether this frame shows the pair as *two* signals: a peak at each tone and
/// a trough between them well below both.
///
/// A frame whose columns are wider than the gap cannot, by construction — there
/// is nothing between the two to be a trough — which is exactly the state the
/// zoom lane exists to get out of, so it reports false rather than asserting.
fn resolves_the_pair(f: &SpectrumFrame) -> bool {
    let base = f.center_hz - f.span_hz / 2.0;
    let n = f.bins.len();
    let col = |hz: f64| ((hz - base) / f.span_hz * n as f64).floor() as isize;
    let (a, b) = (col(TONE_A_HZ), col(TONE_B_HZ));
    if b - a < 2 || a < 0 || b >= n as isize {
        return false;
    }
    // The tone need not land dead centre of its column, so look either side.
    let peak = |c: isize| {
        (c - 3..=c + 3)
            .filter_map(|i| usize::try_from(i).ok().and_then(|i| f.bins.get(i)))
            .copied()
            .max()
            .unwrap_or(0)
    };
    let trough = (a + 4..b - 3)
        .filter_map(|i| usize::try_from(i).ok().and_then(|i| f.bins.get(i)))
        .copied()
        .min()
        .unwrap_or(255);
    i32::from(peak(a).min(peak(b))) - i32::from(trough) >= TROUGH_UNITS
}

/// The point of the lane: two tones the device-wide analyser cannot tell apart
/// are two signals on screen once the operator has zoomed in on them.
#[test]
fn a_zoomed_viewport_resolves_what_the_device_wide_fft_cannot() {
    let mut h = engine();
    let thread = h.thread.take();

    // Zoomed out first, to show the pair is genuinely unresolvable there: one
    // bin of the device-wide analyser is five times the gap between them.
    h.cmd_tx.send(cfg(None)).unwrap();
    let wide = frame_of_span(&mut h, RATE, 1.5);
    assert!(
        !resolves_the_pair(&wide),
        "the device-wide analyser should see one blur, not two tones"
    );

    // Now the window the operator would open on them.
    h.cmd_tx.send(cfg(Some(VIEW_HZ))).unwrap();
    let zoomed = frame_of_span(&mut h, VIEW_HZ, 3.0);
    assert!(resolves_the_pair(&zoomed), "the zoomed window should resolve both tones");

    // And the strongest thing on screen is one of them, at the frequency it
    // really is: a mis-signed NCO offset would mirror the pair about the centre
    // and still "resolve" two peaks.
    let (top, _) =
        zoomed.bins.iter().enumerate().max_by_key(|(_, v)| **v).expect("a frame has bins");
    let hz = zoomed.freq_at_bin(top);
    let tol = TONE_GAP_HZ;
    assert!(
        (hz - TONE_MID_HZ).abs() < tol,
        "the pair read at {hz}, expected {TONE_MID_HZ} ± {tol:.0}"
    );

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// The lane is an addition, not a replacement: with no viewport the frame is
/// still the whole of what the front end is streaming, and going back to it
/// from a zoomed window has to restore it.
#[test]
fn the_zoomed_out_view_is_still_the_whole_window() {
    let mut h = engine();
    let thread = h.thread.take();

    h.cmd_tx.send(cfg(Some(VIEW_HZ))).unwrap();
    let zoomed = frame_of_span(&mut h, VIEW_HZ, 2.0);
    assert!((zoomed.center_hz - TONE_MID_HZ).abs() < VIEW_HZ * 0.05);

    h.cmd_tx.send(cfg(None)).unwrap();
    let wide = frame_of_span(&mut h, RATE, 2.0);
    assert!(
        (wide.span_hz - RATE).abs() < 1.0,
        "the zoomed-out frame should span the whole stream, got {}",
        wide.span_hz
    );

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// A viewport the device-wide analyser can still fill is left to it. The lane
/// costs a decimation of every sample and restarts its averaging whenever the
/// window moves, so it may only exist where it buys something: three quarters
/// of the span still gets three quarters of these 4096 bins, more than the 2048
/// columns [`cfg`] asks the frame for.
///
/// In the running application the client grows its own FFT with the zoom, so
/// this covers everything down to about a thirty-second of the span before the
/// lane is reached at all.
#[test]
fn a_shallow_zoom_is_left_to_the_device_wide_analyser() {
    let mut h = engine();
    let thread = h.thread.take();

    let span = RATE * 0.75;
    h.cmd_tx.send(cfg(Some(span))).unwrap();
    let f = frame_of_span(&mut h, span, 2.0);
    // Served from the device-wide analyser the frame is a slice of its bins, so
    // the tones are still one blur. Through a lane they would be two.
    assert!(
        !resolves_the_pair(&f),
        "a three-quarter-span viewport should not have cost a zoom lane"
    );

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// The panadapter's width is the client's to choose, and the lane follows it.
///
/// Two things at once, because they are the same thing: the frame really is as
/// wide as was asked for, and the zoom lane — whose FFT is sized from that
/// width ([`sdroxide_radio::engine`]'s `zoom_lane_fft`) — still resolves the
/// pair at twice the columns. A lane left at the width it had for a
/// 2048-column display would stair-step here, which is the complaint of issue
/// #172 one zoom level further in.
#[test]
fn the_client_chooses_the_panadapter_width_and_the_lane_follows() {
    let mut h = engine();
    let thread = h.thread.take();

    h.cmd_tx.send(cfg_at(Some(VIEW_HZ), 4096)).unwrap();
    let f = frame_of_span(&mut h, VIEW_HZ, 3.0);
    assert_eq!(f.bins.len(), 4096, "the frame should be as wide as the client asked for");
    assert!(resolves_the_pair(&f), "a 4096-column zoomed window should resolve both tones");

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// A width nobody sane would send is not a width the engine tries to serve.
///
/// The number arrives over the network, so the clamp in
/// [`sdroxide_types::SpectrumConfig::bins`] is the only thing between a hostile
/// or broken client and an allocation the size of its imagination.
#[test]
fn an_absurd_width_is_held_to_the_ceiling() {
    let mut h = engine();
    let thread = h.thread.take();

    h.cmd_tx.send(cfg_at(None, u32::MAX)).unwrap();
    let f = frame_of_span(&mut h, RATE, 2.0);
    assert_eq!(f.bins.len(), sdroxide_types::MAX_DISPLAY_BINS as usize);

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}
