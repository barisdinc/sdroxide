//! Which front end a dial setting asks for, and what the downconverter has to
//! do about it.
//!
//! All arithmetic, no I/O: the awkward parts of the VHF path are the crossover,
//! the two-stage tuning and the spectrum inversion, and every one of them is a
//! decision that can be made — and tested — without a receiver attached.
//!
//! # Coarse LO, fine downconverter
//!
//! The obvious design pins the downconverter on the tuner's IF and lets the
//! dial ride entirely on the tuner. It is unaffordable. An R828D retune sleeps
//! up to 20 ms waiting for PLL lock, on the thread servicing the bulk endpoint,
//! which holds about 32 ms of samples — and dragging the panadapter emits
//! retunes as fast as that thread can spin.
//!
//! So the tuner parks and the downconverter slides inside the IF passband:
//! small dial movements cost nothing, exactly as on HF, and the tuner only
//! follows when the dial leaves the window.

/// The R828D's IF carrier with the 8 MHz filter selected.
///
/// Not chosen here — it is what the tuner driver's `set_bandwidth(8 MHz)`
/// reports, and upstream's `R828D_IF_CARRIER` states the same figure.
pub const IF_CENTER_HZ: f64 = 4_570_000.0;

/// The IF filter's width in that mode, and so the width of the VHF full-band
/// display.
pub const IF_BW_HZ: f64 = 8_000_000.0;

/// The most the downconverter may slide either side of the IF carrier before
/// the tuner has to follow.
///
/// ±1 MHz keeps the whole default 2.025 MHz output inside the flat part of the
/// filter and well clear of the ADC's own DC region. It exists so that small
/// dial movements stay free — see the note on this module. A wider output has
/// less room to slide in; [`fine_span_hz`] is the width that actually applies.
pub const FINE_SPAN_HZ: f64 = 1_000_000.0;

/// How far the downconverter may actually slide, for this clock and output
/// width.
///
/// The downconverter's centre can only reach `out/2 .. Nyquist − out/2` — the
/// selected band has to fit inside the real half-spectrum — so a wide output
/// parked on the IF has little or no slide left before one edge would fall
/// off. The window is whatever slide keeps the whole output reachable, capped
/// at [`FINE_SPAN_HZ`]; at zero the tuner simply follows every dial move.
pub fn fine_span_hz(adc_rate_hz: f64, out_rate_hz: f64) -> f64 {
    let lo_room = IF_CENTER_HZ - out_rate_hz / 2.0;
    let hi_room = adc_rate_hz / 2.0 - out_rate_hz / 2.0 - IF_CENTER_HZ;
    FINE_SPAN_HZ.min(lo_room).min(hi_room).max(0.0)
}

/// The tuner cannot reach below this, so the automatic crossover never hands it
/// a frequency it will not lock on. `librtlsdr` publishes the same floor.
pub const TUNER_MIN_HZ: f64 = 24_000_000.0;
/// Top of the R828D's range.
pub const TUNER_MAX_HZ: f64 = 1_766_000_000.0;

/// Hysteresis at the crossover, so a dial parked on it cannot oscillate between
/// the two front ends. Each switch costs a tuner bring-up and a gap in the
/// stream, which is the same reason the RTL-SDR backend has one.
pub const CROSSOVER_HYSTERESIS_HZ: f64 = 500_000.0;

/// The top of the IF passband, which is what has to fit under the ADC's
/// Nyquist limit for VHF to be possible at all.
pub const IF_TOP_HZ: f64 = IF_CENTER_HZ + IF_BW_HZ / 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Hf,
    Vhf,
}

/// Where the front end is now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandState {
    pub band: Band,
    /// The RF frequency the tuner is parked on. Meaningless in HF.
    pub lo_dial_hz: f64,
}

impl Default for BandState {
    fn default() -> Self {
        BandState { band: Band::Hf, lo_dial_hz: 0.0 }
    }
}

/// What a dial setting asks of the hardware.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandPlan {
    pub band: Band,
    pub lo_dial_hz: f64,
    /// Whether the tuner's PLL has to be reprogrammed — the expensive bit.
    pub move_lo: bool,
    /// Where the wideband downconverter must be centred, in ADC baseband Hz.
    pub ddc_center_hz: f64,
    /// Whether the front end mirrors the spectrum.
    pub conjugate: bool,
}

/// Whether this ADC clock leaves room for the tuner's IF under Nyquist.
///
/// Derived rather than declared: the 8 MHz IF filter reaches up to
/// [`IF_TOP_HZ`], and sampling below twice that folds the top of the passband
/// back over the wanted signal. At 16.2 Msps it does not fit; from 32.4 Msps up
/// it does, with room to spare.
pub fn adc_rate_allows_vhf(adc_rate_hz: f64) -> bool {
    adc_rate_hz / 2.0 > IF_TOP_HZ
}

/// Whether the VHF front end can be used at all.
///
/// Two conditions, both hard: a tuner soldered in, and an ADC clock with room
/// for its IF under Nyquist. The output width is *not* one of them: an output
/// too wide to centre on the IF carrier still contains it — the converter
/// clamps the centre into the half-spectrum and the IF rides off-centre in the
/// output, which [`achieved_dial_center_hz`] reports so the engine demodulates
/// in the right place.
pub fn vhf_available(adc_rate_hz: f64, has_tuner: bool) -> bool {
    has_tuner && adc_rate_allows_vhf(adc_rate_hz)
}

/// Where the automatic switch happens.
///
/// Upstream's rule is the ADC's Nyquist limit, which is where direct sampling
/// runs out. Below a 48 Msps clock that lands under the tuner's own floor, so
/// the floor wins and the receiver has an honest gap between its two ranges
/// rather than a band it claims and cannot hear.
pub fn crossover_hz(adc_rate_hz: f64) -> f64 {
    (adc_rate_hz / 2.0).max(TUNER_MIN_HZ)
}

/// The ranges this receiver can actually tune, for `DeviceCaps`.
pub fn freq_ranges(adc_rate_hz: f64, vhf: bool) -> Vec<(f64, f64)> {
    let nyquist = adc_rate_hz / 2.0;
    let hf = (0.0, nyquist);
    if !vhf {
        return vec![hf];
    }
    let x = crossover_hz(adc_rate_hz);
    // When the crossover sits above Nyquist the two ranges do not meet, and
    // saying so is the point: that gap is real and unreachable.
    vec![hf, (x, TUNER_MAX_HZ)]
}

/// Work out what `dial_hz` requires, given where the front end is now.
///
/// `out_rate_hz` — the downconverter's output width — sets how far the
/// downconverter may slide before the tuner has to follow; see
/// [`fine_span_hz`].
pub fn plan(
    dial_hz: f64,
    adc_rate_hz: f64,
    out_rate_hz: f64,
    vhf: bool,
    cur: BandState,
) -> BandPlan {
    let hf_plan = |move_lo: bool| BandPlan {
        band: Band::Hf,
        lo_dial_hz: 0.0,
        move_lo,
        ddc_center_hz: dial_hz,
        conjugate: false,
    };

    if !vhf {
        return hf_plan(cur.band == Band::Vhf);
    }

    // Cross only once the dial is clearly on the other side, so a dial parked
    // on the boundary cannot flip the front end back and forth.
    let x = crossover_hz(adc_rate_hz);
    let want_vhf = match cur.band {
        Band::Hf => dial_hz >= x + CROSSOVER_HYSTERESIS_HZ,
        Band::Vhf => dial_hz >= x - CROSSOVER_HYSTERESIS_HZ,
    };
    if !want_vhf {
        return hf_plan(cur.band == Band::Vhf);
    }

    let entering = cur.band == Band::Hf;
    let left_window = (dial_hz - cur.lo_dial_hz).abs() > fine_span_hz(adc_rate_hz, out_rate_hz);
    let move_lo = entering || left_window;
    let lo = if move_lo { dial_hz.clamp(TUNER_MIN_HZ, TUNER_MAX_HZ) } else { cur.lo_dial_hz };

    BandPlan {
        band: Band::Vhf,
        lo_dial_hz: lo,
        move_lo,
        // High-side injection: the tuner's LO sits *above* the wanted signal,
        // so the IF runs backwards and the downconverter has to move down as
        // the dial moves up. This sign and `conjugate` are the same physical
        // fact — get one right and the other wrong and the radio tunes
        // backwards while looking like it works.
        ddc_center_hz: IF_CENTER_HZ - (dial_hz - lo),
        conjugate: true,
    }
}

/// Where a plan's stream centre actually lands, on the dial axis.
///
/// The selected band has to fit inside the real half-spectrum, so the
/// converter clamps the plan's DDC centre to `out/2 .. Nyquist − out/2` — the
/// same arithmetic as `WbDdc::set_center_hz`, kept here so the source can know
/// *synchronously* where the stream really ended up. A dial inside the clamped
/// strip is still received — it just sits off-centre in the output — and the
/// whole point of this function is to say where the centre truly is rather
/// than let the engine demodulate against a centre the converter never took.
///
/// On HF the DDC centre *is* the dial axis. On VHF it is an IF, reflected
/// through the tuner's park frequency exactly as [`wide_map`] reflects the
/// display: an output too wide to park on the IF carrier gets its centre
/// pinned at `out/2`, and the dial rides `out/2 − IF_CENTER` above the
/// reported centre. At a full-Nyquist output the clamp's two bounds meet and
/// every tune lands on a quarter of the clock.
pub fn achieved_dial_center_hz(p: &BandPlan, adc_rate_hz: f64, out_rate_hz: f64) -> f64 {
    let lo = out_rate_hz / 2.0;
    let hi = (adc_rate_hz / 2.0 - out_rate_hz / 2.0).max(lo);
    let c = p.ddc_center_hz.clamp(lo, hi);
    match p.band {
        Band::Hf => c,
        Band::Vhf => p.lo_dial_hz + (IF_CENTER_HZ - c),
    }
}

/// Which analyser bins to publish for the full-band display, and the RF axis
/// they sit on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WideMap {
    pub lo_bin: usize,
    pub hi_bin: usize,
    /// Whether the slice ascends in RF, or has to be reversed first.
    pub reverse: bool,
    pub center_hz: f64,
    pub span_hz: f64,
}

/// Map the analyser's frame onto the axis the front end is actually on.
///
/// In HF the analyser's own axis is the answer: DC to Nyquist. In VHF it is
/// not — the analyser is looking at the tuner's IF, so the display is the slice
/// of bins covering the 8 MHz filter, reversed because ascending IF is
/// descending RF, and labelled with the RF the tuner is parked on.
///
/// The centre and span are derived from the bins that survive the clamp rather
/// than from the nominal window, so a slice cut short at the band edge still
/// carries a truthful axis.
pub fn wide_map(band: Band, lo_dial_hz: f64, adc_rate_hz: f64, bins: usize) -> WideMap {
    if band == Band::Hf {
        return WideMap {
            lo_bin: 0,
            hi_bin: bins,
            reverse: false,
            center_hz: adc_rate_hz / 4.0,
            span_hz: adc_rate_hz / 2.0,
        };
    }

    // The analyser's `bins` cover DC..Nyquist.
    let bin_hz = adc_rate_hz / 2.0 / bins as f64;
    let lo_bin = (((IF_CENTER_HZ - IF_BW_HZ / 2.0) / bin_hz).floor().max(0.0) as usize).min(bins);
    let hi_bin = ((((IF_CENTER_HZ + IF_BW_HZ / 2.0) / bin_hz).ceil() as usize) + 1).min(bins);
    let hi_bin = hi_bin.max(lo_bin);

    // Centre of the bins actually taken, expressed as an IF, then reflected
    // through the tuner's park frequency into RF.
    let mid_if = bin_hz * (lo_bin + hi_bin) as f64 / 2.0;
    WideMap {
        lo_bin,
        hi_bin,
        reverse: true,
        center_hz: lo_dial_hz + (IF_CENTER_HZ - mid_if),
        span_hz: bin_hz * (hi_bin - lo_bin) as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADC: f64 = 64_800_000.0;
    /// The default downconverter output: 256 of 8192 bins.
    const OUT: f64 = 2_025_000.0;

    fn vhf_state(lo: f64) -> BandState {
        BandState { band: Band::Vhf, lo_dial_hz: lo }
    }

    /// A dial parked on the crossover must not flip the front end back and
    /// forth. Each switch is a tuner bring-up and a gap in the stream.
    #[test]
    fn the_crossover_does_not_oscillate_on_the_boundary() {
        let x = crossover_hz(ADC);
        let mut st = BandState::default();
        let mut changes = 0;

        // Walk up through the crossover and back down again, feeding each
        // decision back in as the current state — which is what actually
        // happens when someone drags the dial.
        let steps: Vec<f64> =
            (-200..=200).chain((-200..=200).rev()).map(|k| x + f64::from(k) * 10_000.0).collect();
        for hz in steps {
            let p = plan(hz, ADC, OUT, true, st);
            if p.band != st.band {
                changes += 1;
            }
            st = BandState { band: p.band, lo_dial_hz: p.lo_dial_hz };
        }

        assert!(changes <= 2, "the front end switched {changes} times over one sweep and back");
    }

    /// The crossover must never land somewhere the tuner cannot reach, or the
    /// receiver would switch to VHF below the tuner's floor and hear nothing.
    #[test]
    fn the_crossover_never_lands_below_the_tuners_floor() {
        for rate in crate::device::ADC_RATES {
            assert!(
                crossover_hz(*rate) >= TUNER_MIN_HZ,
                "a {rate} Hz clock crosses over at {}",
                crossover_hz(*rate)
            );
        }
    }

    /// Small dial movements must stay free — the whole reason the
    /// downconverter slides instead of the tuner.
    #[test]
    fn the_tuner_only_moves_when_the_dial_leaves_the_window() {
        let st = vhf_state(145_000_000.0);

        for delta in [0.0, 500_000.0, -900_000.0, FINE_SPAN_HZ] {
            let p = plan(145_000_000.0 + delta, ADC, OUT, true, st);
            assert!(!p.move_lo, "a {delta} Hz nudge reprogrammed the PLL");
            assert_eq!(p.lo_dial_hz, 145_000_000.0);
            // The downconverter took up the slack instead.
            assert!((p.ddc_center_hz - (IF_CENTER_HZ - delta)).abs() < 1.0);
        }

        // Past the window the tuner has to follow, and the downconverter goes
        // back to the middle of the IF.
        let p = plan(146_100_000.0, ADC, OUT, true, st);
        assert!(p.move_lo);
        assert_eq!(p.lo_dial_hz, 146_100_000.0);
        assert!((p.ddc_center_hz - IF_CENTER_HZ).abs() < 1.0);
    }

    /// The sliding downconverter must not wander outside the IF filter, or the
    /// signal is attenuated by the very filter that selects it.
    #[test]
    fn the_downconverter_stays_inside_the_if_filter() {
        let st = vhf_state(145_000_000.0);
        // The widest output the converter produces, so both edges are checked.
        let half_out = 2_025_000.0 / 2.0;

        for k in -100..=100 {
            let dial = 145_000_000.0 + f64::from(k) * 10_000.0;
            let p = plan(dial, ADC, OUT, true, st);
            assert!((p.ddc_center_hz - IF_CENTER_HZ).abs() <= FINE_SPAN_HZ + 1.0);
            assert!(
                p.ddc_center_hz - half_out > IF_CENTER_HZ - IF_BW_HZ / 2.0,
                "the low edge fell out of the IF filter at {dial}"
            );
            assert!(
                p.ddc_center_hz + half_out < IF_CENTER_HZ + IF_BW_HZ / 2.0,
                "the high edge fell out of the IF filter at {dial}"
            );
        }
    }

    /// The inversion, stated as a test so it cannot be quietly "fixed" by
    /// someone who has not read the comment next to it.
    #[test]
    fn tuning_up_moves_the_downconverter_down() {
        let st = vhf_state(145_000_000.0);
        let lower = plan(144_800_000.0, ADC, OUT, true, st).ddc_center_hz;
        let higher = plan(145_200_000.0, ADC, OUT, true, st).ddc_center_hz;
        assert!(
            higher < lower,
            "high-side injection inverts the IF: {higher} should be below {lower}"
        );
        // And the same fact is reported to the converter.
        assert!(plan(145_200_000.0, ADC, OUT, true, st).conjugate);
        assert!(!plan(10_000_000.0, ADC, OUT, true, st).conjugate, "HF is not inverted");
    }

    #[test]
    fn a_receiver_without_a_tuner_never_leaves_hf() {
        let p = plan(145_000_000.0, ADC, OUT, false, BandState::default());
        assert_eq!(p.band, Band::Hf);
        assert!(!p.conjugate);
        assert_eq!(freq_ranges(ADC, false), vec![(0.0, 32_400_000.0)]);
    }

    /// The IF has to fit under Nyquist, and at the lowest offered clock it does
    /// not.
    #[test]
    fn vhf_needs_an_adc_clock_with_room_for_the_if() {
        assert!(!adc_rate_allows_vhf(16_200_000.0), "8.1 MHz Nyquist cannot hold an 8.57 MHz IF");
        assert!(adc_rate_allows_vhf(32_400_000.0));
        assert!(adc_rate_allows_vhf(64_800_000.0));
        assert!(!vhf_available(64_800_000.0, false), "no tuner, no VHF");
    }

    /// At the default clock the two ranges meet; at a slower one they do not,
    /// and the gap is reported rather than papered over.
    #[test]
    fn the_published_ranges_admit_the_gap_when_there_is_one() {
        assert_eq!(freq_ranges(ADC, true), vec![(0.0, 32_400_000.0), (32_400_000.0, TUNER_MAX_HZ)]);

        let slow = freq_ranges(32_400_000.0, true);
        assert_eq!(slow, vec![(0.0, 16_200_000.0), (24_000_000.0, TUNER_MAX_HZ)]);
        assert!(slow[0].1 < slow[1].0, "16.2–24 MHz is genuinely unreachable here");
    }

    #[test]
    fn the_hf_strip_is_the_whole_nyquist_band() {
        let m = wide_map(Band::Hf, 0.0, ADC, 4096);
        assert_eq!((m.lo_bin, m.hi_bin, m.reverse), (0, 4096, false));
        assert_eq!(m.center_hz, ADC / 4.0);
        assert_eq!(m.span_hz, ADC / 2.0);
    }

    /// A wider output leaves less slide before the tuner must follow; past the
    /// point where the output cannot park on the IF carrier at all, the slide
    /// is zero and the tuner simply follows every dial move — VHF stays
    /// available, with the IF riding off-centre in the output.
    #[test]
    fn a_wide_output_shrinks_the_fine_window_but_keeps_vhf() {
        // Default width: the classic ±1 MHz survives untouched.
        assert_eq!(fine_span_hz(ADC, OUT), FINE_SPAN_HZ);

        // 1024 of 8192 bins at 64.8 Msps: 8.1 MHz out. The IF carrier sits at
        // 4.57 MHz, so only 0.52 MHz of slide keeps the low edge reachable.
        let out_8m1 = 8_100_000.0;
        assert!((fine_span_hz(ADC, out_8m1) - 520_000.0).abs() < 1.0);
        // And the plan respects the shrunken window: a nudge past it moves the
        // tuner where the default width would have slid the downconverter.
        let st = vhf_state(145_000_000.0);
        assert!(!plan(145_400_000.0, ADC, out_8m1, true, st).move_lo);
        assert!(plan(145_700_000.0, ADC, out_8m1, true, st).move_lo);

        // 2048 bins: 16.2 MHz out. out/2 is past the IF carrier entirely, so
        // there is no slide at all — every VHF tune reprograms the PLL.
        assert_eq!(fine_span_hz(ADC, 16_200_000.0), 0.0);
        assert!(plan(145_100_000.0, ADC, 16_200_000.0, true, st).move_lo);
        // Width never rules VHF out; only the tuner and the clock do.
        assert!(vhf_available(ADC, true));
        assert!(!vhf_available(16_200_000.0, true), "the IF does not fit under Nyquist");
    }

    /// The stream centre clamps to what fits in the half-spectrum, reported on
    /// the dial axis for either band.
    #[test]
    fn the_achieved_center_clamps_to_the_half_spectrum() {
        let hf = |dial: f64| BandPlan {
            band: Band::Hf,
            lo_dial_hz: 0.0,
            move_lo: false,
            ddc_center_hz: dial,
            conjugate: false,
        };
        // Comfortably inside: untouched.
        assert_eq!(achieved_dial_center_hz(&hf(7_100_000.0), ADC, OUT), 7_100_000.0);
        // Below out/2: the centre stops where the band still fits, and the
        // dial rides off-centre in the output. This is the medium-wave case
        // the engine has to be told about.
        assert_eq!(achieved_dial_center_hz(&hf(630_000.0), ADC, OUT), OUT / 2.0);
        // Near Nyquist: same at the top.
        assert_eq!(achieved_dial_center_hz(&hf(32_000_000.0), ADC, OUT), ADC / 2.0 - OUT / 2.0);
        // Full-Nyquist output: pinned at fs/4, wherever the dial is.
        let full = ADC / 2.0;
        assert_eq!(achieved_dial_center_hz(&hf(1_000_000.0), ADC, full), ADC / 4.0);
        assert_eq!(achieved_dial_center_hz(&hf(30_000_000.0), ADC, full), ADC / 4.0);
    }

    /// On VHF a narrow output parks on the IF carrier and the dial is achieved
    /// exactly; a wide one gets its centre pinned at `out/2`, and the dial
    /// rides above the reported centre by the difference — reflected through
    /// the tuner's park frequency because high-side injection runs the IF
    /// backwards.
    #[test]
    fn the_achieved_vhf_center_rides_off_centre_on_a_wide_output() {
        let st = vhf_state(145_000_000.0);
        let p = plan(145_000_000.0, ADC, OUT, true, st);
        assert_eq!(achieved_dial_center_hz(&p, ADC, OUT), 145_000_000.0);

        // 16.2 MHz out at 64.8 Msps: the centre pins at 8.1 MHz, 3.53 MHz
        // above the IF carrier, so the reported centre sits 3.53 MHz below
        // the dial and the wanted signal is 3.53 MHz inside the span.
        let wide = 16_200_000.0;
        let p = plan(145_000_000.0, ADC, wide, true, st);
        let c = achieved_dial_center_hz(&p, ADC, wide);
        assert_eq!(c, 145_000_000.0 + IF_CENTER_HZ - wide / 2.0);
        assert!((145_000_000.0 - c).abs() < wide / 2.0, "the dial must stay inside the span");

        // Full-Nyquist at 48.6 Msps — the configuration that used to switch
        // VHF off outright: centre pinned at 12.15 MHz, dial 7.58 MHz inside.
        let (adc, full) = (48_600_000.0, 24_300_000.0);
        let p = plan(145_000_000.0, adc, full, true, st);
        let c = achieved_dial_center_hz(&p, adc, full);
        assert_eq!(c, 145_000_000.0 + IF_CENTER_HZ - full / 2.0);
        assert!((145_000_000.0 - c).abs() < full / 2.0, "the dial must stay inside the span");
    }

    /// In VHF the strip is a slice of the analyser centred on the tuner, and
    /// it runs backwards.
    #[test]
    fn the_vhf_strip_is_centred_on_the_tuner_and_reversed() {
        let bins = 4096;
        let m = wide_map(Band::Vhf, 145_000_000.0, ADC, bins);

        assert!(m.reverse, "ascending IF is descending RF");
        assert!(m.lo_bin < m.hi_bin && m.hi_bin <= bins);
        // Centred on the dial, to within a bin.
        let bin_hz = ADC / 2.0 / bins as f64;
        assert!(
            (m.center_hz - 145_000_000.0).abs() < bin_hz,
            "the strip is centred on {} rather than the tuner",
            m.center_hz
        );
        // And about as wide as the filter that shapes it.
        assert!((m.span_hz - IF_BW_HZ).abs() < 2.0 * bin_hz, "span was {}", m.span_hz);
    }
}
