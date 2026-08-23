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

use sdroxide_types::Rtl433Settings;

use crate::plan::USABLE_FRACTION;

/// A band the lane can watch.
pub struct Band {
    /// Which bit of [`sdroxide_types::Rtl433Settings::bands`] switches it on.
    pub bit: u32,
    pub label: &'static str,
    pub center_hz: f64,
    /// What to feed rtl_433 there, unless the operator has asked for something
    /// else — see [`rate_for`].
    ///
    /// 250 kHz is rtl_433's own default and is plenty for the OOK remotes and
    /// sensors that fill 315, 345 and 433 MHz. The 868/915 bands carry FSK
    /// devices at higher rates and channel spacing wide enough to need the extra
    /// span, so they get 1024 kHz — also an rtl_433 convention.
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
    // The North American security band: Honeywell/Ademco and 2GIG door and
    // window contacts, glass-break detectors and Vivint sensors. All OOK at
    // around 140 us a half-bit, so rtl_433's own quarter-megahertz is ample.
    Band { bit: 1 << 4, label: "345 MHz US", center_hz: 345_000_000.0, rate_hz: 250_000.0 },
];

/// How wide a window this band gets, given the settings.
///
/// The band's own figure unless the operator has chosen a width, in which case
/// theirs — a narrower one to fit a receiver that cannot deliver the band's
/// default, or a wider one to reach a device sitting further off the band centre
/// than the default covers.
pub fn rate_for(b: &Band, cfg: &Rtl433Settings) -> f64 {
    cfg.bandwidth_for(b.rate_hz)
}

/// Every band, whether switched on or not — for drawing the settings chips.
pub fn all() -> &'static [Band] {
    BANDS
}

pub fn by_bit(bit: u32) -> Option<&'static Band> {
    BANDS.iter().find(|b| b.bit == bit)
}

/// Whether a band `band_rate_hz` wide fits inside a window of `win_rate_hz`
/// centred on `win_center_hz`.
///
/// Same usable-fraction reasoning as the native plan: the outer eighth at each
/// end of a decimated window is in the anti-aliasing taper, and a band placed
/// there is attenuated by the filter that selected it.
///
/// The band's width is a parameter rather than read off `b`, because the
/// operator can choose it: a request for more than the receiver hands over has
/// to fail here rather than be quietly satisfied through the taper.
pub fn fits_at(b: &Band, band_rate_hz: f64, win_center_hz: f64, win_rate_hz: f64) -> bool {
    let half_usable = win_rate_hz * USABLE_FRACTION / 2.0;
    let lo = win_center_hz - half_usable;
    let hi = win_center_hz + half_usable;
    b.center_hz - band_rate_hz / 2.0 >= lo && b.center_hz + band_rate_hz / 2.0 <= hi
}

/// As [`fits_at`], at the width the settings ask for.
pub fn fits(b: &Band, cfg: &Rtl433Settings, win_center_hz: f64, win_rate_hz: f64) -> bool {
    fits_at(b, rate_for(b, cfg), win_center_hz, win_rate_hz)
}

/// The enabled band to run, given where the window ended up.
///
/// Nearest to the window centre among those that fit, so that on a front end
/// wide enough for two the choice is the one least likely to be sitting in a
/// roll-off.
pub fn pick(cfg: &Rtl433Settings, win_center_hz: f64, win_rate_hz: f64) -> Option<&'static Band> {
    BANDS
        .iter()
        .filter(|b| cfg.band_enabled(b.bit) && fits(b, cfg, win_center_hz, win_rate_hz))
        .min_by(|a, b| {
            let da = (a.center_hz - win_center_hz).abs();
            let db = (b.center_hz - win_center_hz).abs();
            da.total_cmp(&db)
        })
}

/// Whether an enabled band would have fitted at its own default width, when
/// none fits at the width the operator asked for.
///
/// The difference between "you are tuned somewhere else" and "you asked for a
/// wider window than this receiver can give", which are the two ways the lane
/// goes quiet and want opposite things done about them.
pub fn fits_at_default(cfg: &Rtl433Settings, win_center_hz: f64, win_rate_hz: f64) -> bool {
    BANDS
        .iter()
        .any(|b| cfg.band_enabled(b.bit) && fits_at(b, b.rate_hz, win_center_hz, win_rate_hz))
}

/// The widest span any enabled band needs, for sizing the window before one is
/// chosen. `None` when nothing is enabled.
pub fn needed_rate_hz(cfg: &Rtl433Settings) -> Option<f64> {
    BANDS
        .iter()
        .filter(|b| cfg.band_enabled(b.bit))
        .map(|b| rate_for(b, cfg))
        .fold(None, |acc: Option<f64>, r| Some(acc.map_or(r, |a| a.max(r))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every band on, at each band's own width.
    fn auto() -> Rtl433Settings {
        Rtl433Settings {
            bands: BANDS.iter().fold(0, |m, b| m | b.bit),
            bandwidth_hz: sdroxide_types::RTL433_BANDWIDTH_AUTO,
        }
    }

    #[test]
    fn bits_are_distinct_and_ordered() {
        for (i, b) in BANDS.iter().enumerate() {
            assert_eq!(b.bit, 1 << i, "{} is out of order", b.label);
        }
    }

    #[test]
    fn a_band_fits_its_own_window_with_room_to_spare() {
        let cfg = auto();
        for b in BANDS {
            // A window exactly as wide as the band cannot hold it: only three
            // quarters of a window is usable.
            assert!(!fits(b, &cfg, b.center_hz, b.rate_hz), "{} fits an exact window", b.label);
            assert!(fits(b, &cfg, b.center_hz, b.rate_hz / USABLE_FRACTION), "{}", b.label);
        }
    }

    #[test]
    fn picks_the_nearest_enabled_band() {
        // A window over 868 with everything enabled must not pick 433.
        let picked = pick(&auto(), 868_650_000.0, 2_025_000.0).expect("868 fits");
        assert_eq!(picked.label, "868 MHz EU");
    }

    #[test]
    fn nothing_fits_a_window_on_another_band() {
        assert!(pick(&auto(), 145_000_000.0, 2_025_000.0).is_none());
    }

    #[test]
    fn disabled_bands_are_not_picked() {
        let only_433 = Rtl433Settings { bands: BANDS[0].bit, ..auto() };
        assert!(pick(&only_433, 868_650_000.0, 2_025_000.0).is_none());
    }

    /// The security band the ticket asked for (issue #141), and the two
    /// decoders that live on it.
    #[test]
    fn the_345_band_is_offered_and_matches_the_published_labels() {
        let b = BANDS.iter().find(|b| b.center_hz == 345_000_000.0).expect("345 MHz");
        assert_eq!(b.rate_hz, 250_000.0, "the OOK security sensors need no more");
        // The panel draws its chips from the types crate's copy of this table
        // and never links any DSP, so the two have to say the same thing.
        assert_eq!(sdroxide_types::RTL433_BAND_LABELS.len(), BANDS.len());
        for ((bit, label, center), b) in sdroxide_types::RTL433_BAND_LABELS.iter().zip(BANDS.iter())
        {
            assert_eq!((*bit, *label, *center), (b.bit, b.label, b.center_hz));
        }
    }

    /// A chosen width replaces the band's own, in both directions.
    #[test]
    fn the_operator_s_width_overrides_the_band_s_own() {
        let narrow = Rtl433Settings { bandwidth_hz: 250_000, ..auto() };
        let eu = by_bit(1 << 1).expect("868");
        assert_eq!(rate_for(eu, &narrow), 250_000.0);
        // And a receiver too narrow for the band's default reaches it anyway.
        assert!(!fits(eu, &auto(), eu.center_hz, 1_000_000.0));
        assert!(fits(eu, &narrow, eu.center_hz, 1_000_000.0));

        let wide = Rtl433Settings { bandwidth_hz: 1_024_000, ..auto() };
        let us315 = by_bit(1 << 3).expect("315");
        assert_eq!(rate_for(us315, &wide), 1_024_000.0);
        assert_eq!(needed_rate_hz(&wide), Some(1_024_000.0));
        // Asked for more than the receiver gives, nothing fits — and the lane
        // can tell that apart from being tuned to the wrong band.
        assert!(pick(&wide, us315.center_hz, 500_000.0).is_none());
        assert!(fits_at_default(&wide, us315.center_hz, 500_000.0));
        assert!(!fits_at_default(&wide, 145_000_000.0, 500_000.0));
    }

    /// A hand-edited `ism.json` cannot ask for a window so narrow that building
    /// it would mean a decimation of hundreds of thousands.
    #[test]
    fn an_absurd_width_is_floored() {
        let silly = Rtl433Settings { bandwidth_hz: 3, ..auto() };
        let eu = by_bit(1 << 1).expect("868");
        assert_eq!(rate_for(eu, &silly), f64::from(sdroxide_types::RTL433_BANDWIDTH_MIN_HZ));
    }
}
