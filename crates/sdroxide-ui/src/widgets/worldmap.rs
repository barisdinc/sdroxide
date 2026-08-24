//! A small pixel/dot-matrix world map for the FT8 QSO panel: renders the
//! continents as glowing dots and marks the home + DX locations with a
//! great-circle path between them, cyberpunk style.
//!
//! The map is centred on the operator's home grid (the world wraps around it
//! rather than being shifted) and smoothly auto-zooms to frame home plus every
//! decoded station, re-fitting whenever new stations appear.
//!
//! The auto-fit is a starting point, not a cage: drag (or one finger) pans,
//! wheel and pinch zoom about the pointer, and a double-click hands the view
//! back to the auto-fit.

use eframe::egui::{
    Align2, Color32, CursorIcon, FontId, PointerButton, Pos2, Response, Sense, Ui, Vec2, pos2, vec2,
};
use sdroxide_types::{great_circle_points, land_cell, land_mask_dims};

use crate::theme;

/// Below this height the map is not worth drawing — the caller should omit it
/// entirely so the QSO controls keep the space.
pub const MIN_HEIGHT: f32 = 72.0;

/// Never zoom tighter than this longitudinal span (degrees), so a single nearby
/// contact doesn't blow the map up to street level.
const MIN_LON_SPAN: f64 = 30.0;
/// The floor for a zoom the user asked for by hand — lower than the auto-fit's,
/// because "show me that corner of Europe" is a real request, but not unlimited:
/// below this the land bitmap (1/6° per cell) is showing its own pixels.
const MIN_USER_LON_SPAN: f64 = 5.0;
/// Fraction of extra margin left around the outermost contact.
const PAD: f64 = 1.4;
/// Per-frame ease toward the target view (0..1); smaller = slower/smoother.
const EASE: f64 = 0.0375;

/// Persistent, animated view state (centre + longitudinal span, in degrees).
/// Owned by the caller so the zoom eases across frames.
pub struct MapView {
    pub(crate) clat: f64,
    pub(crate) clon: f64,
    pub(crate) lon_span: f64,
    pub(crate) initialized: bool,
    /// The user has panned or zoomed by hand: the auto-fit stops moving the
    /// view under them until they double-click to hand it back.
    pub(crate) manual: bool,
}

impl Default for MapView {
    fn default() -> Self {
        MapView { clat: 20.0, clon: 0.0, lon_span: 360.0, initialized: false, manual: false }
    }
}

impl MapView {
    /// The latitude window this zoom covers on a map of the given aspect
    /// (height/width) — the projection is linear in both axes.
    pub(crate) fn lat_span(&self, aspect: f64) -> f64 {
        self.lon_span * aspect
    }

    /// Keep the view legal: the zoom inside its limits, and the latitude window
    /// inside the poles so a pan cannot drift off into empty space above the
    /// map. Longitude wraps instead of clamping — the world repeats sideways.
    pub(crate) fn clamp(&mut self, aspect: f64) {
        self.lon_span = self.lon_span.clamp(MIN_USER_LON_SPAN, 360.0);
        let lat_span = self.lat_span(aspect);
        self.clat = if lat_span >= 180.0 {
            0.0
        } else {
            self.clat.clamp(-90.0 + lat_span / 2.0, 90.0 - lat_span / 2.0)
        };
        self.clon = wrap180(self.clon);
    }

    /// Drag the map by a fraction of its own size, grab-the-content sense: the
    /// land under the pointer stays under it.
    fn pan(&mut self, dx_frac: f64, dy_frac: f64, aspect: f64) {
        self.clon = wrap180(self.clon - dx_frac * self.lon_span);
        self.clat += dy_frac * self.lat_span(aspect);
        self.clamp(aspect);
    }

    /// Put a place in the middle and hold it there.
    ///
    /// Holding it is the point: the auto-fit frames everything on the map,
    /// which is the opposite of what somebody who asked to centre on one
    /// station wants. A double-click hands the view back.
    pub(crate) fn centre_on(&mut self, lat: f64, lon: f64) {
        self.clat = lat;
        self.clon = wrap180(lon);
        self.initialized = true;
        self.manual = true;
    }

    /// Zoom by `factor` (below 1 zooms *in*) about a point given as a fraction
    /// of the map rect — (0,0) top-left, (1,1) bottom-right — keeping whatever
    /// is under that point in place.
    fn zoom_about(&mut self, factor: f64, fx: f64, fy: f64, aspect: f64) {
        // Where the anchor sits relative to the centre, in view fractions, and
        // the place it is currently over.
        let (ax, ay) = (fx - 0.5, 0.5 - fy);
        let lon_a = self.clon + ax * self.lon_span;
        let lat_a = self.clat + ay * self.lat_span(aspect);
        self.lon_span = (self.lon_span * factor).clamp(MIN_USER_LON_SPAN, 360.0);
        // Re-centre so that place lands back under the same fraction.
        self.clon = wrap180(lon_a - ax * self.lon_span);
        self.clat = lat_a - ay * self.lat_span(aspect);
        self.clamp(aspect);
    }
}

/// `c` at `a`/255 opacity — the halo behind a marker, or a station dot faded
/// by age. Takes the alpha as a float because every caller is scaling one.
pub(crate) fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a.clamp(0.0, 255.0) as u8)
}

/// Wrap a longitude delta into [-180, 180).
pub(crate) fn wrap180(mut d: f64) -> f64 {
    d = (d + 180.0).rem_euclid(360.0) - 180.0;
    d
}

/// Centre of a set of points, unwrapping longitude around the first point.
fn centroid(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    if pts.is_empty() {
        return None;
    }
    let n = pts.len() as f64;
    let lat = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let lon_ref = pts[0].1;
    let dlon = pts.iter().map(|p| wrap180(p.1 - lon_ref)).sum::<f64>() / n;
    Some((lat, wrap180(lon_ref + dlon)))
}

/// Mouse and touch control of the view: drag (or one finger) pans, wheel and
/// pinch zoom about the pointer, double-click hands the view back to the
/// auto-fit. Returns true while the user is actually moving it, so the caller
/// can keep the frames coming.
pub(crate) fn interact(ui: &Ui, view: &mut MapView, resp: &Response, aspect: f64) -> bool {
    let rect = resp.rect;
    let mut touched = false;
    let frac = |p: Pos2| {
        (((p.x - rect.left()) / rect.width()) as f64, ((p.y - rect.top()) / rect.height()) as f64)
    };

    // Two fingers zoom and slide the map: on a screen with no wheel there is no
    // other way in.
    //
    // First in the chain, and it swallows the drag, for the reason the
    // panadapter's pinch does: the browser keeps reporting a one-finger drag
    // from the *first* finger down for the whole gesture, so without this a
    // pinch would pan the map at the same time as it zoomed.
    let multi = ui.input(|i| i.multi_touch());
    let pinch = multi.filter(|mt| rect.contains(mt.center_pos));
    if let Some(mt) = pinch {
        if mt.zoom_delta > 0.0 && mt.zoom_delta != 1.0 {
            // Fingers apart is zoom *in*, i.e. a smaller span.
            let (fx, fy) = frac(mt.center_pos);
            view.zoom_about(1.0 / mt.zoom_delta as f64, fx, fy, aspect);
            touched = true;
        }
        let t = mt.translation_delta;
        if t != Vec2::ZERO {
            view.pan((t.x / rect.width()) as f64, (t.y / rect.height()) as f64, aspect);
            touched = true;
        }
    } else if resp.dragged_by(PointerButton::Primary) {
        let d = resp.drag_delta();
        if d != Vec2::ZERO {
            view.pan((d.x / rect.width()) as f64, (d.y / rect.height()) as f64, aspect);
            touched = true;
        }
    }

    // Wheel zoom about the pointer. `zoom_delta` carries a trackpad pinch and
    // ctrl+wheel, which egui reports as a zoom factor rather than as scroll —
    // but it also mirrors a two-finger pinch anywhere on screen, so it is only
    // read when no touch gesture is running.
    if let Some(pos) = resp.hover_pos().filter(|_| multi.is_none()) {
        let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        // Multiplicative, so a wheel click covers the same visual fraction at
        // any zoom — the same rate the panadapter uses.
        let factor = 0.998f64.powf(scroll as f64 * 2.0) / (zoom.max(0.01) as f64);
        if (factor - 1.0).abs() > 1e-4 {
            let (fx, fy) = frac(pos);
            view.zoom_about(factor, fx, fy, aspect);
            touched = true;
        }
    }

    if touched {
        view.manual = true;
    }
    // Double-click hands the view back: the auto-fit resumes and eases home
    // from wherever the user left it.
    if resp.double_clicked() {
        view.manual = false;
    }
    if resp.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }
    touched
}

/// The view to ease toward: centred on home (else the contacts' centroid),
/// zoomed symmetrically to frame home plus every contact.
fn target_view(home: Option<(f64, f64)>, contacts: &[(f64, f64)], aspect: f64) -> (f64, f64, f64) {
    let (clat, clon) = home.or_else(|| centroid(contacts)).unwrap_or((20.0, 0.0));
    if contacts.is_empty() {
        // Nothing to frame yet: whole world, centred on home.
        return (0.0, clon, 360.0);
    }
    let mut max_dlat = 0.0f64;
    let mut max_dlon = 0.0f64;
    for &(lat, lon) in contacts {
        max_dlat = max_dlat.max((lat - clat).abs());
        max_dlon = max_dlon.max(wrap180(lon - clon).abs());
    }
    let need_lon = 2.0 * max_dlon * PAD;
    let need_lat = 2.0 * max_dlat * PAD;
    // Fit both dimensions under the map's aspect (lat_span = lon_span * aspect).
    let lon_span = need_lon.max(need_lat / aspect.max(1e-3)).clamp(MIN_LON_SPAN, 360.0);
    let lat_span = (lon_span * aspect).min(180.0);
    // Keep the latitude window inside the poles (avoids empty polar space).
    let clat = if lat_span >= 180.0 {
        0.0
    } else {
        clat.clamp(-90.0 + lat_span / 2.0, 90.0 - lat_span / 2.0)
    };
    (clat, clon, lon_span)
}

/// The continents, as a dot grid sized to the available pixels (about one dot
/// every ~4 px), sampling the high-res land bitmap for crisp coastlines. Each
/// cell maps to a (lat, lon) in the current view; longitude wraps.
///
/// Returns the dot radius, which is the scale the callers draw their own
/// markers against.
pub(crate) fn draw_land(
    p: &eframe::egui::Painter,
    rect: eframe::egui::Rect,
    clat: f64,
    clon: f64,
    lon_span: f64,
    lat_span: f64,
    land: Color32,
) -> f32 {
    let (mw, mh) = land_mask_dims();
    let cols = ((rect.width() / 4.0) as usize).clamp(80, mw);
    let rows = ((rect.height() / 4.0) as usize).clamp(40, mh);
    let cell_w = rect.width() / cols as f32;
    let cell_h = rect.height() / rows as f32;
    let dot_r = (cell_w.min(cell_h) * 0.44).max(0.7);

    for row in 0..rows {
        let fy = (row as f64 + 0.5) / rows as f64; // 0 top .. 1 bottom
        let lat = clat + (0.5 - fy) * lat_span;
        if !(-90.0..=90.0).contains(&lat) {
            continue; // beyond a pole → open space, no land
        }
        let mrow = (((90.0 - lat) / 180.0 * mh as f64) as usize).min(mh - 1);
        for col in 0..cols {
            let fx = (col as f64 + 0.5) / cols as f64; // 0 left .. 1 right
            let lonw = wrap180(clon + (fx - 0.5) * lon_span);
            let mcol = ((lonw + 180.0) / 360.0 * mw as f64) as usize % mw;
            if land_cell(mcol, mrow) {
                let x = rect.left() + (col as f32 + 0.5) * cell_w;
                let y = rect.top() + (row as f32 + 0.5) * cell_h;
                p.circle_filled(pos2(x, y), dot_r, land);
            }
        }
    }
    dot_r
}

/// Draw the map filling the available width (2:1 aspect). `view` carries the
/// animated centre/zoom across frames. `home`/`dx`/`preview` are (lat, lon) in
/// degrees. `stations` is every decoded station still on the map — drawn as
/// white dots (their fade takes them out over time) under the coloured markers,
/// and used to drive the auto-zoom. (Their callsigns go unused here: at this
/// size a name per dot is a solid block of text. The globe, which has room,
/// draws them.) `preview` is a faint
/// marker for a decode the user clicked but hasn't answered yet (distinct colour
/// from the active DX). When `tx_active`, an animated pulse travels the home→dx
/// path. `max_h` caps the height: on short windows the map shrinks (keeping its
/// aspect, centered) rather than pushing the QSO controls off-screen.
///
/// Drag, wheel and pinch move the view by hand (see [`interact`]); doing so
/// suspends the auto-fit until a double-click reframes it.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    view: &mut MapView,
    home: Option<(f64, f64)>,
    dx: Option<(f64, f64)>,
    preview: Option<(f64, f64)>,
    hover: Option<(f64, f64)>,
    stations: &[crate::digi_map::DigiStation],
    // Network spots with a known location: (lat, lon, rgb tint by kind).
    spots: &[(f64, f64, (u8, u8, u8))],
    // Propagation heat, as an equirectangular RGBA image of the whole world
    // (see `crate::prop_map::PropHeat`). Painted under the continents, so the
    // coastline stays readable on top of it.
    heat: Option<eframe::egui::TextureId>,
    tx_active: bool,
    max_h: f32,
) {
    let avail_w = ui.available_width();
    if avail_w < MIN_HEIGHT {
        return;
    }
    // Fill the caller's width and its (user-draggable) height budget. The map is
    // no longer aspect-locked to 2:1, so it can be dragged taller than half its
    // width; the projection adapts (`lat_span = lon_span * aspect`). Capped at a
    // 1:1 aspect so it never becomes taller than wide.
    let h = max_h.min(avail_w).max(MIN_HEIGHT);
    let w = avail_w;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter_at(rect);
    let map = theme::map();
    p.rect_filled(rect, 0.0, map.sea);

    // ── Ease the view toward the target (fit home + all contacts) ──
    let aspect = (rect.height() / rect.width()) as f64;
    let mut contacts: Vec<(f64, f64)> = stations.iter().map(|d| (d.lat, d.lon)).collect();
    if let Some(c) = dx {
        contacts.push(c);
    }
    if let Some(c) = preview {
        contacts.push(c);
    }
    let (t_clat, t_clon, t_span) = target_view(home, &contacts, aspect);
    if !view.initialized {
        view.clat = t_clat;
        view.clon = t_clon;
        view.lon_span = t_span;
        view.initialized = true;
    } else if view.manual {
        // The user is holding the view. Nothing eases it out from under them —
        // but a resized panel changes the aspect, so it still has to stay legal.
        view.clamp(aspect);
    } else {
        view.clat += (t_clat - view.clat) * EASE;
        view.clon = wrap180(view.clon + wrap180(t_clon - view.clon) * EASE);
        view.lon_span += (t_span - view.lon_span) * EASE;
        let settled = (view.clat - t_clat).abs() < 0.05
            && wrap180(t_clon - view.clon).abs() < 0.05
            && (view.lon_span - t_span).abs() < 0.05;
        if !settled {
            crate::repaint::after_ms(ui.ctx(), 16);
        }
    }
    // Mouse/touch pan and zoom, applied on top of (and suspending) the auto-fit.
    if interact(ui, view, &resp, aspect) {
        crate::repaint::animate(ui.ctx());
    }
    let (clat, clon, lon_span) = (view.clat, view.clon, view.lon_span);
    let lat_span = lon_span * aspect;

    // Propagation heat, first — under everything, because it is the ground the
    // map is drawn on rather than a thing on the map.
    //
    // One textured quad rather than a per-cell fill: the projection is linear
    // in latitude and longitude, so the whole world is an axis-aligned
    // rectangle here, and the texture's own bilinear filtering is what turns
    // 2.5° cells into soft shapes with no visible edges. The quad is repeated
    // sideways to cover a view that straddles the antimeridian; the painter's
    // clip rectangle trims what falls outside.
    if let Some(tex) = heat {
        let lon_to_x =
            |lon: f64| rect.left() + (0.5 + ((lon - clon) / lon_span) as f32) * rect.width();
        let lat_to_y =
            |lat: f64| rect.top() + (0.5 - ((lat - clat) / lat_span) as f32) * rect.height();
        let world = eframe::egui::Rect::from_min_max(
            pos2(lon_to_x(-180.0), lat_to_y(90.0)),
            pos2(lon_to_x(180.0), lat_to_y(-90.0)),
        );
        let world_w = world.width();
        if world_w > 1.0 {
            // Which copies of the world overlap what is on screen.
            let first = ((rect.left() - world.right()) / world_w).floor() as i32;
            let last = ((rect.right() - world.left()) / world_w).ceil() as i32;
            let uv = eframe::egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            for k in first..=last {
                let r = world.translate(vec2(k as f32 * world_w, 0.0));
                if r.intersects(rect) {
                    p.image(tex, r, uv, Color32::WHITE);
                }
            }
        }
    }

    let dot_r = draw_land(&p, rect, clat, clon, lon_span, lat_span, map.land);

    // Project (lat, lon) to screen using the current view; longitude wraps.
    let project = |lat: f64, lon: f64| -> Pos2 {
        let dlon = wrap180(lon - clon);
        let x = rect.left() + (0.5 + (dlon / lon_span) as f32) * rect.width();
        let y = rect.top() + (0.5 - ((lat - clat) / lat_span) as f32) * rect.height();
        pos2(x, y)
    };

    // Every decoded station with a known grid, as small neutral dots that fade
    // with age (`alpha`). The active DX, the clicked preview and home are
    // painted over these below, so a selected/answered station keeps its own
    // colour.
    for d in stations {
        if d.fade <= 0.0 {
            continue;
        }
        let c = project(d.lat, d.lon);
        p.circle_filled(c, 2.6, alpha(map.station, 55.0 * d.fade));
        p.circle_filled(c, 1.7, alpha(map.station, 255.0 * d.fade));
    }
    // Keep the slow fade progressing even after the zoom has settled.
    if !stations.is_empty() {
        crate::repaint::after_ms(ui.ctx(), 300);
    }

    // Network spots (DX cluster / POTA / SOTA / PSK) as small kind-coloured
    // diamonds — drawn under the home/DX markers so an active QSO stays clear.
    for &(lat, lon, rgb) in spots {
        let c = project(lat, lon);
        let kind = theme::data_ink(rgb);
        p.circle_filled(c, dot_r + 2.0, alpha(kind, 55.0));
        p.circle_filled(c, 2.2, kind);
    }

    // Great-circle path as a dotted cyan trail (dots avoid antimeridian wrap).
    if let (Some(hll), Some(dll)) = (home, dx) {
        for (lat, lon) in great_circle_points(hll, dll, 90) {
            p.circle_filled(project(lat, lon), dot_r.max(1.0), alpha(map.trail, 150.0));
        }
    }

    // Faint amber preview marker for a clicked-but-unanswered decode.
    if let Some((lat, lon)) = preview {
        let c = project(lat, lon);
        p.circle_filled(c, dot_r + 3.0, alpha(map.preview, 45.0));
        p.circle_filled(c, 2.4, alpha(map.preview, 190.0));
    }

    // Endpoints with a glow.
    if let Some((lat, lon)) = home {
        let c = project(lat, lon);
        p.circle_filled(c, dot_r + 3.0, alpha(map.home, 60.0));
        p.circle_filled(c, 2.6, map.home);
    }
    if let Some((lat, lon)) = dx {
        let c = project(lat, lon);
        p.circle_filled(c, dot_r + 3.5, alpha(map.dx, 70.0));
        p.circle_filled(c, 3.0, map.dx);
    }
    // The decode row hovered in the table (drawn on top).
    if let Some((lat, lon)) = hover {
        let c = project(lat, lon);
        p.circle_filled(c, dot_r + 4.0, alpha(map.hover, 80.0));
        p.circle_filled(c, 3.2, map.hover);
    }

    // Animated pulse travelling home → dx while we transmit toward the contact.
    if tx_active {
        if let (Some(hll), Some(dll)) = (home, dx) {
            let pts = great_circle_points(hll, dll, 128);
            let n = pts.len();
            if n >= 2 {
                let phase = (ui.input(|i| i.time) * 0.45).rem_euclid(1.0); // ~2.2s sweep
                let head = ((phase * (n - 1) as f64) as usize).min(n - 1);
                // Comet tail behind the head (toward home).
                for k in 1..=6usize {
                    if head >= k {
                        let (la, lo) = pts[head - k];
                        let a = 150.0 - (k as f32) * 22.0;
                        p.circle_filled(project(la, lo), dot_r.max(1.2), alpha(map.comet, a));
                    }
                }
                // Bright leading head with a glow.
                let (la, lo) = pts[head];
                let c = project(la, lo);
                p.circle_filled(c, dot_r + 4.0, alpha(map.comet, 70.0));
                p.circle_filled(c, 3.2, map.station);
                // ~30 fps is plenty for the comet; an unconditional repaint
                // would drive the whole app at vsync rate during TX.
                crate::repaint::after_ms(ui.ctx(), 33);
            }
        }
    }

    // Once the user has taken the view, say how to give it back — but only
    // under the pointer, so an idle map stays a map and not a label.
    if view.manual && resp.hovered() {
        p.text(
            pos2(rect.right() - 5.0, rect.bottom() - 4.0),
            Align2::RIGHT_BOTTOM,
            "DOUBLE-CLICK TO REFRAME",
            FontId::proportional(9.0),
            map.hint,
        );
    }

    // Frame (red-accent, matching the QSO section panels).
    crate::chrome::paint_cut_border(&p, rect.shrink(0.5), map.frame, map.shell);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Aspect of a typical map: half as tall as it is wide.
    const ASPECT: f64 = 0.5;

    /// What the point at rect fraction (fx, fy) is over, in (lat, lon).
    fn under(v: &MapView, fx: f64, fy: f64, aspect: f64) -> (f64, f64) {
        (v.clat + (0.5 - fy) * v.lat_span(aspect), wrap180(v.clon + (fx - 0.5) * v.lon_span))
    }

    fn view(clat: f64, clon: f64, lon_span: f64) -> MapView {
        MapView { clat, clon, lon_span, initialized: true, manual: false }
    }

    /// A drag moves the land with the pointer, not against it: grabbing the map
    /// and pulling right brings what was to the west into view.
    #[test]
    fn a_drag_carries_the_land_with_it() {
        let mut v = view(0.0, 0.0, 180.0);
        // Half a map-width to the right.
        v.pan(0.5, 0.0, ASPECT);
        assert!((v.clon - -90.0).abs() < 1e-9, "clon = {}", v.clon);
        // And a quarter of its height down: the centre moves north.
        v.pan(0.0, 0.25, ASPECT);
        assert!((v.clat - 22.5).abs() < 1e-9, "clat = {}", v.clat);
    }

    /// The point under the cursor is the fixed point of a wheel zoom — that is
    /// the whole feel of "zoom in on *that*".
    #[test]
    fn zoom_keeps_what_is_under_the_cursor_under_it() {
        for (fx, fy) in [(0.5, 0.5), (0.2, 0.8), (0.93, 0.07)] {
            let mut v = view(15.0, 40.0, 200.0);
            let before = under(&v, fx, fy, ASPECT);
            v.zoom_about(0.4, fx, fy, ASPECT);
            let after = under(&v, fx, fy, ASPECT);
            assert!(
                (after.0 - before.0).abs() < 1e-9 && wrap180(after.1 - before.1).abs() < 1e-9,
                "({fx}, {fy}): {before:?} -> {after:?}"
            );
            assert!((v.lon_span - 80.0).abs() < 1e-9, "span = {}", v.lon_span);
        }
    }

    /// Zoom stops at both ends: the whole world out, the bitmap's own pixels in.
    #[test]
    fn zoom_stops_at_its_limits() {
        let mut v = view(0.0, 0.0, 360.0);
        v.zoom_about(4.0, 0.5, 0.5, ASPECT);
        assert!((v.lon_span - 360.0).abs() < 1e-9, "zoomed out past the world: {}", v.lon_span);
        for _ in 0..40 {
            v.zoom_about(0.5, 0.5, 0.5, ASPECT);
        }
        assert!(
            (v.lon_span - MIN_USER_LON_SPAN).abs() < 1e-9,
            "zoomed in past the mask: {}",
            v.lon_span
        );
    }

    /// Panning north cannot walk the window off the top of the world: the map
    /// stops with the pole at its edge rather than showing empty space.
    #[test]
    fn a_pan_stops_at_the_poles() {
        let mut v = view(0.0, 0.0, 90.0); // lat_span = 45
        for _ in 0..20 {
            v.pan(0.0, 0.5, ASPECT);
        }
        assert!((v.clat - (90.0 - 22.5)).abs() < 1e-9, "clat = {}", v.clat);
        // Longitude has no such edge — it wraps, and stays in [-180, 180).
        for _ in 0..20 {
            v.pan(-0.5, 0.0, ASPECT);
        }
        assert!((-180.0..180.0).contains(&v.clon), "clon = {}", v.clon);
    }

    /// A view zoomed out far enough to hold both poles centres itself on the
    /// equator instead of tipping to one side.
    #[test]
    fn a_whole_world_view_sits_on_the_equator() {
        let mut v = view(40.0, 0.0, 360.0);
        v.clamp(0.6); // lat_span = 216 > 180
        assert_eq!(v.clat, 0.0);
    }
}
