use serde::{Deserialize, Serialize};

/// One panadapter frame. `bins` are magnitudes mapped to u8 over
/// `[db_floor, db_ceil]`, ordered from `center_hz - span_hz/2` upward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpectrumFrame {
    pub seq: u32,
    pub center_hz: f64,
    pub span_hz: f64,
    pub db_floor: f32,
    pub db_ceil: f32,
    pub bins: Vec<u8>,
    /// Waterfall rows the engine clocked since the last frame, oldest first,
    /// `bins.len()` bytes each.
    ///
    /// Separate from [`Self::bins`] because the two answer different
    /// questions. `bins` is the spectrum *now* — what the trace draws and what
    /// levels are read off. A row is the strongest thing seen in its slice of
    /// time, and there are as many of them as the client asked for through
    /// [`SpectrumConfig::rows_per_sec`], which need not be — and on a fast
    /// front end should not be — the rate frames are published at.
    ///
    /// Empty on a lane that cannot clock its own rows (a radio's own sweep, a
    /// transmit monitor). The client then scrolls `bins` on its own wall clock,
    /// repeating it, exactly as every build before this one did.
    #[serde(default)]
    pub rows: Vec<u8>,
    /// Whether this lane clocks its own waterfall rows at all.
    ///
    /// Distinct from `rows` being non-empty, and the distinction matters at
    /// every scroll rate below the frame rate: at five rows a second and sixty
    /// frames, fifty-five frames in every sixty carry none — and a client that
    /// read "no rows" as "this lane does not clock rows" would fall back to
    /// scrolling on its own wall clock *as well*, running the waterfall at
    /// twice the rate its own time labels assume.
    #[serde(default)]
    pub rows_clocked: bool,
}

impl SpectrumFrame {
    /// How many whole waterfall rows [`Self::rows`] carries.
    pub fn row_count(&self) -> usize {
        if self.bins.is_empty() { 0 } else { self.rows.len() / self.bins.len() }
    }

    pub fn freq_at_bin(&self, bin: usize) -> f64 {
        let n = self.bins.len().max(1) as f64;
        self.center_hz - self.span_hz / 2.0 + (bin as f64 + 0.5) / n * self.span_hz
    }
}

/// Columns in a frame when nobody has said otherwise, and the fewest anyone
/// may ask for.
///
/// The fixed width every build before issue #172 emitted and drew, so an engine
/// no client has configured — and a client from before the field existed —
/// behaves exactly as it always did. It is the floor as well as the default: a
/// client asking for less is either old or confused, and neither is a reason to
/// draw a coarser picture than sdroxide has ever drawn.
pub const DEFAULT_DISPLAY_BINS: u32 = 2048;

/// The widest frame any client may ask an engine for.
///
/// A ceiling on an untrusted number, not a preference: 8192 columns is two per
/// pixel of a 4K panadapter, half a megabyte a second on the wire, and 32 MB of
/// texture on the client. No display is wider, so past here only the bill grows.
/// Mirrored by `sdroxide_ui::waterfall_gpu::MAX_TEX_W` at the other end.
pub const MAX_DISPLAY_BINS: u32 = 8192;

/// Waterfall rows a second when nobody has said otherwise: the historic
/// `Medium` scroll rate, so an unconfigured engine scrolls as it always did.
pub const DEFAULT_ROWS_PER_SEC: u16 = 28;

/// The fastest row clock any client may ask an engine for.
///
/// A row is `display_bins` bytes on the wire and a texture write at the far
/// end, so this bounds both. 480 a second is a row every 2 ms — finer than any
/// transform rate below about 4 Msps, and already only four seconds of history
/// in the client's 2048-row ring.
pub const MAX_ROWS_PER_SEC: u16 = 480;

/// Client-requested spectrum generation parameters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpectrumConfig {
    pub fft_size: u32,
    /// Columns the client wants in each emitted frame: the panadapter's
    /// horizontal resolution, and the width of its waterfall history texture.
    ///
    /// The client's to ask for, because it is a property of who is *looking*
    /// and not of the radio — a 4K panel wants about one column per pixel, a
    /// Raspberry Pi driving the same panel does not, and the engine has no way
    /// to tell which is in front of it. Read it back through
    /// [`SpectrumConfig::bins`] rather than directly: it arrives over the
    /// network and sizes an allocation sixty times a second.
    ///
    /// It is not the FFT size. The transform is pooled down to these columns,
    /// so past this width a bigger [`SpectrumConfig::fft_size`] buys contrast
    /// — each column is the maximum of more bins — rather than detail.
    pub display_bins: u32,
    /// Waterfall rows a second the client wants the engine to clock.
    ///
    /// The waterfall's vertical axis is time, and this is its sample rate. It
    /// is deliberately *not* [`Self::fps`]: a frame is a repaint, and a repaint
    /// is expensive, where a row is a few kilobytes appended to a texture. An
    /// RX-888 through a 32768-point window runs some five hundred transforms a
    /// second; a screen redraws sixty times. Tying the two together is what
    /// made a fast waterfall draw each line two pixels tall — the same numbers
    /// written twice — instead of showing twice as much of what the radio
    /// heard.
    ///
    /// Read it back through [`SpectrumConfig::rows`], which holds it to a rate
    /// an engine can be asked for by anyone who can reach it.
    pub rows_per_sec: u16,
    pub fps: u8,
    /// Exponential averaging time constant in seconds. 0 disables averaging.
    pub avg_tc: f32,
    /// u8 mapping range for emitted frames.
    pub db_floor: f32,
    pub db_ceil: f32,
    /// Visible sub-span (lo_hz, hi_hz); `None` = full device passband.
    pub viewport: Option<(f64, f64)>,
}

impl SpectrumConfig {
    /// [`Self::display_bins`] as a width a frame builder can use.
    ///
    /// One clamp in one place, so the half-dozen lanes in the engine that build
    /// frames cannot each get it wrong — and so a hand-rolled client asking for
    /// zero, or for four billion, gets a picture rather than a panic.
    pub fn bins(self) -> usize {
        self.display_bins.clamp(DEFAULT_DISPLAY_BINS, MAX_DISPLAY_BINS) as usize
    }

    /// [`Self::rows_per_sec`] as a rate an engine will actually clock at.
    ///
    /// Floored at 1 rather than 0: a waterfall that never advances is not a
    /// setting anyone wants, and zero would be a division by it.
    pub fn rows(self) -> u16 {
        self.rows_per_sec.clamp(1, MAX_ROWS_PER_SEC)
    }
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        SpectrumConfig {
            fft_size: 4096,
            display_bins: DEFAULT_DISPLAY_BINS,
            rows_per_sec: DEFAULT_ROWS_PER_SEC,
            fps: 30,
            avg_tc: 0.2,
            db_floor: -120.0,
            db_ceil: -20.0,
            viewport: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The width is an untrusted number off the network. Every value has to
    /// come back out as something an engine can allocate — and never as
    /// something coarser than sdroxide has always drawn.
    #[test]
    fn any_requested_width_lands_in_range() {
        for want in [0, 1, 255, 2047, 2048, 4096, 8192, 8193, u32::MAX] {
            let n = SpectrumConfig { display_bins: want, ..Default::default() }.bins();
            assert!(
                (DEFAULT_DISPLAY_BINS as usize..=MAX_DISPLAY_BINS as usize).contains(&n),
                "{want} became {n}"
            );
        }
    }

    /// An engine nobody has configured emits what every build before issue
    /// #172 emitted.
    #[test]
    fn the_default_is_the_old_fixed_width() {
        assert_eq!(SpectrumConfig::default().bins(), 2048);
    }

    /// The row clock is an untrusted number too, and one that costs an engine
    /// a pooling pass and a client a texture write every tick.
    #[test]
    fn any_requested_row_rate_lands_in_range() {
        for want in [0, 1, 28, 480, 481, u16::MAX] {
            let n = SpectrumConfig { rows_per_sec: want, ..Default::default() }.rows();
            assert!((1..=MAX_ROWS_PER_SEC).contains(&n), "{want} became {n}");
        }
        assert_eq!(SpectrumConfig::default().rows(), DEFAULT_ROWS_PER_SEC);
    }

    /// A frame's rows are whole rows of its own width, however many there are.
    #[test]
    fn rows_are_counted_in_whole_columns() {
        let mut f = SpectrumFrame {
            seq: 0,
            center_hz: 0.0,
            span_hz: 1.0,
            db_floor: -120.0,
            db_ceil: -20.0,
            bins: vec![0; 2048],
            rows: vec![0; 2048 * 3],
            rows_clocked: true,
        };
        assert_eq!(f.row_count(), 3);
        f.rows.clear();
        assert_eq!(f.row_count(), 0);
        f.bins.clear();
        assert_eq!(f.row_count(), 0, "a frame with no columns has no rows either");
    }
}
