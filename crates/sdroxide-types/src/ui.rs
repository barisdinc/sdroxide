//! UI / display preferences (persisted in `config.toml` under `[ui]`), plus the
//! coarse speed enum shared by the waterfall-scroll and spectrum-averaging
//! settings. Kept wasm-safe (no I/O) so the egui client can use it directly.

use serde::{Deserialize, Serialize};

use crate::SpotKind;

/// Coarse speed setting for the waterfall scroll and the spectrum line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speed {
    Slow,
    Medium,
    Fast,
}

impl Speed {
    pub const ALL: [Speed; 3] = [Speed::Slow, Speed::Medium, Speed::Fast];

    pub fn label(self) -> &'static str {
        match self {
            Speed::Slow => "Slow",
            Speed::Medium => "Medium",
            Speed::Fast => "Fast",
        }
    }
}

/// Coarse font-size step for one family of hand-painted labels — the skimmer
/// boxes, the panadapter's own labels, the popup menus. Each family maps the
/// three steps onto its own point sizes, so `Small` here is "the small end of
/// that family's range", not one absolute size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FontSize {
    Small,
    Large,
    /// Declared last for the same reason as [`UiTheme::Default`]: serde
    /// demands the catch-all be the final variant, so a typo in a hand-edited
    /// config degrades to the middle size instead of throwing the whole
    /// config away.
    #[default]
    #[serde(other)]
    Medium,
}

impl FontSize {
    pub const ALL: [FontSize; 3] = [FontSize::Small, FontSize::Medium, FontSize::Large];

    pub fn label(self) -> &'static str {
        match self {
            FontSize::Small => "Small",
            FontSize::Medium => "Medium",
            FontSize::Large => "Large",
        }
    }
}

/// Which layout the window wears. `Auto` picks one from the viewport size; the
/// rest force it — for testing the compact strips without a phone to hand, and
/// for anyone who would rather have the menus in a small desktop window than a
/// control strip wrapped over three rows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    #[default]
    Auto,
    Desktop,
    Tablet,
    Phone,
}

impl LayoutMode {
    pub const ALL: [LayoutMode; 4] =
        [LayoutMode::Auto, LayoutMode::Desktop, LayoutMode::Tablet, LayoutMode::Phone];

    pub fn label(self) -> &'static str {
        match self {
            LayoutMode::Auto => "Auto",
            LayoutMode::Desktop => "Desktop",
            LayoutMode::Tablet => "Tablet",
            LayoutMode::Phone => "Phone",
        }
    }
}

/// Colour theme for the UI chrome. Every theme recolours the same set of
/// roles (backgrounds, borders, accents, text); content colours — waterfall
/// palettes, band plan, map — are untouched. The phosphor themes are
/// monochrome on purpose, except that transmit/SWR/error indications stay red
/// so an operator never has to wonder whether RF is leaving the antenna.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiTheme {
    GreenPhosphor,
    AmberPhosphor,
    TealOrange,
    Rainbow,
    /// White panels, near-black ink, dark saturated accents — the one theme
    /// that inverts the ground. The instruments (panadapter, S-meter, map,
    /// solar globe) keep their dark glass: a waterfall has no bright-ground
    /// form, and a signal display is read the same way in every theme.
    Light,
    /// White on black at the highest contrast the screen can give, with every
    /// dim shade in the UI pulled up to meet it — nothing is decoratively
    /// faint. For low vision, for glare, and for a display that has lost its
    /// contrast.
    HighContrast,
    /// The classic navy/cyan/pink look. Declared last because serde demands
    /// the catch-all be the final variant: it also swallows an unrecognised
    /// value in a hand-edited config, so a typo degrades to the default theme
    /// instead of throwing the whole config away.
    #[default]
    #[serde(other)]
    Default,
}

impl UiTheme {
    pub const ALL: [UiTheme; 7] = [
        UiTheme::Default,
        UiTheme::Light,
        UiTheme::HighContrast,
        UiTheme::GreenPhosphor,
        UiTheme::AmberPhosphor,
        UiTheme::TealOrange,
        UiTheme::Rainbow,
    ];

    pub fn label(self) -> &'static str {
        match self {
            UiTheme::Default => "Default",
            UiTheme::Light => "Light",
            UiTheme::HighContrast => "High contrast",
            UiTheme::GreenPhosphor => "Green phosphor",
            UiTheme::AmberPhosphor => "Amber phosphor",
            UiTheme::TealOrange => "Teal / orange",
            UiTheme::Rainbow => "Rainbow",
        }
    }

    /// True where the chrome sits on a bright ground, so anything that has to
    /// pick an ink or a shade by hand knows which way round the world is.
    pub fn is_light(self) -> bool {
        matches!(self, UiTheme::Light)
    }
}

/// The shape a piece of chrome wears — one list serves both the buttons and
/// the windows, each chosen separately.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromeStyle {
    Rectangular,
    Rounded,
    Gradient,
    Bevel,
    /// Drawn as if the screen were a character display: frames built out of
    /// `+`, `-` and `|`, buttons wearing `[` and `]`, tick boxes reading
    /// `[X]`, and everything set in the monospace face.
    Terminal,
    /// The classic cut-corner look. Last for the same reason as
    /// [`UiTheme::Default`]: the serde catch-all must be the final variant.
    #[default]
    #[serde(other)]
    Angled,
}

impl ChromeStyle {
    pub const ALL: [ChromeStyle; 6] = [
        ChromeStyle::Angled,
        ChromeStyle::Rectangular,
        ChromeStyle::Rounded,
        ChromeStyle::Gradient,
        ChromeStyle::Bevel,
        ChromeStyle::Terminal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ChromeStyle::Angled => "Angled",
            ChromeStyle::Rectangular => "Rectangular",
            ChromeStyle::Rounded => "Rounded",
            ChromeStyle::Gradient => "Gradient",
            ChromeStyle::Bevel => "3D bevel",
            ChromeStyle::Terminal => "Terminal",
        }
    }
}

/// User display preferences. All have defaults so a missing `[ui]` table loads.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// GUI repaint + spectrum frame rate, in frames per second.
    pub frame_rate_fps: u32,
    /// How fast the waterfall scrolls.
    pub waterfall_speed: Speed,
    /// How fast the spectrum line reacts (averaging; slower = smoother).
    pub spectrum_speed: Speed,
    /// Waterfall colour palette, as an index into the client's palette list.
    pub waterfall_palette: usize,
    /// Fill the spectrum area with a vertical top→bottom colour gradient.
    pub spectrum_gradient: bool,
    /// Gradient colour at the top of the spectrum area (sRGB, 0–255).
    pub gradient_top: [u8; 3],
    /// Gradient colour at the bottom of the spectrum area (sRGB, 0–255).
    pub gradient_bottom: [u8; 3],
    /// The tint each spot kind wears — on the panadapter labels, in the SPOTS
    /// list and on the world map — indexed by [`SpotKind::index`]. Starts at
    /// [`SpotKind::default_color`]; the UI tab retints any of them.
    ///
    /// This screen's preference, like the theme: the spots themselves are the
    /// station's, but what colour they are painted is the operator's, and a
    /// remote client picks its own.
    #[serde(default = "default_spot_colors", deserialize_with = "spot_colors")]
    pub spot_colors: [[u8; 3]; SpotKind::COUNT],
    /// Which layout the window wears, or `Auto` to pick from the viewport.
    pub layout: LayoutMode,
    /// Colour theme for the UI chrome.
    pub theme: UiTheme,
    /// The shape buttons wear.
    pub button_style: ChromeStyle,
    /// The shape floating windows and popups wear.
    pub window_style: ChromeStyle,
    /// Font size for the skimmer / spot boxes overlaid on the waterfall.
    /// `Medium` is the historic size.
    pub skimmer_font_size: FontSize,
    /// Font size for the labels painted onto the spectrum and waterfall —
    /// the frequency scale, the band plan, the measurement and marker
    /// labels. `Small` is the historic size.
    pub waterfall_font_size: FontSize,
    /// Font size for the interface itself — menus, dialogs, windows, the tab
    /// strip, the top bar and every button on it. Applied as the client's zoom
    /// factor, so the spacing around the text follows it and the waterfall and
    /// skimmer sizes below are relative to it. `Medium` is the historic size.
    pub menu_font_size: FontSize,
    /// Ask sdroxide.com once per start whether a newer release has been
    /// published, and say so in the notice banner above the panadapter. In
    /// `[ui]` because it is this screen's preference, like the theme — the
    /// native client checks for its own build, wherever its radio is.
    pub update_check: bool,
    /// How the memory channel window orders its list. This screen's
    /// preference, not the station's: the store keeps its own order and every
    /// client reads it whichever way its operator asked for.
    pub memory_sort: crate::MemorySort,
    /// Read that order backwards — Z to A, highest frequency first, and
    /// newest-stored first for [`crate::MemorySort::Stored`].
    pub memory_sort_desc: bool,
}

/// Default for [`UiSettings::spot_colors`] — every kind on its stock tint.
fn default_spot_colors() -> [[u8; 3]; SpotKind::COUNT] {
    let mut out = [[0u8; 3]; SpotKind::COUNT];
    for kind in SpotKind::ALL {
        let (r, g, b) = kind.default_color();
        out[kind.index()] = [r, g, b];
    }
    out
}

/// Read [`UiSettings::spot_colors`] as a list of any length, so a config
/// written before a spot kind was added — or by a newer build that has one
/// more — still loads. A short list leaves the kinds it doesn't reach on their
/// stock tint; a long one has its tail ignored. Without this the whole `[ui]`
/// table would fail to parse over one extra entry, and the operator would lose
/// their theme, their fonts and their layout along with the colours.
fn spot_colors<'de, D>(d: D) -> Result<[[u8; 3]; SpotKind::COUNT], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let list = Vec::<[u8; 3]>::deserialize(d)?;
    let mut out = default_spot_colors();
    for (slot, c) in out.iter_mut().zip(list) {
        *slot = c;
    }
    Ok(out)
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings {
            frame_rate_fps: 60,
            waterfall_speed: Speed::Medium,
            spectrum_speed: Speed::Medium,
            waterfall_palette: 0,
            spectrum_gradient: true,
            gradient_top: [64, 0, 0],   // dark red
            gradient_bottom: [0, 0, 0], // black
            spot_colors: default_spot_colors(),
            layout: LayoutMode::Auto,
            theme: UiTheme::Default,
            button_style: ChromeStyle::Angled,
            window_style: ChromeStyle::Angled,
            skimmer_font_size: FontSize::Medium,
            waterfall_font_size: FontSize::Small,
            menu_font_size: FontSize::Medium,
            update_check: true,
            memory_sort: crate::MemorySort::Stored,
            memory_sort_desc: false,
        }
    }
}

impl UiSettings {
    /// Selectable frame rates for the UI combo.
    ///
    /// The rates below 30 are for machines that cannot keep up — a Raspberry Pi
    /// driving a 4K panel, a remote client on a thin laptop. They cost detail in
    /// the waterfall (fewer distinct rows; the scroll speed is absolute, so a
    /// row is simply repeated) and nothing else: the engine still processes
    /// every sample, and only the spectrum frame it publishes slows down.
    pub const FPS_OPTIONS: [u32; 6] = [5, 10, 15, 30, 60, 90];

    /// Frame rate clamped to a sane range (guards a hand-edited config).
    pub fn fps(self) -> u32 {
        self.frame_rate_fps.clamp(5, 240)
    }

    /// Waterfall scroll rate in rows per second. Absolute (independent of the
    /// frame rate) so the time axis — and the 60-second gridlines — stay stable
    /// when the frame rate changes.
    ///
    /// `Fast` is twice the old fast rate, which now sits on `Medium`: at 28
    /// rows/s a CW or FT8 trace still smears vertically, and chasing a fading
    /// signal wants the extra time resolution. It costs nothing but rows.
    pub fn waterfall_rows_per_sec(self) -> f32 {
        match self.waterfall_speed {
            Speed::Slow => 5.0,
            Speed::Medium => 28.0,
            Speed::Fast => 56.0,
        }
    }

    /// Exponential averaging time constant (seconds) for the spectrum line.
    /// Fast disables averaging (snappy); slower values smooth it out.
    pub fn spectrum_avg_tc(self) -> f32 {
        match self.spectrum_speed {
            Speed::Fast => 0.0,
            Speed::Medium => 0.1,
            Speed::Slow => 0.2,
        }
    }
}
