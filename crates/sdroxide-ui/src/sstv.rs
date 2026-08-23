//! SSTV image compositing for transmit: crop/scale a source picture to the
//! selected mode's dimensions, stamp the operator's banner strip across the
//! top of it, and overlay the slot's multi-line message (one bundled font, bold
//! with a black outline for readability, with the first line drawn at double
//! size).
//!
//! The banner is what the station puts its name on, so its two texts, its two
//! colours and its height come from [`sdroxide_types::DigiConfig`] rather than
//! from here — see [`Banner`]. The stock settings compose exactly the strip
//! this module used to hard-wire: the callsign at the left, `SDRoxide vX.Y.Z`
//! at the right, white on red fading to black.
//!
//! Pure-Rust (`image` + `ab_glyph`) so it runs identically in the native app and
//! the wasm browser client — the composed buffer is both the live preview and,
//! PNG-encoded, the transmit payload.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use eframe::egui;

/// The banner strip drawn across the top of a transmitted picture, with the
/// operator's templates already resolved against the station identity.
///
/// Resolved rather than carried as templates because the same banner is drawn
/// several times for one picture — the live preview on every keystroke, then
/// the transmit copy — and because it keeps the drawing code free of any
/// notion of what a `{call}` is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Banner {
    /// Height of the strip in picture pixels. The text is sized from it.
    pub height: u16,
    /// Text printed at the left end.
    pub left: String,
    /// Text printed at the right end, right-aligned.
    pub right: String,
    /// Colour at the top of the strip, fading to black at its bottom.
    pub fill: [u8; 3],
    /// Colour both texts are printed in.
    pub ink: [u8; 3],
}

impl Banner {
    /// The banner the station's digital-mode config asks for, or `None` when
    /// the operator has switched it off.
    ///
    /// A banner whose two texts both resolve to nothing still draws its strip:
    /// blanking the text is not the same request as turning the banner off,
    /// and a strip that vanished when a callsign had not been entered yet
    /// would look like a bug rather than like an empty field.
    pub fn from_config(cfg: &sdroxide_types::DigiConfig) -> Option<Banner> {
        cfg.sstv_banner.then(|| Banner {
            // A zero height would be an invisible banner that still pushed the
            // message down by nothing; one pixel is the smallest honest strip.
            height: cfg.sstv_banner_height.max(1),
            left: expand(&cfg.sstv_banner_left, &cfg.my_call, &cfg.my_grid),
            right: expand(&cfg.sstv_banner_right, &cfg.my_call, &cfg.my_grid),
            fill: cfg.sstv_banner_fill,
            ink: cfg.sstv_banner_ink,
        })
    }
}

/// Substitute the banner placeholders in `template`.
///
/// `{call}` is uppercased — a callsign belongs in capitals on the air and the
/// header always printed it that way — but the template around it is left as
/// the operator typed it, which is what stops `SDRoxide` becoming `SDROXIDE`.
/// An unrecognised `{…}` is copied through untouched so a typo is visible in
/// the preview rather than silently printing nothing.
pub fn expand(template: &str, call: &str, grid: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('}') else {
            // An unclosed brace is just text.
            break;
        };
        match rest[1..close].to_ascii_lowercase().as_str() {
            "call" => out.push_str(call.trim().to_uppercase().as_str()),
            "grid" => out.push_str(grid.trim()),
            "version" => out.push_str(env!("CARGO_PKG_VERSION")),
            _ => out.push_str(&rest[..=close]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The placeholders [`expand`] knows, with a word on each, for the hint text
/// beside the two fields that take them.
pub const BANNER_PLACEHOLDERS: [(&str, &str); 3] = [
    ("{call}", "your callsign, in capitals"),
    ("{grid}", "your locator"),
    ("{version}", "the sdroxide version"),
];

/// The single font used for the header and the message overlay
/// (ChakraPetch-SemiBold, already bundled OFL for the UI's own text).
fn message_font() -> Option<FontRef<'static>> {
    const RAW: &[u8] = include_bytes!("../assets/fonts/ChakraPetch-SemiBold.ttf");
    FontRef::try_from_slice(RAW).ok()
}

/// Decode arbitrary image file bytes (PNG/JPEG) to interleaved RGB + size.
pub fn decode_image(bytes: &[u8]) -> Option<(Vec<u8>, u16, u16)> {
    let img = image::load_from_memory(bytes).ok()?.to_rgb8();
    let (w, h) = (img.width() as u16, img.height() as u16);
    Some((img.into_raw(), w, h))
}

/// Decode an image file and downscale it so neither side exceeds `max` pixels
/// (keeping aspect ratio), returning interleaved RGB + size. Bounds the memory
/// held per transmit slot.
pub fn load_source_bounded(bytes: &[u8], max: u16) -> Option<(Vec<u8>, u16, u16)> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > max as u32 || img.height() > max as u32 {
        img.resize(max as u32, max as u32, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let (w, h) = (rgb.width() as u16, rgb.height() as u16);
    Some((rgb.into_raw(), w, h))
}

/// Encode interleaved RGB to PNG.
pub fn encode_png(rgb: &[u8], w: u16, h: u16) -> Option<Vec<u8>> {
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// Crop-and-scale a source image to exactly `(w, h)`, filling the frame
/// (centre-crop, preserving aspect ratio).
pub fn crop_scale(src_rgb: &[u8], sw: u16, sh: u16, w: u16, h: u16) -> Vec<u8> {
    let Some(src) = image::RgbImage::from_raw(sw as u32, sh as u32, src_rgb.to_vec()) else {
        return vec![0u8; w as usize * h as usize * 3];
    };
    image::DynamicImage::ImageRgb8(src)
        .resize_to_fill(w as u32, h as u32, image::imageops::FilterType::Triangle)
        .to_rgb8()
        .into_raw()
}

/// Build the final transmit image: crop/scale the source to `(w, h)`, add the
/// banner strip, then the message overlay. Returns `(rgb, w, h)`.
///
/// The size is the caller's: SSTV takes it from the line format, RIFP from the
/// operator, since the protocol fixes none of its own. `banner` is `None` when
/// the operator has switched the strip off, and the message then starts at the
/// top of the picture instead of below it.
pub fn compose(
    w: u16,
    h: u16,
    src_rgb: &[u8],
    sw: u16,
    sh: u16,
    message: &str,
    banner: Option<&Banner>,
) -> (Vec<u8>, u16, u16) {
    let mut img = crop_scale(src_rgb, sw, sh, w, h);
    let strip = match banner {
        Some(b) => draw_banner(&mut img, w as usize, h as usize, b),
        None => 0,
    };
    draw_message(&mut img, w as usize, h as usize, message, strip);
    (img, w, h)
}

/// Convert interleaved RGB to an egui image for a texture.
pub fn color_image(rgb: &[u8], w: u16, h: u16) -> egui::ColorImage {
    egui::ColorImage::from_rgb([w as usize, h as usize], rgb)
}

// ── Overlay messages: who owns the text ──
//
// The presets live in the engine, so the message for a slot arrives as part of
// its status and can change while the operator is looking at it — another
// screen attached to the same radio, or the echo of this one's own write coming
// back a round trip later. Adopting every echo would put the cursor back where
// it was when the write went out; ignoring them all would mean an edit made
// elsewhere never showed up.
//
// So exactly one slot is client-owned at a time: whichever the operator is
// typing in. Every other slot takes the engine's word. The same arrangement the
// voice keyer's slot labels use, kept here rather than in the panel so it can be
// tested without an `egui::Context`.

/// The text to show for `slot`: what is being typed if it is this slot,
/// otherwise what the engine says.
pub fn message_shown<'a>(
    edit: &'a Option<(usize, String)>,
    presets: &'a sdroxide_types::ImagePresets,
    slot: usize,
) -> &'a str {
    match edit {
        Some((i, text)) if *i == slot => text,
        _ => presets.slots.get(slot).map_or("", |s| s.message.as_str()),
    }
}

/// Give a claimed slot back to the engine, returning the command to send when
/// the text actually changed.
///
/// `None` when nothing was claimed or nothing was edited, so an unfocused click
/// does not spray writes at the engine.
pub fn commit_message(
    edit: &mut Option<(usize, String)>,
    presets: &sdroxide_types::ImagePresets,
) -> Option<sdroxide_types::Command> {
    let (slot, text) = edit.take()?;
    (presets.slots.get(slot).map(|s| s.message.as_str()) != Some(text.as_str()))
        .then(|| sdroxide_types::Command::ImageSetMessage { slot: slot as u8, message: text })
}

/// Claim `slot` for editing, committing whatever was claimed before.
pub fn claim_message(
    edit: &mut Option<(usize, String)>,
    presets: &sdroxide_types::ImagePresets,
    slot: usize,
) -> Option<sdroxide_types::Command> {
    if matches!(edit, Some((i, _)) if *i == slot) {
        return None;
    }
    let cmd = commit_message(edit, presets);
    *edit = Some((slot, message_shown(&None, presets, slot).to_string()));
    cmd
}

fn put(img: &mut [u8], w: usize, h: usize, x: i32, y: i32, r: u8, g: u8, b: u8) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let i = (y as usize * w + x as usize) * 3;
    img[i] = r;
    img[i + 1] = g;
    img[i + 2] = b;
}

fn blend(img: &mut [u8], w: usize, h: usize, x: i32, y: i32, r: u8, g: u8, b: u8, a: f32) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let a = a.clamp(0.0, 1.0);
    let i = (y as usize * w + x as usize) * 3;
    let mix = |o: u8, n: u8| (o as f32 * (1.0 - a) + n as f32 * a).round().clamp(0.0, 255.0) as u8;
    img[i] = mix(img[i], r);
    img[i + 1] = mix(img[i + 1], g);
    img[i + 2] = mix(img[i + 2], b);
}

/// The banner: a strip fading from the operator's colour at the top to black
/// at the bottom, their left text at the left and their right text at the
/// right. Returns how many rows of the picture it actually covered, which is
/// where the message overlay starts.
///
/// The type sizes off the height rather than being a setting of its own — the
/// eleven points the strip was drawn at were eleven points *because* it was
/// sixteen pixels tall, and two controls that have to be turned together are
/// worse than one.
fn draw_banner(img: &mut [u8], w: usize, h: usize, banner: &Banner) -> usize {
    let strip = usize::from(banner.height).min(h);
    if strip == 0 {
        return 0;
    }
    let [fr, fg, fb] = banner.fill;
    for y in 0..strip {
        let t = 1.0 - (y as f32 / strip as f32);
        let shade = |c: u8| (f32::from(c) * t) as u8;
        for x in 0..w {
            put(img, w, h, x as i32, y as i32, shade(fr), shade(fg), shade(fb));
        }
    }
    let Some(font) = message_font() else {
        return strip;
    };
    let ink = (banner.ink[0], banner.ink[1], banner.ink[2]);
    let scale = PxScale::from(strip as f32 * 11.0 / 16.0);
    let baseline = (strip as f32 * 0.72).round();
    // The inset scales with the strip too, so a tall banner does not print
    // hard against the edge of the picture.
    let pad = (strip as f32 * 4.0 / 16.0).max(1.0);
    if !banner.left.is_empty() {
        draw_text(img, w, h, pad, baseline, &banner.left, &font, scale, ink, 1.0);
    }
    if !banner.right.is_empty() {
        let tw = text_width(&banner.right, &font, scale);
        draw_text(img, w, h, w as f32 - tw - pad, baseline, &banner.right, &font, scale, ink, 1.0);
    }
    strip
}

/// Overlay the message in a single font, white with a black outline, starting
/// just below the banner — or at the top of the picture when there is none.
/// The first line is drawn at double the size of the rest (a title line), with
/// its outline thickened to match.
fn draw_message(img: &mut [u8], w: usize, h: usize, message: &str, top: usize) {
    let Some(font) = message_font() else {
        return;
    };
    let base_px = 30.0_f32;
    let mut baseline = top as f32;
    for (i, line) in message.lines().enumerate() {
        // First line twice as large; the line height and outline scale with it.
        let px = if i == 0 { base_px * 1.5 } else { base_px };
        let line_h = px * 1.2;
        baseline += line_h;
        if line.trim().is_empty() {
            continue;
        }
        let scale = PxScale::from(px);
        let outline = px / base_px * 1.5;
        // Black outline: draw the glyphs offset in eight directions.
        for (ox, oy) in [
            (-outline, 0.0),
            (outline, 0.0),
            (0.0, -outline),
            (0.0, outline),
            (-outline, -outline),
            (outline, -outline),
            (-outline, outline),
            (outline, outline),
        ] {
            draw_text(img, w, h, 6.0 + ox, baseline + oy, line, &font, scale, (0, 0, 0), 1.0);
        }
        draw_text(img, w, h, 6.0, baseline, line, &font, scale, (255, 255, 255), 1.0);
        if baseline as usize >= h {
            break;
        }
    }
}

fn text_width(text: &str, font: &FontRef<'static>, scale: PxScale) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut width = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let g = font.glyph_id(ch);
        if let Some(p) = prev {
            width += scaled.kern(p, g);
        }
        width += scaled.h_advance(g);
        prev = Some(g);
    }
    width
}

fn draw_text(
    img: &mut [u8],
    w: usize,
    h: usize,
    x: f32,
    baseline: f32,
    text: &str,
    font: &FontRef<'static>,
    scale: PxScale,
    color: (u8, u8, u8),
    alpha: f32,
) {
    let scaled = font.as_scaled(scale);
    let mut caret = x;
    let mut prev = None;
    for ch in text.chars() {
        let gid = font.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, gid);
        }
        let glyph = gid.with_scale_and_position(scale, point(caret, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, cov| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                blend(img, w, h, px, py, color.0, color.1, color.2, cov * alpha);
            });
        }
        caret += scaled.h_advance(gid);
        prev = Some(gid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdroxide_types::{Command, DigiConfig, ImagePresets, ImageSlotInfo};

    fn presets(messages: &[&str]) -> ImagePresets {
        ImagePresets {
            slots: messages
                .iter()
                .map(|m| ImageSlotInfo { message: (*m).into(), ..ImageSlotInfo::default() })
                .collect(),
        }
    }

    /// The echo of our own write comes back a round trip after it went out.
    /// Adopting it would drop the cursor back to where it was when the operator
    /// started the sentence.
    #[test]
    fn a_server_echo_does_not_clobber_the_slot_being_typed_in() {
        let p = presets(&["a", "b", "old", "d", "e"]);
        let edit = Some((2, "CQ CQ de OE1TEST".to_string()));
        assert_eq!(message_shown(&edit, &p, 2), "CQ CQ de OE1TEST");
    }

    /// An edit made on another screen still has to appear — that is the whole
    /// point of the presets living in the engine.
    #[test]
    fn an_edit_made_elsewhere_reaches_every_slot_but_the_one_in_hand() {
        let p = presets(&["a", "b", "old", "d", "e"]);
        let edit = Some((2, "typing".to_string()));
        assert_eq!(message_shown(&edit, &p, 0), "a");
        assert_eq!(message_shown(&edit, &p, 3), "d");
        // With nothing claimed, every slot follows the engine.
        assert_eq!(message_shown(&None, &p, 2), "old");
        // A slot the engine has never mentioned reads empty rather than panicking.
        assert_eq!(message_shown(&None, &p, 99), "");
    }

    #[test]
    fn leaving_a_slot_commits_it_and_gives_it_back() {
        let p = presets(&["a", "b", "old", "d", "e"]);
        let mut edit = Some((2, "new".to_string()));
        let cmd = commit_message(&mut edit, &p);
        assert_eq!(cmd, Some(Command::ImageSetMessage { slot: 2, message: "new".into() }));
        assert!(edit.is_none(), "the claim is released");

        // Clicking away without having changed anything must not write.
        let mut edit = Some((2, "old".to_string()));
        assert_eq!(commit_message(&mut edit, &p), None);
        assert!(edit.is_none());
        // And with nothing claimed there is nothing to commit.
        assert_eq!(commit_message(&mut None, &p), None);
    }

    #[test]
    fn switching_slots_takes_the_engines_text_for_the_new_one() {
        let p = presets(&["a", "b", "old", "d", "e"]);
        let mut edit = Some((2, "half-typed".to_string()));
        // Moving to slot 4 flushes slot 2 and picks up slot 4's stored text.
        let cmd = claim_message(&mut edit, &p, 4);
        assert_eq!(cmd, Some(Command::ImageSetMessage { slot: 2, message: "half-typed".into() }));
        assert_eq!(edit, Some((4, "e".to_string())));
        // Re-claiming the slot already in hand keeps what is being typed.
        edit = Some((4, "e and more".to_string()));
        assert_eq!(claim_message(&mut edit, &p, 4), None);
        assert_eq!(edit, Some((4, "e and more".to_string())));
    }

    /// The stock settings have to compose the strip the header was hard-wired
    /// to draw before it could be edited, or every existing station's pictures
    /// change appearance on upgrade.
    #[test]
    fn the_stock_banner_is_the_header_that_was_hard_wired() {
        let cfg = DigiConfig { my_call: "oe1test".into(), ..DigiConfig::default() };
        let b = Banner::from_config(&cfg).expect("on by default");
        assert_eq!(b.left, "OE1TEST");
        assert_eq!(b.right, format!("SDRoxide v{}", env!("CARGO_PKG_VERSION")));
        assert_eq!(b.height, 16);
        assert_eq!(b.fill, [170, 0, 0]);
    }

    #[test]
    fn switching_the_banner_off_leaves_no_strip_to_draw() {
        let cfg = DigiConfig { sstv_banner: false, ..DigiConfig::default() };
        assert_eq!(Banner::from_config(&cfg), None);
    }

    /// The callsign goes up in capitals, but only the callsign: uppercasing the
    /// whole template would turn `SDRoxide` into `SDROXIDE`.
    #[test]
    fn only_the_callsign_is_uppercased() {
        assert_eq!(expand("de {call} ", " oe1test ", ""), "de OE1TEST ");
        assert_eq!(expand("SDRoxide {grid}", "", "jn88"), "SDRoxide jn88");
        assert_eq!(expand("v{version}", "", ""), format!("v{}", env!("CARGO_PKG_VERSION")));
        // Case-insensitive, so {CALL} works as well as {call}.
        assert_eq!(expand("{CALL}", "oe1test", ""), "OE1TEST");
    }

    /// A typo has to reach the preview as itself. Swallowing it would leave the
    /// operator staring at a gap with nothing to tell them what went wrong.
    #[test]
    fn an_unknown_placeholder_survives_untouched() {
        assert_eq!(expand("{callsign} {call}", "oe1test", ""), "{callsign} OE1TEST");
        assert_eq!(expand("100% {open", "", ""), "100% {open");
        assert_eq!(expand("}{}{", "", ""), "}{}{");
    }

    /// The message used to start at a fixed 16 rows whether or not a strip was
    /// there. With the banner off, the picture is the operator's from the top.
    #[test]
    fn the_message_starts_below_whatever_the_banner_actually_covered() {
        // A three-pixel-tall picture cannot hold a 16-pixel banner; the strip
        // must report what it covered, not what it was asked for.
        let mut img = vec![0u8; 8 * 3 * 3];
        let tall = Banner { height: 16, ..Banner::default() };
        assert_eq!(draw_banner(&mut img, 8, 3, &tall), 3);
        let none = Banner { height: 0, ..Banner::default() };
        assert_eq!(draw_banner(&mut img, 8, 3, &none), 0);
    }

    /// Composing with no banner must leave the top row of the picture alone —
    /// it is the check that "off" means off rather than "a black strip".
    #[test]
    fn composing_without_a_banner_leaves_the_top_row_of_the_picture() {
        let src = vec![200u8; 4 * 4 * 3];
        let (with, _, _) = compose(
            4,
            4,
            &src,
            4,
            4,
            "",
            Some(&Banner { height: 2, fill: [170, 0, 0], ..Banner::default() }),
        );
        let (without, _, _) = compose(4, 4, &src, 4, 4, "", None);
        assert_eq!(with[0], 170);
        assert_eq!(&without[..12], &[200u8; 12]);
    }
}
