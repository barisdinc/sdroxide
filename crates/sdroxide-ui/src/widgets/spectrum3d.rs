//! The spectrum strip drawn as a receding surface: the newest spectrum across
//! the front, the ones before it flowing away from the viewer.
//!
//! The same numbers the flat trace is drawn from, with time given the depth
//! axis instead of being thrown away — which is what makes a carrier that
//! comes and goes read as a ridge rather than as a line that twitches. The
//! waterfall says the same thing in colour; this says it in shape, and a weak
//! signal a couple of dB over the floor is a bump you can see the length of.
//!
//! Everything here is plain egui geometry — a mesh per row and, in the line
//! rendering, a polyline over it. Deliberately: the waterfall's history is a
//! wgpu texture behind a paint callback, and a second GPU path would have to
//! be written twice over (native and WebGL) and would put the browser client's
//! panadapter on a lane the native one does not use. A few thousand vertices a
//! frame costs less than that, and costs the same everywhere.
//!
//! Hidden surface removal is the painter's algorithm and nothing else: rows go
//! down back to front, each filled from its own curve to its own baseline, so
//! a near ridge covers the far ones behind it and a flat near row leaves them
//! showing over the top. That is why the fills are opaque even in the line
//! rendering, where the fill is the background colour and its whole job is to
//! be something for the lines behind to disappear into.

use std::collections::VecDeque;

use eframe::egui::{self, Align2, Color32, FontId, Rect, Shape, Stroke, pos2};

use crate::view::ViewState;

/// Spectra the surface remembers, and so the depth of the picture. One is kept
/// per *frame* the engine publishes, so the time this covers is the operator's
/// spectrum frame rate: about two seconds at 30 fps, one at 60. Time is not
/// labelled on this axis and does not need to be — the depth is there to show
/// how a signal is changing, and the waterfall next to it is the instrument
/// with a clock on it.
pub const DEPTH: usize = 64;

/// Fraction of the strip's height the receding axis takes; the rest is the
/// amplitude the newest row is drawn at.
const DEPTH_FRAC: f32 = 0.42;

/// How hard the perspective narrows: the oldest row comes out `1/(1 + PERSP)`
/// as wide as the newest, and its amplitude is foreshortened by the same
/// factor, which is what makes the rows sit on one another rather than beside
/// one another.
const PERSP: f32 = 0.85;

/// How far the far end of the surface is dimmed towards the background, so
/// depth reads as depth and not as a signal that got weaker.
const FOG: f32 = 0.55;

/// One remembered spectrum, with the window it was taken over.
///
/// Its own centre and span rather than an index into the current view: a
/// retune, a pan or a zoom changes where these bins belong on screen, and a
/// row that carried only its samples would be redrawn as if it had been
/// measured on the band that is on screen now. Keeping the axis with the
/// samples means an older row simply slides — and runs off the edge, reading
/// as the floor — exactly as it should.
#[derive(Default)]
struct Row {
    center_hz: f64,
    span_hz: f64,
    /// Levels in the frame's own u8 mapping over `[db_floor, db_ceil]`, the
    /// same scale the flat trace and the waterfall are drawn from.
    bins: Vec<u8>,
}

/// The remembered spectra, newest last.
#[derive(Default)]
pub struct Surface {
    rows: VecDeque<Row>,
    /// Sequence of the frame last folded in, so a repaint that carries no new
    /// spectrum does not advance the surface — and so a radio drawn in two
    /// panes of a split view advances it once, not twice.
    last_seq: Option<u32>,
}

impl Surface {
    /// Remember one spectrum. `values` are the UI-smoothed bins the flat trace
    /// would have been drawn from, so the *reaction* setting steadies this
    /// surface exactly as it steadies the line.
    pub fn push(&mut self, center_hz: f64, span_hz: f64, seq: u32, values: &[f32]) {
        if self.last_seq == Some(seq) || values.is_empty() || span_hz <= 0.0 {
            return;
        }
        self.last_seq = Some(seq);
        // Reuse the oldest row's allocation rather than freeing and asking for
        // it again a few kilobytes at a time, sixty times a second.
        let mut row = if self.rows.len() >= DEPTH {
            self.rows.pop_front().unwrap_or_default()
        } else {
            Row::default()
        };
        row.center_hz = center_hz;
        row.span_hz = span_hz;
        row.bins.clear();
        row.bins.extend(values.iter().map(|v| v.clamp(0.0, 255.0) as u8));
        self.rows.push_back(row);
    }

    /// Forget everything. Called while the flat trace is the one being drawn:
    /// coming back to the surface with minutes-old rows still in it would draw
    /// a band that has since been left as though it were the last two seconds,
    /// which is the same trap the full-band strip clears its history for.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.last_seq = None;
    }
}

/// The height the newest row is drawn in: the strip's amplitude axis, from the
/// bottom edge upwards. What the markers that annotate the *current* spectrum
/// — the passband, the filter edges, the tuning lines — are kept inside, so
/// they mark the front plane rather than cutting through the rows behind it.
pub fn front_plane_h(strip_h: f32) -> f32 {
    strip_h * (1.0 - DEPTH_FRAC)
}

/// One-point perspective onto the strip: the newest row across the front at
/// full width, the vanishing point centred above it.
struct Proj {
    cx: f32,
    front_y: f32,
    depth_h: f32,
    amp_h: f32,
}

impl Proj {
    fn new(rect: &Rect) -> Self {
        Proj {
            cx: rect.center().x,
            front_y: rect.bottom(),
            depth_h: rect.height() * DEPTH_FRAC,
            amp_h: front_plane_h(rect.height()),
        }
    }

    /// Foreshortening at depth `d` — 0 the newest row, 1 the oldest.
    fn scale(&self, d: f32) -> f32 {
        1.0 / (1.0 + PERSP * d)
    }

    /// The baseline the row at depth `d` stands on. Spaced by the same
    /// division the width is, so the rows crowd together towards the back the
    /// way the columns narrow.
    fn base_y(&self, d: f32) -> f32 {
        let far = 1.0 / (1.0 + PERSP);
        self.front_y - self.depth_h * (1.0 - self.scale(d)) / (1.0 - far)
    }

    /// Where a point that would be at `x_lin` on the front plane lands on the
    /// plane at foreshortening `s`.
    fn x(&self, x_lin: f32, s: f32) -> f32 {
        self.cx + (x_lin - self.cx) * s
    }
}

/// `c` dimmed towards black by `f` (1 leaves it alone, 0 blacks it out).
fn dim(c: [u8; 3], f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0);
    Color32::from_rgb((c[0] as f32 * f) as u8, (c[1] as f32 * f) as u8, (c[2] as f32 * f) as u8)
}

/// The floor the rows stand on: the frequency gridlines converging on the
/// vanishing point, the back edge, and the two sides. Drawn before the rows,
/// so a ridge in front covers the floor behind it.
fn draw_floor(painter: &egui::Painter, view: &ViewState, rect: &Rect, p: &Proj) {
    let grid = Stroke::new(0.5, crate::theme::scope_gray(38));
    let s_far = p.scale(1.0);
    let back_y = p.base_y(1.0);
    let step = super::spectrum_view::freq_grid_step_for_width(
        view.view_lo_hz,
        view.view_hi_hz,
        rect.width(),
    );
    for hz in super::spectrum_view::gridlines_at(view.view_lo_hz, view.view_hi_hz, step) {
        let x_lin = view.freq_to_x(hz, rect);
        painter.line_segment(
            [pos2(p.x(x_lin, 1.0), p.front_y), pos2(p.x(x_lin, s_far), back_y)],
            grid,
        );
    }
    // The far edge and the two rails, which are what say the floor is a plane
    // and not a fan of unrelated lines.
    let edge = Stroke::new(0.5, crate::theme::scope_gray(52));
    let (bl, br) = (p.x(rect.left(), s_far), p.x(rect.right(), s_far));
    painter.line_segment([pos2(bl, back_y), pos2(br, back_y)], edge);
    painter.line_segment([pos2(rect.left(), p.front_y), pos2(bl, back_y)], edge);
    painter.line_segment([pos2(rect.right(), p.front_y), pos2(br, back_y)], edge);

    // The amplitude axis, on the front plane where the newest row is drawn at
    // full height. Every 20 dB of the display range, as the flat grid does.
    let range = view.db_ceil - view.db_floor;
    if range > 1.0 {
        let mut db = (view.db_floor / 20.0).ceil() * 20.0;
        while db < view.db_ceil {
            let y = p.front_y - (db - view.db_floor) / range * p.amp_h;
            painter.line_segment([pos2(rect.left(), y), pos2(rect.left() + 5.0, y)], grid);
            painter.text(
                pos2(rect.left() + 7.0, y),
                Align2::LEFT_CENTER,
                format!("{db:.0}"),
                FontId::monospace(9.0 * crate::theme::panadapter_font_scale()),
                crate::theme::scope_gray(100),
            );
            db += 20.0;
        }
    }
}

/// Draw the surface into `rect`.
///
/// `solid` picks the rendering: a filled surface coloured by the waterfall
/// palette `palette`, or traces over an opaque fill. `palette` is ignored by
/// the line rendering, which draws every row in the flat trace's own colour so
/// that switching between the two displays does not also switch the ink.
pub fn draw(
    painter: &egui::Painter,
    rect: &Rect,
    view: &ViewState,
    surface: &Surface,
    solid: bool,
    palette: usize,
) {
    if rect.height() < 24.0 || rect.width() < 16.0 {
        return;
    }
    let p = Proj::new(rect);
    draw_floor(painter, view, rect, &p);
    if surface.rows.is_empty() {
        return;
    }

    // One vertex every couple of points across, and one row every couple of
    // points down the depth axis. Not per pixel, unlike the flat trace: the
    // rows are stacked a point or two apart, so detail past this is drawn on
    // top of itself, and the vertex count is paid on every row rather than
    // once. The whole surface stays a few thousand vertices, which is what
    // keeps it the same widget in a browser as on a desktop.
    let cols = ((rect.width() * 0.5).round() as usize).clamp(48, 240);
    let want = ((p.depth_h * 0.5).round() as usize).clamp(12, DEPTH);
    let stride = DEPTH.div_ceil(want).max(1);

    // The screen x and the frequency of each column, worked out once for the
    // whole surface: they are the same for every row (the rows differ only in
    // where they stand and how wide they are drawn), and `x_to_freq` is f64.
    let mut xs = Vec::with_capacity(cols);
    let mut hzs = Vec::with_capacity(cols);
    let last = (cols - 1) as f32;
    for c in 0..cols {
        let x = rect.left() + rect.width() * c as f32 / last;
        xs.push(x);
        hzs.push(view.x_to_freq(x, rect));
    }

    let lut = crate::colormap::lut(palette);
    let newest = surface.rows.len() - 1;
    // Oldest drawn row first: the painter's algorithm is the depth buffer.
    let mut drawn: Vec<usize> = (0..surface.rows.len()).rev().step_by(stride).collect();
    drawn.reverse();
    let mut line = Vec::with_capacity(cols);

    for i in drawn {
        let row = &surface.rows[i];
        let n = row.bins.len();
        if n == 0 || row.span_hz <= 0.0 {
            continue;
        }
        let d = ((newest - i) as f32 / (DEPTH - 1) as f32).min(1.0);
        let s = p.scale(d);
        let base_y = p.base_y(d);
        let fog = 1.0 - FOG * d;
        let lo = row.center_hz - row.span_hz / 2.0;

        let mut mesh = egui::epaint::Mesh::default();
        mesh.vertices.reserve(cols * 2);
        mesh.indices.reserve((cols - 1) * 6);
        line.clear();
        for c in 0..cols {
            let bin = (hzs[c] - lo) / row.span_hz * n as f64;
            // Off the end of the window this row was taken over: the floor,
            // which is what the band outside a frame's own span always reads
            // as (the flat trace does the same).
            let raw = if (0.0..n as f64).contains(&bin) { row.bins[bin as usize] } else { 0 };
            let v = raw as f32 / 255.0;
            let x = p.x(xs[c], s);
            let top = pos2(x, base_y - v * p.amp_h * s);
            let (c_top, c_base) = if solid {
                let rgb =
                    [lut[raw as usize * 4], lut[raw as usize * 4 + 1], lut[raw as usize * 4 + 2]];
                // The curtain under each column fades towards black, which is
                // what separates one row from the row behind without a line
                // drawn between them.
                (dim(rgb, fog), dim(rgb, fog * 0.3))
            } else {
                // The background, lifting a shade towards the back: the fill
                // is here to hide what is behind it, and the lift is the only
                // thing telling a far row's blank from a near one's.
                let g = (6.0 + 14.0 * d) as u8;
                let c = Color32::from_gray(g);
                (c, c)
            };
            let uv = egui::epaint::WHITE_UV;
            mesh.vertices.push(egui::epaint::Vertex { pos: top, uv, color: c_top });
            mesh.vertices.push(egui::epaint::Vertex { pos: pos2(x, base_y), uv, color: c_base });
            if !solid {
                line.push(top);
            }
        }
        for c in 0..cols - 1 {
            let a = (c * 2) as u32;
            mesh.indices.extend_from_slice(&[a, a + 1, a + 2, a + 2, a + 1, a + 3]);
        }
        painter.add(Shape::mesh(mesh));
        if !solid {
            let c = TRACE;
            painter.add(Shape::line(
                line.clone(),
                Stroke::new(1.0, dim([c.r(), c.g(), c.b()], 0.35 + 0.65 * fog)),
            ));
        }
    }
}

/// The flat trace's own colour, so the line rendering reads as that trace
/// given a depth axis rather than as a different instrument.
const TRACE: Color32 = Color32::from_rgb(120, 220, 255);

#[cfg(test)]
mod tests {
    use super::*;

    fn surface_of(rows: usize) -> Surface {
        let mut s = Surface::default();
        for i in 0..rows {
            s.push(14_100_000.0, 100_000.0, i as u32, &[10.0, 200.0, 30.0]);
        }
        s
    }

    /// The history is a ring: it fills to [`DEPTH`] and then drops the oldest,
    /// rather than growing for as long as the radio is on.
    #[test]
    fn the_surface_keeps_a_fixed_depth() {
        let s = surface_of(DEPTH * 3);
        assert_eq!(s.rows.len(), DEPTH);
        assert_eq!(s.last_seq, Some((DEPTH * 3 - 1) as u32));
    }

    /// A repaint carrying the frame already folded in must not advance the
    /// surface — nor must the second pane of a split view, which draws the
    /// same radio from the same state a second time in the same frame.
    #[test]
    fn a_redrawn_frame_does_not_advance_the_surface() {
        let mut s = Surface::default();
        s.push(14_100_000.0, 100_000.0, 7, &[1.0, 2.0]);
        s.push(14_100_000.0, 100_000.0, 7, &[1.0, 2.0]);
        assert_eq!(s.rows.len(), 1, "the same spectrum was remembered twice");
    }

    /// Clearing has to reset the seq as well, or the first frame after coming
    /// back to the surface is dropped as one already seen.
    #[test]
    fn a_cleared_surface_takes_the_frame_it_last_saw() {
        let mut s = Surface::default();
        s.push(14_100_000.0, 100_000.0, 9, &[1.0]);
        s.clear();
        s.push(14_100_000.0, 100_000.0, 9, &[1.0]);
        assert_eq!(s.rows.len(), 1);
    }

    /// The front row is drawn at full width on the bottom edge, and the oldest
    /// stands a depth's height above it, narrowed. Everything the projection
    /// is for, asserted at its two ends.
    #[test]
    fn the_newest_row_is_across_the_front() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let p = Proj::new(&rect);
        assert_eq!(p.scale(0.0), 1.0);
        assert_eq!(p.base_y(0.0), rect.bottom());
        assert_eq!(p.x(rect.left(), 1.0), rect.left());
        assert_eq!(p.x(rect.right(), 1.0), rect.right());

        let far = p.scale(1.0);
        assert!(far < 1.0, "the oldest row is not foreshortened");
        assert!((p.base_y(1.0) - (rect.bottom() - rect.height() * DEPTH_FRAC)).abs() < 1e-3);
        assert!(p.x(rect.left(), far) > rect.left(), "the back edge is not inset");
        assert!(p.x(rect.right(), far) < rect.right(), "the back edge is not inset");
    }

    /// The rows must never reach past the strip they are drawn in: a full-scale
    /// signal on the oldest row is the highest anything can go.
    #[test]
    fn a_full_scale_signal_stays_inside_the_strip() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(300.0, 120.0));
        let p = Proj::new(&rect);
        for step in 0..=10 {
            let d = step as f32 / 10.0;
            let top = p.base_y(d) - p.amp_h * p.scale(d);
            assert!(top >= rect.top() - 0.001, "depth {d} drew above the strip: {top}");
            assert!(top <= rect.bottom(), "depth {d} drew below the strip: {top}");
        }
    }
}
