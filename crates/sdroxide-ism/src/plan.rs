//! The EU 868 MHz channel plan the decoder listens on, and which of those
//! channels a given receiver window can actually reach.
//!
//! # Why fixed channels rather than a wideband burst detector
//!
//! Every protocol in scope transmits on a channel fixed by ETSI EN 300 220 and
//! by the manufacturer's choice within it. Detecting a burst anywhere in the
//! band and then going back for the samples means keeping a wideband IQ history
//! long enough to re-extract from — at 2 Msps that is 16 MB a second — and the
//! answer it produces is a channel that was in this table all along. Parking a
//! decoder on each channel costs one [`sdroxide_dsp::Ddc`] per channel and
//! nothing else, and it cannot miss the start of a burst because it was already
//! listening.
//!
//! # Channel rates
//!
//! Each channel's rate is set by the widest protocol on it, not by the
//! narrowest: the same baseband feeds every decoder that shares the channel, so
//! it has to hold the largest occupied bandwidth of the group. Rates are
//! *targets* — [`sdroxide_dsp::Ddc`] snaps to an integer decimation of whatever
//! the window rate turns out to be.

use sdroxide_types::IsmFamily;

/// One channel the decoder can park on.
pub struct Channel {
    /// Centre, in absolute RF Hz.
    pub center_hz: f64,
    /// Target baseband rate, Hz. Set by the widest protocol on the channel.
    pub rate_hz: f64,
    /// What sits here, for the panel.
    pub label: &'static str,
    /// Families with at least one protocol on this channel. A channel none of
    /// the enabled families needs is not opened at all.
    pub families: &'static [IsmFamily],
}

/// The channels, ascending.
///
/// `868.30` and `868.95` are the two that matter most in Europe: the first
/// carries nearly every sensor and the wireless M-Bus S mode, the second carries
/// M-Bus T and C, which is what most modern meters use.
pub const CHANNELS: &[Channel] = &[
    Channel {
        center_hz: 868_300_000.0,
        // Widest here is wM-Bus mode S: ±50 kHz deviation at 32.768 kcps, so
        // about 150 kHz occupied. 250 kHz leaves room for the sensors' own
        // crystal error, which on a battery-powered thermometer is not small.
        rate_hz: 250_000.0,
        label: "sensors, wM-Bus S, KNX-RF, EnOcean, Homematic",
        families: &[IsmFamily::Weather, IsmFamily::Meter, IsmFamily::HomeAuto],
    },
    Channel {
        center_hz: 868_420_000.0,
        // Z-Wave's slowest rate — the only one decoded here — is 19 200 chips a
        // second at about ±20 kHz, so it occupies well under 100 kHz.
        //
        // This was 400 kHz, sized for Z-Wave's 100 kbps rate at ±58 kHz, and that
        // width made the 9.6 kbit/s traffic *undecodable*: deviation and symbol
        // rate are measured across the whole channel, so a wideband neighbour
        // three hundred kilohertz away dominated both. The same bursts that read
        // 22.9 kHz deviation and 19 191 baud through a 150 kHz window read 160 to
        // 191 kHz and no measurable rate through a 400 kHz one.
        //
        // So the channel is as narrow as the traffic that is actually read here.
        // Adding the faster Z-Wave rates later means a second, wider channel on
        // this centre rather than widening this one back — see the note on
        // `Channel::rate_hz`.
        rate_hz: 150_000.0,
        label: "Z-Wave EU",
        families: &[IsmFamily::HomeAuto],
    },
    Channel {
        center_hz: 868_950_000.0,
        // wM-Bus T and C: 100 kcps at ±50 kHz.
        rate_hz: 500_000.0,
        label: "wM-Bus T / C",
        families: &[IsmFamily::Meter],
    },
    Channel {
        center_hz: 869_525_000.0,
        rate_hz: 125_000.0,
        label: "wM-Bus N, Homematic long range",
        families: &[IsmFamily::Meter, IsmFamily::HomeAuto],
    },
];

/// Where the receiver would ideally be tuned: the centre that fits the whole
/// plan into the least bandwidth.
///
/// Not the midpoint of the channel centres — the midpoint of their *edges*, so
/// that the outermost channels' bandwidths fit too. Worth stating as code
/// rather than as a constant, because adding a channel to the table above has
/// to move it.
pub fn ideal_center_hz() -> f64 {
    let lo = CHANNELS.iter().map(|c| c.center_hz - c.rate_hz / 2.0).fold(f64::INFINITY, f64::min);
    let hi =
        CHANNELS.iter().map(|c| c.center_hz + c.rate_hz / 2.0).fold(f64::NEG_INFINITY, f64::max);
    (lo + hi) / 2.0
}

/// How much bandwidth the whole plan needs.
pub fn span_hz() -> f64 {
    let lo = CHANNELS.iter().map(|c| c.center_hz - c.rate_hz / 2.0).fold(f64::INFINITY, f64::min);
    let hi =
        CHANNELS.iter().map(|c| c.center_hz + c.rate_hz / 2.0).fold(f64::NEG_INFINITY, f64::max);
    hi - lo
}

/// Fraction of a receiver's sample rate that is actually usable.
///
/// Not a safety margin picked by feel. The RX-888's VHF path reaches 868 MHz
/// through its tuner and delivers the wideband downconverter's 2.025 Msps
/// output, whose band edges are deliberately tapered over the outermost eighth
/// at each end — see `EDGE_TAPER` in [`sdroxide_dsp::WbDdc`]. That leaves
/// exactly three quarters flat, and a channel placed in the taper is attenuated
/// by the very filter that selected it. Other front ends have an
/// anti-aliasing roll-off in roughly the same place, so the figure is right for
/// them too, if slightly conservative.
pub const USABLE_FRACTION: f64 = 0.75;

/// Whether `ch` fits inside a window of `window_rate_hz` centred on
/// `window_center_hz`, allowing for the unusable edges.
pub fn fits(ch: &Channel, window_center_hz: f64, window_rate_hz: f64) -> bool {
    let half_usable = window_rate_hz * USABLE_FRACTION / 2.0;
    let lo = window_center_hz - half_usable;
    let hi = window_center_hz + half_usable;
    ch.center_hz - ch.rate_hz / 2.0 >= lo && ch.center_hz + ch.rate_hz / 2.0 <= hi
}

/// Where to centre the decoder's window, given where the front end is tuned and
/// how much of the spectrum it hands over.
///
/// Ideally [`ideal_center_hz`], but the window has to stay inside the IQ the
/// receiver actually delivers: it is a decimation of that stream, not a second
/// tuner. So the ideal centre is clamped into what the hardware centre can
/// reach. On a receiver tuned well away from 868 MHz the result reaches no
/// channel at all, which is the honest outcome — [`fits`] then rejects
/// everything and the panel says so.
pub fn window_center_hz(hw_center_hz: f64, hw_rate_hz: f64, window_rate_hz: f64) -> f64 {
    window_center_for(ideal_center_hz(), hw_center_hz, hw_rate_hz, window_rate_hz)
}

/// As [`window_center_hz`], but aiming somewhere other than the native plan.
pub fn window_center_for(
    want_hz: f64,
    hw_center_hz: f64,
    hw_rate_hz: f64,
    window_rate_hz: f64,
) -> f64 {
    // The window is centred by an NCO offset inside the hardware span, so its
    // own edges must stay inside the usable part of that span.
    let slack = (hw_rate_hz * USABLE_FRACTION - window_rate_hz) / 2.0;
    if slack <= 0.0 {
        return hw_center_hz;
    }
    want_hz.clamp(hw_center_hz - slack, hw_center_hz + slack)
}

/// Where the decoder's one window should sit and how wide it should be.
pub struct WindowPlan {
    /// What to aim the window at, before the hardware span clamps it.
    pub want_center_hz: f64,
    /// The bandwidth to ask the downconverter for.
    pub target_rate_hz: f64,
}

/// Choose the window from what is switched on.
///
/// There is one window and two lanes that may want different parts of the
/// spectrum — the native decoders are fixed on the European 868 MHz channels,
/// while rtl_433 can be pointed at any of four bands hundreds of megahertz
/// apart. A window aimed at 868 would never reach a 433 MHz band, so the aim
/// point cannot be a constant any more.
///
/// Candidates are scored by how many enabled lanes they would actually serve,
/// so a receiver wide enough for both the native plan and rtl_433's 868 band
/// gets the window that covers both. Ties go to the native plan, then to band
/// order. Whatever is left uncovered is reported as such, with somewhere to
/// tune — which is the honest outcome when no front end spans both bands.
pub fn window_plan(
    cfg: &sdroxide_types::IsmSettings,
    hw_center_hz: f64,
    hw_rate_hz: f64,
) -> Option<WindowPlan> {
    let mut candidates: Vec<(f64, f64)> = Vec::new(); // (want_center, target_rate)

    if cfg.native_enabled() {
        candidates.push((ideal_center_hz(), span_hz() / USABLE_FRACTION));
    }

    #[cfg(feature = "rtl433")]
    if cfg.rtl433_enabled() {
        for b in crate::rtl433::bands::all() {
            if cfg.rtl433.band_enabled(b.bit) {
                candidates.push((b.center_hz, b.rate_hz / USABLE_FRACTION));
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // Never ask for more than the front end delivers: the window is a
    // decimation of that stream, not a second tuner.
    for c in &mut candidates {
        c.1 = c.1.min(hw_rate_hz);
    }

    let score = |want: f64, rate: f64| -> u32 {
        let center = window_center_for(want, hw_center_hz, hw_rate_hz, rate);
        let mut n = 0;
        if cfg.native_enabled() && CHANNELS.iter().any(|ch| fits(ch, center, rate)) {
            n += 1;
        }
        #[cfg(feature = "rtl433")]
        if cfg.rtl433_enabled() {
            for b in crate::rtl433::bands::all() {
                if cfg.rtl433.band_enabled(b.bit) && crate::rtl433::bands::fits(b, center, rate) {
                    n += 1;
                }
            }
        }
        n
    };

    let best = candidates
        .iter()
        .enumerate()
        .max_by_key(|(i, (want, rate))| {
            // Higher score first; earlier candidate wins a tie, and the native
            // plan is first in the list when it is enabled at all.
            (score(*want, *rate), std::cmp::Reverse(*i))
        })
        .map(|(_, c)| *c)?;

    Some(WindowPlan { want_center_hz: best.0, target_rate_hz: best.1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RX-888 reaching 868 MHz through its tuner, which is the receiver
    /// this was developed against: the wideband downconverter's output rate.
    const RX888_VHF_RATE: f64 = 2_025_000.0;

    #[test]
    fn the_channels_are_ascending_and_have_sane_rates() {
        for w in CHANNELS.windows(2) {
            assert!(w[0].center_hz < w[1].center_hz, "channel table is not sorted");
        }
        for c in CHANNELS {
            assert!(c.rate_hz >= 100_000.0 && c.rate_hz <= 1_000_000.0, "{} Hz", c.rate_hz);
            assert!(!c.families.is_empty(), "a channel nothing listens on");
        }
    }

    /// The plan has to fit what the RX-888's VHF path can deliver, or the
    /// receiver this was written for reaches only part of its own channel
    /// table. This is the constraint that set the rates above.
    #[test]
    fn the_whole_plan_fits_the_rx888_vhf_window() {
        let usable = RX888_VHF_RATE * USABLE_FRACTION;
        assert!(
            span_hz() <= usable,
            "the plan needs {:.0} kHz but only {:.0} kHz is usable",
            span_hz() / 1e3,
            usable / 1e3
        );

        // And with the window placed where it belongs, every channel is in.
        let center = ideal_center_hz();
        for c in CHANNELS {
            assert!(fits(c, center, RX888_VHF_RATE), "{:.3} MHz missed", c.center_hz / 1e6);
        }
    }

    /// On a receiver whose whole IQ stream is barely wider than the plan there is
    /// no room to slide a window inside it, so the window *is* the stream and the
    /// operator has to tune to the right place. Saying so is the point: the panel
    /// turns this into "tune to 868.88 MHz" rather than "no devices found".
    #[test]
    fn a_receiver_with_no_slack_pins_the_window_to_its_own_centre() {
        // The window can be no narrower than the plan, so on the RX-888 the Ddc
        // decimates by one and the window rate is the hardware rate.
        let c = window_center_hz(868_900_000.0, RX888_VHF_RATE, RX888_VHF_RATE);
        assert_eq!(c, 868_900_000.0, "there is no slack to slide into");

        // Tuned close enough, the plan still fits — that is what matters.
        for ch in CHANNELS {
            assert!(fits(ch, c, RX888_VHF_RATE), "{:.3} MHz missed", ch.center_hz / 1e6);
        }

        // Tuned 2 MHz low, it cannot: the channels are simply not in the stream.
        let c = window_center_hz(866_900_000.0, RX888_VHF_RATE, RX888_VHF_RATE);
        assert_eq!(c, 866_900_000.0);
        assert!(
            !CHANNELS.iter().any(|ch| fits(ch, c, RX888_VHF_RATE)),
            "a receiver 2 MHz away should reach none of the plan"
        );
    }

    /// A front end with room to spare puts the window where the plan wants it,
    /// wherever the operator has tuned within reach.
    #[test]
    fn a_wide_front_end_slides_the_window_onto_the_plan() {
        let c = window_center_hz(866_900_000.0, 8_000_000.0, 2_000_000.0);
        assert!((c - ideal_center_hz()).abs() < 1.0, "got {c}, wanted the ideal centre");
        for ch in CHANNELS {
            assert!(fits(ch, c, 2_000_000.0), "{:.3} MHz missed", ch.center_hz / 1e6);
        }
    }

    /// A wide front end can hold the plan wherever it is tuned within reach,
    /// and the clamp must not push the window outside the hardware span.
    #[test]
    fn a_wide_front_end_keeps_the_window_inside_its_own_span() {
        let hw_rate = 8_000_000.0;
        for hw_center in [866_000_000.0, 868_900_000.0, 871_000_000.0] {
            let w = window_center_hz(hw_center, hw_rate, 2_000_000.0);
            let slack = (hw_rate * USABLE_FRACTION - 2_000_000.0) / 2.0;
            assert!(
                (w - hw_center).abs() <= slack + 1.0,
                "window {w} is {} Hz off a centre with only {slack} Hz of slack",
                (w - hw_center).abs()
            );
        }
    }

    /// A narrow receiver — an RTL-SDR at 2.4 Msps has 1.8 MHz usable, which is
    /// enough; at 1 Msps it is not, and `fits` has to say so rather than let a
    /// channel be decoded through the anti-alias roll-off.
    #[test]
    fn a_window_narrower_than_a_channel_admits_nothing() {
        let ch = &CHANNELS[2]; // 868.95 MHz at 500 kHz
        assert!(!fits(ch, ch.center_hz, 500_000.0), "500 kHz has only 375 kHz usable");
        assert!(fits(ch, ch.center_hz, 700_000.0));
    }
}
