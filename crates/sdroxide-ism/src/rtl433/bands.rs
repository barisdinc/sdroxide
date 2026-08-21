//! Where the rtl_433 lane can listen.
//!
//! The native decoders sit on a plan of individual channels ([`crate::plan`]),
//! because each protocol there transmits on one known frequency. rtl_433 is the
//! opposite case: hundreds of decoders spread across a whole regional band, none
//! of them worth a channel of its own. So this is a short list of band-wide
//! windows, one live at a time.
//!
//! One at a time because the alternative does not exist in hardware: 433 and 868
//! MHz are 435 MHz apart, and no front end sdroxide drives hands over a span
//! that wide. A band that does not fit the current window is reported as such,
//! with the frequency to tune to.

use crate::plan::USABLE_FRACTION;

/// A band the lane can watch.
pub struct Band {
    /// Which bit of [`sdroxide_types::Rtl433Settings::bands`] switches it on.
    pub bit: u32,
    pub label: &'static str,
    pub center_hz: f64,
    /// What to feed rtl_433 there.
    ///
    /// 250 kHz is rtl_433's own default and is plenty for the OOK remotes and
    /// sensors that fill 433 and 315 MHz. The 868/915 bands carry FSK devices at
    /// higher rates and channel spacing wide enough to need the extra span, so
    /// they get 1024 kHz — also an rtl_433 convention.
    pub rate_hz: f64,
}

pub const BANDS: &[Band] = &[
    Band { bit: 1 << 0, label: "433.92 MHz", center_hz: 433_920_000.0, rate_hz: 250_000.0 },
    // Centred between the two busy European sub-bands (868.30 and 868.95) so one
    // window reaches both, which is also where this crate's native channel plan
    // sits.
    Band { bit: 1 << 1, label: "868 MHz EU", center_hz: 868_650_000.0, rate_hz: 1_024_000.0 },
    Band { bit: 1 << 2, label: "915 MHz US", center_hz: 915_000_000.0, rate_hz: 1_024_000.0 },
    Band { bit: 1 << 3, label: "315 MHz US", center_hz: 315_000_000.0, rate_hz: 250_000.0 },
];

/// Every band, whether switched on or not — for drawing the settings chips.
pub fn all() -> &'static [Band] {
    BANDS
}

pub fn by_bit(bit: u32) -> Option<&'static Band> {
    BANDS.iter().find(|b| b.bit == bit)
}

/// Whether `b` fits inside a window of `win_rate_hz` centred on `win_center_hz`.
///
/// Same usable-fraction reasoning as the native plan: the outer eighth at each
/// end of a decimated window is in the anti-aliasing taper, and a band placed
/// there is attenuated by the filter that selected it.
pub fn fits(b: &Band, win_center_hz: f64, win_rate_hz: f64) -> bool {
    let half_usable = win_rate_hz * USABLE_FRACTION / 2.0;
    let lo = win_center_hz - half_usable;
    let hi = win_center_hz + half_usable;
    b.center_hz - b.rate_hz / 2.0 >= lo && b.center_hz + b.rate_hz / 2.0 <= hi
}

/// The enabled band to run, given where the window ended up.
///
/// Nearest to the window centre among those that fit, so that on a front end
/// wide enough for two the choice is the one least likely to be sitting in a
/// roll-off.
pub fn pick(mask: u32, win_center_hz: f64, win_rate_hz: f64) -> Option<&'static Band> {
    BANDS.iter().filter(|b| mask & b.bit != 0 && fits(b, win_center_hz, win_rate_hz)).min_by(
        |a, b| {
            let da = (a.center_hz - win_center_hz).abs();
            let db = (b.center_hz - win_center_hz).abs();
            da.total_cmp(&db)
        },
    )
}

/// The widest span any enabled band needs, for sizing the window before one is
/// chosen. `None` when nothing is enabled.
pub fn needed_rate_hz(mask: u32) -> Option<f64> {
    BANDS
        .iter()
        .filter(|b| mask & b.bit != 0)
        .map(|b| b.rate_hz)
        .fold(None, |acc: Option<f64>, r| Some(acc.map_or(r, |a| a.max(r))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_distinct_and_ordered() {
        for (i, b) in BANDS.iter().enumerate() {
            assert_eq!(b.bit, 1 << i, "{} is out of order", b.label);
        }
    }

    #[test]
    fn a_band_fits_its_own_window_with_room_to_spare() {
        for b in BANDS {
            // A window exactly as wide as the band cannot hold it: only three
            // quarters of a window is usable.
            assert!(!fits(b, b.center_hz, b.rate_hz), "{} fits an exact window", b.label);
            assert!(fits(b, b.center_hz, b.rate_hz / USABLE_FRACTION), "{}", b.label);
        }
    }

    #[test]
    fn picks_the_nearest_enabled_band() {
        // A window over 868 with everything enabled must not pick 433.
        let all = BANDS.iter().fold(0, |m, b| m | b.bit);
        let picked = pick(all, 868_650_000.0, 2_025_000.0).expect("868 fits");
        assert_eq!(picked.label, "868 MHz EU");
    }

    #[test]
    fn nothing_fits_a_window_on_another_band() {
        let all = BANDS.iter().fold(0, |m, b| m | b.bit);
        assert!(pick(all, 145_000_000.0, 2_025_000.0).is_none());
    }

    #[test]
    fn disabled_bands_are_not_picked() {
        let only_433 = BANDS[0].bit;
        assert!(pick(only_433, 868_650_000.0, 2_025_000.0).is_none());
    }
}
