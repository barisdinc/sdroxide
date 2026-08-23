//! The Airspy R2 / Mini wire protocol, with no `nusb` in it.
//!
//! Every constant and every encode/decode lives here as a pure function, so
//! that the half of this driver most likely to be transcribed wrong is also the
//! half that is fully testable without a receiver. [`crate::usb`] turns these
//! into control transfers; nothing here does I/O.
//!
//! Transcribed from libairspy's `airspy.c` and `airspy_commands.h`. Three
//! things here are not what they look like, and each has a test asserting
//! against literals:
//!
//! * **The receiver streams *real* samples at twice the rate you asked for.**
//!   [`program_rate_hz`] is where that doubling lives. See [`crate::convert`]
//!   for what the host then has to do about it.
//! * **A sample rate goes out either as an index or as kilohertz**, depending
//!   on which side of [`MIN_SAMPLERATE_BY_VALUE`] it falls — see
//!   [`encode_samplerate`].
//! * **The gain tables are indexed backwards.** libairspy's combined-gain
//!   setters do `value = GAIN_COUNT - 1 - value` before looking anything up, so
//!   index 0 is *maximum* gain in the tables and *minimum* to a caller.
//!   [`GainCurve::stages`] hides that, and the test pins which end is which.

/// USB vendor and product id. One pair for **both** models — an R2 and a Mini
/// are indistinguishable until [`Request::BoardPartidSerialnoRead`] is asked,
/// and even then only by the rates they offer.
///
/// **And not only for both models.** HydraSDR's prototype RFOne boards were
/// flashed with this same pair, before that vendor got an id of its own, so a
/// device here is not necessarily an Airspy at all — see [`looks_like_hydrasdr`].
pub const VID: u16 = 0x1d50;
pub const PID: u16 = 0x60a1;

/// Whether a device on [`VID`]/[`PID`] is really a HydraSDR RFOne prototype
/// rather than an Airspy.
///
/// HydraSDR forked libairspy, and its early boards kept Airspy's USB id;
/// `hydrasdr-host`'s device registry still lists `1d50:60a1` beside its own
/// `38af:0001`. The two firmwares are *not* interchangeable — HydraSDR's
/// `SET_FREQ` carries a `uint64_t` where this one carries a `uint32_t` — so a
/// board that says HydraSDR in its descriptors is left to `sdroxide-hydrasdr`,
/// and its owner is pointed at the interface that will actually tune it.
///
/// The strings come from the OS's cached descriptors, so this costs nothing and
/// opens nothing. A prototype that carries neither string is still claimed
/// here; that case is caught after opening, where the firmware version string
/// says `HydraSDR RF` rather than `AirSpy NOS`.
pub fn looks_like_hydrasdr(product: Option<&str>, serial: Option<&str>) -> bool {
    product.is_some_and(|p| p.trim().to_ascii_lowercase().contains("hydrasdr"))
        || serial.is_some_and(|s| s.trim().to_ascii_uppercase().starts_with("HYDRASDR"))
}

/// The configuration and interface the sample endpoint lives in.
pub const CONFIGURATION: u8 = 1;
pub const INTERFACE: u8 = 0;

/// The alternate setting the sample endpoint lives in — and there is only one.
///
/// The firmware's configuration descriptor (`airspy_m0/usb_descriptor.c`)
/// declares interface 0 with a **single** alternate setting, 0, carrying two
/// bulk endpoints. There is no alternate setting 1 to select, which is why
/// libairspy claims interface 0 and streams without ever calling
/// `libusb_set_interface_alt_setting`. Selecting one anyway is not a harmless
/// extra step: macOS fails `SetAlternateInterface` with `kIOReturnNotFound`
/// (`0xe00002f0`) and the receiver never opens at all.
///
/// So this is only what [`crate::usb::UsbDev`] checks the endpoint list
/// against; nothing sends a `SET_INTERFACE`.
pub const ALT_SETTING: u8 = 0;

/// The bulk IN endpoint carrying the sample stream.
pub const BULK_EP: u8 = 0x81;

/// Control-transfer timeout, matching libairspy's `LIBUSB_CTRL_TIMEOUT_MS`.
pub const CTRL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Rates at or above this go out as a value in kilohertz; anything below goes
/// out as an index into the receiver's own list. libairspy's
/// `MIN_SAMPLERATE_BY_VALUE`.
pub const MIN_SAMPLERATE_BY_VALUE: u32 = 1_000_000;

/// The rates every R2 reports when its firmware is too old to be asked.
/// libairspy falls back to exactly these two.
pub const FALLBACK_RATES: [f64; 2] = [10.0e6, 2.5e6];

/// Vendor requests, as `bRequest`. Values from `airspy_commands.h`.
///
/// # Two shapes, and the direction is not a formality
///
/// Most of what look like *writes* here are control **reads**: the firmware
/// handles them entirely in the setup stage and then queues a one-byte return
/// code on the IN endpoint (`usb_vendor_request_set_lna_gain` and friends), so
/// the host has to come and collect it. Sending one as an OUT with no data
/// leaves that byte queued where the host expects a zero-length status stage —
/// the transfer errors, and the byte is still sitting there to corrupt the next
/// control read. See [`crate::usb::UsbDev::set`].
///
/// The genuine OUTs are the ones whose handler answers with
/// `usb_transfer_schedule_ack`: [`Request::ReceiverMode`], [`Request::SetFreq`]
/// (which also carries four bytes of payload), [`Request::SetRfBias`], and the
/// register/GPIO writes.
///
/// Note also where the argument goes. Of the requests this driver sends, only
/// [`Request::ReceiverMode`] reads `wValue` and only [`Request::SetFreq`] takes
/// a payload; every other one — gains, AGC, packing, rate, and the bias tee —
/// reads `wIndex`, and putting the value in `wValue` instead is a transfer that
/// succeeds and changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Request {
    Invalid = 0,
    ReceiverMode = 1,
    Si5351cWrite = 2,
    Si5351cRead = 3,
    R820tWrite = 4,
    R820tRead = 5,
    SpiFlashErase = 6,
    SpiFlashWrite = 7,
    SpiFlashRead = 8,
    BoardIdRead = 9,
    VersionStringRead = 10,
    BoardPartidSerialnoRead = 11,
    SetSamplerate = 12,
    SetFreq = 13,
    SetLnaGain = 14,
    SetMixerGain = 15,
    SetVgaGain = 16,
    SetLnaAgc = 17,
    SetMixerAgc = 18,
    MsVendorCmd = 19,
    SetRfBias = 20,
    GpioWrite = 21,
    GpioRead = 22,
    GpioDirWrite = 23,
    GpioDirRead = 24,
    GetSamplerates = 25,
    SetPacking = 26,
}

impl Request {
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Whether a firmware is allowed not to have this request.
    ///
    /// Both were added after the product shipped, and an older receiver stalls
    /// them. A stalled control transfer self-clears, so the only correct
    /// response is to carry on with a documented fallback — the two rates in
    /// [`FALLBACK_RATES`] for the rate list, and unpacked samples for packing.
    pub fn is_optional(self) -> bool {
        matches!(self, Request::GetSamplerates | Request::SetPacking)
    }
}

/// The receiver's operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ReceiverMode {
    Off = 0,
    Rx = 1,
}

/// The rate to *program*, given the complex rate the operator wants.
///
/// **This is the doubling.** The R2's ADC is real, not complex: the receiver
/// digitises a real IF and the host makes complex baseband out of it. So "10
/// Msps" is an ADC running at 20 Msps, and libairspy multiplies by two before
/// sending precisely because its own output type is IQ (`airspy.c:1139-1141`).
///
/// Getting this wrong is not a loud failure. Ask for 10 Msps without the
/// doubling and the receiver runs at 10 Msps real, the host decimates by two,
/// and everything works — at half the bandwidth the operator asked for, with a
/// dial that tunes correctly and a span that is quietly wrong.
pub fn program_rate_hz(complex_rate_hz: f64) -> f64 {
    complex_rate_hz * 2.0
}

/// The complex rate that results from a programmed rate — the inverse of
/// [`program_rate_hz`], for reporting back.
pub fn complex_rate_hz(programmed_hz: f64) -> f64 {
    programmed_hz / 2.0
}

/// How a rate is carried in `wIndex` for [`Request::SetSamplerate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateArg {
    /// An index into the receiver's own list, for anything under
    /// [`MIN_SAMPLERATE_BY_VALUE`].
    Index(u16),
    /// The rate in kilohertz.
    Khz(u16),
}

impl RateArg {
    pub fn value(self) -> u16 {
        match self {
            RateArg::Index(v) | RateArg::Khz(v) => v,
        }
    }
}

/// Encode a *programmed* (already doubled) rate for the wire.
///
/// The bimodal encoding is libairspy's `airspy_set_samplerate`, and the order
/// of the two branches is part of it. A rate the receiver **listed** goes out
/// as its *index* in that list, whatever its magnitude; only a rate that is not
/// on the list falls through to being sent as kilohertz, and then only if it is
/// at or above [`MIN_SAMPLERATE_BY_VALUE`]. The firmware makes the same
/// distinction from the other side — `usb_vendor_request_set_samplerate` reads
/// `wIndex` as an index while it is below the number of configurations it has,
/// and as a kilohertz figure above that — so the two branches are not
/// interchangeable for a supported rate.
///
/// The kilohertz figure is of the *programmed* rate, which is why this takes
/// one: libairspy doubles the operator's complex rate before dividing by 1000.
///
/// `rates` is the receiver's programmed-rate list, in the order it reported
/// them. Returns `None` when a rate is neither in that list nor expressible as
/// kilohertz, because there is then nothing to send and guessing would be worse
/// than refusing.
pub fn encode_samplerate(programmed_hz: f64, rates: &[f64]) -> Option<RateArg> {
    let hz = programmed_hz.round().max(0.0) as u32;
    if let Some(i) = rates.iter().position(|r| (r.round() as u32) == hz) {
        return Some(RateArg::Index(i as u16));
    }
    if hz < MIN_SAMPLERATE_BY_VALUE {
        return None;
    }
    u16::try_from(hz / 1000).ok().map(RateArg::Khz)
}

/// The rate list the receiver reports, in Hz. Little-endian `u32` apiece.
///
/// **These are complex rates, not what the ADC runs at.** libairspy hands this
/// list straight to a caller asking for IQ and doubles every entry for a caller
/// asking for the raw real stream (`airspy_get_samplerates`), which is what
/// fixes the units: an R2 reports 10 and 2.5 Msps and digitises at 20 and 5.
/// Read them as programmed rates and every rate on offer comes out half its
/// real size, with a receiver that streams correctly and a span that is
/// silently wrong by an octave. [`program_rate_hz`] is what converts.
pub fn parse_rates(b: &[u8]) -> Vec<f64> {
    b.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
        .filter(|r| *r > 0.0)
        .collect()
}

/// A `u32` count, as the two-step "ask for the count, then ask for that many"
/// query returns it.
pub fn parse_count(b: &[u8]) -> Option<u32> {
    (b.len() >= 4).then(|| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The 24-byte reply to [`Request::BoardPartidSerialnoRead`]:
/// `{ u32 part_id[2]; u32 serial_no[4] }`, all little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartIdSerial {
    pub part_id: [u32; 2],
    pub serial: [u32; 4],
}

impl PartIdSerial {
    pub fn parse(b: &[u8]) -> Option<PartIdSerial> {
        if b.len() < 24 {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(PartIdSerial {
            part_id: [word(0), word(4)],
            serial: [word(8), word(12), word(16), word(20)],
        })
    }

    /// The serial as the hex digits every Airspy tool prints, which is also
    /// what the USB `iSerial` descriptor carries.
    pub fn serial_hex(&self) -> String {
        self.serial.iter().map(|w| format!("{w:08X}")).collect()
    }
}

/// A NUL-padded ASCII reply, such as the firmware version string.
pub fn parse_ascii(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// Whether an operator-supplied serial names this receiver.
///
/// Suffix matching, case-insensitive and whitespace-tolerant: the value in
/// `radio.json` was typed or pasted by a human, and the descriptor's case is
/// the firmware's choice. An empty `want` matches everything — that is "no
/// serial configured", not "a serial that happens to be blank".
pub fn serial_matches(want: &str, found: Option<&str>) -> bool {
    let want = want.trim();
    if want.is_empty() {
        return true;
    }
    match found {
        Some(f) => {
            let f = f.trim();
            f.len() >= want.len() && f[f.len() - want.len()..].eq_ignore_ascii_case(want)
        }
        None => false,
    }
}

/// How many steps the combined-gain curves have.
pub const GAIN_COUNT: u8 = 22;

/// Which combined-gain curve to drive the three stages from.
///
/// The R820T2 has an LNA, a mixer and a VGA, and setting them independently is
/// a good way to make a receiver that either overloads or hisses. libairspy
/// therefore publishes two curated curves through the three stages, and this
/// driver offers the same choice rather than three sliders — it is what every
/// other Airspy program does, and what the numbers were tuned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GainCurve {
    /// Favours strong-signal handling: least intermodulation for a given
    /// sensitivity. The right default on an antenna with broadcast nearby.
    #[default]
    Linearity,
    /// Favours weak-signal sensitivity, at the cost of overload margin.
    Sensitivity,
}

/// The three stage settings for one step of a curve, as `(lna, mixer, vga)`,
/// each 0–15.
impl GainCurve {
    /// The stage values for `step`, where **0 is least gain** and
    /// [`GAIN_COUNT`]`- 1` is most.
    ///
    /// The tables are stored the other way round — libairspy indexes them with
    /// `GAIN_COUNT - 1 - value`, so table index 0 is maximum gain. Reversing
    /// here rather than at the call sites is what keeps "more slider is more
    /// signal" true everywhere above this line.
    pub fn stages(self, step: u8) -> (u8, u8, u8) {
        // Transcribed from libairspy's `airspy_linearity_*_gains` and
        // `airspy_sensitivity_*_gains`.
        const LIN_VGA: [u8; 22] =
            [13, 12, 11, 11, 11, 11, 11, 10, 10, 10, 10, 10, 10, 10, 10, 10, 9, 8, 7, 6, 5, 4];
        const LIN_MIXER: [u8; 22] =
            [12, 12, 11, 9, 8, 7, 6, 6, 5, 0, 0, 1, 0, 0, 2, 2, 1, 1, 1, 1, 0, 0];
        const LIN_LNA: [u8; 22] =
            [14, 14, 14, 13, 12, 10, 9, 9, 8, 9, 8, 6, 5, 3, 1, 0, 0, 0, 0, 0, 0, 0];
        const SENS_VGA: [u8; 22] =
            [13, 12, 11, 10, 9, 8, 7, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4];
        const SENS_MIXER: [u8; 22] =
            [12, 12, 12, 12, 11, 10, 10, 9, 9, 8, 7, 4, 4, 4, 3, 2, 2, 1, 0, 0, 0, 0];
        const SENS_LNA: [u8; 22] =
            [14, 14, 14, 14, 14, 14, 14, 14, 14, 13, 12, 12, 9, 9, 8, 7, 6, 5, 3, 2, 1, 0];

        let step = step.min(GAIN_COUNT - 1);
        // The reversal. `step` counts up from least gain; the tables count down
        // from most.
        let i = (GAIN_COUNT - 1 - step) as usize;
        match self {
            GainCurve::Linearity => (LIN_LNA[i], LIN_MIXER[i], LIN_VGA[i]),
            GainCurve::Sensitivity => (SENS_LNA[i], SENS_MIXER[i], SENS_VGA[i]),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            GainCurve::Linearity => "linearity",
            GainCurve::Sensitivity => "sensitivity",
        }
    }

    pub fn from_code(code: u8) -> GainCurve {
        match code {
            1 => GainCurve::Sensitivity,
            _ => GainCurve::Linearity,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            GainCurve::Linearity => 0,
            GainCurve::Sensitivity => 1,
        }
    }
}

/// Unpack the receiver's 12-bit packing: three little-endian `u32` words carry
/// eight 12-bit samples.
///
/// Packing is worth having. At the top rate the ADC runs at 20 Msps real, which
/// is 40 MB/s unpacked and 30 MB/s packed — and the difference decides whether
/// a marginal USB 2.0 link keeps up.
///
/// Appends to `out`. Input not a multiple of three words is truncated to one:
/// a partial group carries no whole samples, and the caller carries the
/// remainder into the next transfer.
pub fn unpack12(words: &[u32], out: &mut Vec<u16>) {
    for w in words.chunks_exact(3) {
        unpack_group(w[0], w[1], w[2], out);
    }
}

/// How many bytes one packed group occupies: three `u32` words, eight samples.
///
/// The stream is continuous across USB transfers, so this is also the alignment
/// a completion has to be decoded on — see [`unpack12_bytes`].
pub const PACKED_GROUP_BYTES: usize = 12;

/// [`unpack12`] straight off the wire, decoding only whole groups.
///
/// Saves materialising a `Vec<u32>` per completion — at the top rate that is a
/// 128 KiB allocation every thirteen milliseconds — and, more to the point,
/// lets the caller hold a partial group back until the bytes that finish it
/// arrive. A trailing partial group is *ignored* here, not consumed: it is the
/// caller's to carry.
pub fn unpack12_bytes(bytes: &[u8], out: &mut Vec<u16>) {
    for g in bytes.chunks_exact(PACKED_GROUP_BYTES) {
        let word = |i: usize| u32::from_le_bytes([g[i], g[i + 1], g[i + 2], g[i + 3]]);
        unpack_group(word(0), word(4), word(8), out);
    }
}

/// The bit layout itself, in one place so the two entry points cannot drift.
fn unpack_group(a: u32, b: u32, c: u32, out: &mut Vec<u16>) {
    out.push(((a >> 20) & 0xfff) as u16);
    out.push(((a >> 8) & 0xfff) as u16);
    out.push((((a & 0xff) << 4) | ((b >> 28) & 0xf)) as u16);
    out.push(((b & 0x0fff_0000) >> 16) as u16);
    out.push(((b & 0x0000_fff0) >> 4) as u16);
    out.push((((b & 0xf) << 8) | ((c & 0xff00_0000) >> 24)) as u16);
    out.push(((c >> 12) & 0xfff) as u16);
    out.push((c & 0xfff) as u16);
}

/// How many whole `u32` words of a packed buffer carry complete sample groups.
pub fn packed_whole_words(words: usize) -> usize {
    words / 3 * 3
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doubling, stated as an equation rather than a comment. Getting it
    /// wrong halves the span with nothing on screen to say so.
    #[test]
    fn the_programmed_rate_is_twice_the_complex_rate() {
        assert_eq!(program_rate_hz(10.0e6), 20.0e6);
        assert_eq!(program_rate_hz(2.5e6), 5.0e6);
        assert_eq!(program_rate_hz(3.0e6), 6.0e6);
        // And back again, exactly.
        for complex in [2.5e6, 3.0e6, 6.0e6, 10.0e6] {
            assert_eq!(complex_rate_hz(program_rate_hz(complex)), complex);
        }
    }

    /// A listed rate goes out as its index; only an unlisted one goes out as
    /// kilohertz. Sending the wrong one does not fail — the receiver runs at
    /// some other rate and says nothing.
    #[test]
    fn a_listed_rate_goes_out_as_its_index_and_the_rest_as_kilohertz() {
        // The R2's own list, as programmed (doubled) rates.
        let rates = [20.0e6, 5.0e6];

        // Both of the R2's rates are on that list, so both go out as indices —
        // which is what libairspy sends and what the firmware reads first.
        assert_eq!(encode_samplerate(program_rate_hz(10.0e6), &rates), Some(RateArg::Index(0)));
        assert_eq!(encode_samplerate(program_rate_hz(2.5e6), &rates), Some(RateArg::Index(1)));
        // A Mini's list, to pin that the index is a position and not a rank.
        let mini = [12.0e6, 6.0e6, 3.0e6];
        assert_eq!(encode_samplerate(program_rate_hz(3.0e6), &mini), Some(RateArg::Index(1)));

        // Not on the list, and big enough to name in kilohertz — of the
        // programmed rate, not the complex one.
        assert_eq!(encode_samplerate(program_rate_hz(4.0e6), &rates), Some(RateArg::Khz(8_000)));
        assert_eq!(
            encode_samplerate(MIN_SAMPLERATE_BY_VALUE as f64, &rates),
            Some(RateArg::Khz(1_000))
        );
        // Sub-megahertz and unlisted: no index to send, and guessing one would
        // be worse than refusing.
        let slow = [500_000.0, 250_000.0];
        assert_eq!(encode_samplerate(250_000.0, &slow), Some(RateArg::Index(1)));
        assert_eq!(encode_samplerate(300_000.0, &slow), None);
        // Too large for `wIndex` in kilohertz is also a refusal rather than a
        // wrapped number that would program some unrelated rate.
        assert_eq!(encode_samplerate(100.0e9, &rates), None);
    }

    /// The tables are indexed backwards. If this reverses, every gain slider in
    /// the program runs the wrong way — which is obvious on a bench and
    /// invisible in a diff.
    #[test]
    fn step_zero_is_least_gain_and_the_top_step_is_most() {
        for curve in [GainCurve::Linearity, GainCurve::Sensitivity] {
            let (lna_lo, mix_lo, vga_lo) = curve.stages(0);
            let (lna_hi, mix_hi, vga_hi) = curve.stages(GAIN_COUNT - 1);
            let lo = lna_lo as u32 + mix_lo as u32 + vga_lo as u32;
            let hi = lna_hi as u32 + mix_hi as u32 + vga_hi as u32;
            assert!(hi > lo, "{curve:?}: step 0 must be the quiet end, not the loud one");
        }
        // Against literals: step 0 is the *last* row of each table.
        assert_eq!(GainCurve::Linearity.stages(0), (0, 0, 4));
        assert_eq!(GainCurve::Linearity.stages(21), (14, 12, 13));
        assert_eq!(GainCurve::Sensitivity.stages(0), (0, 0, 4));
        assert_eq!(GainCurve::Sensitivity.stages(21), (14, 12, 13));

        // Every stage value must be inside the R820T2's 0–15 range, or the
        // receiver rejects the write.
        for curve in [GainCurve::Linearity, GainCurve::Sensitivity] {
            for step in 0..=GAIN_COUNT {
                let (l, m, v) = curve.stages(step);
                assert!(l <= 15 && m <= 15 && v <= 15, "{curve:?} step {step}: {l},{m},{v}");
            }
        }
        // Out of range clamps rather than panicking on a hand-edited config.
        assert_eq!(GainCurve::Linearity.stages(200), GainCurve::Linearity.stages(GAIN_COUNT - 1));
    }

    /// Three words in, eight samples out, and every one of them 12 bits.
    #[test]
    fn the_packing_unpacks_three_words_into_eight_samples() {
        // A group whose nibbles are individually recognisable, so a misplaced
        // shift shows up as a recognisably wrong digit rather than as noise.
        let words = [0xBCDE_F012u32, 0x3456_789A, 0xBCDE_F012];
        let mut out = Vec::new();
        unpack12(&words, &mut out);
        assert_eq!(out.len(), 8, "three words carry eight samples");
        for (i, s) in out.iter().enumerate() {
            assert!(*s <= 0xfff, "sample {i} = {s:#x} is not 12 bits");
        }

        // The first two come straight out of the top of word 0.
        let w0 = words[0];
        assert_eq!(out[0], ((w0 >> 20) & 0xfff) as u16);
        assert_eq!(out[1], ((w0 >> 8) & 0xfff) as u16);
        // The third straddles words 0 and 1, which is the join most likely to
        // be transcribed wrong.
        assert_eq!(out[2], (((w0 & 0xff) << 4) | ((words[1] >> 28) & 0xf)) as u16);
        // The last is the bottom of word 2.
        assert_eq!(out[7], (words[2] & 0xfff) as u16);

        // A partial group carries no whole samples and must be left alone.
        let mut out = Vec::new();
        unpack12(&words[..2], &mut out);
        assert!(out.is_empty(), "two words are not a group");
        assert_eq!(packed_whole_words(7), 6);
        assert_eq!(packed_whole_words(2), 0);
        assert_eq!(packed_whole_words(3), 3);

        // The wire-order entry point decodes the same samples from the same
        // bytes, and stops at the last whole group rather than consuming the
        // tail — the caller carries that into the next completion.
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut from_words = Vec::new();
        unpack12(&words, &mut from_words);
        let mut from_bytes = Vec::new();
        unpack12_bytes(&bytes, &mut from_bytes);
        assert_eq!(from_words, from_bytes);
        assert_eq!(PACKED_GROUP_BYTES, 12);
        let mut partial = Vec::new();
        unpack12_bytes(&bytes[..PACKED_GROUP_BYTES - 1], &mut partial);
        assert!(partial.is_empty(), "eleven bytes are not a group");
    }

    #[test]
    fn the_rate_list_is_little_endian_and_drops_zeroes() {
        // An R2's actual reply. These are *complex* rates — the receiver
        // digitises at twice each of them — which is why the driver doubles
        // them into programmed rates before using them for anything.
        let mut b = Vec::new();
        for r in [10_000_000u32, 2_500_000] {
            b.extend_from_slice(&r.to_le_bytes());
        }
        assert_eq!(parse_rates(&b), vec![10.0e6, 2.5e6]);
        assert_eq!(
            parse_rates(&b).iter().map(|r| program_rate_hz(*r)).collect::<Vec<_>>(),
            vec![20.0e6, 5.0e6],
            "the ADC runs at twice the listed rate"
        );
        // A trailing partial word is dropped, not misread.
        b.push(0x01);
        assert_eq!(parse_rates(&b).len(), 2);
        // A zero rate is not a rate.
        assert_eq!(parse_rates(&0u32.to_le_bytes()), Vec::<f64>::new());
        assert_eq!(parse_count(&9u32.to_le_bytes()), Some(9));
        assert_eq!(parse_count(&[1, 2, 3]), None);
    }

    #[test]
    fn the_part_id_reply_splits_into_two_words_and_four() {
        let mut b = Vec::new();
        for w in [0x6906_0004u32, 0x0030_0037, 0, 0, 0x4711_0000, 0x0000_0042] {
            b.extend_from_slice(&w.to_le_bytes());
        }
        let p = PartIdSerial::parse(&b).expect("24 bytes is a whole reply");
        assert_eq!(p.part_id, [0x6906_0004, 0x0030_0037]);
        assert_eq!(p.serial_hex(), "00000000000000004711000000000042");
        // A short reply is not a half-parsed serial — that would pin the wrong
        // receiver out of several.
        assert_eq!(PartIdSerial::parse(&b[..23]), None);
        assert_eq!(PartIdSerial::parse(&[]), None);
    }

    /// Both requests were added after the product shipped; an older receiver
    /// stalls them, and treating that as a fault would refuse to open it.
    #[test]
    fn only_the_later_requests_are_optional() {
        assert!(Request::GetSamplerates.is_optional());
        assert!(Request::SetPacking.is_optional());
        for r in [
            Request::ReceiverMode,
            Request::SetFreq,
            Request::SetSamplerate,
            Request::SetLnaGain,
            Request::SetMixerGain,
            Request::SetVgaGain,
        ] {
            assert!(!r.is_optional(), "{r:?} is what makes the receiver work");
        }
        assert_eq!(Request::ReceiverMode.code(), 1);
        assert_eq!(Request::SetFreq.code(), 13);
        assert_eq!(Request::GetSamplerates.code(), 25);
        assert_eq!(Request::SetPacking.code(), 26);
    }

    /// The USB id this receiver answers on is not exclusively its own: HydraSDR
    /// shipped prototype RFOne boards with it, and that radio needs the other
    /// driver — an eight-byte `SET_FREQ` where this one sends four.
    #[test]
    fn a_hydrasdr_prototype_on_this_usb_id_is_recognised_and_left_alone() {
        assert!(looks_like_hydrasdr(Some("HydraSDR RFOne"), None));
        assert!(looks_like_hydrasdr(Some("hydrasdr rfone"), None), "case is the firmware's choice");
        assert!(looks_like_hydrasdr(None, Some("HYDRASDR SN:0011223344556677")));
        // A real Airspy must still be claimed, whatever it does or does not
        // publish about itself.
        assert!(!looks_like_hydrasdr(Some("AirSpy"), Some("644064DC3238C33F")));
        assert!(!looks_like_hydrasdr(None, None));
    }

    #[test]
    fn a_serial_matches_on_its_suffix() {
        let full = Some("644064DC3238C33F");
        assert!(serial_matches("3238C33F", full));
        assert!(serial_matches("3238c33f", full), "case is the firmware's choice");
        assert!(serial_matches(" 3238C33F ", full));
        assert!(!serial_matches("644064DC", full), "a prefix is not a suffix");
        assert!(!serial_matches("F".repeat(64).as_str(), full), "longer than the serial");
        assert!(serial_matches("", full), "no serial configured means any receiver");
        assert!(!serial_matches("3238C33F", None));
    }

    #[test]
    fn ascii_replies_stop_at_the_nul() {
        assert_eq!(
            parse_ascii(b"AirSpy NOS v1.0.0-rc10-6-g4008185\0\0"),
            "AirSpy NOS v1.0.0-rc10-6-g4008185"
        );
        assert_eq!(parse_ascii(b"\0junk"), "");
        assert_eq!(parse_ascii(b""), "");
    }
}
