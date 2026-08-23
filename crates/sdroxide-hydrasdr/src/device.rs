//! The receiver's state, and the sequences that change it.
//!
//! # The orderings that matter
//!
//! **Identity comes first.** A prototype RFOne shares its USB id with the
//! Airspy R2, so the firmware version string is read before anything is
//! programmed: this driver's `SET_FREQ` is eight bytes wide and the Airspy's is
//! four, and a receiver that has been configured by the wrong driver is a
//! receiver quietly tuned somewhere else. See [`Device::identify`].
//!
//! **A rate change is a stop, a reprogram and a restart.** The R828D's IF and
//! the Si5351C's dividers both move with the sample rate, so changing it while
//! the endpoint is streaming leaves samples in flight that belong to the old
//! configuration. [`Device::set_rate`] therefore brackets the change with
//! [`ReceiverMode::Off`] and leaves restarting to the stream, which is the only
//! thing that knows whether an endpoint is open.
//!
//! **A gain change is three writes.** The curves set the LNA, the mixer and the
//! VGA together, and a front end left with one stage from one step and two from
//! another is in a state neither curve describes. They go out as a group, and
//! the AGC loops are switched off first — a loop still running would overwrite
//! whichever stage it owns a moment later.

use sdroxide_types::HydraSdrConfig;

use crate::error::{Error, Result};
use crate::protocol::{
    ALT_RATES, BoardId, Capabilities, FALLBACK_RATES, GAIN_COUNT, GainCurve, PartIdSerial,
    RateInfo, ReceiverMode, Request, RfPort, complex_rate_hz, encode_freq_hz, encode_samplerate,
    is_hydrasdr_firmware, parse_ascii, parse_count, parse_rates, program_rate_hz,
};
use crate::usb::UsbDev;

/// The receiver, its current settings, and the sequences that change them.
pub struct Device {
    usb: UsbDev,
    /// Programmed (real) rates the receiver *listed*, in the order it listed
    /// them. Twice the complex rates, and the only ones that can go out as an
    /// index.
    rates_programmed: Vec<f64>,
    /// Every programmed rate this board can actually be put on: the listed ones
    /// plus whichever alternates survived being tried. See [`Device::apply_rate`].
    reachable_programmed: Vec<f64>,
    /// The programmed rate in use.
    rate_programmed: f64,
    /// Set when the configured rate was not on offer and had to be snapped.
    pub snapped_from: Option<f64>,

    center_hz: f64,
    curve: GainCurve,
    gain_step: u8,
    lna_agc: bool,
    mixer_agc: bool,
    rf_port: RfPort,
    bias_tee: bool,
    packing: bool,
    /// Whether the firmware actually took the packing request. Older builds
    /// stall it, and the stream has to unpack differently depending.
    packing_active: bool,
    /// Whether the firmware took the RF port request. A board whose port cannot
    /// be commanded is on `ANT`, which is what it powers up on.
    rf_port_active: bool,

    pub firmware: String,
    pub board_id: Option<BoardId>,
    pub caps: Capabilities,
    /// The extended per-rate table, where the firmware publishes one. Empty
    /// otherwise, which means "every rate is 12-bit raw ADC" — true of every
    /// RFOne firmware to date.
    rate_info: Vec<RateInfo>,
    pub part_serial: Option<PartIdSerial>,
    mode: ReceiverMode,
}

impl Device {
    /// Open the receiver and program it for receive, without starting the
    /// stream.
    pub fn open(usb: UsbDev, cfg: &HydraSdrConfig, center_hz: f64) -> Result<Device> {
        let mut dev = Device {
            usb,
            rates_programmed: Vec::new(),
            reachable_programmed: Vec::new(),
            rate_programmed: program_rate_hz(cfg.sample_rate_hz),
            snapped_from: None,
            center_hz,
            curve: GainCurve::from_code(cfg.gain_curve.code()),
            gain_step: cfg.gain_step.min(GAIN_COUNT - 1),
            lna_agc: cfg.lna_agc,
            mixer_agc: cfg.mixer_agc,
            rf_port: RfPort::from_code(cfg.rf_port.code()),
            bias_tee: cfg.bias_tee,
            packing: cfg.packing,
            packing_active: false,
            rf_port_active: false,
            firmware: String::new(),
            board_id: None,
            caps: Capabilities::RFONE_FALLBACK,
            rate_info: Vec::new(),
            part_serial: None,
            // Unknown until the opening sequence states it, which is what
            // stops it being assumed.
            mode: ReceiverMode::Rx,
        };

        // Stopped first, whatever a previous program left behind. Everything
        // below reprograms clocks and dividers, and doing that under a running
        // endpoint is how a receiver ends up streaming a configuration nobody
        // asked for.
        dev.set_mode(ReceiverMode::Off)?;
        dev.identify()?;
        dev.read_capabilities();
        dev.read_rates();
        dev.apply_packing();
        dev.apply_rf_port();
        dev.choose_rate(cfg.sample_rate_hz)?;
        dev.apply_rate()?;
        dev.retune()?;
        dev.apply_gains()?;
        dev.apply_bias_tee()?;
        Ok(dev)
    }

    /// Read what the receiver says about itself, and refuse it if it is not one
    /// of ours.
    ///
    /// **This is the only fully dependable HydraSDR test there is.** A
    /// prototype RFOne enumerates on `1d50:60a1`, which is the Airspy R2's own
    /// pair; the enumeration in [`crate::usb::list`] separates them by their
    /// descriptor strings, but descriptors are what a firmware chose to write
    /// and the version string is what libhydrasdr itself checks. An Airspy
    /// answers `AirSpy NOS …`, and being told so plainly is far better than
    /// being tuned four bytes' worth of somewhere else.
    ///
    /// Everything after the version string is best-effort: a receiver that will
    /// not name its board still receives.
    ///
    /// A version string that will not come back at all is only fatal on the
    /// *shared* id. On HydraSDR's own `38af:0001` there is nothing else it could
    /// be, so refusing to open over a control read that failed would cost a
    /// working receiver for no safety; on `1d50:60a1` the read is the whole
    /// evidence, and without it this driver would be programming a tuner that
    /// may belong to another radio.
    fn identify(&mut self) -> Result<()> {
        match self.usb.control_in(Request::VersionStringRead, 0, 0, 255) {
            Ok(reply) => {
                self.firmware = parse_ascii(&reply);
                // Always shorter than the buffer it is asked for, so the SHORT
                // marker beside it means nothing. Saying so keeps the marker
                // meaningful everywhere else in this trace.
                self.usb.trace().note("(the short version-string reply above is expected)");
                if !is_hydrasdr_firmware(&self.firmware) {
                    let what = if self.firmware.to_ascii_lowercase().contains("airspy") {
                        " — that is an Airspy R2 or Mini, which shares this USB id with \
                         HydraSDR's prototype boards. Choose \"Airspy R2 / Mini (USB)\" \
                         in Settings → Radio instead: the two receivers program their \
                         tuners differently and neither driver will tune the other's \
                         hardware correctly."
                            .to_string()
                    } else {
                        String::new()
                    };
                    return Err(Error::WrongRadio(format!(
                        "{} answered {:?}, which is not HydraSDR firmware{what}",
                        self.usb.label(),
                        self.firmware,
                    )));
                }
            }
            Err(e) if self.usb.is_legacy_usb_id() => {
                return Err(Error::WrongRadio(format!(
                    "{} would not say which firmware it is running ({e}), and it is on \
                     the USB id HydraSDR's prototype boards share with the Airspy R2 and \
                     Mini. Refusing rather than programming a tuner that may belong to \
                     the other radio — if this really is an Airspy, choose \
                     \"Airspy R2 / Mini (USB)\" in Settings → Radio.",
                    self.usb.label(),
                )));
            }
            Err(e) => self.usb.trace().note(format!(
                "the receiver would not name its firmware ({e}); carrying on, because \
                 the USB id is HydraSDR's own and nothing else answers on it"
            )),
        }

        if let Ok(b) = self.usb.control_in(Request::BoardIdRead, 0, 0, 1)
            && let Some(code) = b.first()
        {
            self.board_id = Some(BoardId::from_code(*code));
        }
        if let Ok(b) = self.usb.control_in(Request::BoardPartidSerialnoRead, 0, 0, 24) {
            self.part_serial = PartIdSerial::parse(&b);
        }
        let serial = self.part_serial.map(|p| p.serial_hex()).unwrap_or_default();
        self.usb.trace().set_identity(format!(
            "{} — firmware {:?}, board {}, usb {:04x}:{:04x}, serial {}, {}",
            self.usb.label(),
            self.firmware,
            self.board_id.map(|b| b.name()).unwrap_or_else(|| "unread".to_string()),
            self.usb.usb_id().0,
            self.usb.usb_id().1,
            if serial.is_empty() { "unknown".into() } else { serial },
            self.usb.speed_name(),
        ));
        Ok(())
    }

    /// Ask the firmware what it can do, and what its rates really are.
    ///
    /// Optional in both halves. A firmware without `GetCapabilities` gets
    /// libhydrasdr's own `RFONE_HARDCODED_CAPS`, which is what every other
    /// program assumes for the same receiver; a firmware without the extended
    /// rate table is one whose every rate is 12-bit raw ADC, which is the only
    /// arrangement this driver decodes and the only one shipped so far.
    fn read_capabilities(&mut self) {
        if let Some(b) = self.usb.optional_in(Request::GetCapabilities, 0, 0, 4)
            && let Some(c) = Capabilities::parse(&b)
        {
            self.caps = c;
            self.usb
                .trace()
                .note(format!("firmware reports capabilities: {}", c.names().join(", ")));
        } else {
            self.usb.trace().note(
                "this firmware has no capability word (it predates v1.1.0); \
                 assuming the RFOne feature set libhydrasdr falls back to",
            );
        }
    }

    /// Ask the receiver which rates it has.
    ///
    /// Two-step: ask for the count, then for that many words. Firmware too old
    /// to answer falls back to the RFOne's three listed rates.
    ///
    /// **The receiver lists complex rates**, so they are doubled on the way in
    /// — this list is programmed rates throughout, and the reply is the one
    /// place the two units meet. See [`parse_rates`].
    ///
    /// **And the list is not all of them.** The firmware's alternate
    /// configurations are never reported and can only be reached by value, so
    /// they are appended here from [`ALT_RATES`] and then *checked* when one is
    /// selected — see [`Device::apply_rate`].
    fn read_rates(&mut self) {
        let count = self
            .usb
            .optional_in(Request::GetSamplerates, 0, 0, 4)
            .and_then(|b| parse_count(&b))
            .filter(|n| *n > 0 && *n <= 32);
        let listed = count
            .and_then(|n| {
                self.usb.optional_in(Request::GetSamplerates, 0, n as u16, (n * 4) as u16)
            })
            .map(|b| parse_rates(&b))
            .filter(|r| !r.is_empty());

        match listed {
            Some(rates) => {
                self.usb.trace().note(format!(
                    "the receiver lists {} complex, digitising at twice each",
                    rates
                        .iter()
                        .map(|r| format!("{:.3} Msps", r / 1e6))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                self.rates_programmed = rates.iter().map(|r| program_rate_hz(*r)).collect();
            }
            None => {
                // The values in `FALLBACK_RATES` are complex; the list this
                // holds is programmed.
                self.rates_programmed =
                    FALLBACK_RATES.iter().map(|r| program_rate_hz(*r)).collect();
                self.usb.trace().note(
                    "the receiver did not report its sample rates (firmware predates the \
                     request); assuming the RFOne's 10, 5 and 2.5 Msps",
                );
            }
        }

        // The extended table, where there is one. Its only use here is to
        // exclude a rate this driver cannot decode — see `RateInfo`.
        if self.caps.has(Capabilities::EXTENDED_SAMPLERATES) {
            let n = self.rates_programmed.len() as u16;
            if let Some(b) = self.usb.optional_in(Request::GetSamplerates, 1, n, n * 8) {
                self.rate_info = RateInfo::parse_table(&b);
                for r in &self.rate_info {
                    if !r.is_decodable() {
                        self.usb.trace().note(format!(
                            "{:.3} Msps arrives as {:?} at {} bits, which this driver does \
                             not decode; it will not be offered",
                            r.rate_hz as f64 / 1e6,
                            r.data_format,
                            r.adc_bits
                        ));
                    }
                }
            }
        }

        // Everything the board can be put on, listed first so a listed rate
        // always wins a tie against an alternate at the same distance.
        //
        // The alternates are offered blind — they are not in the extended table
        // and never will be, because the firmware only ever describes its
        // primary configurations. That is fine while at least one listed rate
        // still arrives as raw ADC samples: the alternates are the same signal
        // path at another clock. It stops being fine on a firmware where *every*
        // listed rate has moved to the on-device DDC, because then the raw path
        // this driver decodes is gone and an alternate would hand back complex
        // samples to be read as a real IF — a picture that looks like a working
        // receiver and is nonsense. So that case drops them.
        let mut reachable: Vec<f64> =
            self.rates_programmed.iter().copied().filter(|p| self.rate_is_decodable(*p)).collect();
        if reachable.is_empty() {
            self.usb.trace().note(
                "every rate this firmware lists arrives already downconverted, which \
                 this driver does not decode; the alternate configurations are not \
                 offered either, because there is no way to ask whether they are raw",
            );
        } else {
            reachable.extend(ALT_RATES.iter().map(|r| program_rate_hz(*r)));
        }
        self.reachable_programmed = reachable;
    }

    /// Whether the extended table forbids this programmed rate. Anything the
    /// table does not mention — every alternate, and every rate on a firmware
    /// without the table — is assumed decodable, which is what an RFOne has
    /// always been.
    fn rate_is_decodable(&self, programmed_hz: f64) -> bool {
        let complex = complex_rate_hz(programmed_hz).round() as u32;
        match self.rate_info.iter().find(|i| i.rate_hz == complex) {
            Some(i) => i.is_decodable(),
            None => true,
        }
    }

    /// Pick the programmed rate closest to what was asked for, and record it if
    /// that meant moving.
    fn choose_rate(&mut self, want_complex_hz: f64) -> Result<()> {
        let want = program_rate_hz(want_complex_hz);
        if self.reachable_programmed.is_empty() {
            return Err(Error::Unsupported(
                "the receiver offers no sample rates this driver can decode".to_string(),
            ));
        }
        let best = self
            .reachable_programmed
            .iter()
            .copied()
            .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
            .expect("the list is not empty");
        if (best - want).abs() > 1.0 {
            self.snapped_from = Some(want_complex_hz);
            self.usb.trace().note(format!(
                "{:.3} Msps complex is not on offer; using {:.3}",
                want_complex_hz / 1e6,
                complex_rate_hz(best) / 1e6
            ));
        }
        self.rate_programmed = best;
        Ok(())
    }

    // ---- state -----------------------------------------------------------

    pub fn usb(&self) -> &UsbDev {
        &self.usb
    }

    /// The complex rate the host produces — half what the receiver runs at.
    pub fn sample_rate_hz(&self) -> f64 {
        complex_rate_hz(self.rate_programmed)
    }

    /// Every complex rate this receiver can be put on, for `DeviceCaps` — the
    /// listed ones and the alternates together, because an operator choosing
    /// from a menu should not have to know which table a rate came out of.
    pub fn available_rates(&self) -> Vec<f64> {
        let mut r: Vec<f64> =
            self.reachable_programmed.iter().map(|p| complex_rate_hz(*p)).collect();
        r.sort_by(|a, b| b.total_cmp(a));
        r.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        r
    }

    pub fn center_hz(&self) -> f64 {
        self.center_hz
    }

    /// Whether the firmware took the packing request. The stream unpacks only
    /// when this is true.
    pub fn packing_active(&self) -> bool {
        self.packing_active
    }

    /// Whether the RF port could be commanded at all.
    pub fn rf_port_active(&self) -> bool {
        self.rf_port_active
    }

    pub fn rf_port(&self) -> RfPort {
        self.rf_port
    }

    pub fn describe(&self) -> String {
        match self.part_serial.map(|p| p.serial_hex()) {
            Some(s) if !s.is_empty() => {
                format!("{} ({})", self.usb.label(), &s[s.len().saturating_sub(8)..])
            }
            _ => self.usb.label().to_string(),
        }
    }

    pub fn gain_step(&self) -> u8 {
        self.gain_step
    }

    // ---- primitives ------------------------------------------------------

    fn set_mode(&mut self, mode: ReceiverMode) -> Result<()> {
        self.usb.out(Request::ReceiverMode, mode as u16, 0)?;
        self.mode = mode;
        Ok(())
    }

    /// Start the sample flow. Called by the stream once its endpoint is armed.
    pub fn start(&mut self) -> Result<()> {
        self.set_mode(ReceiverMode::Rx)
    }

    /// Stop the sample flow.
    pub fn stop(&mut self) -> Result<()> {
        self.set_mode(ReceiverMode::Off)
    }

    fn retune(&mut self) -> Result<()> {
        // **Eight** little-endian bytes of hertz — see `encode_freq_hz`, and
        // note that the Airspy this protocol was forked from sends four. The
        // firmware does all the R828D and Si5351C register programming from
        // this; unlike the RTL-SDR, where the host owns the tuner, there is no
        // frequency planning to do here.
        self.usb.control_out(Request::SetFreq, 0, 0, &encode_freq_hz(self.center_hz))
    }

    /// Program the chosen rate, and prove that it took.
    ///
    /// The check is the point. Three of this receiver's seven rates live in the
    /// firmware's *alternate* table, which no enumeration reports; a build
    /// without them answers the request with a stall rather than an error the
    /// operator would see, and the receiver would then keep streaming at
    /// whatever rate it was already on — a span that is silently wrong while
    /// everything else about the picture looks right. So an alternate that is
    /// refused falls back to the nearest listed rate and says so.
    pub fn apply_rate(&mut self) -> Result<()> {
        let listed = self.rates_programmed.contains(&self.rate_programmed);
        let arg =
            encode_samplerate(self.rate_programmed, &self.rates_programmed).ok_or_else(|| {
                Error::Unsupported(format!(
                    "{:.3} Msps is not one of the rates the receiver listed and cannot be \
                     named in whole kilohertz, so there is nothing to send for it",
                    self.rate_programmed / 1e6
                ))
            })?;
        self.usb.trace().rate_plan(
            complex_rate_hz(self.rate_programmed),
            self.rate_programmed,
            &format!("{arg:?}"),
            &self.rates_programmed,
        );

        match self.usb.set(Request::SetSamplerate, 0, arg.value()) {
            Ok(()) => Ok(()),
            // A listed rate that is refused is a real fault: there is nothing
            // else to try and nothing sensible to assume.
            Err(e) if listed => Err(e),
            Err(e) => {
                let refused = complex_rate_hz(self.rate_programmed);
                self.usb.trace().note(format!(
                    "{:.3} Msps is an alternate configuration this firmware does not \
                     have ({e}); falling back to the nearest listed rate",
                    refused / 1e6
                ));
                // Never offer it again this session, so a later rate change
                // cannot walk back into it.
                self.reachable_programmed.retain(|p| *p != self.rate_programmed);
                // And retry against the *listed* rates only. Falling back onto
                // another alternate could be refused in turn, and this is not a
                // loop: one refusal is a firmware that does not have the second
                // table, so nothing in it is worth a second attempt.
                let want = self.rate_programmed;
                let Some(best) = self
                    .rates_programmed
                    .iter()
                    .copied()
                    .filter(|p| self.rate_is_decodable(*p))
                    .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
                else {
                    return Err(e);
                };
                self.snapped_from = Some(refused);
                self.rate_programmed = best;
                let arg = encode_samplerate(best, &self.rates_programmed).ok_or_else(|| {
                    Error::Unsupported(
                        "no sample rate this receiver listed can be encoded".to_string(),
                    )
                })?;
                self.usb.trace().rate_plan(
                    complex_rate_hz(best),
                    best,
                    &format!("{arg:?}"),
                    &self.rates_programmed,
                );
                self.usb.set(Request::SetSamplerate, 0, arg.value())
            }
        }
    }

    fn apply_packing(&mut self) {
        // Optional: firmware predating it stalls the request, and the only
        // correct answer is to carry on unpacked.
        let took = self.usb.optional_in(Request::SetPacking, 0, u16::from(self.packing), 1);
        self.packing_active = self.packing && took.is_some();
        if self.packing && !self.packing_active {
            self.usb.trace().note(
                "this firmware has no 12-bit packing; streaming unpacked, which is \
                 a third more USB bandwidth",
            );
        }
    }

    /// Select the RF input.
    ///
    /// HydraSDR's own, with no Airspy equivalent: the RFOne brings out three
    /// sockets and only `ANT` has the bias tee on it. The firmware validates
    /// the port itself and answers 1 for "taken", so a board that does not have
    /// the request — or does not have three ports — falls back to `ANT`, which
    /// is what it powers up on.
    fn apply_rf_port(&mut self) {
        let took = self.usb.optional_in(Request::SetRfPort, 0, u16::from(self.rf_port.code()), 1);
        self.rf_port_active = matches!(took.as_deref(), Some([1, ..]));
        if !self.rf_port_active {
            if self.rf_port != RfPort::Ant {
                self.usb.trace().note(format!(
                    "this firmware would not select the {} port; the receiver is on ANT",
                    self.rf_port.name()
                ));
            }
            self.rf_port = RfPort::Ant;
        }
    }

    /// The three tuner stages, as a group, with the AGC loops settled first.
    ///
    /// Order matters: a loop still running would overwrite whichever stage it
    /// owns a moment after this sets it, which reads as a gain control that
    /// does nothing.
    fn apply_gains(&mut self) -> Result<()> {
        // Every one of these carries its argument in `wIndex` and is a control
        // *read* with a one-byte return code — see `UsbDev::set`.
        self.usb.set(Request::SetLnaAgc, 0, u16::from(self.lna_agc))?;
        self.usb.set(Request::SetMixerAgc, 0, u16::from(self.mixer_agc))?;
        let (lna, mixer, vga) = self.curve.stages(self.gain_step);
        // Only the stages the AGC is not driving. Writing a stage under its own
        // loop is not harmful, but it is a write that does nothing and it
        // clutters the trace a fault would be read from.
        if !self.lna_agc {
            self.usb.set(Request::SetLnaGain, 0, lna as u16)?;
        }
        if !self.mixer_agc {
            self.usb.set(Request::SetMixerGain, 0, mixer as u16)?;
        }
        self.usb.set(Request::SetVgaGain, 0, vga as u16)
    }

    /// The bias tee, which is one of the few genuine control-OUTs — and which
    /// still reads its argument from `wIndex`, not `wValue`. Putting it in
    /// `wValue` leaves the feedline unpowered however the switch is set, with a
    /// transfer that completes and says nothing.
    ///
    /// Only the antenna port has one. Asking for DC while the tuner is on a
    /// cable port would leave a switch on in the settings and nothing on the
    /// coax, so this refuses it here rather than pretending.
    fn apply_bias_tee(&mut self) -> Result<()> {
        let on = self.bias_tee && self.rf_port.has_bias_tee();
        if self.bias_tee && !on {
            self.usb.trace().note(format!(
                "the bias tee is only on the ANT port; the receiver is on {}, so it \
                 stays off",
                self.rf_port.name()
            ));
        }
        self.usb.out(Request::SetRfBias, 0, u16::from(on))
    }

    // ---- operations ------------------------------------------------------

    pub fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center_hz = hz;
        self.retune()
    }

    /// Change the sample rate.
    ///
    /// Stops the receiver first: the rate moves the tuner's IF and the clock
    /// dividers together, and reprogramming those under a running endpoint
    /// leaves samples in flight that belong to the old configuration. Restarting
    /// is the stream's job — it is the only thing that knows whether an
    /// endpoint is armed — so this returns with the receiver stopped.
    pub fn set_rate(&mut self, complex_hz: f64) -> Result<()> {
        self.stop()?;
        self.choose_rate(complex_hz)?;
        self.apply_rate()?;
        // The tuner's IF moved with the rate, so the frequency has to be
        // restated or the receiver is left near — but not on — the dial.
        self.retune()
    }

    pub fn set_gain_step(&mut self, step: u8) -> Result<()> {
        self.gain_step = step.min(GAIN_COUNT - 1);
        self.apply_gains()
    }

    pub fn set_curve(&mut self, curve: GainCurve) -> Result<()> {
        self.curve = curve;
        self.apply_gains()
    }

    pub fn set_lna_agc(&mut self, on: bool) -> Result<()> {
        self.lna_agc = on;
        self.apply_gains()
    }

    pub fn set_mixer_agc(&mut self, on: bool) -> Result<()> {
        self.mixer_agc = on;
        self.apply_gains()
    }

    /// Move the tuner to another socket.
    ///
    /// The bias tee is restated afterwards because it belongs to the antenna
    /// port alone: leaving it on while the tuner walks to a cable port would
    /// leave DC where nothing expects it, and moving back would otherwise
    /// arrive with the feed unpowered.
    pub fn set_rf_port(&mut self, port: RfPort) -> Result<()> {
        self.rf_port = port;
        self.apply_rf_port();
        self.apply_bias_tee()
    }

    pub fn set_bias_tee(&mut self, on: bool) -> Result<()> {
        self.bias_tee = on;
        self.apply_bias_tee()
    }

    /// Put the receiver away.
    ///
    /// Best-effort and idempotent: every step is attempted even if an earlier
    /// one failed, because this runs on the way out — including from a `Drop`
    /// after something has already gone wrong.
    pub fn shutdown(&mut self) {
        let _ = self.usb.out(Request::ReceiverMode, ReceiverMode::Off as u16, 0);
        self.mode = ReceiverMode::Off;
        // The bias tee outlives the process on this hardware, so leaving it on
        // would leave DC on a feedline with nobody driving it. Off is `wIndex`
        // zero, as it is everywhere else.
        let _ = self.usb.out(Request::SetRfBias, 0, 0);
    }
}
