//! The QO-100 BEACON window: calibrate the station's frequency chain against
//! Es'hail-2's narrowband beacon.
//!
//! The narrowband transponder carries three beacons — one at each edge and one
//! in the middle — of which [`BEACON_HZ`] is the easiest to pick out: an
//! unmodulated carrier, not a telemetry signal buried in noise. Turning
//! tracking ON tunes there and hunts the strongest bin in view; a double-click
//! on the strip picks one by hand when the automatic search cannot (the
//! signal is too weak, or drift has carried it out of the default window).
//! Either way the difference between where the beacon actually sits and where
//! it *should* — [`BEACON_HZ`] exactly — is the station's whole frequency
//! error: the LNB's real LO plus whatever the SDR's own clock is off by,
//! lumped together the way an operator would correct them by hand. APPLY
//! writes that correction into [`sdroxide_types::RadioConfig::converter_offset_hz`]
//! and reopens the front end — the same round trip Settings ▸ Radio ▸ Apply
//! makes — rather than doing it silently on every frame, so a bad reading
//! never yanks a running receiver.
//!
//! Deliberately its own small waterfall rather than a second
//! [`crate::widgets::spectrum_view`] or a borrowed
//! [`crate::widgets::wide_spectrum::WideWaterfall`]: neither offers a
//! double-click-to-pick gesture, and both are built for the main panadapter's
//! much larger job. This one reads the same [`sdroxide_types::SpectrumFrame`]
//! everything else on screen already gets, cropped to a window around
//! [`BEACON_HZ`].

use std::collections::VecDeque;

use eframe::egui::{
    self, Color32, ColorImage, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, Vec2,
};
use sdroxide_types::{Command, SpectrumFrame, Vfo};

use crate::app::SdroxideApp;
use crate::{colormap, theme};

/// The beacon this window tracks: the upper edge of the QO-100 narrowband
/// transponder. The band-edge and mid-band beacons carry telemetry and are
/// harder to pick a clean centre frequency from; this one is a plain carrier.
pub(in crate::app) const BEACON_HZ: f64 = 10_489_750_000.0;

/// Columns the mini waterfall's history is resampled to — independent of the
/// widget's pixel width, like [`crate::widgets::wide_spectrum`]'s `COLS`.
const COLS: usize = 240;
/// Rows of history kept.
const ROWS: usize = 90;
/// Height of the strip, in points.
const STRIP_H: f32 = 130.0;

/// Half-widths the width buttons step through, in Hz. 5 kHz either side is
/// where most stations land after a first calibration; the wider steps are
/// for finding the beacon at all when the station has drifted further than
/// that or has never been calibrated.
const HALF_WIDTHS: [f64; 6] = [2_000.0, 5_000.0, 10_000.0, 25_000.0, 50_000.0, 100_000.0];
/// Index into [`HALF_WIDTHS`] the window opens with.
const DEFAULT_HW_IDX: usize = 1;

/// How far above the visible slice's own median a bin must stand to count as
/// the beacon rather than noise. A plain carrier stands proud of the noise
/// floor by a lot more than this; the point is only to refuse a flat,
/// beacon-free slice rather than reporting its loudest bin of static.
const SNR_DB: f32 = 6.0;

/// Everything the window remembers between frames.
pub(in crate::app) struct Qo100WinState {
    /// Whether the strip is hunting the beacon each frame.
    pub tracking: bool,
    /// Index into [`HALF_WIDTHS`] — how wide a slice either side of
    /// [`BEACON_HZ`] is shown and searched.
    hw_idx: usize,
    /// The beacon's last-known dial-domain frequency: from the automatic
    /// search, or from a double-click that overrode it.
    measured_hz: Option<f64>,
    /// Whether `measured_hz` came from a double-click. While set, the
    /// automatic search stops overwriting it — a manual pick stands until the
    /// operator clicks again or tracking is switched off.
    manual_pick: bool,
    /// Newest row last, each [`COLS`] bytes of 0..=255 magnitude, resampled
    /// from whatever of the live [`SpectrumFrame`] falls in the current
    /// window.
    rows: VecDeque<Vec<u8>>,
    tex: Option<TextureHandle>,
    last_seq: u32,
    /// The (lo_hz, hi_hz) the stored rows were drawn against — a width change
    /// invalidates them outright rather than trying to rescale history drawn
    /// at a different span.
    window: Option<(f64, f64)>,
    /// The last correction actually written: old offset, new offset, and the
    /// wall-clock second it was applied, so the operator sees what happened
    /// even after the numbers above have moved on.
    applied: Option<(f64, f64, i64)>,
}

impl Default for Qo100WinState {
    fn default() -> Self {
        Self {
            tracking: false,
            hw_idx: DEFAULT_HW_IDX,
            measured_hz: None,
            manual_pick: false,
            rows: VecDeque::new(),
            tex: None,
            last_seq: 0,
            window: None,
            applied: None,
        }
    }
}

fn fmt_hz_signed(hz: f64) -> String {
    if hz.abs() >= 1000.0 { format!("{:+.2} kHz", hz / 1000.0) } else { format!("{hz:+.0} Hz") }
}

/// New [`sdroxide_types::RadioConfig::converter_offset_hz`] that puts the
/// beacon — currently read at `measured_hz` where it should read `target_hz`
/// — exactly on `target_hz`, for the same physical LNB and receiver.
///
/// Derived from the converter's own convention
/// (`sdroxide_radio::converter_open_hz`: `hardware_hz = dial_hz + offset`, so
/// `dial_hz = hardware_hz - offset`). The same physical signal read under two
/// offsets gives `measured_hz - target_hz = new_offset - old_offset`.
pub(in crate::app) fn corrected_offset_hz(old_offset_hz: f64, measured_hz: f64, target_hz: f64) -> f64 {
    old_offset_hz + (measured_hz - target_hz)
}

/// The bin range of `frame` overlapping `[lo_hz, hi_hz)`, clamped to what the
/// frame actually covers. `None` when the frame has no bins, no span, or does
/// not reach the requested window at all.
fn bin_range(frame: &SpectrumFrame, lo_hz: f64, hi_hz: f64) -> Option<std::ops::Range<usize>> {
    let n = frame.bins.len();
    if n == 0 || frame.span_hz <= 0.0 {
        return None;
    }
    let flo = frame.center_hz - frame.span_hz / 2.0;
    let fhi = frame.center_hz + frame.span_hz / 2.0;
    if hi_hz <= flo || lo_hz >= fhi {
        return None;
    }
    let bin_hz = frame.span_hz / n as f64;
    let b0 = (((lo_hz.max(flo) - flo) / bin_hz).floor() as isize).clamp(0, n as isize - 1) as usize;
    let b1 = (((hi_hz.min(fhi) - flo) / bin_hz).ceil() as isize).clamp(1, n as isize) as usize;
    (b1 > b0).then_some(b0..b1)
}

/// The strongest bin of `frame` inside `range`, refined to sub-bin precision
/// by a three-point parabolic fit around it, and how far above the range's
/// own median (in dB) it stands. `None` for an empty range or a peak that
/// does not clear [`SNR_DB`] — a flat slice with nothing in it, most often
/// meaning the beacon has drifted outside the current window.
fn find_peak(frame: &SpectrumFrame, range: std::ops::Range<usize>) -> Option<(f64, f32)> {
    if range.is_empty() {
        return None;
    }
    let bins = &frame.bins[range.clone()];
    let (mut best_i, mut best_v) = (0usize, bins[0]);
    for (i, &v) in bins.iter().enumerate() {
        if v > best_v {
            (best_i, best_v) = (i, v);
        }
    }
    let mut sorted = bins.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let db_span = frame.db_ceil - frame.db_floor;
    let to_db = |v: u8| frame.db_floor + (v as f32 / 255.0) * db_span;
    let snr = to_db(best_v) - to_db(median);
    if snr < SNR_DB {
        return None;
    }

    // Parabolic refinement against the *full* bin array, so a peak at the
    // edge of the search range can still borrow the one neighbour outside it.
    let gi = range.start + best_i;
    let y0 = frame.bins.get(gi.wrapping_sub(1)).copied().unwrap_or(best_v) as f32;
    let y1 = best_v as f32;
    let y2 = frame.bins.get(gi + 1).copied().unwrap_or(best_v) as f32;
    let denom = y0 - 2.0 * y1 + y2;
    let delta = if denom.abs() > f32::EPSILON { (0.5 * (y0 - y2) / denom).clamp(-1.0, 1.0) } else { 0.0 };

    let bin_hz = frame.span_hz / frame.bins.len().max(1) as f64;
    let lo = frame.center_hz - frame.span_hz / 2.0;
    let freq = lo + (gi as f64 + 0.5 + delta as f64) * bin_hz;
    Some((freq, snr))
}

/// Fold a new frame into the history, resampled onto [`COLS`] against the
/// fixed `(lo, hi)` window. A width change clears the history outright rather
/// than rescaling it — cheap, and correct, since nothing here promises a
/// scrolling record of *other* widths.
fn push_row(win: &mut Qo100WinState, frame: &SpectrumFrame, lo: f64, hi: f64) {
    if frame.seq == win.last_seq && win.window == Some((lo, hi)) {
        return;
    }
    if win.window != Some((lo, hi)) {
        win.rows.clear();
        win.window = Some((lo, hi));
    }
    win.last_seq = frame.seq;

    let flo = frame.center_hz - frame.span_hz / 2.0;
    let fhi = frame.center_hz + frame.span_hz / 2.0;
    let n = frame.bins.len();
    let mut row = vec![0u8; COLS];
    if n > 0 && frame.span_hz > 0.0 && hi > flo && lo < fhi {
        let bin_hz = frame.span_hz / n as f64;
        for (c, slot) in row.iter_mut().enumerate() {
            let x0 = lo + (hi - lo) * c as f64 / COLS as f64;
            let x1 = lo + (hi - lo) * (c + 1) as f64 / COLS as f64;
            let (ox0, ox1) = (x0.max(flo), x1.min(fhi));
            if ox1 <= ox0 {
                continue; // this column falls outside what the receiver currently covers
            }
            let b0 = (((ox0 - flo) / bin_hz).floor() as isize).clamp(0, n as isize - 1) as usize;
            let b1 = (((ox1 - flo) / bin_hz).ceil() as isize).clamp(1, n as isize) as usize;
            if b1 > b0 {
                *slot = frame.bins[b0..b1].iter().copied().max().unwrap_or(0);
            }
        }
    }
    win.rows.push_back(row);
    while win.rows.len() > ROWS {
        win.rows.pop_front();
    }
    win.tex = None; // rebuilt on the next draw
}

fn texture<'a>(win: &'a mut Qo100WinState, ctx: &egui::Context, palette: usize) -> Option<&'a TextureHandle> {
    if win.tex.is_none() && !win.rows.is_empty() {
        let lut = colormap::lut(palette);
        // Newest row first, so the strip scrolls downward like every other
        // waterfall in the app.
        let mut pixels = Vec::with_capacity(COLS * win.rows.len());
        for row in win.rows.iter().rev() {
            pixels.extend(row.iter().map(|v| {
                let i = *v as usize * 4;
                Color32::from_rgb(lut[i], lut[i + 1], lut[i + 2])
            }));
        }
        let img = ColorImage::new([COLS, win.rows.len()], pixels);
        win.tex = Some(ctx.load_texture("qo100-waterfall", img, egui::TextureOptions::LINEAR));
    }
    win.tex.as_ref()
}

/// Draw the strip and handle its one gesture — a double-click picks a beacon
/// by hand. Returns the picked frequency, if any.
fn paint_strip(
    ui: &mut egui::Ui,
    win: &mut Qo100WinState,
    frame: Option<&SpectrumFrame>,
    palette: usize,
) -> Option<f64> {
    let (lo, hi) = (BEACON_HZ - HALF_WIDTHS[win.hw_idx], BEACON_HZ + HALF_WIDTHS[win.hw_idx]);
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), STRIP_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return None;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::BLACK);

    if let Some(f) = frame {
        push_row(win, f, lo, hi);
    }
    let ctx = ui.ctx().clone();
    if let Some(tex) = texture(win, &ctx, palette) {
        painter.image(
            tex.id(),
            rect,
            Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    let x_of = |hz: f64| -> f32 {
        rect.left() + ((hz - lo) / (hi - lo)).clamp(0.0, 1.0) as f32 * rect.width()
    };
    // The target: where the beacon belongs.
    let tx = x_of(BEACON_HZ);
    painter.line_segment(
        [Pos2::new(tx, rect.top()), Pos2::new(tx, rect.bottom())],
        Stroke::new(1.0, theme::CYAN()),
    );
    // Where it was actually found (or picked).
    if let Some(m) = win.measured_hz
        && (lo..=hi).contains(&m)
    {
        let mx = x_of(m);
        let colour = if win.manual_pick { theme::YELLOW() } else { theme::GREEN() };
        painter.line_segment(
            [Pos2::new(mx, rect.top()), Pos2::new(mx, rect.bottom())],
            Stroke::new(1.6, colour),
        );
    }

    let font = egui::FontId::monospace(9.0);
    let dim = Color32::from_white_alpha(200);
    painter.text(
        Pos2::new(rect.left() + 3.0, rect.top() + 1.0),
        egui::Align2::LEFT_TOP,
        format!("{:.3} MHz", lo / 1e6),
        font.clone(),
        dim,
    );
    painter.text(
        Pos2::new(rect.right() - 3.0, rect.top() + 1.0),
        egui::Align2::RIGHT_TOP,
        format!("{:.3} MHz", hi / 1e6),
        font,
        dim,
    );
    crate::chrome::paint_cut_border(&painter, rect, theme::LINE_LIT(), theme::PANEL());

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }
    resp.double_clicked()
        .then(|| resp.interact_pointer_pos())
        .flatten()
        .map(|p| lo + ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * (hi - lo))
}

impl SdroxideApp {
    pub(in crate::app) fn qo100_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        if !self.show_qo100 {
            return;
        }
        let mut win = std::mem::take(&mut self.qo100_win);
        let mut open = self.show_qo100;
        let resp = egui::Window::new("QO-100 BEACON")
            .id(crate::layout::salted_id(ctx, "Qo100"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(crate::layout::window_w(ctx, 420.0))
            .show(ctx, |ui| {
                crate::chrome::window_body_bg(ui);
                self.qo100_body(ui, &mut win, cmds)
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_qo100 = open;
        self.qo100_win = win;
    }

    fn qo100_body(&mut self, ui: &mut egui::Ui, win: &mut Qo100WinState, cmds: &mut Vec<Command>) {
        let reachable = self.caps.as_ref().is_none_or(|c| c.can_rx_hz(BEACON_HZ));
        let frame = self.frame.clone();
        let (lo, hi) = (BEACON_HZ - HALF_WIDTHS[win.hw_idx], BEACON_HZ + HALF_WIDTHS[win.hw_idx]);
        // Whether whatever the receiver is *actually* capturing right now
        // reaches anywhere near the beacon at all — as against `reachable`,
        // which asks whether it ever could. A capable receiver still shows a
        // blank strip while it is parked on 144 MHz, and that is the second
        // most likely reason (after no converter at all) this window opens on
        // an empty waterfall.
        let in_view = frame.as_deref().is_some_and(|f| bin_range(f, lo, hi).is_some());
        let dial_hz = self.state.active_freq_hz();

        ui.horizontal(|ui| {
            let run = crate::chrome::chip_enabled(
                ui,
                reachable,
                win.tracking,
                if win.tracking { "ON" } else { "OFF" },
            );
            if run.clicked() {
                win.tracking = !win.tracking;
                if win.tracking {
                    cmds.push(Command::SetVfo { vfo: Vfo::A, hz: BEACON_HZ });
                } else {
                    win.manual_pick = false;
                }
            }
            if !reachable {
                run.on_hover_text(
                    "This receiver cannot reach 10489.750 MHz on its own — set up an \
                     LNB/converter offset first (Settings ▸ Radio)",
                );
            } else {
                run.on_hover_text("Tune to the beacon and hunt its centre frequency");
            }

            ui.add_space(8.0);
            ui.label(RichText::new("width").size(10.0).color(theme::CYAN_DIM()));
            if ui.small_button("−").on_hover_text("Narrower — search a smaller slice").clicked() {
                win.hw_idx = win.hw_idx.saturating_sub(1);
            }
            ui.label(
                RichText::new(format!("±{:.0} kHz", HALF_WIDTHS[win.hw_idx] / 1000.0))
                    .size(11.0)
                    .monospace(),
            );
            if ui
                .small_button("+")
                .on_hover_text("Wider — for when the beacon isn't visible at the current width")
                .clicked()
            {
                win.hw_idx = (win.hw_idx + 1).min(HALF_WIDTHS.len() - 1);
            }
        });

        // Unmissable, not just a hover: a fresh station has no converter set
        // up yet, so ON stays disabled and the strip would otherwise sit
        // there blank with no clue why — the single most likely reason
        // anyone ever opens this window and sees nothing.
        if !reachable {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(
                        "This radio's own tuning range doesn't reach 10489.750 MHz — it needs an \
                         LNB/converter offset (Settings ▸ Radio ▸ Converter) before this window can \
                         do anything.",
                    )
                    .size(10.5)
                    .color(theme::YELLOW()),
                );
                if ui.small_button("Open Settings ▸ Radio").clicked() {
                    self.open_radio_settings();
                }
            });
        }

        // The receiver *can* reach the beacon but currently is not — parked
        // on some other band, most often. The strip has nothing to show
        // either way, but here the fix is one click rather than a trip to
        // Settings, and worth naming exactly where the dial actually is.
        if reachable && !in_view {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "The receiver is currently listening around {:.6} MHz — nowhere near the \
                         10489.750 MHz beacon, so there is nothing here to show.",
                        dial_hz / 1e6
                    ))
                    .size(10.5)
                    .color(theme::YELLOW()),
                );
                if ui.small_button("Tune to 10489.750 MHz").clicked() {
                    cmds.push(Command::SetVfo { vfo: Vfo::A, hz: BEACON_HZ });
                }
            });
        }

        if let Some(f) = self.frame.as_ref() {
            let capped = f.span_hz / 2.0 < HALF_WIDTHS[win.hw_idx];
            if capped && f.span_hz > 0.0 {
                ui.label(
                    RichText::new(format!(
                        "receiver currently covers only ±{:.0} kHz here — the rest of the strip stays blank",
                        f.span_hz / 2e3
                    ))
                    .size(9.5)
                    .color(theme::YELLOW()),
                );
            }
        }

        ui.add_space(4.0);
        let picked = paint_strip(ui, win, frame.as_deref(), self.ui_settings.waterfall_palette);
        if let Some(hz) = picked {
            win.measured_hz = Some(hz);
            win.manual_pick = true;
        } else if win.tracking && !win.manual_pick {
            win.measured_hz = frame
                .as_deref()
                .and_then(|f| bin_range(f, lo, hi).and_then(|r| find_peak(f, r)))
                .map(|(hz, _)| hz);
        } else if !win.tracking {
            win.measured_hz = None;
        }

        ui.add_space(6.0);
        ui.label(
            RichText::new(if win.manual_pick { "picked by hand — double-click again to repick" } else { "" })
                .size(9.5)
                .color(theme::CYAN_DIM()),
        );

        let cfg = self.ctrl.radio_config();
        let old_offset = cfg.as_ref().map(|c| c.converter_offset_hz).unwrap_or(0.0);

        egui::Grid::new("qo100-grid").num_columns(2).spacing([16.0, 3.0]).show(ui, |ui| {
            let dim = |s: &str| RichText::new(s).size(9.5).color(theme::CYAN_DIM());
            // What the receiver is actually listening to right now — the
            // direct answer to "why is the strip empty", always on screen
            // rather than only when it explains a problem.
            ui.label(dim("RECEIVER"));
            ui.label(
                RichText::new(format!("{:.6} MHz", dial_hz / 1e6))
                    .size(11.0)
                    .monospace()
                    .color(if in_view { theme::TEXT() } else { theme::YELLOW() }),
            );
            ui.end_row();

            ui.label(dim("TARGET"));
            ui.label(RichText::new(format!("{:.6} MHz", BEACON_HZ / 1e6)).size(12.0).monospace());
            ui.end_row();

            ui.label(dim("MEASURED"));
            match win.measured_hz {
                Some(hz) => ui.label(
                    RichText::new(format!("{:.6} MHz", hz / 1e6)).size(12.0).monospace().strong(),
                ),
                None => ui.label(
                    RichText::new(if win.tracking { "not found — try a wider width" } else { "—" })
                        .size(11.0)
                        .color(theme::CYAN_DIM()),
                ),
            };
            ui.end_row();

            ui.label(dim("DRIFT"));
            match win.measured_hz {
                Some(hz) => {
                    let err = hz - BEACON_HZ;
                    let colour = if err.abs() < 200.0 {
                        theme::GREEN()
                    } else if err.abs() < 3000.0 {
                        theme::YELLOW()
                    } else {
                        theme::TEXT()
                    };
                    ui.label(RichText::new(fmt_hz_signed(err)).size(12.0).monospace().color(colour))
                }
                None => ui.label(RichText::new("—").size(11.0).color(theme::CYAN_DIM())),
            };
            ui.end_row();

            ui.label(dim("CONVERTER OFFSET"));
            ui.label(RichText::new(format!("{old_offset:.0} Hz")).size(11.0).monospace());
            ui.end_row();
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_apply = win.measured_hz.is_some() && cfg.is_some();
            let apply = ui.add_enabled(
                can_apply,
                egui::Button::new(RichText::new(" APPLY CORRECTION ").strong()),
            );
            let apply = apply.on_hover_text(
                "Write the corrected converter/LNB offset and reopen the receiver — a brief \
                 interruption, the same one Settings ▸ Radio ▸ Apply makes",
            );
            if apply.clicked()
                && let (Some(mut c), Some(measured)) = (cfg.clone(), win.measured_hz)
            {
                let new_offset = corrected_offset_hz(c.converter_offset_hz, measured, BEACON_HZ);
                c.converter_offset_hz = new_offset;
                self.ctrl.set_radio_config(c.clone());
                self.ctrl.reopen_source();
                self.radio_cfg = Some(c);
                win.applied = Some((old_offset, new_offset, crate::time::now_unix()));
                win.manual_pick = false;
            }
            if let Some((old, new, at)) = win.applied {
                let ago = crate::time::now_unix() - at;
                ui.label(
                    RichText::new(format!(
                        "last applied {ago}s ago: {old:.0} → {new:.0} Hz ({:+.0} Hz)",
                        new - old
                    ))
                    .size(9.5)
                    .color(theme::CYAN_DIM()),
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_beacon_read_high_needs_the_offset_raised() {
        // Beacon really sits on BEACON_HZ but reads 5 kHz high — the LNB's LO
        // is 5 kHz low, which the software currently under-subtracts.
        let old = -9_750_000_000.0;
        let measured = BEACON_HZ + 5_000.0;
        assert_eq!(corrected_offset_hz(old, measured, BEACON_HZ), old + 5_000.0);
    }

    #[test]
    fn a_beacon_read_low_needs_the_offset_lowered() {
        let old = -9_750_000_000.0;
        let measured = BEACON_HZ - 1_200.0;
        assert_eq!(corrected_offset_hz(old, measured, BEACON_HZ), old - 1_200.0);
    }

    #[test]
    fn a_correctly_calibrated_station_gets_the_same_offset_back() {
        let old = -9_750_000_000.0;
        assert_eq!(corrected_offset_hz(old, BEACON_HZ, BEACON_HZ), old);
    }

    fn frame_with(center_hz: f64, span_hz: f64, bins: Vec<u8>) -> SpectrumFrame {
        SpectrumFrame { seq: 1, center_hz, span_hz, db_floor: -120.0, db_ceil: -20.0, bins }
    }

    #[test]
    fn a_flat_peak_lands_on_its_own_bin_centre() {
        // A symmetric peak (equal neighbours) has no reason to shift either
        // way: the parabola through equal shoulders is flat at the top.
        let mut bins = vec![20u8; 64];
        bins[32] = 200;
        bins[31] = 120;
        bins[33] = 120;
        let frame = frame_with(10_489_750_000.0, 640_000.0, bins);
        let (hz, snr) = find_peak(&frame, 0..64).expect("clears SNR_DB");
        assert!(snr > SNR_DB);
        let expected = frame.freq_at_bin(32);
        assert!((hz - expected).abs() < 1.0, "expected {expected}, got {hz}");
    }

    #[test]
    fn a_lopsided_peak_is_pulled_toward_its_stronger_shoulder() {
        let mut bins = vec![20u8; 64];
        bins[32] = 200;
        bins[31] = 100;
        bins[33] = 160; // stronger on the high side — true peak sits above bin 32
        let frame = frame_with(10_489_750_000.0, 640_000.0, bins);
        let (hz, _) = find_peak(&frame, 0..64).expect("clears SNR_DB");
        assert!(hz > frame.freq_at_bin(32));
    }

    #[test]
    fn a_flat_noise_floor_reports_no_beacon() {
        let bins = vec![40u8; 64];
        let frame = frame_with(10_489_750_000.0, 640_000.0, bins);
        assert!(find_peak(&frame, 0..64).is_none());
    }

    #[test]
    fn bin_range_refuses_a_window_the_frame_never_reaches() {
        let frame = frame_with(14_000_000.0, 100_000.0, vec![0u8; 64]);
        assert!(bin_range(&frame, BEACON_HZ - 5_000.0, BEACON_HZ + 5_000.0).is_none());
    }

    #[test]
    fn bin_range_clamps_to_what_the_frame_actually_covers() {
        // Requested window straddles the frame's edge; the range returned
        // must stay inside 0..bins.len().
        let frame = frame_with(BEACON_HZ + 3_000.0, 10_000.0, vec![0u8; 100]);
        let r = bin_range(&frame, BEACON_HZ - 5_000.0, BEACON_HZ + 5_000.0).expect("overlaps");
        assert!(r.end <= 100);
    }
}
