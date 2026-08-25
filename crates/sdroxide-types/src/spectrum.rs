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
}

impl SpectrumFrame {
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
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        SpectrumConfig {
            fft_size: 4096,
            display_bins: DEFAULT_DISPLAY_BINS,
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
}
