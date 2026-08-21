//! The one place an `lms_device_t*` is dereferenced.
//!
//! Everything that takes a device pointer goes through [`DevCtl`], including
//! the LimeRFE calls — which reach the board by bit-banging I²C on the
//! LimeSDR's GPIO pins and so touch the same device. Keeping them behind one
//! type is what makes it possible to say where the boundary is: the streaming
//! calls take an `lms_stream_t*` and touch only LimeSuite's own FIFO, so they
//! never come through here.

use std::ffi::c_char;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ffi;

/// What the board says it is.
#[derive(Debug, Clone, Default)]
pub struct DevInfo {
    pub name: String,
    pub firmware: String,
    pub hardware: String,
    pub gateware: String,
    pub serial: String,
}

pub struct DevCtl {
    api: Arc<ffi::Api>,
    dev: ffi::Device,
    channel: usize,
}

// The pointer is only ever reachable through `&mut self`, and the type is not
// `Clone`, so there is exactly one owner at a time.
unsafe impl Send for DevCtl {}

impl DevCtl {
    pub(crate) fn new(api: Arc<ffi::Api>, dev: ffi::Device, channel: usize) -> DevCtl {
        DevCtl { api, dev, channel }
    }

    pub(crate) fn api(&self) -> &Arc<ffi::Api> {
        &self.api
    }

    pub(crate) fn raw(&self) -> ffi::Device {
        self.dev
    }

    pub fn channel(&self) -> usize {
        self.channel
    }

    /// Every call here funnels through this: LimeSuite reports failure as `-1`
    /// and puts the reason somewhere else entirely.
    fn check(&self, call: &'static str, rc: std::ffi::c_int) -> Result<()> {
        if rc == ffi::OK { Ok(()) } else { Err(Error::api(call, self.api.err_text())) }
    }

    /// Put the chip into the state LimeSuite calls "ready for operation". Must
    /// come before anything else — the datasheet default is not it.
    pub fn init(&mut self) -> Result<()> {
        let rc = unsafe { (self.api.init)(self.dev) };
        self.check("LMS_Init", rc)
    }

    pub fn num_channels(&self, tx: bool) -> usize {
        let n = unsafe { (self.api.get_num_channels)(self.dev, tx) };
        if n < 0 { 0 } else { n as usize }
    }

    pub fn enable_channel(&mut self, tx: bool, on: bool) -> Result<()> {
        let rc = unsafe { (self.api.enable_channel)(self.dev, tx, self.channel, on) };
        self.check("LMS_EnableChannel", rc)
    }

    /// Set the host sample rate for every channel at once — LimeSuite has no
    /// per-channel form, and on this silicon the two directions share a clock
    /// tree anyway.
    pub fn set_sample_rate(&mut self, rate: f64, oversample: u8) -> Result<()> {
        let rc = unsafe { (self.api.set_sample_rate)(self.dev, rate, oversample as usize) };
        self.check("LMS_SetSampleRate", rc)
    }

    /// The rate actually in force, host side. Worth reading back rather than
    /// assuming: LimeSuite snaps to what the clock tree can synthesise.
    pub fn sample_rate(&self, tx: bool) -> Result<f64> {
        let mut host = 0.0f64;
        let mut rf = 0.0f64;
        let rc =
            unsafe { (self.api.get_sample_rate)(self.dev, tx, self.channel, &mut host, &mut rf) };
        self.check("LMS_GetSampleRate", rc)?;
        Ok(host)
    }

    pub fn rate_range(&self, tx: bool) -> Result<ffi::Range> {
        let mut r = ffi::Range::default();
        let rc = unsafe { (self.api.get_sample_rate_range)(self.dev, tx, &mut r) };
        self.check("LMS_GetSampleRateRange", rc)?;
        Ok(r)
    }

    pub fn set_lo(&mut self, tx: bool, hz: f64) -> Result<()> {
        let rc = unsafe { (self.api.set_lo_frequency)(self.dev, tx, self.channel, hz) };
        self.check("LMS_SetLOFrequency", rc)
    }

    pub fn lo(&self, tx: bool) -> Result<f64> {
        let mut hz = 0.0f64;
        let rc = unsafe { (self.api.get_lo_frequency)(self.dev, tx, self.channel, &mut hz) };
        self.check("LMS_GetLOFrequency", rc)?;
        Ok(hz)
    }

    /// The synthesiser's reach.
    ///
    /// Read, never assumed, and the single most load-bearing thing this module
    /// reports: a LimeSDR asked for a frequency below the LMS7002M's range
    /// reconfigures its interface clock, fails half way, and then delivers
    /// nothing at all until the process restarts. The engine's retune guard is
    /// what stops the call being made, and it can only work from a published
    /// range.
    pub fn lo_range(&self, tx: bool) -> Result<ffi::Range> {
        let mut r = ffi::Range::default();
        let rc = unsafe { (self.api.get_lo_frequency_range)(self.dev, tx, &mut r) };
        self.check("LMS_GetLOFrequencyRange", rc)?;
        if !(r.min.is_finite() && r.max.is_finite()) || r.max <= r.min {
            return Err(Error::api(
                "LMS_GetLOFrequencyRange",
                format!("nonsensical range {}..{}", r.min, r.max),
            ));
        }
        Ok(r)
    }

    /// The port names this board offers, minus `NONE` — which is a real entry
    /// in LimeSuite's list and means "disconnected", not a choice anyone makes
    /// from a combo.
    pub fn antennas(&self, tx: bool) -> Vec<String> {
        let n = unsafe {
            (self.api.get_antenna_list)(self.dev, tx, self.channel, std::ptr::null_mut())
        };
        if n <= 0 {
            return Vec::new();
        }
        let mut buf = vec![[0 as c_char; ffi::NAME_LEN]; n as usize];
        let n =
            unsafe { (self.api.get_antenna_list)(self.dev, tx, self.channel, buf.as_mut_ptr()) };
        if n <= 0 {
            return Vec::new();
        }
        buf.iter()
            .take(n as usize)
            .map(|e| ffi::c_field(e))
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
            .collect()
    }

    /// Select a port by name. The index is the position in LimeSuite's own
    /// list, `NONE` included, so the lookup is done against the unfiltered list
    /// rather than the one shown to the operator.
    pub fn set_antenna_named(&mut self, tx: bool, name: &str) -> Result<()> {
        let n = unsafe {
            (self.api.get_antenna_list)(self.dev, tx, self.channel, std::ptr::null_mut())
        };
        if n <= 0 {
            return Err(Error::api("LMS_GetAntennaList", self.api.err_text()));
        }
        let mut buf = vec![[0 as c_char; ffi::NAME_LEN]; n as usize];
        let n =
            unsafe { (self.api.get_antenna_list)(self.dev, tx, self.channel, buf.as_mut_ptr()) };
        let idx = buf
            .iter()
            .take(n.max(0) as usize)
            .position(|e| ffi::c_field(e).eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| {
                Error::api("LMS_SetAntenna", format!("this board has no port called {name:?}"))
            })?;
        let rc = unsafe { (self.api.set_antenna)(self.dev, tx, self.channel, idx) };
        self.check("LMS_SetAntenna", rc)
    }

    pub fn antenna(&self, tx: bool) -> String {
        let idx = unsafe { (self.api.get_antenna)(self.dev, tx, self.channel) };
        if idx < 0 {
            return String::new();
        }
        let n = unsafe {
            (self.api.get_antenna_list)(self.dev, tx, self.channel, std::ptr::null_mut())
        };
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![[0 as c_char; ffi::NAME_LEN]; n as usize];
        let n =
            unsafe { (self.api.get_antenna_list)(self.dev, tx, self.channel, buf.as_mut_ptr()) };
        buf.get(idx as usize).filter(|_| n > 0).map(|e| ffi::c_field(e)).unwrap_or_default()
    }

    /// The combined gain. LimeSuite takes an integer, so this rounds and clamps
    /// — and [`Self::gain_db`] reads back what the chip actually got, which is
    /// what the settings panel shows.
    pub fn set_gain_db(&mut self, tx: bool, db: f64) -> Result<()> {
        let g = db
            .round()
            .clamp(sdroxide_types::LimeConfig::GAIN_MIN_DB, sdroxide_types::LimeConfig::GAIN_MAX_DB)
            as u32;
        let rc = unsafe { (self.api.set_gain_db)(self.dev, tx, self.channel, g) };
        self.check("LMS_SetGaindB", rc)
    }

    pub fn gain_db(&self, tx: bool) -> Option<f64> {
        let mut g = 0u32;
        let rc = unsafe { (self.api.get_gain_db)(self.dev, tx, self.channel, &mut g) };
        (rc == ffi::OK).then_some(f64::from(g))
    }

    pub fn set_lpf_bw(&mut self, tx: bool, hz: f64) -> Result<()> {
        let rc = unsafe { (self.api.set_lpf_bw)(self.dev, tx, self.channel, hz) };
        self.check("LMS_SetLPFBW", rc)
    }

    pub fn lpf_range(&self, tx: bool) -> Result<ffi::Range> {
        let mut r = ffi::Range::default();
        let rc = unsafe { (self.api.get_lpf_bw_range)(self.dev, tx, &mut r) };
        self.check("LMS_GetLPFBWRange", rc)?;
        Ok(r)
    }

    /// LimeSuite's own DC-offset and IQ-imbalance calibration. Hundreds of
    /// milliseconds, so never in a tuning path.
    pub fn calibrate(&mut self, tx: bool, bw_hz: f64) -> Result<()> {
        let rc =
            unsafe { (self.api.calibrate)(self.dev, tx, self.channel, bw_hz, ffi::CAL_FLAGS_NONE) };
        self.check("LMS_Calibrate", rc)
    }

    pub fn chip_temp_c(&self) -> Option<f64> {
        let mut t = 0.0f64;
        let rc = unsafe { (self.api.get_chip_temperature)(self.dev, 0, &mut t) };
        (rc == ffi::OK).then_some(t)
    }

    pub fn info(&self) -> DevInfo {
        let p = unsafe { (self.api.get_device_info)(self.dev) };
        if p.is_null() {
            return DevInfo::default();
        }
        // Copied out while the device is open: LimeSuite frees this storage on
        // close, and the header says so.
        let i = unsafe { *p };
        DevInfo {
            name: ffi::c_field(&i.device_name),
            firmware: ffi::c_field(&i.firmware_version),
            hardware: ffi::c_field(&i.hardware_version),
            gateware: ffi::c_field(&i.gateware_version),
            serial: format!("{:016X}", i.board_serial_number),
        }
    }
}

impl DevCtl {
    /// Close the device now rather than waiting for the last holder to drop —
    /// the reopen path needs the board free *before* the replacement's
    /// `LMS_Open`. Idempotent, so `Drop` running afterwards is harmless.
    ///
    /// The caller answers for ordering: nothing else may be using the pointer
    /// when this runs — see `LimeHandle::close` for what that means for the
    /// LimeRFE's board link.
    pub(crate) fn close(&mut self) {
        if !self.dev.is_null() {
            unsafe { (self.api.close)(self.dev) };
            self.dev = std::ptr::null_mut();
        }
    }
}

impl Drop for DevCtl {
    fn drop(&mut self) {
        self.close();
    }
}

/// The analog filter width to use for a given sample rate, when the operator
/// has not named one.
///
/// **Wide on purpose.** A filter narrower than a quarter of the span silently
/// withdraws the zero-IF LO offset rather than merely softening the band edges
/// — see `sdroxide_radio::lo_offset_for`, whose doc spells out the trap — so
/// this errs generous and lets the digital filters do the selectivity.
pub fn auto_lpf_bw(rate_hz: f64, range: ffi::Range) -> f64 {
    let want = rate_hz * 1.25;
    if range.max > range.min && range.min > 0.0 { want.clamp(range.min, range.max) } else { want }
}

/// Below this, LimeSuite parks the synthesiser *at* it and the TSP NCO makes
/// up the difference — the LMS7002M's SX simply stops at 30 MHz
/// (`LMS7_Device::SetFrequency`).
pub const NCO_LO_FLOOR_HZ: f64 = 30e6;

/// The analog filter width to actually program, given the width the operator
/// wants and the centre the synthesiser is about to be handed.
///
/// Above 30 MHz this is the wanted width unchanged. Below it, the NCO trick
/// above puts the wanted signal up to 30 MHz away from DC *inside the analog
/// chain* — LimeSuite retunes the data converters to span that offset but
/// leaves the analog low-pass wherever it was told (`LMS7_Device::SetLPF`
/// tunes around DC, NCO-blind). A filter chosen from the sample rate alone
/// then sits with its corner at a few MHz while the signal rides at 8–28 MHz,
/// which on transmit is the difference between full power and milliwatts —
/// issue #118's "TX very low compared to SDR-Console" was exactly this.
///
/// The floor is the worst case for the whole of HF rather than the current
/// offset, deliberately: retuning these filters costs LimeSuite's MCU a few
/// hundred milliseconds, so the only boundary an ordinary tune may cross is
/// 30 MHz itself, once — never band-to-band within HF.
pub fn effective_lpf_bw(want_hz: f64, center_hz: f64, rate_hz: f64, range: ffi::Range) -> f64 {
    let mut bw = want_hz;
    if center_hz < NCO_LO_FLOOR_HZ {
        bw = bw.max((2.0 * NCO_LO_FLOOR_HZ + rate_hz) * 1.25);
    }
    if range.max > range.min && range.min > 0.0 { bw.clamp(range.min, range.max) } else { bw }
}

/// The receive port to use when the operator has not named one.
///
/// LimeSuite has an "auto" value for this, but what it does is undocumented, so
/// the choice is made here where it can be read. LNAL is the low-band input and
/// LNAH the high one; LNAW spans both at the cost of a couple of dB.
pub fn auto_antenna_rx(hz: f64, available: &[String]) -> Option<String> {
    let want = if hz < 1.5e9 { "LNAL" } else { "LNAH" };
    available
        .iter()
        .find(|a| a.eq_ignore_ascii_case(want))
        .or_else(|| available.iter().find(|a| a.eq_ignore_ascii_case("LNAW")))
        .or_else(|| available.first())
        .cloned()
}

/// The transmit port to use when the operator has not named one. BAND1 is the
/// one wired to a connector on every board in the family.
pub fn auto_antenna_tx(available: &[String]) -> Option<String> {
    available
        .iter()
        .find(|a| a.eq_ignore_ascii_case("BAND1"))
        .or_else(|| available.first())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter has to stay clear of the quarter-span the LO offset needs,
    /// or the offset is withdrawn and the LO leakage lands back on the VFO.
    #[test]
    fn the_automatic_filter_is_wider_than_the_lo_offset_needs() {
        let range = ffi::Range { min: 1.4e6, max: 130.0e6, step: 0.0 };
        for rate in [2.0e6, 5.0e6, 10.0e6, 20.0e6] {
            let bw = auto_lpf_bw(rate, range);
            assert!(
                bw > rate * 0.25 / 0.45,
                "at {rate} the filter {bw} is too narrow to keep the LO offset"
            );
        }
    }

    /// And it stays inside what the chip will accept.
    #[test]
    fn the_automatic_filter_is_clamped_to_the_chips_range() {
        let range = ffi::Range { min: 1.4e6, max: 130.0e6, step: 0.0 };
        assert_eq!(auto_lpf_bw(0.2e6, range), 1.4e6, "clamped up to the minimum");
        assert_eq!(auto_lpf_bw(200.0e6, range), 130.0e6, "clamped down to the maximum");
    }

    /// Below 30 MHz the synthesiser parks there and the NCO carries the rest,
    /// so the signal rides at the offset inside the analog chain — the filter
    /// must span it, or transmit comes out at milliwatts (issue #118).
    #[test]
    fn below_30_mhz_the_filter_opens_for_the_nco_offset() {
        let range = ffi::Range { min: 5.0e6, max: 130.0e6, step: 0.0 };
        let rate = 5.0e6;
        let want = auto_lpf_bw(rate, range);
        let hf = effective_lpf_bw(want, 14.1e6, rate, range);
        assert!(
            hf >= 2.0 * NCO_LO_FLOOR_HZ + rate,
            "{hf} does not span the worst NCO offset plus the span"
        );
        // One figure for the whole of HF: tuning band to band below 30 MHz
        // must never land the slow filter retune.
        assert_eq!(hf, effective_lpf_bw(want, 1.8e6, rate, range));
        assert_eq!(hf, effective_lpf_bw(want, 29.9e6, rate, range));
        // Above the boundary the wanted width passes through untouched.
        assert_eq!(effective_lpf_bw(want, 145.5e6, rate, range), want);
        // And the chip's ceiling holds where the floor would pass it.
        let fast = effective_lpf_bw(auto_lpf_bw(61.44e6, range), 14.1e6, 61.44e6, range);
        assert!(fast <= range.max);
    }

    /// A hand-set narrow filter gets the same floor: the operator's number is
    /// a width for the *signal*, not permission to park the passband 20 MHz
    /// away from where the NCO put it.
    #[test]
    fn the_nco_floor_applies_to_a_hand_set_width_too() {
        let range = ffi::Range { min: 5.0e6, max: 130.0e6, step: 0.0 };
        let hf = effective_lpf_bw(2.5e6, 14.1e6, 2.0e6, range);
        assert!(hf >= 2.0 * NCO_LO_FLOOR_HZ + 2.0e6);
        assert_eq!(effective_lpf_bw(8.0e6, 145.5e6, 2.0e6, range), 8.0e6);
    }

    #[test]
    fn the_automatic_port_follows_the_frequency_and_falls_back() {
        let all: Vec<String> = ["LNAH", "LNAL", "LNAW"].iter().map(|s| s.to_string()).collect();
        assert_eq!(auto_antenna_rx(14.2e6, &all).as_deref(), Some("LNAL"));
        assert_eq!(auto_antenna_rx(2.4e9, &all).as_deref(), Some("LNAH"));

        // A board that offers only the wideband input still gets an answer.
        let wide = vec!["LNAW".to_string()];
        assert_eq!(auto_antenna_rx(14.2e6, &wide).as_deref(), Some("LNAW"));
        // And one that offers nothing at all gets none, rather than a guess.
        assert_eq!(auto_antenna_rx(14.2e6, &[]), None);
    }

    #[test]
    fn the_automatic_transmit_port_prefers_band1() {
        let all: Vec<String> = ["BAND1", "BAND2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(auto_antenna_tx(&all).as_deref(), Some("BAND1"));
        let only2 = vec!["BAND2".to_string()];
        assert_eq!(auto_antenna_tx(&only2).as_deref(), Some("BAND2"));
        assert_eq!(auto_antenna_tx(&[]), None);
    }
}
