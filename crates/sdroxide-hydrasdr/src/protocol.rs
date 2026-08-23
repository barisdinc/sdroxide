//! The HydraSDR RFOne wire protocol, with no `nusb` in it.
//!
//! Every constant and every encode/decode lives here as a pure function, so
//! that the half of this driver most likely to be transcribed wrong is also the
//! half that is fully testable without a receiver. [`crate::usb`] turns these
//! into control transfers; nothing here does I/O.
//!
//! Transcribed from `hydrasdr-host`'s `hydrasdr_commands.h`,
//! `hydrasdr_shared.c` and `hydrasdr_rfone.c`, and — where the host library is
//! not the authority — from the RFOne firmware itself (`rfone_fw`, `m0/usb_req.c`
//! and `common/hydrasdr_rfone_conf.c`). Both are MIT/BSD-3-Clause, compatible
//! with this workspace's GPL-3.0-or-later.
//!
//! # What is *not* the same as an Airspy R2
//!
//! Requests 0–26 are the Airspy's, number for number. Four things are not, and
//! each has a test pinning it against a literal:
//!
//! * **[`Request::SetFreq`] carries eight bytes, not four.** The firmware's
//!   handler schedules a receive of `sizeof(uint64_t)` and libhydrasdr sends
//!   exactly that. This is the single wire difference that makes the two
//!   drivers non-interchangeable, and it is the quiet kind: on a little-endian
//!   host a four-byte write lands in the low half of a variable whose high half
//!   happens to start at zero, so it can appear to work until something has
//!   written a frequency above 4 GHz into it.
//! * **Two USB id pairs**, one of which is Airspy's own — see [`USB_IDS`].
//! * **Three RF input ports** ([`RfPort`]), selected with [`Request::SetRfPort`].
//! * **Seven sample rates rather than two**, only three of which the receiver
//!   lists — see [`ALT_RATES`].
//!
//! Three things *are* the same, and are the same for a reason worth knowing:
//!
//! * **The receiver streams real samples at twice the rate you asked for.**
//!   [`program_rate_hz`] is where that doubling lives.
//! * **A sample rate goes out either as an index or as kilohertz**, depending
//!   on whether the receiver listed it — see [`encode_samplerate`].
//! * **The gain tables are indexed backwards.** `rfone_set_linearity_gain` does
//!   `value = RFONE_GAIN_TABLE_SIZE - 1 - value` before looking anything up, so
//!   table index 0 is *maximum* gain and *minimum* to a caller.
//!   [`GainCurve::stages`] hides that, and the test pins which end is which.

/// The USB id pairs an RFOne can enumerate as, official first.
///
/// **The second pair is Airspy's own.** `hydrasdr-host`'s device registry
/// carries both — `0x38af:0x0001` for production boards and `0x1d50:0x60a1`
/// for the prototypes, which were flashed before the vendor id existed. A
/// legacy-id RFOne is therefore indistinguishable from a real Airspy R2 by
/// `idVendor`/`idProduct` alone, and the two need different drivers: see
/// [`Request::SetFreq`].
///
/// What separates them is what the device *says about itself*.
/// [`is_hydrasdr_strings`] answers it from the descriptors, without opening
/// anything; [`is_hydrasdr_firmware`] answers it from the firmware version
/// string, which is what libhydrasdr checks and the only fully dependable one.
pub const USB_IDS: [(u16, u16); 2] = [(VID_OFFICIAL, PID_OFFICIAL), (VID_LEGACY, PID_LEGACY)];

/// usb.org vendor id 14511 (Vernoux Benjamin), product 1: the RFOne.
pub const VID_OFFICIAL: u16 = 0x38af;
pub const PID_OFFICIAL: u16 = 0x0001;

/// The prototype pair, shared with the Airspy R2 and Mini.
pub const VID_LEGACY: u16 = 0x1d50;
pub const PID_LEGACY: u16 = 0x60a1;

/// The configuration and interface the sample endpoint lives in.
pub const CONFIGURATION: u8 = 1;
pub const INTERFACE: u8 = 0;

/// The alternate setting the sample endpoint lives in — and there is only one.
///
/// `m0/usb_descriptor.c` declares interface 0 with a single alternate setting,
/// 0, carrying two bulk endpoints. There is no alternate setting 1 to select,
/// which is why libhydrasdr claims interface 0 and streams without ever calling
/// `libusb_set_interface_alt_setting`. Selecting one anyway is not a harmless
/// extra step — on macOS `SetAlternateInterface` fails with `kIOReturnNotFound`
/// and the receiver never opens at all, which is how the Airspy driver next
/// door once failed.
///
/// So this is only what [`crate::usb::UsbDev`] checks the endpoint list
/// against; nothing sends a `SET_INTERFACE`.
pub const ALT_SETTING: u8 = 0;

/// The bulk IN endpoint carrying the sample stream (`USB_BULK_IN_EP_ADDR`).
pub const BULK_EP: u8 = 0x81;

/// Control-transfer timeout, matching libhydrasdr's `LIBUSB_CTRL_TIMEOUT_MS`.
pub const CTRL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Rates at or above this go out as a value in kilohertz; anything below goes
/// out as an index into the receiver's own list. libhydrasdr's
/// `MIN_SAMPLERATE_BY_VALUE`.
pub const MIN_SAMPLERATE_BY_VALUE: u32 = 1_000_000;

/// The prefix every RFOne firmware puts at the front of its version string
/// (`HYDRASDR_EXPECTED_FW_PREFIX`).
///
/// This is libhydrasdr's own test for "is this really one of ours", applied
/// after opening, and it is the only one that works on a legacy-id board: an
/// Airspy R2 answers the same request with `AirSpy NOS …`.
pub const FW_PREFIX: &str = "HydraSDR RF";

/// The prefix on the USB serial-number descriptor
/// (`HYDRASDR_EXPECTED_SERIAL_PREFIX`), followed by sixteen hex digits of the
/// board's 64-bit serial. An Airspy's descriptor carries the bare hex.
pub const SERIAL_PREFIX: &str = "HYDRASDR SN:";

/// The complex rates every RFOne firmware *lists*, for the rare board that
/// cannot be asked. From `hydrasdr_rfone_conf.c`'s three primary
/// configurations; the receiver reports `r82x_if_freq * 2` for each.
pub const FALLBACK_RATES: [f64; 3] = [10.0e6, 5.0e6, 2.5e6];

/// Complex rates the firmware has but does **not** list.
///
/// `usb_vendor_request_get_samplerates_command` only ever reports the primary
/// configurations. The alternate table in `hydrasdr_rfone_conf.c` holds four
/// more, and they are reachable — `usb_vendor_request_set_samplerate` falls
/// through to matching `wIndex * 1000` against every configuration's
/// `r82x_if_freq * 4`, alternates included. So these are real rates that no
/// enumeration will ever mention, and the only way to offer them is to know
/// they are there.
///
/// A firmware that does not have one answers the request with a stall, which is
/// why asking for an alternate is attempted and then checked rather than
/// assumed — see [`crate::device::Device::apply_rate`].
pub const ALT_RATES: [f64; 4] = [12.0e6, 8.0e6, 6.0e6, 4.096e6];

/// Every complex rate an RFOne can be asked for, listed ones first and then the
/// alternates, each descending. The order the settings combo offers.
pub const ALL_RATES: [f64; 7] = [12.0e6, 10.0e6, 8.0e6, 6.0e6, 5.0e6, 4.096e6, 2.5e6];

/// Tuning range, in Hz: `RFONE_MIN_FREQ_HZ`..`RFONE_MAX_FREQ_HZ`.
pub const FREQ_RANGE: (f64, f64) = (24.0e6, 1_800.0e6);

/// Vendor requests, as `bRequest`. Values from `hydrasdr_commands.h`.
///
/// # Two shapes, and the direction is not a formality
///
/// Most of what look like *writes* here are control **reads**: the firmware
/// handles them entirely in the setup stage and then queues a one-byte return
/// code on the IN endpoint, so the host has to come and collect it. Sending one
/// as an OUT with no data leaves that byte queued where the host expects a
/// zero-length status stage — the transfer errors, and the byte is still
/// sitting there to corrupt the next control read. See [`crate::usb::UsbDev::set`].
///
/// The genuine OUTs are the ones whose handler answers with
/// `usb_transfer_schedule_ack`: [`Request::ReceiverMode`], [`Request::SetFreq`]
/// (which also carries eight bytes of payload), [`Request::SetRfBias`], and the
/// register/GPIO writes.
///
/// Note also where the argument goes. Of the requests this driver sends, only
/// [`Request::ReceiverMode`] reads `wValue` and only [`Request::SetFreq`] takes
/// a payload; every other one — gains, AGC, packing, rate, the RF port and the
/// bias tee — reads `wIndex`, and putting the value in `wValue` instead is a
/// transfer that succeeds and changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Request {
    Reset = 0,
    ReceiverMode = 1,
    ClockgenWrite = 2,
    ClockgenRead = 3,
    RfFrontendWrite = 4,
    RfFrontendRead = 5,
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
    SpiFlashEraseSector = 27,
    /// Which of the three RF inputs the tuner sees. HydraSDR's own, with no
    /// Airspy equivalent.
    SetRfPort = 28,
    /// Everything from here up arrived with firmware v1.1.0.
    GetCapabilities = 29,
    SetBandwidth = 30,
    GetBandwidths = 31,
    GetTemperature = 32,
    SetGain = 33,
}

impl Request {
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Whether a firmware is allowed not to have this request.
    ///
    /// Everything above [`Request::SetPacking`] postdates some shipped
    /// firmware, and an older receiver stalls what it does not have. A stalled
    /// control transfer self-clears, so the only correct response is to carry
    /// on with a documented fallback — [`FALLBACK_RATES`] for the rate list,
    /// unpacked samples for packing, [`RfPort::Ant`] for the port, and
    /// [`Capabilities::RFONE_FALLBACK`] for the feature word.
    pub fn is_optional(self) -> bool {
        matches!(
            self,
            Request::GetSamplerates
                | Request::SetPacking
                | Request::SetRfPort
                | Request::GetCapabilities
                | Request::SetBandwidth
                | Request::GetBandwidths
                | Request::GetTemperature
                | Request::SetGain
        )
    }
}

/// The receiver's operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ReceiverMode {
    Off = 0,
    Rx = 1,
}

/// Which board answered [`Request::BoardIdRead`].
///
/// **Not a way to tell an RFOne from an Airspy.** A legacy-id board reports
/// `0`, and so does an Airspy R2 — both firmwares call their first board zero.
/// Use [`is_hydrasdr_firmware`] for that; this only says which of the two
/// HydraSDR builds is answering, which is worth having in a bug report because
/// it also says which USB id the board was flashed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardId {
    /// `HYDRASDR_BOARD_ID_PROTO_HYDRASDR` — a prototype on the legacy USB id.
    Proto,
    /// `HYDRASDR_BOARD_ID_HYDRASDR_RFONE_OFFICIAL` — a production RFOne.
    RfOne,
    Unknown(u8),
}

impl BoardId {
    pub fn from_code(code: u8) -> BoardId {
        match code {
            0 => BoardId::Proto,
            1 => BoardId::RfOne,
            other => BoardId::Unknown(other),
        }
    }

    pub fn name(self) -> String {
        match self {
            BoardId::Proto => "HydraSDR RFOne (prototype / legacy USB id)".to_string(),
            BoardId::RfOne => "HydraSDR RFOne".to_string(),
            BoardId::Unknown(c) => format!("unknown HydraSDR board id {c}"),
        }
    }
}

/// Which of the three RF inputs the tuner is connected to.
///
/// The RFOne brings out three, and `rfone_get_device_info` names them and says
/// which one carries the bias tee. Only the antenna port does; asking for DC on
/// either cable port is a request the firmware will take and the hardware will
/// ignore, which is why the settings panel ties the two together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RfPort {
    /// `ANT` — the SMA the antenna goes on, and the only one with a bias tee.
    #[default]
    Ant,
    /// `CABLE1`.
    Cable1,
    /// `CABLE2`.
    Cable2,
}

impl RfPort {
    pub fn code(self) -> u8 {
        match self {
            RfPort::Ant => 0,
            RfPort::Cable1 => 1,
            RfPort::Cable2 => 2,
        }
    }

    pub fn from_code(code: u8) -> RfPort {
        match code {
            1 => RfPort::Cable1,
            2 => RfPort::Cable2,
            _ => RfPort::Ant,
        }
    }

    /// The name the firmware itself publishes for the port.
    pub fn name(self) -> &'static str {
        match self {
            RfPort::Ant => "ANT",
            RfPort::Cable1 => "CABLE1",
            RfPort::Cable2 => "CABLE2",
        }
    }

    /// Whether this port can carry the bias tee. Only `ANT` can.
    pub fn has_bias_tee(self) -> bool {
        matches!(self, RfPort::Ant)
    }
}

/// The feature bitmask [`Request::GetCapabilities`] returns.
///
/// Read for the trace and for two decisions: whether the RF port select is
/// worth attempting, and whether [`Request::SetSamplerate`] answers with one
/// byte or four. Everything else here is recorded and not acted on — this
/// driver does what an RFOne has always been able to do, and a capability word
/// is a promise about the future rather than a licence to skip a fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const LNA_GAIN: u32 = 1 << 0;
    pub const RF_GAIN: u32 = 1 << 1;
    pub const MIXER_GAIN: u32 = 1 << 2;
    pub const FILTER_GAIN: u32 = 1 << 3;
    pub const VGA_GAIN: u32 = 1 << 4;
    pub const LNA_AGC: u32 = 1 << 5;
    pub const RF_AGC: u32 = 1 << 6;
    pub const MIXER_AGC: u32 = 1 << 7;
    pub const FILTER_AGC: u32 = 1 << 8;
    pub const LINEARITY_GAIN: u32 = 1 << 9;
    pub const SENSITIVITY_GAIN: u32 = 1 << 10;
    pub const BIAS_TEE: u32 = 1 << 11;
    pub const PACKING: u32 = 1 << 12;
    pub const RF_PORT_SELECT: u32 = 1 << 13;
    pub const GPIO: u32 = 1 << 14;
    pub const SPIFLASH: u32 = 1 << 15;
    pub const CLOCKGEN: u32 = 1 << 16;
    pub const RF_FRONTEND: u32 = 1 << 17;
    pub const BANDWIDTH: u32 = 1 << 18;
    pub const TEMPERATURE: u32 = 1 << 19;
    pub const RX: u32 = 1 << 20;
    pub const EXTENDED_SAMPLERATES: u32 = 1 << 21;
    pub const EXTENDED_GAIN: u32 = 1 << 22;

    /// What `RFONE_HARDCODED_CAPS` says an RFOne has when its firmware predates
    /// the request. libhydrasdr uses exactly this fallback, so a driver that
    /// used a different one would disagree with every other program about the
    /// same receiver.
    pub const RFONE_FALLBACK: Capabilities = Capabilities(
        Self::RX
            | Self::LNA_GAIN
            | Self::MIXER_GAIN
            | Self::VGA_GAIN
            | Self::LNA_AGC
            | Self::MIXER_AGC
            | Self::LINEARITY_GAIN
            | Self::SENSITIVITY_GAIN
            | Self::BIAS_TEE
            | Self::PACKING
            | Self::RF_PORT_SELECT
            | Self::GPIO
            | Self::SPIFLASH
            | Self::CLOCKGEN
            | Self::RF_FRONTEND,
    );

    pub fn parse(b: &[u8]) -> Option<Capabilities> {
        (b.len() >= 4).then(|| Capabilities(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
    }

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    /// The bits that are set, named, for the trace.
    pub fn names(self) -> Vec<&'static str> {
        const NAMED: [(u32, &str); 23] = [
            (Capabilities::LNA_GAIN, "lna-gain"),
            (Capabilities::RF_GAIN, "rf-gain"),
            (Capabilities::MIXER_GAIN, "mixer-gain"),
            (Capabilities::FILTER_GAIN, "filter-gain"),
            (Capabilities::VGA_GAIN, "vga-gain"),
            (Capabilities::LNA_AGC, "lna-agc"),
            (Capabilities::RF_AGC, "rf-agc"),
            (Capabilities::MIXER_AGC, "mixer-agc"),
            (Capabilities::FILTER_AGC, "filter-agc"),
            (Capabilities::LINEARITY_GAIN, "linearity"),
            (Capabilities::SENSITIVITY_GAIN, "sensitivity"),
            (Capabilities::BIAS_TEE, "bias-tee"),
            (Capabilities::PACKING, "packing"),
            (Capabilities::RF_PORT_SELECT, "rf-port"),
            (Capabilities::GPIO, "gpio"),
            (Capabilities::SPIFLASH, "spiflash"),
            (Capabilities::CLOCKGEN, "clockgen"),
            (Capabilities::RF_FRONTEND, "rf-frontend"),
            (Capabilities::BANDWIDTH, "bandwidth"),
            (Capabilities::TEMPERATURE, "temperature"),
            (Capabilities::RX, "rx"),
            (Capabilities::EXTENDED_SAMPLERATES, "extended-samplerates"),
            (Capabilities::EXTENDED_GAIN, "extended-gain"),
        ];
        NAMED.iter().filter(|(bit, _)| self.has(*bit)).map(|(_, n)| *n).collect()
    }
}

/// One entry of the extended sample-rate table
/// (`hydrasdr_samplerate_info_t`), eight bytes: the rate, the ADC's bit depth,
/// and whether that rate arrives as raw ADC samples or as complex I/Q the
/// device has already downconverted.
///
/// Only firmware advertising [`Capabilities::EXTENDED_SAMPLERATES`] has this.
/// It matters here for one reason: a rate whose `data_format` is
/// [`DataFormat::IqDirect`] does **not** need — and must not be given — the
/// host conversion in [`sdroxide_airspy::convert`], because the device has done
/// it. This driver does not implement that path yet, so it declines such a rate
/// rather than decoding complex samples as if they were a real IF, which would
/// produce a spectrum that looks plausible and is nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateInfo {
    pub rate_hz: u32,
    pub adc_bits: u8,
    pub data_format: DataFormat,
}

/// What a rate's samples arrive as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFormat {
    /// Real samples straight off the ADC, wanted signal at fs/4. What every
    /// RFOne firmware to date sends, and what this driver decodes.
    RawAdc,
    /// Complex baseband, already downconverted on the device.
    IqDirect,
    Unknown(u8),
}

impl DataFormat {
    pub fn from_code(code: u8) -> DataFormat {
        match code {
            0 => DataFormat::RawAdc,
            1 => DataFormat::IqDirect,
            other => DataFormat::Unknown(other),
        }
    }
}

impl RateInfo {
    /// The bit depth this driver's host conversion is written for.
    pub const SUPPORTED_ADC_BITS: u8 = 12;

    /// Parse the extended table. Anything that is not a whole number of
    /// entries, or whose rate is zero, is dropped rather than half-read.
    pub fn parse_table(b: &[u8]) -> Vec<RateInfo> {
        b.chunks_exact(8)
            .map(|c| RateInfo {
                rate_hz: u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                adc_bits: c[4],
                data_format: DataFormat::from_code(c[5]),
            })
            .filter(|r| r.rate_hz > 0)
            .collect()
    }

    /// Whether this driver can decode the stream this rate produces.
    pub fn is_decodable(&self) -> bool {
        self.data_format == DataFormat::RawAdc && self.adc_bits == Self::SUPPORTED_ADC_BITS
    }
}

/// The rate to *program*, given the complex rate the operator wants.
///
/// **This is the doubling.** The RFOne's ADC is real, not complex: the receiver
/// digitises a real IF and the host makes complex baseband out of it. So "10
/// Msps" is an ADC running at 20 Msps, and libhydrasdr multiplies by two before
/// sending precisely because its own output type is IQ (`send_hw_samplerate`).
/// The firmware agrees from the other side: it matches the value it is sent
/// against each configuration's `r82x_if_freq * 4`, and the IF is a quarter of
/// the ADC rate.
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
    /// An index into the receiver's own list.
    Index(u16),
    /// The rate in kilohertz — the only way to reach an alternate
    /// configuration, because those are never listed.
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
/// The bimodal encoding is libhydrasdr's `send_hw_samplerate`, and the order of
/// the two branches is part of it. A rate the receiver **listed** goes out as
/// its *index* in that list, whatever its magnitude; only a rate that is not on
/// the list falls through to being sent as kilohertz, and then only if it is at
/// or above [`MIN_SAMPLERATE_BY_VALUE`]. The firmware makes the same
/// distinction from the other side — `usb_vendor_request_set_samplerate` reads
/// `wIndex` as an index while it is below `HYDRASDR_CONF_NB_MAX` and as a
/// kilohertz figure above that — so the two branches are not interchangeable
/// for a listed rate.
///
/// The kilohertz figure is of the *programmed* rate, which is what reaches the
/// alternate configurations: an alternate matches when `wIndex * 1000` equals
/// its `r82x_if_freq * 4`, and that product is the ADC rate.
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
    // Whole kilohertz only. Every rate the firmware has is one — the awkwardest
    // is 4.096 Msps complex, whose ADC rate is 8192 kHz exactly — and a rate
    // that is not would be silently rounded onto a configuration that does not
    // exist, which the firmware answers with a stall.
    let khz = hz / 1000;
    if khz * 1000 != hz {
        return None;
    }
    u16::try_from(khz).ok().map(RateArg::Khz)
}

/// The rate list the receiver reports, in Hz. Little-endian `u32` apiece.
///
/// **These are complex rates, not what the ADC runs at.** The firmware answers
/// `r82x_if_freq * 2` for each configuration, and the IF is a quarter of the
/// ADC rate — so an RFOne reports 10, 5 and 2.5 Msps and digitises at 20, 10
/// and 5. Read them as programmed rates and every rate on offer comes out half
/// its real size, with a receiver that streams correctly and a span that is
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

/// The payload of [`Request::SetFreq`]: **eight** little-endian bytes.
///
/// This is the one thing an Airspy R2 and an RFOne genuinely disagree about on
/// the wire. libairspy sends `struct { uint32_t freq_hz; }`; the RFOne firmware
/// declares `struct { uint64_t freq_hz; }` and schedules a receive of
/// `sizeof(set_freq_params_t)` — eight bytes — before handing the value to
/// `r82x_set_freq`.
///
/// Sending four would not fail loudly. The firmware's copy is a static that
/// starts at 100 MHz, so on a little-endian host a short write lands in the low
/// half and leaves a high half that is usually already zero: it would work,
/// until it did not.
pub fn encode_freq_hz(hz: f64) -> [u8; 8] {
    // Negative and NaN both clamp to zero rather than wrapping into an enormous
    // frequency; the firmware rejects zero, which is the right answer for a
    // caller that has asked for something impossible.
    let hz = if hz.is_finite() { hz.round().max(0.0) } else { 0.0 };
    (hz as u64).to_le_bytes()
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

    /// The serial as hex digits. The last sixteen of these are what the USB
    /// serial-number descriptor carries after [`SERIAL_PREFIX`] — the firmware
    /// prints the low two words of the SPIFI serial there.
    pub fn serial_hex(&self) -> String {
        self.serial.iter().map(|w| format!("{w:08X}")).collect()
    }
}

/// A NUL-padded ASCII reply, such as the firmware version string.
pub fn parse_ascii(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// Whether a firmware version string came from an RFOne.
///
/// libhydrasdr's check, and the one that decides a legacy-id board: it reads
/// the version string after opening and refuses anything not beginning
/// `HydraSDR RF`. An Airspy R2 on the same USB id answers `AirSpy NOS …`, so
/// this is what keeps the two drivers off each other's hardware.
pub fn is_hydrasdr_firmware(version: &str) -> bool {
    version.trim_start().starts_with(FW_PREFIX)
}

/// Whether the USB descriptors alone say this is a HydraSDR.
///
/// Cheaper than [`is_hydrasdr_firmware`] and, unlike it, usable during
/// enumeration — the strings come from the OS's cached descriptors, so nothing
/// is opened and nothing another program is streaming is disturbed. An RFOne
/// calls itself `HydraSDR RFOne` and prefixes its serial with
/// [`SERIAL_PREFIX`]; an Airspy R2 does neither.
///
/// Only ever consulted for the *legacy* id pair, where the question is real.
/// A device on the official pair is an RFOne by definition.
pub fn is_hydrasdr_strings(product: Option<&str>, serial: Option<&str>) -> bool {
    let product_says = product.is_some_and(|p| p.trim().to_ascii_lowercase().contains("hydrasdr"));
    let serial_says = serial.is_some_and(|s| {
        s.trim().to_ascii_uppercase().starts_with(&SERIAL_PREFIX.to_ascii_uppercase())
    });
    product_says || serial_says
}

/// The serial without its `HYDRASDR SN:` prefix, so a configured serial can be
/// typed as the sixteen digits the operator actually sees on a label.
///
/// A descriptor that does not carry the prefix is returned unchanged: a
/// prototype's may not, and refusing to name it would be worse than naming it
/// oddly.
pub fn strip_serial_prefix(serial: &str) -> &str {
    let s = serial.trim();
    match s.len() >= SERIAL_PREFIX.len()
        && s[..SERIAL_PREFIX.len()].eq_ignore_ascii_case(SERIAL_PREFIX)
    {
        true => s[SERIAL_PREFIX.len()..].trim(),
        false => s,
    }
}

/// Whether an operator-supplied serial names this receiver.
///
/// Suffix matching, case-insensitive and whitespace-tolerant: the value in
/// `radio.json` was typed or pasted by a human, and the descriptor's case is
/// the firmware's choice. Matching on the suffix is also what makes the
/// `HYDRASDR SN:` prefix a non-issue — the digits are at the end.
///
/// An empty `want` matches everything — that is "no serial configured", not "a
/// serial that happens to be blank".
pub fn serial_matches(want: &str, found: Option<&str>) -> bool {
    let want = strip_serial_prefix(want);
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

/// How many steps the combined-gain curves have (`RFONE_GAIN_TABLE_SIZE`).
pub const GAIN_COUNT: u8 = 22;

/// The step `rfone_gain_defs` starts a receiver on.
pub const DEFAULT_GAIN_STEP: u8 = 10;

/// Which combined-gain curve to drive the three stages from.
///
/// The R828D has an LNA, a mixer and a VGA, and setting them independently is a
/// good way to make a receiver that either overloads or hisses. HydraSDR
/// therefore publishes two curated curves through the three stages — inherited
/// unchanged from libairspy, tuner change and all — and this driver offers the
/// same choice rather than three sliders, because it is what every program that
/// drives this hardware does and what the numbers were tuned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GainCurve {
    /// Favours strong-signal handling: least intermodulation for a given
    /// sensitivity. The right default on an antenna with broadcast nearby.
    #[default]
    Linearity,
    /// Favours weak-signal sensitivity, at the cost of overload margin.
    Sensitivity,
}

impl GainCurve {
    /// The stage values for `step`, as `(lna, mixer, vga)`, where **0 is least
    /// gain** and [`GAIN_COUNT`]` - 1` is most.
    ///
    /// The tables are stored the other way round — `rfone_set_linearity_gain`
    /// indexes them with `RFONE_GAIN_TABLE_SIZE - 1 - value`, so table index 0
    /// is maximum gain. Reversing here rather than at the call sites is what
    /// keeps "more slider is more signal" true everywhere above this line.
    pub fn stages(self, step: u8) -> (u8, u8, u8) {
        // Transcribed from `hydrasdr_rfone.c`'s `rfone_linearity_*_gains` and
        // `rfone_sensitivity_*_gains`. Byte for byte the tables libairspy has:
        // the tuner moved from the R820T2 to the R828D and the stage ranges did
        // not, so HydraSDR kept the curves.
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

/// The stage ceilings the firmware clamps to (`RFONE_*_MAX_GAIN`). Every value
/// the curves produce is inside them; the test says so, because a stage written
/// out of range is rejected and leaves the front end mid-change.
pub const LNA_MAX_GAIN: u8 = 14;
pub const MIXER_MAX_GAIN: u8 = 15;
pub const VGA_MAX_GAIN: u8 = 15;

#[cfg(test)]
mod tests {
    use super::*;

    /// The one wire difference that makes this a separate driver. Four bytes
    /// where the firmware wants eight is the sort of mistake that works on a
    /// bench and fails in the field.
    #[test]
    fn the_frequency_goes_out_as_eight_little_endian_bytes() {
        assert_eq!(encode_freq_hz(100.0e6).len(), 8, "the firmware reads a u64");
        assert_eq!(encode_freq_hz(100.0e6), 100_000_000u64.to_le_bytes());
        // Above 32 bits is exactly where a four-byte write stops being
        // survivable. Nothing an RFOne tunes reaches here, but the encoding
        // must not be the thing that decides that.
        assert_eq!(encode_freq_hz(5.0e9), 5_000_000_000u64.to_le_bytes());
        // Rounded, not truncated: a dial that lands on 144.999999 Hz should
        // tune 145 000 000, not 144 999 999.
        assert_eq!(encode_freq_hz(144_999_999.6), 145_000_000u64.to_le_bytes());
        // Nonsense clamps rather than wrapping into a multi-gigahertz request.
        assert_eq!(encode_freq_hz(-1.0), [0; 8]);
        assert_eq!(encode_freq_hz(f64::NAN), [0; 8]);
    }

    /// The doubling, stated as an equation rather than a comment. Getting it
    /// wrong halves the span with nothing on screen to say so.
    #[test]
    fn the_programmed_rate_is_twice_the_complex_rate() {
        assert_eq!(program_rate_hz(10.0e6), 20.0e6);
        assert_eq!(program_rate_hz(2.5e6), 5.0e6);
        assert_eq!(program_rate_hz(4.096e6), 8.192e6);
        for complex in ALL_RATES {
            assert_eq!(complex_rate_hz(program_rate_hz(complex)), complex);
        }
    }

    /// A listed rate goes out as its index; an alternate goes out as the
    /// kilohertz of its *ADC* rate, which is the only thing the firmware will
    /// match it against.
    #[test]
    fn a_listed_rate_goes_out_as_its_index_and_an_alternate_as_kilohertz() {
        // What an RFOne reports, as programmed (doubled) rates.
        let listed: Vec<f64> = FALLBACK_RATES.iter().map(|r| program_rate_hz(*r)).collect();
        assert_eq!(listed, vec![20.0e6, 10.0e6, 5.0e6]);

        assert_eq!(encode_samplerate(program_rate_hz(10.0e6), &listed), Some(RateArg::Index(0)));
        assert_eq!(encode_samplerate(program_rate_hz(5.0e6), &listed), Some(RateArg::Index(1)));
        assert_eq!(encode_samplerate(program_rate_hz(2.5e6), &listed), Some(RateArg::Index(2)));

        // The four alternates, against the literals in the firmware's table:
        // each one's `r82x_if_freq * 4`, in kilohertz.
        for (complex, khz) in [(12.0e6, 24_000), (8.0e6, 16_000), (6.0e6, 12_000), (4.096e6, 8_192)]
        {
            assert_eq!(
                encode_samplerate(program_rate_hz(complex), &listed),
                Some(RateArg::Khz(khz)),
                "{complex} is an alternate configuration and has to go out by value"
            );
        }
        // Every alternate in the table is reachable, and none of them collides
        // with an index — the firmware reads anything under 64 as one.
        for r in ALT_RATES {
            match encode_samplerate(program_rate_hz(r), &listed) {
                Some(RateArg::Khz(k)) => assert!(k >= 64, "{r} would be read as an index"),
                other => panic!("{r} encoded as {other:?}"),
            }
        }

        // Not a whole number of kilohertz: nothing to send, and rounding would
        // aim at a configuration that does not exist.
        assert_eq!(encode_samplerate(1_000_500.0, &listed), None);
        // Sub-megahertz and unlisted: no index to send either.
        assert_eq!(encode_samplerate(300_000.0, &listed), None);
        // Too large for `wIndex` in kilohertz is a refusal rather than a wrapped
        // number that would program some unrelated rate.
        assert_eq!(encode_samplerate(100.0e9, &listed), None);
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

        // Every stage value must be inside the ceiling the firmware clamps to,
        // or a write is rejected and the front end is left mid-change.
        for curve in [GainCurve::Linearity, GainCurve::Sensitivity] {
            for step in 0..=GAIN_COUNT {
                let (l, m, v) = curve.stages(step);
                assert!(
                    l <= LNA_MAX_GAIN && m <= MIXER_MAX_GAIN && v <= VGA_MAX_GAIN,
                    "{curve:?} step {step}: {l},{m},{v}"
                );
            }
        }
        // Out of range clamps rather than panicking on a hand-edited config.
        assert_eq!(GainCurve::Linearity.stages(200), GainCurve::Linearity.stages(GAIN_COUNT - 1));
    }

    #[test]
    fn the_rate_list_is_little_endian_and_drops_zeroes() {
        // An RFOne's actual reply: three *complex* rates, digitising at twice
        // each of them.
        let mut b = Vec::new();
        for r in [10_000_000u32, 5_000_000, 2_500_000] {
            b.extend_from_slice(&r.to_le_bytes());
        }
        assert_eq!(parse_rates(&b), FALLBACK_RATES.to_vec());
        assert_eq!(
            parse_rates(&b).iter().map(|r| program_rate_hz(*r)).collect::<Vec<_>>(),
            vec![20.0e6, 10.0e6, 5.0e6],
            "the ADC runs at twice the listed rate"
        );
        // A trailing partial word is dropped, not misread.
        b.push(0x01);
        assert_eq!(parse_rates(&b).len(), 3);
        assert_eq!(parse_rates(&0u32.to_le_bytes()), Vec::<f64>::new());
        assert_eq!(parse_count(&9u32.to_le_bytes()), Some(9));
        assert_eq!(parse_count(&[1, 2, 3]), None);
    }

    /// A rate whose samples are already complex must be declined, not decoded:
    /// running the fs/4 conversion over I/Q produces a picture that looks like
    /// a working receiver and is nonsense.
    #[test]
    fn the_extended_table_says_which_rates_this_driver_can_decode() {
        let mut b = Vec::new();
        // 10 Msps raw 12-bit, then 20 Msps of on-device I/Q, then a 14-bit
        // raw rate — one decodable, two not.
        for (rate, bits, fmt) in
            [(10_000_000u32, 12u8, 0u8), (20_000_000, 12, 1), (5_000_000, 14, 0)]
        {
            b.extend_from_slice(&rate.to_le_bytes());
            b.extend_from_slice(&[bits, fmt, 0, 0]);
        }
        let t = RateInfo::parse_table(&b);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].data_format, DataFormat::RawAdc);
        assert!(t[0].is_decodable());
        assert_eq!(t[1].data_format, DataFormat::IqDirect);
        assert!(!t[1].is_decodable(), "the device has already downconverted this one");
        assert!(!t[2].is_decodable(), "the host conversion is written for 12 bits");

        // A short tail is dropped rather than half-read, and a zero rate is not
        // a rate.
        b.push(0);
        assert_eq!(RateInfo::parse_table(&b).len(), 3);
        assert!(RateInfo::parse_table(&[0u8; 8]).is_empty());
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

    /// Everything the Airspy also has is mandatory; everything HydraSDR added
    /// is not, because a board in the field may predate it.
    #[test]
    fn only_the_later_requests_are_optional() {
        for r in [
            Request::GetSamplerates,
            Request::SetPacking,
            Request::SetRfPort,
            Request::GetCapabilities,
            Request::GetTemperature,
            Request::SetGain,
        ] {
            assert!(r.is_optional(), "{r:?} postdates some shipped firmware");
        }
        for r in [
            Request::ReceiverMode,
            Request::SetFreq,
            Request::SetSamplerate,
            Request::SetLnaGain,
            Request::SetMixerGain,
            Request::SetVgaGain,
            Request::VersionStringRead,
        ] {
            assert!(!r.is_optional(), "{r:?} is what makes the receiver work");
        }
        // Against the literals in `hydrasdr_commands.h`, including the four
        // that share their numbers with the Airspy and the four that do not.
        assert_eq!(Request::ReceiverMode.code(), 1);
        assert_eq!(Request::BoardIdRead.code(), 9);
        assert_eq!(Request::VersionStringRead.code(), 10);
        assert_eq!(Request::SetFreq.code(), 13);
        assert_eq!(Request::GetSamplerates.code(), 25);
        assert_eq!(Request::SetPacking.code(), 26);
        assert_eq!(Request::SetRfPort.code(), 28);
        assert_eq!(Request::GetCapabilities.code(), 29);
        assert_eq!(Request::GetTemperature.code(), 32);
        assert_eq!(Request::SetGain.code(), 33);
    }

    /// The whole legacy-id problem, in one test: on `1d50:60a1` the descriptors
    /// are the only thing separating an RFOne from an Airspy R2 without opening
    /// either.
    #[test]
    fn a_legacy_id_board_is_told_apart_by_what_it_calls_itself() {
        assert!(is_hydrasdr_strings(Some("HydraSDR RFOne"), Some("HYDRASDR SN:0011223344556677")));
        // Either alone is enough — a prototype may carry only one of them.
        assert!(is_hydrasdr_strings(Some("HydraSDR RFOne"), None));
        assert!(is_hydrasdr_strings(None, Some("HYDRASDR SN:0011223344556677")));
        assert!(is_hydrasdr_strings(Some("hydrasdr rfone"), None), "case is the firmware's choice");
        // A real Airspy R2 on the same id must not be claimed.
        assert!(!is_hydrasdr_strings(Some("AirSpy"), Some("644064DC3238C33F")));
        assert!(!is_hydrasdr_strings(None, None));

        // And the check that settles it once the device is open.
        assert!(is_hydrasdr_firmware("HydraSDR RFOne v1.1.0"));
        assert!(!is_hydrasdr_firmware("AirSpy NOS v1.0.0-rc10-6-g4008185"));
        assert!(!is_hydrasdr_firmware(""));
    }

    #[test]
    fn a_serial_matches_on_its_suffix_with_or_without_the_prefix() {
        let full = Some("HYDRASDR SN:0011223344556677");
        assert!(serial_matches("44556677", full));
        assert!(serial_matches("44556677 ", full));
        assert!(serial_matches("0011223344556677", full));
        // The whole descriptor pasted in, prefix and all.
        assert!(serial_matches("HYDRASDR SN:0011223344556677", full));
        assert!(!serial_matches("00112233", full), "a prefix is not a suffix");
        assert!(serial_matches("", full), "no serial configured means any receiver");
        assert!(!serial_matches("44556677", None));
        assert_eq!(strip_serial_prefix("HYDRASDR SN:00AA"), "00AA");
        assert_eq!(strip_serial_prefix("00AA"), "00AA");
    }

    /// The fallback capability word is libhydrasdr's, not a guess: a driver
    /// that invented its own would disagree with every other program about what
    /// the same receiver can do.
    #[test]
    fn the_capability_fallback_matches_the_reference_and_parses_from_the_wire() {
        let c = Capabilities::RFONE_FALLBACK;
        assert!(c.has(Capabilities::RX));
        assert!(c.has(Capabilities::RF_PORT_SELECT));
        assert!(c.has(Capabilities::PACKING));
        assert!(c.has(Capabilities::BIAS_TEE));
        // The RFOne has no bandwidth control, no temperature sensor and no
        // on-device DDC, and the fallback must not claim otherwise.
        assert!(!c.has(Capabilities::BANDWIDTH));
        assert!(!c.has(Capabilities::TEMPERATURE));
        assert!(!c.has(Capabilities::EXTENDED_SAMPLERATES));
        assert!(c.names().contains(&"rf-port"));

        let wire = Capabilities::parse(&(Capabilities::RX | Capabilities::PACKING).to_le_bytes());
        assert_eq!(wire, Some(Capabilities(Capabilities::RX | Capabilities::PACKING)));
        assert_eq!(Capabilities::parse(&[1, 2, 3]), None);
    }

    #[test]
    fn the_rf_ports_are_the_three_the_firmware_names_and_only_one_has_dc_on_it() {
        assert_eq!(RfPort::Ant.code(), 0);
        assert_eq!(RfPort::Cable1.code(), 1);
        assert_eq!(RfPort::Cable2.code(), 2);
        assert_eq!(RfPort::Ant.name(), "ANT");
        assert!(RfPort::Ant.has_bias_tee());
        assert!(!RfPort::Cable1.has_bias_tee());
        assert!(!RfPort::Cable2.has_bias_tee());
        for p in [RfPort::Ant, RfPort::Cable1, RfPort::Cable2] {
            assert_eq!(RfPort::from_code(p.code()), p);
        }
        // A board id is not a way to tell an RFOne from an Airspy — both call
        // their first board zero — so the prototype's name says which it is.
        assert_eq!(BoardId::from_code(0), BoardId::Proto);
        assert_eq!(BoardId::from_code(1), BoardId::RfOne);
        assert!(matches!(BoardId::from_code(7), BoardId::Unknown(7)));
    }

    #[test]
    fn ascii_replies_stop_at_the_nul() {
        assert_eq!(parse_ascii(b"HydraSDR RFOne v1.1.0\0\0"), "HydraSDR RFOne v1.1.0");
        assert_eq!(parse_ascii(b"\0junk"), "");
        assert_eq!(parse_ascii(b""), "");
    }

    /// The rate menu has to cover both tables and nothing else: an unlisted
    /// rate the firmware does not have is answered with a stall.
    #[test]
    fn every_offered_rate_is_one_the_firmware_has() {
        for r in ALL_RATES {
            assert!(
                FALLBACK_RATES.contains(&r) || ALT_RATES.contains(&r),
                "{r} is on the menu but in neither firmware table"
            );
        }
        assert_eq!(ALL_RATES.len(), FALLBACK_RATES.len() + ALT_RATES.len());
        // Descending, so the combo reads from widest to narrowest.
        assert!(ALL_RATES.windows(2).all(|w| w[0] > w[1]));
    }
}
