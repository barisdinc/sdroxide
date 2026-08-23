//! The receiver's state, and the sequences that change it.
//!
//! # The two orderings that matter
//!
//! **A rate change is a stop, a reprogram and a restart.** The R820T2's IF and
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

use sdroxide_types::AirspyConfig;

use crate::error::{Error, Result};
use crate::protocol::{
    FALLBACK_RATES, GAIN_COUNT, GainCurve, PartIdSerial, ReceiverMode, Request, complex_rate_hz,
    encode_samplerate, parse_ascii, parse_count, parse_rates, program_rate_hz,
};
use crate::usb::UsbDev;

/// The receiver, its current settings, and the sequences that change them.
pub struct Device {
    usb: UsbDev,
    /// Programmed (real) rates the receiver offers, in the order it listed
    /// them. Twice the complex rates an operator picks between.
    rates_programmed: Vec<f64>,
    /// The programmed rate in use.
    rate_programmed: f64,
    /// Set when the configured rate was not on offer and had to be snapped.
    pub snapped_from: Option<f64>,

    center_hz: f64,
    curve: GainCurve,
    gain_step: u8,
    lna_agc: bool,
    mixer_agc: bool,
    bias_tee: bool,
    packing: bool,
    /// Whether the firmware actually took the packing request. Older builds
    /// stall it, and the stream has to unpack differently depending.
    packing_active: bool,

    pub firmware: String,
    pub part_serial: Option<PartIdSerial>,
    mode: ReceiverMode,
}

impl Device {
    /// Open the receiver and program it for receive, without starting the
    /// stream.
    pub fn open(usb: UsbDev, cfg: &AirspyConfig, center_hz: f64) -> Result<Device> {
        let mut dev = Device {
            usb,
            rates_programmed: Vec::new(),
            rate_programmed: program_rate_hz(cfg.sample_rate_hz),
            snapped_from: None,
            center_hz,
            curve: GainCurve::from_code(cfg.gain_curve.code()),
            gain_step: cfg.gain_step.min(GAIN_COUNT - 1),
            lna_agc: cfg.lna_agc,
            mixer_agc: cfg.mixer_agc,
            bias_tee: cfg.bias_tee,
            packing: cfg.packing,
            packing_active: false,
            firmware: String::new(),
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
        dev.read_rates();
        dev.apply_packing();
        dev.choose_rate(cfg.sample_rate_hz)?;
        dev.apply_rate()?;
        dev.retune()?;
        dev.apply_gains()?;
        dev.apply_bias_tee()?;
        Ok(dev)
    }

    /// Read what the receiver says about itself.
    ///
    /// Best-effort with one exception: a receiver that will not name itself
    /// still receives, so nothing here is fatal *unless* it names itself as
    /// something else. HydraSDR's prototype RFOne boards were flashed with this
    /// USB id, and the two firmwares are not interchangeable — their `SET_FREQ`
    /// carries eight bytes where this one carries four, so an RFOne driven from
    /// here would tune somewhere the operator did not ask for and say nothing
    /// about it. `looks_like_hydrasdr` catches the ones whose descriptors say
    /// so; this catches the rest.
    fn identify(&mut self) -> Result<()> {
        if let Ok(b) = self.usb.control_in(Request::VersionStringRead, 0, 0, 255) {
            self.firmware = parse_ascii(&b);
            // Always shorter than the buffer it is asked for, so the SHORT
            // marker beside it means nothing. Saying so keeps the marker
            // meaningful everywhere else in this trace.
            self.usb.trace().note("(the short version-string reply above is expected)");
            if self.firmware.trim_start().starts_with("HydraSDR") {
                return Err(Error::WrongRadio(format!(
                    "{} answered {:?} — that is a HydraSDR RFOne prototype, which shares \
                     this USB id with the Airspy R2 and Mini. Choose \"HydraSDR RFOne \
                     (USB)\" in Settings → Radio instead: the two receivers program their \
                     tuners differently and neither driver will tune the other's hardware \
                     correctly.",
                    self.usb.label(),
                    self.firmware,
                )));
            }
        }
        if let Ok(b) = self.usb.control_in(Request::BoardPartidSerialnoRead, 0, 0, 24) {
            self.part_serial = PartIdSerial::parse(&b);
        }
        let serial = self.part_serial.map(|p| p.serial_hex()).unwrap_or_default();
        self.usb.trace().set_identity(format!(
            "{} — firmware {:?}, serial {}, {}",
            self.usb.label(),
            self.firmware,
            if serial.is_empty() { "unknown".into() } else { serial },
            self.usb.speed_name(),
        ));
        Ok(())
    }

    /// Ask the receiver which rates it has.
    ///
    /// Two-step: ask for the count, then for that many words. Firmware too old
    /// to answer falls back to the R2's two rates, which is what libairspy does
    /// — and which is right for an R2 and wrong for a Mini, so the fallback is
    /// traced rather than silent.
    ///
    /// **The receiver lists complex rates**, so they are doubled on the way in
    /// — this list is programmed rates throughout, and the reply is the one
    /// place the two units meet. See [`parse_rates`].
    fn read_rates(&mut self) {
        let count = self
            .usb
            .optional_in(Request::GetSamplerates, 0, 0, 4)
            .and_then(|b| parse_count(&b))
            .filter(|n| *n > 0 && *n <= 32);
        if let Some(n) = count
            && let Some(b) =
                self.usb.optional_in(Request::GetSamplerates, 0, n as u16, (n * 4) as u16)
        {
            let rates = parse_rates(&b);
            if !rates.is_empty() {
                self.usb.trace().note(format!(
                    "the receiver offers {} complex, digitising at twice each",
                    rates
                        .iter()
                        .map(|r| format!("{:.3} Msps", r / 1e6))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                self.rates_programmed = rates.iter().map(|r| program_rate_hz(*r)).collect();
                return;
            }
        }
        // The values in `FALLBACK_RATES` are complex; the list this holds is
        // programmed.
        self.rates_programmed = FALLBACK_RATES.iter().map(|r| program_rate_hz(*r)).collect();
        self.usb.trace().note(
            "the receiver did not report its sample rates (firmware predates the \
             request); assuming an R2's 10 and 2.5 Msps — a Mini will be wrong here",
        );
    }

    /// Pick the programmed rate closest to what was asked for, and record it if
    /// that meant moving.
    fn choose_rate(&mut self, want_complex_hz: f64) -> Result<()> {
        let want = program_rate_hz(want_complex_hz);
        if self.rates_programmed.is_empty() {
            return Err(Error::Unsupported(
                "the receiver offers no sample rates at all".to_string(),
            ));
        }
        let best = self
            .rates_programmed
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

    /// Every complex rate this receiver offers, for `DeviceCaps`.
    pub fn available_rates(&self) -> Vec<f64> {
        self.rates_programmed.iter().map(|r| complex_rate_hz(*r)).collect()
    }

    pub fn center_hz(&self) -> f64 {
        self.center_hz
    }

    /// Whether the firmware took the packing request. The stream unpacks only
    /// when this is true.
    pub fn packing_active(&self) -> bool {
        self.packing_active
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
        // Little-endian u32 of hertz. The firmware does all the R820T2 and
        // Si5351C register programming from this — unlike the RTL-SDR, where
        // the host owns the tuner, there is no frequency planning to do here.
        let hz = self.center_hz.round().max(0.0) as u32;
        self.usb.control_out(Request::SetFreq, 0, 0, &hz.to_le_bytes())
    }

    fn apply_rate(&mut self) -> Result<()> {
        let arg =
            encode_samplerate(self.rate_programmed, &self.rates_programmed).ok_or_else(|| {
                Error::Unsupported(format!(
                    "{:.3} Msps is below a megahertz and is not one of the rates \
                     the receiver listed, so there is no index to send for it",
                    self.rate_programmed / 1e6
                ))
            })?;
        self.usb.trace().rate_plan(
            complex_rate_hz(self.rate_programmed),
            self.rate_programmed,
            &format!("{arg:?}"),
            &self.rates_programmed,
        );
        self.usb.set(Request::SetSamplerate, 0, arg.value())
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
    /// still reads its argument from `wIndex`, not `wValue`
    /// (`usb_vendor_request_set_rf_bias_command` compares `setup.index` with
    /// 1). Putting it in `wValue` leaves the feedline unpowered however the
    /// switch is set, with a transfer that completes and says nothing.
    fn apply_bias_tee(&mut self) -> Result<()> {
        self.usb.out(Request::SetRfBias, 0, u16::from(self.bias_tee))
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
