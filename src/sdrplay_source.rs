//! An [`IqSource`] for an SDRplay RSP driven through the vendor's API service
//! by the native driver in `sdroxide-sdrplay` — no SoapySDR in the path.
//!
//! Receive only: the trait's transmit methods already default to errors, which
//! is the correct answer for this hardware.

use std::time::Duration;

use sdroxide_radio::{Complex32, IqSource, Result, lo_offset_for};
use sdroxide_sdrplay::SdrPlayHandle;
use sdroxide_types::{SdrPlayAgc, SdrPlayConfig, SdrPlayModel};

/// How long the receiver may deliver nothing before the connection counts as
/// dead. The service delivers continuously while healthy, so three seconds of
/// silence means the session is gone, same as the other local backends.
const SILENCE_BEFORE_REOPEN: Duration = Duration::from_secs(3);

pub struct SdrPlaySource {
    handle: SdrPlayHandle,
    center: f64,
    rx_scratch: Vec<f32>,
    label: String,
    lo_offset: f64,
    /// Last values the operator asked for, reported back until the hardware
    /// says otherwise (the GainChange event keeps `IF` honest under AGC).
    if_gr_db: i32,
    agc: SdrPlayAgc,
    bias_tee: bool,
    antenna: String,
    /// The ports this model (and, on the RSPduo, this tuner) offers — what
    /// the capabilities advertise. Empty hides the selector.
    antennas: Vec<String>,
}

impl SdrPlaySource {
    pub fn open(cfg: &SdrPlayConfig, center_hz: f64) -> anyhow::Result<Self> {
        let handle = sdroxide_sdrplay::spawn(cfg, center_hz)?;
        let rate = handle.out_rate_hz();
        let label = format!("{} @ {:.3} Msps", handle.label(), rate / 1e6);
        // Zero-IF part: park the LO off the VFO where the analog filter
        // allows it — see `sdroxide_radio::lo_offset_for`.
        let lo_offset = lo_offset_for(rate, handle.analog_bw_hz());
        let antennas: Vec<String> =
            handle.model().antennas(cfg.duo_tuner).iter().map(|s| s.to_string()).collect();
        let antenna = if cfg.antenna.is_empty() {
            antennas.first().cloned().unwrap_or_default()
        } else {
            cfg.antenna.clone()
        };
        tracing::info!(
            "SDRplay source ready: {label}, center {center_hz:.0} Hz, \
             LO offset {lo_offset:.0} Hz (0 = LO on the VFO)"
        );
        Ok(SdrPlaySource {
            center: center_hz,
            rx_scratch: Vec::new(),
            label,
            lo_offset,
            if_gr_db: cfg.if_gr_db,
            agc: cfg.agc,
            bias_tee: cfg.bias_tee,
            antenna,
            antennas,
            handle,
        })
    }

    pub fn model(&self) -> SdrPlayModel {
        self.handle.model()
    }

    pub fn antennas(&self) -> &[String] {
        &self.antennas
    }
}

impl IqSource for SdrPlaySource {
    /// The engine is transmitting and has stopped reading, or has started
    /// again. Passed straight through to the stream thread, which keeps
    /// receiving either way — this only decides whether a full ring is
    /// reported as an overrun or as the ordinary cost of an over. See
    /// [`IqSource::set_rx_paused`].
    fn set_rx_paused(&mut self, paused: bool) {
        self.handle.set_rx_paused(paused);
    }

    fn sample_rate(&self) -> f64 {
        self.handle.out_rate_hz()
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.handle.set_center_hz(hz);
        Ok(())
    }

    fn lo_offset_hz(&self) -> f64 {
        self.lo_offset
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let need = buf.len() * 2;
        if self.rx_scratch.len() < need {
            self.rx_scratch.resize(need, 0.0);
        }
        let n = self.handle.read(&mut self.rx_scratch[..need]);
        let pairs = n / 2;
        if pairs == 0 {
            // Nothing yet — brief nap so the DSP loop doesn't spin hot.
            std::thread::sleep(Duration::from_millis(2));
            return Ok(0);
        }
        for p in 0..pairs {
            buf[p] = Complex32::new(self.rx_scratch[2 * p], self.rx_scratch[2 * p + 1]);
        }
        Ok(pairs)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    /// The two real gain elements, plus pseudo-elements for everything else.
    ///
    /// Both real elements are carried negated — `IF` is −(gain reduction),
    /// `LNA` is −(state) — so that on the sliders more is louder, like every
    /// other backend. The switches ride `SetGain` for the usual reason: no
    /// new `Command` variant for settings only this backend has.
    fn set_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        match name {
            SdrPlayConfig::IF_GAIN_ELEMENT => {
                let gr = (-db).round() as i32;
                self.if_gr_db = gr.clamp(SdrPlayConfig::IF_GR_MIN, SdrPlayConfig::IF_GR_MAX);
                self.handle.set_if_gr_db(self.if_gr_db);
            }
            SdrPlayConfig::LNA_ELEMENT => {
                let state = (-db).round().clamp(0.0, 255.0) as u8;
                self.handle.set_lna_state(state);
            }
            SdrPlayConfig::AGC_ELEMENT => {
                self.agc = SdrPlayAgc::from_code(db);
                self.handle.set_agc(self.agc.code() as i32);
            }
            SdrPlayConfig::AGC_SETPOINT_ELEMENT => {
                self.handle.set_agc_setpoint(db.round() as i32);
            }
            SdrPlayConfig::PPM_ELEMENT => {
                self.handle.set_ppm(db);
            }
            SdrPlayConfig::BIAS_TEE_ELEMENT => {
                self.bias_tee = db >= 0.5;
                self.handle.set_bias_tee(self.bias_tee);
            }
            SdrPlayConfig::RF_NOTCH_ELEMENT => {
                self.handle.set_rf_notch(db >= 0.5);
            }
            SdrPlayConfig::DAB_NOTCH_ELEMENT => {
                self.handle.set_dab_notch(db >= 0.5);
            }
            SdrPlayConfig::HDR_ELEMENT => {
                self.handle.set_hdr(db >= 0.5);
            }
            _ => {}
        }
        Ok(())
    }

    /// What the hardware is actually doing. `IF` prefers the GainChange
    /// event's answer, which follows the AGC; `LNA` is the programmed state
    /// after any per-band clamp.
    fn current_gains(&self) -> Vec<(String, f64)> {
        let gr = self.handle.effective_if_gr_db().unwrap_or(self.if_gr_db);
        vec![
            (SdrPlayConfig::LNA_ELEMENT.to_string(), -(self.handle.lna_state() as f64)),
            (SdrPlayConfig::IF_GAIN_ELEMENT.to_string(), -(gr as f64)),
        ]
    }

    fn set_antenna(&mut self, name: &str) -> Result<()> {
        self.antenna = name.to_string();
        self.handle.set_antenna(name);
        Ok(())
    }

    fn current_antenna(&self) -> String {
        self.antenna.clone()
    }

    /// An RSP whose service session died — unplug, service restart — reports
    /// as needing a reopen so the engine reconnects on its own.
    fn needs_reopen(&self) -> bool {
        !self.handle.is_alive() || self.handle.silent_for() >= SILENCE_BEFORE_REOPEN
    }

    /// Hand the device back to the service before the engine opens its
    /// replacement; without this, Apply fails with "in use" — by us.
    fn release(&mut self) {
        self.handle.release();
    }

    /// Standing conditions an operator needs to see: a degraded enumeration,
    /// DC on the antenna port, and an ADC being overloaded right now.
    fn open_status(&self) -> Option<String> {
        let mut notes = Vec::new();
        // A device the service lists without an identity streams but often
        // hears nothing — and nothing else about the session looks wrong, so
        // this note is the only thing standing between the operator and a
        // deaf receiver with no explanation.
        if let Some(w) =
            sdroxide_types::SdrPlayDevice::degraded_warning(self.handle.serial(), self.model())
        {
            notes.push(w);
        }
        if self.bias_tee {
            notes.push(format!("{}: bias tee is ON — ~4.7 V DC on the antenna port", self.label));
        }
        if self.handle.overloaded() {
            notes.push(
                "RF overload — raise the LNA slider (more attenuation), lower IF gain, or \
                 enable AGC"
                    .to_string(),
            );
        }
        if notes.is_empty() { None } else { Some(notes.join("\n")) }
    }
}
