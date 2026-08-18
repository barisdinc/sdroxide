//! An [`IqSource`] for an ELAD FDM-DUO, FDM-S2 or FDM-S1: wideband I/Q over
//! the native USB driver in `sdroxide-elad`, rig control over the transceiver's
//! CAT port, and transmit audio out through its USB sound card.
//!
//! # One radio, three USB devices
//!
//! An FDM-DUO enumerates three times — ELAD's own vendor interface for the
//! DDC's I/Q, an FTDI bridge for CAT, and a C-Media sound card — and this file
//! is where the three become one interface. That shape is why the pieces here
//! are borrowed rather than rebuilt: `sdroxide_cat` already drives the serial
//! link (with [`CatFamily::Elad`], the same profile `Backend::Cat` uses) and
//! `sdroxide_audio` already carries transmit audio, so what is left is the
//! joining.
//!
//! An FDM-S1 or FDM-S2 has only the first of the three and comes up
//! receive-only, with no control path at all.
//!
//! # The dial is not the centre
//!
//! Unlike a CAT rig, this front end hands over a whole DDC window — 192 kHz at
//! the least — so [`IqSource::center_is_dial`] is false and the engine tunes
//! inside it in software. The transceiver's own VFO is put where sdroxide will
//! *transmit* (see [`EladSource::set_tx_freq_hz`]), which is the dial in
//! ordinary use and the transmit frequency under split or XIT.
//!
//! # Not verified against hardware
//!
//! Nothing here has been run against a radio. Two assumptions in particular
//! deserve checking by the first person who can: that the DDC feeding this USB
//! interface is independent of the receiver the transceiver uses for its own
//! audio (so moving one does not move the other), and that the stream survives
//! a transmit cycle. The second is assumed *not* to hold — the interface is
//! declared half duplex — which is the safe way to be wrong.

use std::time::Duration;

use sdroxide_elad::{EladHandle, Model};
use sdroxide_radio::{Complex32, ControlUpdate, IqSource, Result};
use sdroxide_types::{CatConfig, CatFamily, EladConfig, Mode, TxTelemetry};

/// How long the device may deliver nothing before the connection counts as
/// dead. Same three seconds as the other native USB backends: this is a local
/// device, so there is no network to be briefly slow.
const SILENCE_BEFORE_REOPEN: Duration = Duration::from_secs(3);

/// How long a reading from the rig's S-meter stands in for the next one — the
/// same window `AudioCatSource` uses, and for the same reason: a gap between
/// answers is not a signal that went away, but a link that has gone quiet must
/// not leave a needle standing. Follows the configured poll rate, which is what
/// decides how far apart two honest answers are.

/// How the transceiver is reached.
///
/// Three cases and they are genuinely different, which is why this is an enum
/// rather than an `Option`. The serial port is the full link — it reads as well
/// as writes, so the meters, the mode and the operator's own knob all come
/// back. The USB gateway writes only. Nothing at all is an FDM-S.
enum Control {
    /// The rig's CAT serial port.
    Serial(Box<sdroxide_cat::CatHandle>),
    /// The FDM-DUO's CAT gateway on the streaming USB interface.
    ///
    /// A write-only path, and the reason it exists is worth stating: it works
    /// with **no serial cable plugged in**. A DUO on one USB lead can still be
    /// tuned, put in a mode and keyed. What is given up is everything that
    /// needs an answer — the S-meter, the SWR, the transmit power, and any
    /// notice that the operator has touched the front panel.
    Gateway,
    /// An FDM-S1 or FDM-S2: a receiver, with nothing to control.
    None,
}

pub struct EladSource {
    handle: EladHandle,
    control: Control,
    center: f64,
    rx_scratch: Vec<f32>,
    label: String,
    model: Model,

    // TX audio to the rig (interleaved stereo playback ring), exactly as
    // `AudioCatSource` drives it.
    out: Option<(sdroxide_audio::AudioOutput, sdroxide_radio::rtrb::Producer<f32>)>,
    tx_resampler: Option<sdroxide_dsp::MonoResampler>,
    tx_scratch: Vec<f32>,

    /// Mirrors of the front-end switches, so `current_gains` can answer without
    /// a round trip to the stream thread.
    attenuator: bool,
    preselector: bool,

    /// The frequency last commanded to the rig, held until the rig reports it
    /// back.
    ///
    /// Without this, split and XIT feed back on themselves: we put the rig's
    /// VFO on the *transmit* frequency ahead of key-down, the rig dutifully
    /// reports having moved there, and the engine — which cannot tell that
    /// report from the operator turning the knob — follows it with the receive
    /// dial. Suppressing exactly one report per command leaves genuine
    /// front-panel movements getting through, which is the whole point of
    /// reading the rig at all.
    expect_freq: Option<f64>,
    /// Latest SWR the rig reported while keyed, and the latest S-meter reading
    /// with the time it arrived. Held for the same reason `AudioCatSource`
    /// holds them: the engine's meter tick is far faster than the rig answers.
    last_telem: Option<TxTelemetry>,
    last_signal: Option<(std::time::Instant, f32)>,
    /// How long that S-meter reading stands in for the next one — derived from
    /// the configured poll rate, which is what decides how far apart two of the
    /// rig's answers are.
    signal_max_age: Duration,
    /// Warnings from the open, plus the one the stream thread can only raise
    /// once samples have been flowing.
    status: Vec<String>,
}

impl EladSource {
    /// Open the device, and — on an FDM-DUO — its control link and transmit
    /// audio.
    ///
    /// `cat` is the same `RadioConfig::cat` block every other CAT rig uses; an
    /// empty serial path is how an operator says there is no control cable, and
    /// on a DUO that falls through to the USB gateway rather than to nothing.
    pub fn open(
        cfg: &EladConfig,
        cat: &CatConfig,
        audio_out: Option<&str>,
        center_hz: f64,
    ) -> anyhow::Result<Self> {
        let handle = EladHandle::open(cfg, center_hz)?;
        let model = handle.model;
        let label = handle.label.clone();
        let mut status = handle.warnings.clone();

        let control = if model != Model::Duo {
            Control::None
        } else if cat.serial.path.trim().is_empty() {
            status.push(
                "no CAT serial port is set, so the FDM-DUO is being driven through its \
                 USB interface — it can be tuned and keyed, but its S-meter, SWR and \
                 front-panel knob cannot be read. Set the port under Settings → Radio."
                    .to_string(),
            );
            Control::Gateway
        } else {
            // The family is forced rather than read: this interface *is* an
            // ELAD, and a config carried over from another radio could easily
            // still say Kenwood.
            let mut cat = cat.clone();
            cat.family = CatFamily::Elad;
            Control::Serial(Box::new(sdroxide_cat::spawn(cat)))
        };
        let signal_max_age = sdroxide_cat::signal_max_age(cat);

        // TX audio is best-effort and only on the transceiver: a missing device
        // means no transmit audio, not a failed open.
        let out = if model.transmits() {
            if audio_out.is_none() {
                status.push(
                    "no sound card is set for transmit audio — the FDM-DUO's own USB \
                     Audio port should be chosen under Settings → General → Radio audio, \
                     or transmit will go out through whatever the system default is"
                        .to_string(),
                );
            }
            match sdroxide_audio::start_output(audio_out, 48_000) {
                Ok((o, p)) => Some((o, p)),
                Err(e) => {
                    tracing::warn!("ELAD TX audio device unavailable ({e}); receive only");
                    status.push(format!("transmit audio device unavailable ({e})"));
                    None
                }
            }
        } else {
            None
        };
        // `MonoResampler::new` returns None when the rates match.
        let tx_resampler = out
            .as_ref()
            .and_then(|(o, _)| sdroxide_dsp::MonoResampler::new(48_000.0, o.sample_rate));

        tracing::info!("ELAD source ready: {label}, center {center_hz:.0} Hz");
        Ok(EladSource {
            handle,
            control,
            center: center_hz,
            rx_scratch: Vec::new(),
            label,
            model,
            out,
            tx_resampler,
            tx_scratch: Vec::new(),
            attenuator: cfg.attenuator,
            preselector: cfg.preselector,
            expect_freq: None,
            last_telem: None,
            last_signal: None,
            signal_max_age,
            status,
        })
    }

    pub fn model(&self) -> Model {
        self.model
    }

    /// Whether the rig can be keyed at all — which is the question the caps'
    /// transmit channel count is really asking.
    pub fn can_transmit(&self) -> bool {
        self.model.transmits() && !matches!(self.control, Control::None)
    }

    /// Whether the control link can answer questions as well as ask them.
    pub fn reads_rig(&self) -> bool {
        matches!(self.control, Control::Serial(_))
    }

    /// Put the rig's own VFO on `hz`, by whichever path is available.
    fn command_freq(&mut self, hz: f64) {
        match &self.control {
            Control::Serial(cat) => cat.set_freq(hz),
            Control::Gateway => self.handle.send_cat(sdroxide_cat::elad::freq_frame(hz)),
            Control::None => return,
        }
        self.expect_freq = Some(hz);
    }

    fn command_ptt(&self, on: bool) {
        match &self.control {
            Control::Serial(cat) => cat.set_ptt(on),
            Control::Gateway => self.handle.send_cat(sdroxide_cat::elad::ptt_frame(on)),
            Control::None => {}
        }
    }
}

impl IqSource for EladSource {
    fn sample_rate(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.handle.set_center_hz(hz);
        Ok(())
    }

    /// The DDC window is the panadapter and the dial moves inside it, the same
    /// as any other SDR here. The transceiver's own VFO follows the *transmit*
    /// frequency instead — see [`IqSource::set_tx_freq_hz`].
    fn center_is_dial(&self) -> bool {
        false
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let need = buf.len() * 2;
        if self.rx_scratch.len() < need {
            self.rx_scratch.resize(need, 0.0);
        }
        let n = self.handle.rx_read(&mut self.rx_scratch[..need]);
        let pairs = n / 2;
        if pairs == 0 {
            // Nothing yet — brief nap so the DSP loop doesn't spin hot.
            std::thread::sleep(Duration::from_millis(2));
            return Ok(0);
        }
        for (p, out) in buf.iter_mut().enumerate().take(pairs) {
            *out = Complex32::new(self.rx_scratch[2 * p], self.rx_scratch[2 * p + 1]);
        }
        Ok(pairs)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    /// The attenuator, plus a pseudo-element carrying the pre-selection filter
    /// switch.
    ///
    /// Routing a switch through `SetGain` rather than adding a `Command`
    /// variant keeps `Command`, `DeviceCaps` and the engine untouched for a
    /// setting only this backend has — the same trick the HackRF and Airspy HF+
    /// backends use, with the names living on [`EladConfig`] so the two ends
    /// cannot drift apart.
    fn set_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        match name {
            EladConfig::ATT_ELEMENT => {
                // A gain, so negative is attenuation: anything below half the
                // pad's depth is the pad in.
                self.attenuator = db <= -sdroxide_types::ELAD_ATTENUATOR_DB / 2.0;
                self.handle.set_attenuator(self.attenuator);
            }
            EladConfig::LPF_ELEMENT => {
                self.preselector = db >= 0.5;
                self.handle.set_preselector(self.preselector);
            }
            _ => {}
        }
        Ok(())
    }

    fn current_gains(&self) -> Vec<(String, f64)> {
        vec![(
            EladConfig::ATT_ELEMENT.to_string(),
            if self.attenuator { -sdroxide_types::ELAD_ATTENUATOR_DB } else { 0.0 },
        )]
    }

    // ── Rig control ──────────────────────────────────────────────────────────

    fn poll_control(&mut self) -> Vec<ControlUpdate> {
        let Control::Serial(cat) = &self.control else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for u in cat.poll() {
            match u {
                sdroxide_cat::CatUpdate::Freq(hz) => {
                    // One report per command is ours coming back; see
                    // `expect_freq`. Anything else is the operator's knob.
                    let ours = self.expect_freq.is_some_and(|w| (w - hz).abs() < 1.0);
                    // Retired either way, and the "either way" is the point: a
                    // command the rig ignored because it was already there
                    // never produces a report, and a guard left standing would
                    // then swallow the operator's own next move to that same
                    // frequency — leaving the dial and the radio silently a
                    // band apart.
                    self.expect_freq = None;
                    if !ours {
                        out.push(ControlUpdate::Freq(hz));
                    }
                }
                sdroxide_cat::CatUpdate::Mode(m) => out.push(ControlUpdate::Mode(m)),
                // The power the rig came up on, read once when the port opened.
                sdroxide_cat::CatUpdate::Power(frac) => out.push(ControlUpdate::TxDrive(frac)),
                // The ELAD family asks for no transmit state (see
                // `Protocol::tx_state_requests`), so this never arrives — but
                // the day it does, it means the same thing here as anywhere.
                sdroxide_cat::CatUpdate::Ptt(on) => out.push(ControlUpdate::RigTx(on)),
                // The meters arrive on their own telemetry channels, not here.
                sdroxide_cat::CatUpdate::Swr(_)
                | sdroxide_cat::CatUpdate::Alc(_)
                | sdroxide_cat::CatUpdate::Signal(_) => {}
            }
        }
        out
    }

    fn set_control_mode(&mut self, mode: Mode) -> Result<()> {
        match &self.control {
            Control::Serial(cat) => cat.set_mode(mode),
            Control::Gateway => self.handle.send_cat(sdroxide_cat::elad::mode_frame(mode)),
            Control::None => {}
        }
        Ok(())
    }

    /// The rig's own receive filter.
    ///
    /// Sent even though sdroxide demodulates the wideband stream itself and has
    /// its own filter in front of the operator: the rig's filter is what shapes
    /// the audio it *transmits*, and on a radio that is also usable standalone
    /// it is the one the operator sees on the front panel. Only over the serial
    /// link — the USB gateway would carry it, but with no readback there is no
    /// way to know the rig took it.
    fn set_control_filter(&mut self, mode: Mode, lo_hz: f64, hi_hz: f64) {
        if let Control::Serial(cat) = &self.control {
            cat.set_filter(mode, lo_hz as f32, hi_hz as f32);
        }
    }

    /// Keep the transceiver's VFO on the frequency it would transmit on.
    ///
    /// The engine pushes this whenever it changes and while receiving, which is
    /// exactly what a transmitter wants: the radio is already on frequency when
    /// the key goes down rather than retuning into the first tens of
    /// milliseconds of the over. In ordinary use it is the dial; under split or
    /// XIT it is the transmit frequency, which is also what the radio's own
    /// display should then be showing.
    fn set_tx_freq_hz(&mut self, hz: f64) {
        self.command_freq(hz);
    }

    fn set_tx_drive(&mut self, frac: f64) {
        if let Control::Serial(cat) = &self.control {
            cat.set_power(frac as f32);
        }
    }

    /// Nothing to set: the rig has one power register, and the engine commands
    /// the tune level through [`Self::set_tx_drive`] for as long as TUNE holds
    /// the transmitter.
    fn set_tune_drive(&mut self, _frac: f64) {}

    fn commands_tx_power(&self) -> bool {
        match &self.control {
            Control::Serial(cat) => cat.commands_power(),
            _ => false,
        }
    }

    // The FDM-DUO has no text keyer — `SW` plays a message stored in the radio
    // and nothing hands it text — so `cw_text_keying` stays at its default of
    // `None` and CW is keyed by the operator's own key or paddle. See the
    // module header of `sdroxide_cat::elad`.

    // ── Transmit ─────────────────────────────────────────────────────────────

    fn tx_begin(&mut self, center_hz: f64, _rate: f64) -> Result<f64> {
        // The engine has already pushed this frequency through
        // `set_tx_freq_hz`, so this is a re-assertion rather than a retune —
        // and the CAT driver drops a write that would not change anything.
        // It is sent anyway because "the radio is where we last told it" is an
        // assumption with a transmitter on the end of it.
        self.command_freq(center_hz);
        self.command_ptt(true);
        Ok(self.out.as_ref().map(|(o, _)| o.sample_rate).unwrap_or(48_000.0))
    }

    fn tx_end(&mut self) -> Result<()> {
        self.command_ptt(false);
        self.last_telem = None; // drop the stale SWR reading on unkey
        Ok(())
    }

    fn tx_write_audio(&mut self, audio: &[f32]) -> Result<()> {
        let Some((_, producer)) = self.out.as_mut() else {
            return Ok(()); // no TX audio device — PTT still keyed the rig
        };
        // Resample 48 kHz → card rate, then interleave to stereo (both
        // channels), with backpressure so the engine's TX loop is paced to real
        // time rather than generating a whole burst at CPU speed.
        self.tx_scratch.clear();
        match self.tx_resampler.as_mut() {
            Some(rs) => rs.push(audio, &mut self.tx_scratch),
            None => self.tx_scratch.extend_from_slice(audio),
        }
        for &s in &self.tx_scratch {
            for _ in 0..2 {
                let mut v = s;
                let mut tries = 0u32;
                while let Err(sdroxide_radio::rtrb::PushError::Full(x)) = producer.push(v) {
                    v = x;
                    tries += 1;
                    if tries > 200 {
                        break; // output device stalled — drop rather than hang TX
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        Ok(())
    }

    fn tx_drain(&mut self) {
        // The output ring holds about a second; wait for it to play out before
        // PTT is released so the tail of a burst — which FT8 decoding depends
        // on — is not cut off.
        if let Some((_, producer)) = self.out.as_ref() {
            let cap = producer.buffer().capacity();
            for _ in 0..1000 {
                let buffered = cap.saturating_sub(producer.slots());
                if buffered <= cap / 40 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    fn tx_telemetry(&mut self) -> Option<TxTelemetry> {
        if let Control::Serial(cat) = &self.control
            && let Some(t) = cat.poll_telemetry()
        {
            self.last_telem = Some(t);
        }
        self.last_telem
    }

    fn rx_signal_dbm(&mut self) -> Option<f32> {
        // The rig's own meter, latched between its ~5 Hz answers and dropped
        // once it goes stale — a link that has stopped answering must not leave
        // a needle standing. With no serial link there is no meter, and the
        // engine falls back to measuring the I/Q itself, which for this
        // interface is a perfectly good answer.
        if let Control::Serial(cat) = &self.control
            && let Some(dbm) = cat.poll_signal()
        {
            self.last_signal = Some((std::time::Instant::now(), dbm));
        }
        self.last_signal.filter(|(at, _)| at.elapsed() < self.signal_max_age).map(|(_, dbm)| dbm)
    }

    // ── Lifecycle ────────────────────────────────────────────────────────────

    /// A device that has been unplugged, or whose thread has died, is reported
    /// as needing a reopen so the engine reconnects on its own — which is what
    /// makes replugging one Just Work rather than needing Apply pressed.
    fn needs_reopen(&self) -> bool {
        !self.handle.is_alive() || self.handle.silent_for() >= SILENCE_BEFORE_REOPEN
    }

    /// Hand the device back before the engine opens its replacement. Without
    /// this, changing anything in Settings → Radio on a running ELAD fails with
    /// "held by another program" — the other program being us.
    fn release(&mut self) {
        self.handle.release();
    }

    /// Surface what an operator needs to know but cannot see.
    fn open_status(&self) -> Option<String> {
        let mut parts = self.status.clone();
        // The sample-rate check needs a couple of seconds of stream before it
        // can say anything, so it arrives long after the open did.
        if let Some(w) = self.handle.take_late_warning() {
            parts.push(w);
        }
        parts.push(
            "ELAD support is new and has not been verified against real hardware. \
             If it misbehaves, Settings → Radio has a Copy diagnostic report button."
                .to_string(),
        );
        Some(parts.join(" — "))
    }
}
