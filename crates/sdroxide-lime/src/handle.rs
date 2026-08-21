//! An open LimeSDR: the device, its two streams, and everything done to them.
//!
//! # No thread
//!
//! Unlike every native USB backend here this one has no background thread and
//! no ring buffer, because LimeSuite already has both. `LMS_RecvStream` takes a
//! timeout and reads out of LimeSuite's own FIFO, which is exactly the
//! `IqSource::read` contract — so the shape to copy is the SoapySDR source in
//! `sdroxide-radio`, which drives this same library through SoapyLMS7 from the
//! engine thread and has done since before this backend existed.
//!
//! Stacking a second FIFO on top of LimeSuite's would add latency and buy
//! nothing. What it *does* mean is that a slow call — `LMS_Calibrate` above
//! all — must never land in a tuning path; see [`LimeHandle::set_center_hz`].
//!
//! # Both streams are set up at open
//!
//! `LMS_SetupStream` stops LimeSuite's running data threads to make room, which
//! is recorded next door in `sdroxide-radio`'s SoapySDR device as the reason a
//! stream restart there is so disruptive. Creating both directions while the
//! device is idle and then only starting and stopping them means that path is
//! never taken mid-session.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use num_complex::Complex32;
use sdroxide_types::LimeConfig;

use crate::device::{self, DevCtl, DevInfo};
use crate::error::{Error, Result};
use crate::ffi;

// The zero-copy receive below hands LimeSuite a `&mut [Complex32]` as its
// interleaved-f32 buffer. `num_complex::Complex<T>` is `#[repr(C)]`, so that is
// exactly what it is — and this is what makes the day that stops being true a
// compile error rather than a stream of transposed samples.
const _: () = assert!(std::mem::size_of::<Complex32>() == 8);
const _: () = assert!(std::mem::align_of::<Complex32>() == 4);

/// Timeout for a receive that is allowed to wait. Long enough to be worth
/// asking for, short enough that the engine's loop stays responsive. Matches
/// what the SoapySDR source next door uses for the same call.
pub const RX_TIMEOUT_MS: u32 = 200;

/// Timeout for a transmit write. LimeSuite blocks until its FIFO has room.
const TX_TIMEOUT_MS: u32 = 500;

/// How often to ask LimeSuite whether the stream is still running.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

pub struct LimeHandle {
    /// The device, shared because the LimeRFE's board link bit-bangs I²C on
    /// this same device's GPIO pins from its own thread. The boundary is
    /// exactly LimeSuite's own: a call taking an `lms_device_t*` goes through
    /// here, and a call taking an `lms_stream_t*` touches only LimeSuite's FIFO
    /// and does not — which is why the receive path never takes this lock.
    ctl: Arc<Mutex<DevCtl>>,
    /// The library, held directly so the streaming calls need no lock.
    api: Arc<ffi::Api>,
    rx: ffi::StreamT,
    tx: Option<ffi::StreamT>,
    rx_running: bool,
    tx_running: bool,

    info: DevInfo,
    label: String,
    rate: f64,
    center: f64,
    tx_center: f64,
    /// The receive filter actually in force — on HF this is wider than asked;
    /// see [`device::effective_lpf_bw`].
    analog_bw: f64,
    /// The receive filter width the operator (or the automatic choice) asked
    /// for, kept so a retune across 30 MHz can recompute what to program.
    lpf_rx_want: f64,
    lpf_tx_want: f64,
    /// The transmit filter actually in force, compared against on every
    /// key-down so the slow retune only happens when the answer changes.
    tx_lpf_applied: f64,
    /// The filter ranges, read once — `set_center_hz` is the panadapter's drag
    /// path and should not make even a cheap FFI call it does not need.
    lpf_range_rx: ffi::Range,
    lpf_range_tx: ffi::Range,

    antennas_rx: Vec<String>,
    antennas_tx: Vec<String>,
    antenna_rx: String,
    antenna_tx: String,
    rx_gain_db: f64,
    tx_gain_db: f64,

    cfg: LimeConfig,
    last_status: Instant,
    overruns: u64,
    underruns: u64,
    restarts: u64,
    /// Set when a stream was found stopped and put back. Reported once through
    /// `open_status` rather than every tick.
    note: Option<String>,
    /// Set once [`LimeHandle::close`] has run: the streams are destroyed and
    /// the device is closed, so nothing here may touch either again.
    closed: bool,
}

impl LimeHandle {
    /// The device, locked. Poisoning is recovered from rather than propagated:
    /// a panic on another thread mid-transaction leaves the radio in an unknown
    /// state, but refusing to talk to it afterwards helps nobody.
    fn ctl(&self) -> MutexGuard<'_, DevCtl> {
        self.ctl.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A handle on the device for the LimeRFE's board link, which drives its
    /// I²C through these same GPIO pins.
    pub fn shared_device(&self) -> Arc<Mutex<DevCtl>> {
        Arc::clone(&self.ctl)
    }

    /// Open, configure and start receiving.
    ///
    /// The order is not arbitrary: `LMS_Init` first because it overwrites
    /// everything, then the rate (which reprograms the clock tree every later
    /// setting depends on), then the analog filter, then the synthesiser, then
    /// gains and ports, then calibration, then the streams.
    pub fn open(cfg: &LimeConfig, center_hz: f64) -> Result<LimeHandle> {
        let (api, dev, listed) = crate::api::open(&cfg.device)?;
        let channel = usize::from(cfg.channel);
        let mut ctl = DevCtl::new(Arc::clone(&api), dev, channel);

        let n_rx = ctl.num_channels(false);
        if channel >= n_rx {
            return Err(Error::NotFound(format!(
                "{} has {n_rx} receive channel(s); channel {channel} was asked for",
                listed.label()
            )));
        }
        let want_tx = cfg.tx_enabled && ctl.num_channels(true) > channel;

        ctl.init()?;
        ctl.enable_channel(false, true)?;
        if want_tx {
            ctl.enable_channel(true, true)?;
        }

        // The rate reprograms the clock tree that the synthesiser and the
        // filters are both derived from, so it goes before either.
        ctl.set_sample_rate(cfg.sample_rate_hz, cfg.oversample)?;
        let rate = ctl.sample_rate(false).unwrap_or(cfg.sample_rate_hz);

        let lpf_range =
            ctl.lpf_range(false).unwrap_or(ffi::Range { min: 0.0, max: 0.0, step: 0.0 });
        let lpf_rx_want =
            if cfg.lpf_rx_hz > 0.0 { cfg.lpf_rx_hz } else { device::auto_lpf_bw(rate, lpf_range) };
        let analog_bw = device::effective_lpf_bw(lpf_rx_want, center_hz, rate, lpf_range);
        if analog_bw > lpf_rx_want {
            tracing::info!(
                "below 30 MHz the signal rides at the NCO offset inside the analog chain, so \
                 the receive filter opens to {:.1} MHz (instead of {:.1} MHz)",
                analog_bw / 1e6,
                lpf_rx_want / 1e6
            );
        }
        ctl.set_lpf_bw(false, analog_bw)?;

        ctl.set_lo(false, center_hz)?;

        let antennas_rx = ctl.antennas(false);
        let antennas_tx = if want_tx { ctl.antennas(true) } else { Vec::new() };
        let antenna_rx = if cfg.antenna_rx.trim().is_empty() {
            device::auto_antenna_rx(center_hz, &antennas_rx).unwrap_or_default()
        } else {
            cfg.antenna_rx.clone()
        };
        if !antenna_rx.is_empty() {
            ctl.set_antenna_named(false, &antenna_rx)?;
        }
        ctl.set_gain_db(false, cfg.rx_gain_db)?;

        let mut antenna_tx = String::new();
        let lpf_range_tx = ctl.lpf_range(true).unwrap_or(lpf_range);
        let lpf_tx_want = if cfg.lpf_tx_hz > 0.0 {
            cfg.lpf_tx_hz
        } else {
            device::auto_lpf_bw(rate, lpf_range_tx)
        };
        let mut tx_lpf_applied = 0.0;
        if want_tx {
            // Same 30 MHz rule as the receive filter above — this one is the
            // whole difference between full power and milliwatts on HF.
            let tx_bw = device::effective_lpf_bw(lpf_tx_want, center_hz, rate, lpf_range_tx);
            if ctl.set_lpf_bw(true, tx_bw).is_ok() {
                tx_lpf_applied = tx_bw;
            }
            antenna_tx = if cfg.antenna_tx.trim().is_empty() {
                device::auto_antenna_tx(&antennas_tx).unwrap_or_default()
            } else {
                cfg.antenna_tx.clone()
            };
            if !antenna_tx.is_empty() {
                ctl.set_antenna_named(true, &antenna_tx)?;
            }
            ctl.set_gain_db(true, cfg.tx_gain_db)?;
            ctl.set_lo(true, center_hz)?;
        }

        if cfg.calibrate {
            // Best-effort: an uncalibrated radio still receives, and refusing
            // to open because the calibration would not converge would be a
            // poor trade. The image is visible and the log says why.
            //
            // Calibrated for the *wanted* width, not the NCO-widened filter:
            // the span the operator uses is what the DC and image corrections
            // should be best over.
            if let Err(e) = ctl.calibrate(false, lpf_rx_want) {
                tracing::warn!("LimeSDR receive calibration failed, continuing: {e}");
            }
            if want_tx && let Err(e) = ctl.calibrate(true, lpf_tx_want) {
                tracing::warn!("LimeSDR transmit calibration failed, continuing: {e}");
            }
        }

        // Both streams while the device is idle — see the module doc.
        let mut rx = ffi::StreamT {
            handle: 0,
            is_tx: false,
            channel: cfg.channel as u32,
            fifo_size: cfg.fifo_ksamples.max(16) * 1024,
            throughput_vs_latency: cfg.throughput_vs_latency.clamp(0.0, 1.0),
            data_fmt: ffi::FMT_F32,
            // 12 bits per component over the link: three bytes a sample
            // instead of four, and nothing is lost because the converters are
            // 12-bit to begin with.
            link_fmt: ffi::LINK_FMT_I12,
        };
        let rc = unsafe { (api.setup_stream)(dev, &mut rx) };
        if rc != ffi::OK {
            return Err(Error::api("LMS_SetupStream", api.err_text()));
        }
        let mut tx = None;
        if want_tx {
            let mut s = ffi::StreamT { is_tx: true, ..rx };
            s.handle = 0;
            let rc = unsafe { (api.setup_stream)(dev, &mut s) };
            if rc != ffi::OK {
                let text = api.err_text();
                unsafe { (api.destroy_stream)(dev, &mut rx) };
                return Err(Error::api("LMS_SetupStream (tx)", text));
            }
            tx = Some(s);
        }

        let rc = unsafe { (api.start_stream)(&mut rx) };
        if rc != ffi::OK {
            let text = api.err_text();
            unsafe { (api.destroy_stream)(dev, &mut rx) };
            if let Some(mut s) = tx {
                unsafe { (api.destroy_stream)(dev, &mut s) };
            }
            return Err(Error::api("LMS_StartStream", text));
        }

        let info = ctl.info();
        let label = if info.name.is_empty() { listed.label() } else { info.name.clone() };
        let rx_gain_db = ctl.gain_db(false).unwrap_or(cfg.rx_gain_db);
        let tx_gain_db = ctl.gain_db(true).unwrap_or(cfg.tx_gain_db);

        tracing::info!(
            "LimeSDR ready: {label} (firmware {}, gateware {}), {:.3} Msps, filter {:.2} MHz, \
             centre {center_hz:.0} Hz, gain {rx_gain_db} dB{}",
            info.firmware,
            info.gateware,
            rate / 1e6,
            analog_bw / 1e6,
            if want_tx { ", transmitter armed" } else { "" }
        );

        Ok(LimeHandle {
            ctl: Arc::new(Mutex::new(ctl)),
            api: Arc::clone(&api),
            rx,
            tx,
            rx_running: true,
            tx_running: false,
            info,
            label,
            rate,
            center: center_hz,
            tx_center: center_hz,
            analog_bw,
            lpf_rx_want,
            lpf_tx_want,
            tx_lpf_applied,
            lpf_range_rx: lpf_range,
            lpf_range_tx,
            antennas_rx,
            antennas_tx,
            antenna_rx,
            antenna_tx,
            rx_gain_db,
            tx_gain_db,
            cfg: cfg.clone(),
            last_status: Instant::now(),
            overruns: 0,
            underruns: 0,
            restarts: 0,
            note: None,
            closed: false,
        })
    }

    /// Refuse a control call on a handle [`Self::close`] has already been
    /// through. The engine keeps a released source callable while the
    /// replacement is opened, so this is an answer, not an assertion.
    fn ensure_open(&self) -> Result<()> {
        if self.closed { Err(Error::Closed) } else { Ok(()) }
    }

    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn info(&self) -> &DevInfo {
        &self.info
    }
    pub fn sample_rate(&self) -> f64 {
        self.rate
    }
    pub fn analog_bw(&self) -> f64 {
        self.analog_bw
    }
    pub fn center_hz(&self) -> f64 {
        self.center
    }
    pub fn antennas_rx(&self) -> &[String] {
        &self.antennas_rx
    }
    pub fn antennas_tx(&self) -> &[String] {
        &self.antennas_tx
    }
    pub fn antenna_rx(&self) -> &str {
        &self.antenna_rx
    }
    pub fn antenna_tx(&self) -> &str {
        &self.antenna_tx
    }
    pub fn rx_gain_db(&self) -> f64 {
        self.rx_gain_db
    }
    pub fn tx_gain_db(&self) -> f64 {
        self.tx_gain_db
    }
    pub fn can_tx(&self) -> bool {
        self.tx.is_some()
    }
    pub fn chip_temp_c(&self) -> Option<f64> {
        if self.closed {
            return None;
        }
        self.ctl().chip_temp_c()
    }
    pub fn lo_range(&self, tx: bool) -> Result<ffi::Range> {
        self.ensure_open()?;
        self.ctl().lo_range(tx)
    }
    pub fn rate_range(&self, tx: bool) -> Result<ffi::Range> {
        self.ensure_open()?;
        self.ctl().rate_range(tx)
    }

    /// Retune the receive synthesiser.
    ///
    /// Deliberately *not* recalibrating: `LMS_Calibrate` costs hundreds of
    /// milliseconds and this is called from the engine's loop every time the
    /// operator drags the panadapter past the edge of the span. The calibration
    /// from open remains good across a retune of ordinary size; a band change
    /// can be recalibrated explicitly from the settings panel.
    pub fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.ensure_open()?;
        self.ctl().set_lo(false, hz)?;
        self.center = hz;
        // A port chosen automatically follows the frequency, because LNAL and
        // LNAH are wired to different pins and the wrong one is deaf rather
        // than merely worse.
        if self.cfg.antenna_rx.trim().is_empty()
            && let Some(want) = device::auto_antenna_rx(hz, &self.antennas_rx)
            && want != self.antenna_rx
        {
            self.ctl().set_antenna_named(false, &want)?;
            self.antenna_rx = want;
        }
        // Crossing 30 MHz changes which side of the NCO trick the filter has
        // to serve (see `device::effective_lpf_bw`). The answer is constant on
        // each side, so this slow call fires only on the crossing itself —
        // never while dragging around within a band. Best-effort: a tune that
        // succeeded is not refused because the filter would not follow.
        let bw = device::effective_lpf_bw(self.lpf_rx_want, hz, self.rate, self.lpf_range_rx);
        if (bw - self.analog_bw).abs() > 1.0 {
            let retuned = self.ctl().set_lpf_bw(false, bw);
            match retuned {
                Ok(()) => {
                    tracing::info!(
                        "receive filter retuned to {:.1} MHz for the 30 MHz crossing",
                        bw / 1e6
                    );
                    self.analog_bw = bw;
                    // LimeSuite's filter tuning moves the receive gain stages
                    // and does not put them back (its `SetLPF` preserves only
                    // the transmit IAMP).
                    let _ = self.ctl().set_gain_db(false, self.rx_gain_db);
                }
                Err(e) => tracing::warn!("receive filter did not follow the tune: {e}"),
            }
        }
        Ok(())
    }

    pub fn set_gain_db(&mut self, tx: bool, db: f64) -> Result<()> {
        self.ensure_open()?;
        self.ctl().set_gain_db(tx, db)?;
        // Read back rather than storing the request: LimeSuite takes an
        // integer, so what the chip got is not always what was asked for, and
        // the panel should show the truth.
        let applied = self.ctl().gain_db(tx).unwrap_or(db);
        if tx {
            self.tx_gain_db = applied;
        } else {
            self.rx_gain_db = applied;
        }
        Ok(())
    }

    pub fn set_antenna(&mut self, tx: bool, name: &str) -> Result<()> {
        self.ensure_open()?;
        self.ctl().set_antenna_named(tx, name)?;
        if tx {
            self.antenna_tx = name.to_string();
        } else {
            self.antenna_rx = name.to_string();
        }
        Ok(())
    }

    pub fn set_lpf_bw(&mut self, tx: bool, hz: f64) -> Result<()> {
        self.ensure_open()?;
        let range = if tx { self.lpf_range_tx } else { self.lpf_range_rx };
        let want = if hz > 0.0 { hz } else { device::auto_lpf_bw(self.rate, range) };
        // The 30 MHz floor applies to the operator's number too: a hand-set
        // 2.5 MHz filter under a 14 MHz dial is a transmitter at milliwatts
        // and a half-deaf receiver, which nobody has ever meant.
        let center = if tx { self.tx_center } else { self.center };
        let bw = device::effective_lpf_bw(want, center, self.rate, range);
        if bw > want {
            tracing::info!(
                "the {} filter opens to {:.1} MHz (asked {:.1} MHz): below 30 MHz the signal \
                 rides at the NCO offset inside the analog chain",
                if tx { "transmit" } else { "receive" },
                bw / 1e6,
                want / 1e6
            );
        }
        self.ctl().set_lpf_bw(tx, bw)?;
        if tx {
            self.lpf_tx_want = want;
            self.tx_lpf_applied = bw;
        } else {
            self.lpf_rx_want = want;
            self.analog_bw = bw;
            // See `set_center_hz`: the filter tuning moves the gain stages.
            let _ = self.ctl().set_gain_db(false, self.rx_gain_db);
        }
        Ok(())
    }

    /// Run LimeSuite's calibration now. Only ever from an explicit request:
    /// it stalls whatever thread calls it for the better part of a second.
    pub fn calibrate(&mut self) -> Result<()> {
        self.ensure_open()?;
        // The wanted widths, not the NCO-widened filters: the span the
        // operator uses is what the corrections should be best over.
        let bw = self.lpf_rx_want;
        self.ctl().calibrate(false, bw)?;
        if self.tx.is_some() {
            let tx_bw = self.lpf_tx_want;
            self.ctl().calibrate(true, tx_bw)?;
        }
        Ok(())
    }

    /// Read what is there, waiting up to `timeout_ms` for it.
    ///
    /// `Ok(0)` on a timeout, which is the trait's contract: the caller retries.
    pub fn read_within(&mut self, buf: &mut [Complex32], timeout_ms: u32) -> Result<usize> {
        if !self.rx_running || buf.is_empty() {
            return Ok(0);
        }
        // `Complex<f32>` is `#[repr(C)]`, so interleaved f32 I/Q *is* this
        // slice's memory — no conversion and no scratch buffer. Pinned by the
        // assert below.
        let n = unsafe {
            (self.api.recv_stream)(
                &mut self.rx,
                buf.as_mut_ptr().cast(),
                buf.len(),
                std::ptr::null_mut(),
                timeout_ms,
            )
        };
        if n < 0 {
            return Err(Error::api("LMS_RecvStream", self.api.err_text()));
        }
        self.poll_status();
        Ok(n as usize)
    }

    /// Ask LimeSuite how the stream is doing, occasionally.
    ///
    /// The reason this exists rather than being left to fail loudly: LimeSuite
    /// is recorded as stopping a running stream on its own when the chip is
    /// reconfigured — the SoapySDR path next door carries `reassert_gains` for
    /// the same behaviour. A stream that has quietly stopped delivers zeroes
    /// forever otherwise.
    fn poll_status(&mut self) {
        if self.last_status.elapsed() < STATUS_INTERVAL {
            return;
        }
        self.last_status = Instant::now();
        let mut st = ffi::StreamStatusT::default();
        let rc = unsafe { (self.api.get_stream_status)(&mut self.rx, &mut st) };
        if rc != ffi::OK {
            return;
        }
        self.overruns += u64::from(st.overrun);
        self.underruns += u64::from(st.underrun);
        if st.overrun > 0 {
            tracing::debug!("LimeSDR receive overrun ({} samples dropped)", st.overrun);
        }
        if !st.active {
            self.restarts += 1;
            tracing::warn!("LimeSDR receive stream had stopped; restarting it");
            let rc = unsafe { (self.api.start_stream)(&mut self.rx) };
            if rc == ffi::OK {
                // Whatever stopped it also reset the chip's settings, so put
                // them back rather than assuming they survived.
                let _ = self.ctl().set_gain_db(false, self.rx_gain_db);
                let _ = self.ctl().set_lo(false, self.center);
                self.note = Some(format!(
                    "the receive stream stopped and was restarted {} time(s) — if this keeps \
                     happening, try a lower sample rate",
                    self.restarts
                ));
            } else {
                self.rx_running = false;
                self.note = Some(format!(
                    "the receive stream stopped and could not be restarted: {}",
                    self.api.err_text()
                ));
            }
        }
    }

    /// Whether the session has failed badly enough to want reopening.
    pub fn needs_reopen(&self) -> bool {
        !self.rx_running
    }

    /// Stop both streams and close the device *now*, ahead of `Drop`, leaving
    /// the handle inert but callable: reads deliver nothing, controls answer
    /// [`Error::Closed`], and [`Self::needs_reopen`] says yes. Idempotent.
    ///
    /// This is `IqSource::release`'s half of a reopen. The engine builds this
    /// front end's replacement *before* the old source is dropped, and what a
    /// second `LMS_Open` does against a board still held here depends on the
    /// platform — both answers are wrong. On Linux, libusb refuses the second
    /// interface claim, so every Apply failed as "held by another program"
    /// (us). On Windows, CyAPI opens the device *shared*, so the open
    /// succeeded and the replacement's `LMS_Init` and stream setup landed on
    /// top of the running stream — both sessions came out of that dead, which
    /// is how changing the sample rate froze the waterfall until the program
    /// was restarted.
    ///
    /// Ordering contract: a LimeRFE reached through this board's GPIO (see
    /// [`Self::shared_device`]) must be dropped first — its handle keeps a
    /// pointer into the device this closes, and LimeSuite would dereference it.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.teardown_streams();
        self.ctl().close();
    }

    /// Stop and destroy both streams, in the order that leaves the radio
    /// quiet: stop transmitting, stop receiving, then let go of the streams.
    /// Shared by [`Self::close`] and `Drop`; runs at most once, which the
    /// `closed` flag guards.
    fn teardown_streams(&mut self) {
        let api = Arc::clone(&self.api);
        // The device pointer is read once, before anything borrows a stream:
        // holding the guard across those calls would overlap the two borrows,
        // and the pointer is stable for the life of the device anyway.
        let dev = self.ctl().raw();
        if let Some(tx) = self.tx.as_mut() {
            if self.tx_running {
                unsafe { (api.stop_stream)(tx) };
            }
            unsafe { (api.destroy_stream)(dev, tx) };
        }
        self.tx = None;
        self.tx_running = false;
        if self.rx_running {
            unsafe { (api.stop_stream)(&mut self.rx) };
        }
        unsafe { (api.destroy_stream)(dev, &mut self.rx) };
        self.rx_running = false;
    }

    /// Standing conditions worth telling the operator about.
    pub fn status_note(&self) -> Option<String> {
        self.note.clone()
    }

    /// Start transmitting on `center_hz`. Returns the transmit sample rate.
    pub fn tx_begin(&mut self, center_hz: f64) -> Result<f64> {
        self.ensure_open()?;
        if self.tx.is_none() {
            return Err(Error::api("LMS_StartStream", "the transmitter is not armed".into()));
        }
        // The transmit filter has to serve the right side of the 30 MHz NCO
        // boundary for the frequency this over is on (see
        // `device::effective_lpf_bw` — below it, a rate-derived filter
        // transmits milliwatts). The answer is constant on each side, so the
        // slow retune fires only when a band change actually crossed over;
        // an ordinary key-down compares two numbers and moves on.
        let bw =
            device::effective_lpf_bw(self.lpf_tx_want, center_hz, self.rate, self.lpf_range_tx);
        if (bw - self.tx_lpf_applied).abs() > 1.0 {
            let retuned = self.ctl().set_lpf_bw(true, bw);
            match retuned {
                Ok(()) => {
                    self.tx_lpf_applied = bw;
                    tracing::info!(
                        "transmit filter retuned to {:.1} MHz for the 30 MHz crossing",
                        bw / 1e6
                    );
                }
                // A filter already wider than needed still passes the signal,
                // so an over is not refused because the *narrowing* failed.
                // One too narrow would go out at milliwatts — that over is
                // refused with the reason in hand.
                Err(e) if self.tx_lpf_applied >= bw => {
                    tracing::warn!(
                        "transmit filter stayed at {:.1} MHz: {e}",
                        self.tx_lpf_applied / 1e6
                    );
                }
                Err(e) => return Err(e),
            }
        }
        // Retune before taking hold of the stream: the device lock and the
        // stream borrow must not overlap, and the LO is the device's.
        self.ctl().set_lo(true, center_hz)?;
        self.tx_center = center_hz;
        let Some(tx) = self.tx.as_mut() else { unreachable!("checked above") };
        if !self.tx_running {
            let rc = unsafe { (self.api.start_stream)(tx) };
            if rc != ffi::OK {
                return Err(Error::api("LMS_StartStream", self.api.err_text()));
            }
            self.tx_running = true;
        }
        Ok(self.ctl().sample_rate(true).unwrap_or(self.rate))
    }

    /// Write one block of modulated baseband.
    pub fn tx_write(&mut self, samples: &[Complex32]) -> Result<()> {
        let Some(tx) = self.tx.as_mut() else {
            return Err(Error::api("LMS_SendStream", "the transmitter is not armed".into()));
        };
        if !self.tx_running || samples.is_empty() {
            return Ok(());
        }
        let meta = ffi::StreamMetaT {
            timestamp: 0,
            // Send as it arrives rather than at a scheduled time: the engine
            // paces the over, not the hardware clock.
            wait_for_timestamp: false,
            flush_partial_packet: false,
        };
        let mut sent = 0usize;
        while sent < samples.len() {
            let n = unsafe {
                (self.api.send_stream)(
                    tx,
                    samples[sent..].as_ptr().cast(),
                    samples.len() - sent,
                    &meta,
                    TX_TIMEOUT_MS,
                )
            };
            if n < 0 {
                return Err(Error::api("LMS_SendStream", self.api.err_text()));
            }
            if n == 0 {
                // The FIFO stayed full for the whole timeout. Dropping the rest
                // of the block is better than blocking the engine forever.
                tracing::debug!(
                    "LimeSDR transmit FIFO stalled, dropping {} samples",
                    samples.len() - sent
                );
                break;
            }
            sent += n as usize;
        }
        Ok(())
    }

    /// Push the last partial packet out.
    ///
    /// Without this the tail of a burst sits in LimeSuite's FIFO waiting for a
    /// packet that never comes — which on a mode with a hard timing boundary
    /// means the last symbols never reach the air.
    pub fn tx_drain(&mut self) {
        let Some(tx) = self.tx.as_mut() else { return };
        if !self.tx_running {
            return;
        }
        let meta = ffi::StreamMetaT {
            timestamp: 0,
            wait_for_timestamp: false,
            flush_partial_packet: true,
        };
        let silence = [Complex32::new(0.0, 0.0); 64];
        let _ = unsafe {
            (self.api.send_stream)(tx, silence.as_ptr().cast(), silence.len(), &meta, TX_TIMEOUT_MS)
        };
    }

    /// Stop transmitting.
    pub fn tx_end(&mut self) -> Result<()> {
        self.tx_drain();
        if let Some(tx) = self.tx.as_mut()
            && self.tx_running
        {
            let rc = unsafe { (self.api.stop_stream)(tx) };
            self.tx_running = false;
            if rc != ffi::OK {
                return Err(Error::api("LMS_StopStream", self.api.err_text()));
            }
        }
        Ok(())
    }

    pub fn tx_active(&self) -> bool {
        self.tx_running
    }
}

impl Drop for LimeHandle {
    fn drop(&mut self) {
        if self.closed {
            return; // `close` already ran, on the engine's release path.
        }
        self.teardown_streams();
        // Not `ctl().close()`: on this path a LimeRFE's board link may still
        // hold the shared device, so `DevCtl::drop` closes it only once the
        // last holder lets go.
    }
}
