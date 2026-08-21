//! An [`IqSource`] for a LimeSDR driven through LimeSuite by `sdroxide-lime`,
//! and the LimeRFE in front of it.
//!
//! # No thread
//!
//! Unlike the native USB backends this one calls the library straight from the
//! engine's thread. That is not an omission: LimeSuite already runs its own USB
//! workers behind its own FIFO, and `LMS_RecvStream` takes a timeout, so the
//! shape here is the SoapySDR source's — one `read_within` called with 200 ms
//! from [`IqSource::read`] and zero from [`IqSource::read_available`]. Stacking
//! a second ring on top would add latency and buy nothing.
//!
//! # Drive stays digital
//!
//! [`IqSource::set_tx_drive`] is left at its trait default and
//! `commands_tx_power` stays false, for the reason spelled out in
//! `hackrf_source.rs`: the engine already applies drive to the samples, so a
//! source that also mapped it onto the transmit gain would attenuate twice.
//! The gain is published as an ordinary transmit element instead.
//!
//! # Where the LimeRFE band-follow hooks in
//!
//! Two signals, and the receive one is not obvious. [`IqSource::set_tx_freq_hz`]
//! is the transmit side and exists for exactly this — its doc names
//! band-switching amplifiers as the case it was written for. The receive side
//! is **not** `set_center_hz` alone: this is a zero-IF radio, so the engine
//! parks the LO a quarter-span above the dial, and near a band edge that is the
//! difference between the right filter and the wrong one. The operator's actual
//! receive frequency is `centre + if_offset`, and `set_if_offset` is what
//! carries the second term.

use std::time::Duration;

use sdroxide_dsp::IqCorrect;
use sdroxide_lime::LimeHandle;
use sdroxide_lime::handle::RX_TIMEOUT_MS;
use sdroxide_limerfe::LimeRfeHandle;
use sdroxide_radio::{Complex32, DC_BLOCK_HZ, IqSource, RadioError, Result, lo_offset_for};
use sdroxide_types::{LimeConfig, RfeLink, RfeModeControl};

pub struct LimeSource {
    handle: LimeHandle,
    cfg: LimeConfig,
    center: f64,
    lo_offset: f64,
    /// The operator's receive dial relative to the centre, as the engine last
    /// reported it. Zero until the first `set_if_offset`, which is within a
    /// quarter span and corrected on the first tuning update.
    if_offset: f64,
    iq_correct: Option<IqCorrect>,
    rfe: Option<LimeRfeHandle>,
    /// Set when the LimeRFE could not be opened. Reported through
    /// `open_status` rather than refusing the radio: a front end that failed to
    /// answer should not cost the operator their receiver.
    rfe_note: Option<String>,
    label: String,
}

impl LimeSource {
    pub fn open(cfg: &LimeConfig, center_hz: f64) -> anyhow::Result<Self> {
        let handle = LimeHandle::open(cfg, center_hz)?;
        let rate = handle.sample_rate();
        let label = format!("{} @ {:.3} Msps", handle.label(), rate / 1e6);
        // Zero-IF part: park the LO off the VFO where the analog filter allows
        // it — see `sdroxide_radio::lo_offset_for`.
        let lo_offset = lo_offset_for(rate, handle.analog_bw());
        let iq_correct = cfg.iq_correction.then(|| IqCorrect::new(DC_BLOCK_HZ, rate));

        // The LimeRFE. Its own USB cable is opened here; the path through the
        // board's GPIO needs the device handle and so is built in the crate
        // that owns it.
        let (rfe, rfe_note) = Self::open_rfe(cfg, &handle);

        tracing::info!(
            "LimeSDR source ready: {label}, centre {center_hz:.0} Hz, LO offset \
             {lo_offset:.0} Hz (0 = LO on the VFO){}",
            match &rfe {
                Some(r) => format!(", {}", r.describe()),
                None => String::new(),
            }
        );

        Ok(LimeSource {
            handle,
            cfg: cfg.clone(),
            center: center_hz,
            lo_offset,
            if_offset: 0.0,
            iq_correct,
            rfe,
            rfe_note,
            label,
        })
    }

    /// Open the front end, if one is configured.
    ///
    /// A LimeRFE that will not answer costs a note, not the radio: an operator
    /// who came to listen should not lose their receiver because an accessory
    /// is unplugged.
    fn open_rfe(cfg: &LimeConfig, handle: &LimeHandle) -> (Option<LimeRfeHandle>, Option<String>) {
        if cfg.rfe.link == RfeLink::Off {
            return (None, None);
        }
        match sdroxide_lime::open_rfe(&cfg.rfe, handle) {
            Ok(h) => (h, None),
            Err(e) => {
                tracing::warn!("LimeRFE not opened: {e}");
                (None, Some(format!("the LimeRFE was not opened: {e}")))
            }
        }
    }

    /// Whether the transmitter is armed and usable.
    pub fn tx_enabled(&self) -> bool {
        self.handle.can_tx()
    }

    /// The synthesiser's reach in one direction, as the board reports it.
    ///
    /// `None` where LimeSuite would not say, which the capability layer treats
    /// as "unknown" rather than "nowhere" — but on this hardware a missing
    /// range is what lets the engine make the call that wedges the device, so
    /// it is logged rather than passed over quietly.
    pub fn freq_range(&self, tx: bool) -> Option<(f64, f64)> {
        match self.handle.lo_range(tx) {
            Ok(r) => Some((r.min, r.max)),
            Err(e) => {
                tracing::warn!(
                    "LimeSDR did not report its {} tuning range ({e}) — the engine's retune \
                     guard has nothing to work from, so a frequency below the chip's range may \
                     stop the stream",
                    if tx { "transmit" } else { "receive" }
                );
                None
            }
        }
    }

    /// The sample-rate range the board accepts.
    pub fn rate_range(&self) -> Option<(f64, f64)> {
        self.handle.rate_range(false).ok().map(|r| (r.min, r.max))
    }

    /// The port names this board offers in one direction.
    pub fn antennas(&self, tx: bool) -> Vec<String> {
        if tx { self.handle.antennas_tx().to_vec() } else { self.handle.antennas_rx().to_vec() }
    }

    /// The operator's receive frequency, which is not the hardware centre on a
    /// zero-IF radio. See the module doc.
    fn rx_dial_hz(&self) -> f64 {
        self.center + self.if_offset
    }

    fn push_rfe_rx(&self) {
        if let Some(rfe) = &self.rfe {
            rfe.set_rx_hz(self.rx_dial_hz());
        }
    }
}

impl IqSource for LimeSource {
    fn sample_rate(&self) -> f64 {
        self.handle.sample_rate()
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.handle.set_center_hz(hz).map_err(|e| RadioError::Msg(e.to_string()))?;
        self.center = hz;
        self.push_rfe_rx();
        Ok(())
    }

    fn lo_offset_hz(&self) -> f64 {
        self.lo_offset
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self
            .handle
            .read_within(buf, RX_TIMEOUT_MS)
            .map_err(|e| RadioError::Msg(e.to_string()))?;
        if n > 0
            && let Some(iq) = self.iq_correct.as_mut()
        {
            iq.process(&mut buf[..n]);
        }
        Ok(n)
    }

    /// Take only what is already waiting.
    ///
    /// This radio is full duplex, so the engine calls this every tick during an
    /// over, sharing the thread with the transmit feed — waiting here for
    /// samples that have not arrived would come out of the transmitter's
    /// budget.
    fn read_available(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.handle.read_within(buf, 0).map_err(|e| RadioError::Msg(e.to_string()))?;
        if n > 0
            && let Some(iq) = self.iq_correct.as_mut()
        {
            iq.process(&mut buf[..n]);
        }
        Ok(n)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    fn set_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        match name {
            LimeConfig::RX_GAIN_ELEMENT => {
                self.handle.set_gain_db(false, db).map_err(|e| RadioError::Msg(e.to_string()))?;
            }
            LimeConfig::LPF_RX_ELEMENT => {
                self.handle.set_lpf_bw(false, db).map_err(|e| RadioError::Msg(e.to_string()))?;
                // The filter and the LO offset are coupled: a narrower filter
                // than the offset needs withdraws the offset entirely.
                self.lo_offset = lo_offset_for(self.handle.sample_rate(), self.handle.analog_bw());
            }
            LimeConfig::LPF_TX_ELEMENT => {
                self.handle.set_lpf_bw(true, db).map_err(|e| RadioError::Msg(e.to_string()))?;
            }
            LimeConfig::CALIBRATE_ELEMENT if db >= 0.5 => {
                // Momentary, and slow — the better part of a second. Only ever
                // from an explicit request, never from a tuning path.
                self.handle.calibrate().map_err(|e| RadioError::Msg(e.to_string()))?;
            }
            LimeConfig::IQ_CORRECTION_ELEMENT => {
                if db >= 0.5 {
                    match self.iq_correct.as_mut() {
                        Some(iq) => iq.reset(),
                        None => {
                            self.iq_correct =
                                Some(IqCorrect::new(DC_BLOCK_HZ, self.handle.sample_rate()));
                        }
                    }
                } else {
                    self.iq_correct = None;
                }
            }
            // The LimeRFE controls. Carried through here for the usual reason:
            // no new `Command` variant for settings only this backend has.
            LimeConfig::RFE_ATTEN_ELEMENT => {
                self.cfg.rfe.atten_steps = (db / 2.0).round().clamp(0.0, 7.0) as u8;
                if let Some(r) = &self.rfe {
                    r.set_config(self.cfg.rfe.clone());
                }
            }
            LimeConfig::RFE_NOTCH_ELEMENT => {
                self.cfg.rfe.notch = db >= 0.5;
                if let Some(r) = &self.rfe {
                    r.set_config(self.cfg.rfe.clone());
                }
            }
            LimeConfig::RFE_FAN_ELEMENT => {
                self.cfg.rfe.fan = db >= 0.5;
                if let Some(r) = &self.rfe {
                    r.set_fan(self.cfg.rfe.fan);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// What the chip really has, not what it was asked for: LimeSuite takes an
    /// integer number of decibels, so a slider left between two of them is a
    /// radio at the lower one.
    fn current_gains(&self) -> Vec<(String, f64)> {
        vec![(LimeConfig::RX_GAIN_ELEMENT.to_string(), self.handle.rx_gain_db())]
    }

    fn set_antenna(&mut self, name: &str) -> Result<()> {
        self.handle.set_antenna(false, name).map_err(|e| RadioError::Msg(e.to_string()))
    }

    fn current_antenna(&self) -> String {
        self.handle.antenna_rx().to_string()
    }

    // ---- transmit --------------------------------------------------------

    fn tx_begin(&mut self, center_hz: f64, _rate: f64) -> Result<f64> {
        // The interlock. A LimeRFE pinned to receive will not open its transmit
        // relay, so the drive would go into the receive path with the amplifier
        // bypassed. Refusing here is the same discipline as an unarmed radio
        // publishing no transmit channel: the point is that no path can key it,
        // not that most paths remember to check.
        if let Some(reason) = self.cfg.rfe.tx_refusal() {
            return Err(RadioError::Msg(reason));
        }
        // Tell the front end before any RF appears. The board thread is already
        // holding the band; this is the relay.
        if let Some(rfe) = &self.rfe {
            rfe.set_tx_hz(center_hz);
            rfe.set_keyed(true);
            // On a shared connector the relay has to have thrown before drive
            // arrives. One round trip, and the transport knows what it costs.
            if self.cfg.rfe.needs_ptt_switching() {
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        let rate = self.handle.tx_begin(center_hz).map_err(|e| {
            // Do not leave the front end keyed if the radio refused.
            if let Some(rfe) = &self.rfe {
                rfe.set_keyed(false);
            }
            RadioError::Msg(e.to_string())
        })?;
        Ok(rate)
    }

    fn tx_write(&mut self, samples: &[Complex32]) -> Result<()> {
        self.handle.tx_write(samples).map_err(|e| RadioError::Msg(e.to_string()))
    }

    fn tx_drain(&mut self) {
        self.handle.tx_drain();
    }

    fn tx_end(&mut self) -> Result<()> {
        let stopped = self.handle.tx_end();
        // Unkey the front end whatever the radio did — an amplifier left keyed
        // because the stop failed is the worse of the two failures.
        if let Some(rfe) = &self.rfe {
            rfe.set_keyed(false);
        }
        stopped.map_err(|e| RadioError::Msg(e.to_string()))
    }

    fn set_tx_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        if name == LimeConfig::TX_GAIN_ELEMENT {
            self.handle.set_gain_db(true, db).map_err(|e| RadioError::Msg(e.to_string()))?;
        }
        Ok(())
    }

    fn current_tx_gains(&self) -> Vec<(String, f64)> {
        vec![(LimeConfig::TX_GAIN_ELEMENT.to_string(), self.handle.tx_gain_db())]
    }

    fn set_tx_antenna(&mut self, name: &str) -> Result<()> {
        self.handle.set_antenna(true, name).map_err(|e| RadioError::Msg(e.to_string()))
    }

    fn current_tx_antenna(&self) -> String {
        self.handle.antenna_tx().to_string()
    }

    /// The transmit frequency, told to us *while receiving* so the LimeRFE can
    /// be on the right band before any RF appears. This is what the trait
    /// method exists for.
    fn set_tx_freq_hz(&mut self, hz: f64) {
        if let Some(rfe) = &self.rfe {
            rfe.set_tx_hz(hz);
        }
    }

    /// How far the operator's receive dial sits from the hardware centre.
    ///
    /// Needed here for the LimeRFE rather than for the radio: on a zero-IF part
    /// the centre is parked a quarter-span off the dial, and choosing a band
    /// filter from the centre would pick the wrong one either side of a band
    /// edge.
    fn set_if_offset(&mut self, hz: f64) {
        self.if_offset = hz;
        self.push_rfe_rx();
    }

    fn needs_reopen(&self) -> bool {
        self.handle.needs_reopen()
    }

    /// Hand the hardware back before the engine opens its replacement.
    ///
    /// Two halves, in this order. The LimeRFE first: dropping the handle joins
    /// its thread, and where the link is the board's GPIO that thread bit-bangs
    /// I²C through the very device the second half closes — and where it is the
    /// RFE's own serial port, this is what frees the port for the replacement.
    /// Then the board itself: `LimeHandle::close` stops the streams and closes
    /// the device, because a board still held here fails the replacement's
    /// open on Linux (libusb refuses the second claim, as "in use" — by us)
    /// and *shares* it on Windows, where CyAPI opens the device non-exclusive
    /// and the replacement's `LMS_Init` and stream setup land on top of the
    /// running stream — both sessions dead until a program restart (changing
    /// the sample rate froze the waterfall exactly this way, issue #118).
    fn release(&mut self) {
        self.rfe = None;
        self.handle.close();
    }

    fn open_status(&self) -> Option<String> {
        let mut notes = Vec::new();
        if let Some(n) = self.handle.status_note() {
            notes.push(n);
        }
        if let Some(n) = &self.rfe_note {
            notes.push(n.clone());
        }
        if let Some(rfe) = &self.rfe
            && let Some(n) = rfe.status()
        {
            notes.push(n);
        }
        // The cabling cost, stated once where it can be acted on.
        if self.rfe.is_some()
            && let Some(n) = self.cfg.rfe.switching_note()
        {
            notes.push(n);
        }
        if self.cfg.rfe.link != RfeLink::Off && self.cfg.rfe.mode == RfeModeControl::Rx {
            notes.push(
                "the LimeRFE is pinned to receive, so this radio cannot transmit through it"
                    .to_string(),
            );
        }
        if notes.is_empty() { None } else { Some(notes.join("\n")) }
    }
}
