//! An [`IqSource`] for an Icom reached over its LAN or WiFi port.
//!
//! The radio is both the front end and the rig: control, audio and the
//! spectrum scope all arrive on one connection, so this source owns the whole
//! conversation the way [`crate::audio_cat_source::AudioCatSource`] owns a
//! serial port and a sound card.
//!
//! # Two receive paths
//!
//! No Icom offers I/Q, over any interface. What the LAN gives instead is two
//! different things, and the operator picks between them:
//!
//! * **AF** — the radio demodulates and we get audio, exactly as with a CAT rig
//!   and a sound card. `caps.audio_mode` is set, and the *main* panadapter is
//!   the radio's own scope rather than an FFT of that audio: what the rig sends
//!   here is not a picture of the band, it is a picture of what already came
//!   through its filter, one-sided by construction and never wider than that
//!   filter. (The audio FFT comes back for the digital modes, which place
//!   stations by their offset inside the passband.)
//! * **12 kHz IF** — Icom's DRM output, a real IF in the audio stream. Mixed to
//!   baseband and decimated by two here, it becomes a 24 kHz complex stream the
//!   engine treats like any other front end, so sdroxide's own filters, noise
//!   reduction and decoders apply over roughly ±12 kHz.
//!
//! Either way the *wide* panadapter is fed from the radio's own `27 00` scope
//! sweep through [`IqSource::wide_spectrum_db`] — 475 finished bins covering up
//! to ±500 kHz. That is not I/Q and cannot be demodulated; it is a picture, and
//! it is the only wide view an Icom has. The span it sweeps is commanded from
//! here (`IcomNetConfig::scope_span`) rather than left at whatever the operator
//! last set on the radio's own screen, which is what made the strip come up
//! barely wider than the ±12 kHz IF beneath it.
//!
//! # Status
//!
//! An IC-705 has received through this over WiFi, on the 12 kHz IF. Everything
//! else — transmit, the other models' menu numbering, the meters — is still
//! only exercised against `sdroxide_icomnet::sim`, and the session trace is
//! copyable from the settings tab so a user with a radio can report what
//! actually happened.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sdroxide_cat::civ;
use sdroxide_dsp::Ddc;
use sdroxide_icomnet::{IcomNetDevice, IcomNetOptions};
use sdroxide_radio::rtrb;
use sdroxide_radio::{Complex32, ControlUpdate, IqSource, Result};
use sdroxide_types::{CwKeying, IcomNetConfig, IcomRxSource, Mode, TxTelemetry};

use crate::dial::Dial;

/// The last session's trace, kept after the source is dropped.
///
/// A connection that fails or misbehaves is usually replaced immediately — by
/// the engine's background retry, or by the operator pressing Apply — and the
/// trace of the *interesting* session would go with it. Holding the most recent
/// one here is what lets Settings → Radio still offer it afterwards.
static LAST_TRACE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The most recent Icom LAN session trace, for a bug report.
pub fn last_diagnostics() -> Option<String> {
    LAST_TRACE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The trace if there is one, and an explanation of what to do with it either
/// way — the settings dialog offers the button whether or not a session has run.
pub fn diagnostics_or_hint() -> String {
    match last_diagnostics() {
        Some(t) => format!("{t}\n{}\n", sdroxide_icomnet::FIELD_REPORT_HINT),
        None => format!(
            "No Icom LAN session has run yet — connect to a radio first.\n\n{}\n",
            sdroxide_icomnet::FIELD_REPORT_HINT
        ),
    }
}

fn record_trace(dev: &IcomNetDevice) {
    *LAST_TRACE.lock().unwrap_or_else(|e| e.into_inner()) = Some(dev.trace().dump());
}

/// The IF the radio puts in the audio stream when its LAN output is set to IF.
/// Fixed by Icom — it is the DRM standard's intermediate frequency, which is
/// why the setting exists at all.
const IF_CENTER_HZ: f64 = 12_000.0;

/// Complex rate the 12 kHz IF is delivered at. Exactly half the 48 kHz stream,
/// which puts the usable band at ±12 kHz — the whole of what a real IF at that
/// centre can carry.
const IF_OUT_RATE: f64 = 24_000.0;

/// How long a reading from the rig's own meter stands in for the next one, as
/// for a serial CAT rig: enough to cover an ordinary gap between answers, not
/// enough to leave a needle standing on a link that has gone quiet.
const METER_MAX_AGE: Duration = Duration::from_millis(1500);

/// How often to ask the radio where it is tuned and what its meter reads.
const POLL_PERIOD: Duration = Duration::from_millis(200);

/// How long a mode we have commanded outranks the mode the radio reports.
///
/// The mode is polled every [`POLL_PERIOD`], so a read issued just before the
/// operator changed mode comes back carrying the mode the radio was in a moment
/// ago — and adopting that would drag the app straight back off the mode it had
/// just chosen. Several poll periods, to cover a WiFi link's latency, and no
/// more: a radio that simply will not go where it was told has to be allowed to
/// win, or the two stay out of step for the rest of the session.
const MODE_SETTLE: Duration = Duration::from_millis(1000);

/// A scope that has sent nothing for this long has stopped. Sweeps arrive
/// about ten times a second, so this is a silence, not a slow sweep — and it is
/// long enough to sit through a band change or a menu the operator opened on
/// the radio without reading it as a fault.
const SCOPE_STALL: Duration = Duration::from_secs(3);

/// How soon after a stall to ask the radio again, and the ceiling the interval
/// backs off to while it stays quiet. The enables are idempotent, but a radio
/// with no scope at all — or one whose operator has switched it off — must not
/// be asked twice a second forever.
const SCOPE_RETRY: Duration = Duration::from_secs(2);
const SCOPE_RETRY_MAX: Duration = Duration::from_secs(30);

/// The scope's amplitude scale is 0..=160 with no documented dB per step. The
/// engine's `auto_levels` normalises whatever it is given, so this only has to
/// put the trace in a plausible range and keep it linear; the constant is a
/// calibration knob for whoever first sees one against a known signal.
const SCOPE_DB_PER_UNIT: f32 = 0.5;

pub struct IcomNetSource {
    dev: IcomNetDevice,
    audio: rtrb::Consumer<f32>,
    tx: Option<rtrb::Producer<f32>>,
    rate: f64,
    rx_source: IcomRxSource,
    audio_bw: f64,
    cw_keying: CwKeying,
    civ_addr: u8,
    label: String,
    status: Option<String>,

    dial: Dial,
    /// Rolling buffer for the CI-V parser. Frames arrive whole, but reusing the
    /// tested parser costs nothing and it drops our own echoes for us.
    civ_buf: Vec<u8>,
    pending: Vec<ControlUpdate>,

    // The 12 kHz IF path.
    ddc: Option<Ddc>,
    if_in: Vec<Complex32>,
    if_out: VecDeque<Complex32>,

    // The radio's own scope.
    scope: civ::ScopeAssembler,
    scope_frame: Option<(f64, f64, Vec<u8>)>,
    /// Whether this session uses the radio's scope at all, and the half-span it
    /// was asked for — both kept so the watchdog can ask again.
    scope_wanted: bool,
    scope_half_span: Option<f64>,
    /// When the last complete sweep landed — the watchdog's clock.
    last_sweep: Instant,
    /// When the watchdog last re-asserted the enables, and how long it waits
    /// before doing so again.
    last_scope_nudge: Instant,
    scope_retry: Duration,
    /// Whether the scope is currently considered stopped, so the log says so
    /// once per episode rather than once per attempt.
    scope_stalled: bool,

    last_poll: Instant,
    /// The CI-V mode byte last commanded from here, and when — see
    /// [`MODE_SETTLE`]. Held as the wire byte rather than a [`Mode`] because
    /// the two do not spell the same thing: every digital mode is commanded as
    /// plain USB, and comes back as plain USB.
    mode_cmd: Option<(u8, Instant)>,
    last_signal: Option<(Instant, f32)>,
    last_telem: Option<TxTelemetry>,
    transmitting: bool,
}

impl IcomNetSource {
    /// Connect, then put the radio into the state this session needs.
    pub fn open(cfg: &IcomNetConfig) -> anyhow::Result<IcomNetSource> {
        let rx_source = cfg.effective_rx_source();
        // The trace lives out here so a connect that *fails* still leaves one
        // behind — the wedged-radio and wrong-address sessions are exactly the
        // ones a bug report is filed about, and they never become a device.
        let trace = sdroxide_icomnet::Trace::new();
        let dev = IcomNetDevice::connect_traced(
            IcomNetOptions {
                address: cfg.address.clone(),
                control_port: cfg.control_port,
                username: cfg.username.clone(),
                password: cfg.password.clone(),
                client_name: "sdroxide".into(),
                rx_sample_rate: cfg.sample_rate_hz,
                tx_sample_rate: cfg.sample_rate_hz,
                tx_buffer_ms: cfg.tx_latency_ms,
                civ_address_override: cfg.civ_address_override,
                timeout: Duration::from_secs(10),
            },
            trace.clone(),
        );
        let dev = match dev {
            Ok(dev) => dev,
            Err(e) => {
                *LAST_TRACE.lock().unwrap_or_else(|x| x.into_inner()) = Some(trace.dump());
                return Err(e.into());
            }
        };

        let info = dev.info().clone();
        let audio = dev.take_audio_rx().ok_or_else(|| anyhow::anyhow!("no receive stream"))?;
        let tx = dev.take_audio_tx();
        let civ_addr = info.civ_address;

        let mut notes = Vec::new();
        if cfg.rx_source == IcomRxSource::If12k && rx_source == IcomRxSource::Af {
            notes.push(format!(
                "The 12 kHz IF needs a 48 kHz audio stream; this connection asked for {} Hz, \
                 so the radio's demodulated audio is being used instead.",
                cfg.sample_rate_hz
            ));
        }
        if !info.can_transmit {
            notes.push(format!("{} offered no transmit stream — receive only.", info.radio_name));
        }

        let mut src = IcomNetSource {
            dev,
            audio,
            tx,
            rate: match rx_source {
                IcomRxSource::Af => f64::from(cfg.sample_rate_hz),
                IcomRxSource::If12k => IF_OUT_RATE,
            },
            rx_source,
            audio_bw: cfg.audio_bw_hz,
            cw_keying: cfg.cw_keying,
            civ_addr,
            label: format!(
                "{} over LAN at {} ({})",
                info.radio_name,
                cfg.address,
                match rx_source {
                    IcomRxSource::Af => "AF",
                    IcomRxSource::If12k => "12 kHz IF",
                }
            ),
            status: None,
            dial: Dial::default(),
            civ_buf: Vec::with_capacity(1024),
            pending: Vec::new(),
            ddc: (rx_source == IcomRxSource::If12k).then(|| {
                let mut d = Ddc::new(f64::from(cfg.sample_rate_hz), IF_OUT_RATE);
                d.set_offset_hz(IF_CENTER_HZ);
                d
            }),
            if_in: Vec::new(),
            if_out: VecDeque::new(),
            scope: civ::ScopeAssembler::default(),
            scope_frame: None,
            scope_wanted: false,
            scope_half_span: None,
            last_sweep: Instant::now(),
            last_scope_nudge: Instant::now(),
            scope_retry: SCOPE_RETRY,
            scope_stalled: false,
            last_poll: Instant::now(),
            mode_cmd: None,
            last_signal: None,
            last_telem: None,
            transmitting: false,
        };
        notes.extend(src.configure(cfg));
        // Adopt the radio's current dial before returning, the way the CAT
        // backend does with `query_once`. Without this the source hands the
        // engine a centre of 0 Hz — the `Dial::default()` above, before the
        // first frequency reply has folded in — and the engine clamps that up
        // to the receiver's lowest frequency and *commands the radio there*.
        // That is the "it jumps to ~30 kHz on reconnect / power-toggle" bug:
        // `configure` has already asked for the dial, so this only waits for
        // the answer.
        src.adopt_initial_dial();
        src.status = (!notes.is_empty()).then(|| notes.join("\n"));
        Ok(src)
    }

    /// Put the radio into the state the session needs, and say what could not
    /// be done. Returns operator-facing notes, not errors: none of this is
    /// fatal, and a session that receives is worth having even when a menu item
    /// could not be written.
    fn configure(&mut self, cfg: &IcomNetConfig) -> Vec<String> {
        let mut notes = Vec::new();
        let model = self.dev.info().model;

        // The rig may still be holding an offset from a previous session; we
        // carry RIT, XIT and split on the dial ourselves.
        for f in civ::clear_offsets_frames(self.civ_addr) {
            self.send(f);
        }
        self.send(civ::read_freq_frame(self.civ_addr));
        self.send(civ::read_mode_frame(self.civ_addr));
        // What the radio's power is set to, so the Drive slider starts where the
        // radio already is instead of imposing a remembered level on it.
        self.send(civ::read_power_frame(self.civ_addr));

        match model.lan_afif_select {
            Some(item) => {
                self.send(civ::set_menu_frame(self.civ_addr, item, &[cfg.rx_source.menu_value()]));
            }
            None if cfg.rx_source == IcomRxSource::If12k => notes.push(
                "sdroxide does not know this model's menu numbering, so it cannot switch the \
                 LAN output to IF — set SET > Connectors > LAN AF/IF Output > Output Select \
                 to IF on the radio."
                    .into(),
            ),
            None => {}
        }

        // Only where there is something to modulate. A receive-only radio has no
        // modulation input to set and no transmit audio that could go unheard,
        // so both the write and its warning are noise — and an IC-R8600 got the
        // warning, because the menu numbering of a set that cannot transmit is
        // naturally not in the table.
        if cfg.set_mod_input_on_open && self.can_transmit() {
            match model.lan_mod_input {
                Some((data_off, data_on, lan)) => {
                    self.send(civ::set_menu_frame(self.civ_addr, data_off, &[lan]));
                    self.send(civ::set_menu_frame(self.civ_addr, data_on, &[lan]));
                }
                None => notes.push(
                    "sdroxide does not know this model's menu numbering, so it cannot switch \
                     the modulation input to LAN — set SET > Connectors > MOD Input on the \
                     radio, or transmit audio will not be heard."
                        .into(),
                ),
            }
        }

        if cfg.scope {
            self.scope_wanted = true;
            self.scope_half_span = cfg.scope_span.half_span_hz();
            self.enable_scope();
        }
        notes
    }

    /// Wait, briefly, for the radio to say where its dial is, and fold it in.
    ///
    /// The engine reads [`IqSource::center_hz`] the instant it adopts a source
    /// and builds the whole display window — and, on this backend where the
    /// dial *is* the VFO, the tune it commands back at the rig — from it. A
    /// centre of 0 Hz (the opening [`Dial::default`]) is clamped up to the
    /// receiver's lowest frequency and sent to the radio, which is why a
    /// reconnect used to yank an IC-R8600 down to ~30 kHz. `configure` has
    /// already asked for the frequency; this just gives the answer a moment to
    /// arrive, re-asking through [`Self::pump`] if the first read was lost.
    ///
    /// Bounded and best-effort: a radio that never answers times out and the
    /// old behaviour stands, which is no worse than before. A real dial is
    /// never 0 Hz, so that is a safe "not yet known" sentinel.
    fn adopt_initial_dial(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(1500);
        while self.dial.vfo == 0.0 && Instant::now() < deadline {
            self.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        // Whatever arrived came in as a pending `Freq` update; the engine has
        // not been built yet, so drop it — the dial itself now carries the
        // frequency, and `center_hz` reports it. Leaving it would make the
        // first `poll_control` announce a "retune" to where the radio already
        // is.
        self.pending.retain(|u| !matches!(u, ControlUpdate::Freq(_)));
        if self.dial.vfo == 0.0 {
            tracing::warn!(
                "Icom LAN: the radio did not report its frequency at connect; the display                  will adopt it on the first poll instead"
            );
        }
    }

    /// Whether the radio offered a transmit stream. An IC-R8600 does not, and
    /// neither does a transceiver somebody else is already transmitting from.
    pub fn can_transmit(&self) -> bool {
        self.dev.info().can_transmit && self.tx.is_some()
    }

    /// The label the settings dialog and the engine show for this session.
    pub fn describe(&self) -> String {
        self.label.clone()
    }

    fn send(&self, frame: Vec<u8>) {
        self.dev.send_civ(frame);
    }

    /// Ask the radio to run its scope and stream it here.
    ///
    /// Every frame is idempotent, which is what lets [`Self::watch_scope`] send
    /// the lot again without having to know what went missing.
    fn enable_scope(&mut self) {
        if !self.scope_wanted {
            return;
        }
        self.send(civ::scope_on_frame(self.civ_addr, true));
        self.send(civ::scope_output_frame(self.civ_addr, true));
        // The radio keeps whatever span was last chosen on its own screen, and
        // that is routinely ±5 kHz — a full-band strip no wider than the
        // panadapter above it. Centre mode as well as the span: a scope left in
        // one of the fixed modes ignores `27 15` and sits on a slice of band the
        // dial is not even in.
        if let Some(half) = self.scope_half_span {
            self.send(civ::scope_mode_frame(self.civ_addr, civ::ScopeMode::Center));
            self.send(civ::scope_span_frame(self.civ_addr, half));
        }
    }

    /// Start the scope again when it stops on its own.
    ///
    /// Nothing on this link reports that the sweeps have stopped, and several
    /// ordinary things stop them: `27 11` is fire-and-forget over UDP, so an
    /// enable lost on the way is never missed; the radio drops its output when
    /// its own scope screen closes; and a session that comes up slowly can have
    /// the whole opening burst land before the radio will accept any of it. The
    /// waterfall then sat dead until the operator reconnected — and a reconnect
    /// can lose the enable exactly the same way, which is why it sometimes took
    /// several. Asking again costs two frames, so the watchdog does what the
    /// operator was doing by hand.
    fn watch_scope(&mut self) {
        if !self.scope_wanted {
            return;
        }
        // A radio does not sweep while it transmits, and an over is not a fault:
        // hold the clock rather than nudging through every transmission.
        if self.transmitting {
            self.last_sweep = Instant::now();
            return;
        }
        if self.last_sweep.elapsed() < SCOPE_STALL
            || self.last_scope_nudge.elapsed() < self.scope_retry
        {
            return;
        }
        self.last_scope_nudge = Instant::now();
        if !self.scope_stalled {
            self.scope_stalled = true;
            tracing::warn!(
                "Icom LAN: the radio's scope has stopped sending; asking it to stream again"
            );
            // Also into the wire trace, where it sits beside the frames that
            // stopped — which is where anyone diagnosing a recurrence looks.
            self.dev.trace().note("scope stalled; re-sending the enables");
        }
        // A sweep left half-assembled when the stream stopped would otherwise be
        // joined to the first one that comes back.
        self.scope = civ::ScopeAssembler::default();
        self.enable_scope();
        // Back off while it stays quiet: a radio that has no scope, or whose
        // operator has switched it off for good, is not worth two frames every
        // two seconds for the rest of the session.
        self.scope_retry = (self.scope_retry * 2).min(SCOPE_RETRY_MAX);
    }

    /// Drain whatever the radio has said, and ask it the periodic questions.
    fn pump(&mut self) {
        while let Ok(frame) = self.dev.civ_frames().try_recv() {
            self.civ_buf.extend_from_slice(&frame);
        }
        for reply in civ::parse_frames(&mut self.civ_buf) {
            // Our own frames come back on a shared bus; the parser already
            // drops the ones we sent.
            self.on_reply(reply);
        }

        if self.last_poll.elapsed() >= POLL_PERIOD {
            self.last_poll = Instant::now();
            self.send(civ::read_freq_frame(self.civ_addr));
            self.send(civ::read_mode_frame(self.civ_addr));
            if self.transmitting {
                self.send(civ::read_swr_frame(self.civ_addr));
            } else {
                self.send(civ::read_smeter_frame(self.civ_addr));
            }
        }
        self.watch_scope();
    }

    fn on_reply(&mut self, reply: civ::CivReply) {
        match reply.cmd {
            // Frequency, both the polled read and an unsolicited transceive
            // report when the operator turns the knob.
            0x00 | 0x03 => {
                if let Some(hz) = civ::decode_freq(&reply.data) {
                    // The dial carries RIT, and for the length of an over it
                    // carries XIT or split instead, so what the radio reports
                    // has to be folded back before it means "the VFO moved".
                    if let Some(vfo) = self.dial.report(hz) {
                        self.pending.push(ControlUpdate::Freq(vfo));
                    }
                }
            }
            0x01 | 0x04 => {
                let Some(&b) = reply.data.first() else { return };
                // A report that disagrees with a mode we have just commanded is
                // the stale one — the read went out before the set did.
                if let Some((want, at)) = self.mode_cmd {
                    if b == want {
                        self.mode_cmd = None;
                    } else if at.elapsed() < MODE_SETTLE {
                        return;
                    } else {
                        // Long enough: the radio is somewhere else and means
                        // it. Its mode is the real one, so stop arguing.
                        self.mode_cmd = None;
                    }
                }
                if let Some(m) = civ::civ_to_mode(b) {
                    self.pending.push(ControlUpdate::Mode(m));
                }
            }
            // The transmit power the radio is set to, asked for once when the
            // session opens. Adopted into the Drive slider rather than
            // commanded back: the radio's own setting is the operator's.
            0x14 => {
                if let Some(frac) = civ::parse_power_reply(&reply.data) {
                    self.pending.push(ControlUpdate::TxDrive(frac));
                }
            }
            0x15 => {
                if let Some(dbm) = civ::parse_smeter_reply(&reply.data) {
                    self.last_signal = Some((Instant::now(), dbm));
                }
                if let Some(swr) = civ::parse_swr_reply(&reply.data) {
                    self.last_telem = Some(TxTelemetry { swr: Some(swr), ..Default::default() });
                }
            }
            0x27 => {
                if let Some((info, bins)) =
                    civ::parse_scope_frame(&reply.data).and_then(|s| self.scope.push(s))
                {
                    self.scope_frame = Some((info.center_hz, info.span_hz, bins));
                    // A whole sweep is the only thing that proves the lane is
                    // alive — frames still arriving while none of them ever
                    // completes leaves the waterfall just as frozen.
                    self.last_sweep = Instant::now();
                    self.scope_retry = SCOPE_RETRY;
                    if self.scope_stalled {
                        self.scope_stalled = false;
                        tracing::info!("Icom LAN: the radio's scope is sweeping again");
                        self.dev.trace().note("scope sweeping again");
                    }
                }
            }
            _ => {}
        }
    }

    /// Move audio out of the network ring, converting on the way when the
    /// stream is carrying a 12 kHz IF rather than demodulated audio.
    fn fill(&mut self, want: usize) {
        match self.ddc.as_mut() {
            None => {}
            Some(ddc) => {
                // Two input samples per output sample, plus a little slack so a
                // partly-filled filter still produces something.
                let need = want.saturating_sub(self.if_out.len()) * 2 + 64;
                self.if_in.clear();
                for _ in 0..need {
                    match self.audio.pop() {
                        // A real IF: the imaginary half is zero, and the mixer
                        // plus the decimating low-pass is what removes the
                        // image the real signal carries.
                        Ok(s) => self.if_in.push(Complex32::new(s, 0.0)),
                        Err(_) => break,
                    }
                }
                if !self.if_in.is_empty() {
                    let mut out = Vec::with_capacity(self.if_in.len() / 2 + 8);
                    let input = std::mem::take(&mut self.if_in);
                    ddc.process(&input, &mut out);
                    self.if_in = input;
                    self.if_out.extend(out);
                }
            }
        }
    }

    /// Take whatever the radio has already sent, and no more.
    fn drain(&mut self, buf: &mut [Complex32]) -> usize {
        self.pump();
        match self.rx_source {
            IcomRxSource::Af => {
                let mut n = 0;
                while n < buf.len() {
                    let Ok(s) = self.audio.pop() else { break };
                    buf[n] = Complex32::new(s, 0.0);
                    n += 1;
                }
                n
            }
            IcomRxSource::If12k => {
                self.fill(buf.len());
                let mut n = 0;
                while n < buf.len() {
                    let Some(s) = self.if_out.pop_front() else { break };
                    buf[n] = s;
                    n += 1;
                }
                n
            }
        }
    }
}

impl IqSource for IcomNetSource {
    fn sample_rate(&self) -> f64 {
        self.rate
    }

    fn center_hz(&self) -> f64 {
        self.dial.vfo
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        if let Some(f) = self.dial.set_vfo(hz) {
            self.send(civ::set_freq_frame(self.civ_addr, f));
        }
        Ok(())
    }

    fn set_rit_hz(&mut self, hz: f64) {
        if let Some(f) = self.dial.set_rit(hz) {
            self.send(civ::set_freq_frame(self.civ_addr, f));
        }
    }

    /// Both receive paths hang off the radio's own dial: the 12 kHz IF is
    /// mixed down from it, and AF is what it demodulated there. Either way
    /// there is no second oscillator to leave parked while the dial moves.
    fn center_is_dial(&self) -> bool {
        true
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.drain(buf);
        if n == 0 {
            // Nothing yet: the radio paces this stream, so yielding here is
            // what keeps the engine thread off a spin.
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(n)
    }

    /// The same drain without the nap — see `AudioCatSource::read_available`
    /// for why a source that may not be the one pacing the engine needs it.
    fn read_available(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        Ok(self.drain(buf))
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    fn open_status(&self) -> Option<String> {
        self.status.clone()
    }

    fn display_bandwidth(&self) -> Option<f64> {
        (self.rx_source == IcomRxSource::Af).then_some(self.audio_bw)
    }

    /// The radio's own scope, as the full-band panadapter.
    ///
    /// These are finished magnitude bins on the radio's 0..=160 scale, not
    /// anything derived from I/Q — there is no I/Q on this interface. Mapping
    /// them onto a dB axis is a linear guess with an uncalibrated slope; the
    /// engine's auto-levelling makes the picture right even where the absolute
    /// numbers are not.
    fn wide_spectrum_db(&mut self, out: &mut Vec<f32>) -> Option<(f64, f64)> {
        let (center, span, bins) = self.scope_frame.take()?;
        out.clear();
        out.extend(bins.iter().map(|&b| (f32::from(b) - 160.0) * SCOPE_DB_PER_UNIT));
        Some((center, span))
    }

    fn poll_control(&mut self) -> Vec<ControlUpdate> {
        self.pump();
        std::mem::take(&mut self.pending)
    }

    /// The radio in front of us is the rig, so the mode control has to reach it
    /// on both receive paths — including the 12 kHz IF, where sdroxide does the
    /// demodulating but the radio's mode still picks the IF filter the stream
    /// arrives through, and its own display would otherwise disagree with ours.
    ///
    /// Not [`IqSource::tracks_rx_mode`], which would also assert the session's
    /// mode the moment the connection opens: this backend adopts the dial and
    /// the mode the transceiver is already on, the way the CAT backend does.
    fn commands_rx_mode(&self) -> bool {
        true
    }

    fn set_control_mode(&mut self, mode: Mode) -> Result<()> {
        // CW keyed as audio (MCW) rides a sideband: a rig put in CW keys its
        // own transmitter and ignores the modulator input, so the keyed
        // sidetone only reaches the air from USB — the same plain USB this
        // backend already commands for every digital mode (`mode_to_civ`
        // folds them all to 0x01). Mapped before the echo-suppression store
        // so the rig's USB report matches what was commanded.
        let mode = match mode {
            Mode::Cw if matches!(self.cw_keying, CwKeying::Audio) => Mode::Usb,
            m => m,
        };
        self.mode_cmd = Some((civ::mode_to_civ(mode), Instant::now()));
        // The mode *and* the DATA switch beside it, which is the pair the
        // serial CAT path has always sent. The plain mode command clears the
        // switch, so sending it alone took a rig the operator had put in FM-D
        // straight back to FM — and it is sent before every over, so a beacon
        // could never go out through the data path at all (issue #150).
        for f in civ::set_mode_frames(self.civ_addr, mode, self.dev.info().model.data_mode_sub) {
            self.send(f);
        }
        Ok(())
    }

    // ── CW from the rig's own keyer ─────────────────────────────────────────
    // A rig *in* CW does not modulate what we send it — it keys its own
    // transmitter — so the keyer hands it text rather than a sidetone.
    fn cw_text_keying(&self) -> Option<usize> {
        matches!(self.cw_keying, CwKeying::Cat).then_some(civ::CW_MAX)
    }
    // …and with the keyer on the sound path instead, the engine must expect
    // the mode poll to answer USB while the panel shows CW.
    fn cw_audio_keyed(&self) -> bool {
        matches!(self.cw_keying, CwKeying::Audio)
    }
    fn send_cw(&mut self, text: &str) {
        if let Some(f) = civ::send_cw_frame(self.civ_addr, text) {
            self.send(f);
        }
    }
    fn abort_cw(&mut self) {
        self.send(civ::stop_cw_frame(self.civ_addr));
    }
    fn set_cw_wpm(&mut self, wpm: f32) {
        self.send(civ::keyer_speed_frame(self.civ_addr, wpm));
    }

    // ── Output power ────────────────────────────────────────────────────────
    // The radio's own power control, which is the only transmit level that
    // applies in every mode: the audio on this link is just the modulating
    // signal, and in CW there is none at all — the radio keys itself from its
    // own keyer. The engine asserts the level that applies (the drive, or the
    // tune level while tuning) before each over.
    fn set_tx_drive(&mut self, frac: f64) {
        self.send(civ::set_power_frame(self.civ_addr, frac as f32));
    }
    /// One power register on the radio; the engine commands the tune level
    /// through [`Self::set_tx_drive`] for as long as TUNE holds it.
    fn set_tune_drive(&mut self, _frac: f64) {}
    fn commands_tx_power(&self) -> bool {
        true
    }

    fn tx_begin(&mut self, center_hz: f64, _rate: f64) -> Result<f64> {
        // Split and XIT have no DDC to ride on: the radio's dial is its whole
        // frequency control, so an over that transmits away from where we
        // listen borrows the dial until unkey.
        if let Some(f) = self.dial.begin_tx(center_hz) {
            self.send(civ::set_freq_frame(self.civ_addr, f));
        }
        self.send(civ::ptt_frame(self.civ_addr, true));
        self.transmitting = true;
        // Transmit audio is always plain AF at the negotiated rate, whatever
        // the receive stream happens to be carrying.
        Ok(f64::from(self.dev.info().tx_sample_rate))
    }

    fn tx_write_audio(&mut self, audio: &[f32]) -> Result<()> {
        let Some(tx) = self.tx.as_mut() else {
            return Ok(()); // receive-only radio; PTT still keyed it
        };
        // Block on a full ring so the engine's transmit loop is paced by the
        // radio rather than by the CPU: without this a long burst is generated
        // far faster than 48 kHz and most of it is dropped.
        for &s in audio {
            let mut v = s;
            let mut tries = 0u32;
            while let Err(rtrb::PushError::Full(x)) = tx.push(v) {
                v = x;
                tries += 1;
                if tries > 200 {
                    break; // the link stalled — drop rather than hang transmit
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    fn tx_drain(&mut self) {
        // Wait for the ring to empty before unkey, so the tail of a burst —
        // which is exactly what an FT8 decoder needs — is not cut off.
        let Some(tx) = self.tx.as_ref() else { return };
        let cap = tx.buffer().capacity();
        for _ in 0..1000 {
            if cap.saturating_sub(tx.slots()) <= cap / 40 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn tx_end(&mut self) -> Result<()> {
        self.send(civ::ptt_frame(self.civ_addr, false));
        self.transmitting = false;
        if let Some(f) = self.dial.end_tx() {
            self.send(civ::set_freq_frame(self.civ_addr, f));
        }
        self.last_telem = None;
        Ok(())
    }

    fn discard_pending_rx(&mut self) {
        while self.audio.pop().is_ok() {}
        self.if_out.clear();
    }

    fn tx_telemetry(&mut self) -> Option<TxTelemetry> {
        self.pump();
        self.last_telem
    }

    fn rx_signal_dbm(&mut self) -> Option<f32> {
        self.pump();
        self.last_signal.filter(|(at, _)| at.elapsed() < METER_MAX_AGE).map(|(_, dbm)| dbm)
    }

    fn needs_reopen(&self) -> bool {
        if !self.dev.is_alive() {
            // The session that just died is the one worth reporting on.
            record_trace(&self.dev);
            return true;
        }
        false
    }

    fn release(&mut self) {
        record_trace(&self.dev);
        self.dev.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdroxide_icomnet::sim::{Sim, SimOptions};

    fn cfg(sim: &Sim) -> IcomNetConfig {
        IcomNetConfig {
            address: "127.0.0.1".into(),
            control_port: sim.port(),
            username: "operator".into(),
            password: "hunter2".into(),
            ..Default::default()
        }
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn the_dial_is_adopted_at_open_so_the_engine_never_retunes_the_radio() {
        // The engine reads `center_hz()` the instant it adopts the source and
        // commands the rig there. If that read is 0 (the dial before the first
        // frequency reply), the rig is dragged to the bottom of its range — the
        // "jumps to ~30 kHz on reconnect" bug. `open` must not return until the
        // radio's real dial is in hand.
        let sim =
            Sim::start(SimOptions { freq_hz: 7_074_000.0, scope: false, ..Default::default() })
                .unwrap();
        let src = IcomNetSource::open(&cfg(&sim)).expect("open");
        assert_eq!(src.center_hz(), 7_074_000.0, "the radio's own dial, not 0");
    }

    #[test]
    fn af_mode_delivers_packed_real_audio_at_the_negotiated_rate() {
        let sim =
            Sim::start(SimOptions { tone_hz: Some(1_000.0), scope: false, ..Default::default() })
                .unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        assert_eq!(src.sample_rate(), 48_000.0);
        assert_eq!(src.display_bandwidth(), Some(4000.0));

        let mut buf = vec![Complex32::default(); 4096];
        let mut got = Vec::new();
        wait_for("audio", || {
            let n = src.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            got.len() > 4_800
        });
        // Demodulated audio is real: the engine's audio-mode path reads the
        // real part and would see a phantom sideband if anything landed in the
        // imaginary one.
        assert!(got.iter().all(|c| c.im == 0.0));
        assert!(got.iter().any(|c| c.re.abs() > 0.4));
    }

    #[test]
    fn the_12_khz_if_lands_a_tone_at_the_offset_it_was_sent_at() {
        // The radio emits a real IF with a tone 3 kHz above the 12 kHz centre.
        // After mixing down, that tone must sit at +3 kHz — this is the one
        // piece of the IF path that can be checked without hardware.
        let sim = Sim::start(SimOptions {
            tone_hz: Some(IF_CENTER_HZ + 3_000.0),
            scope: false,
            ..Default::default()
        })
        .unwrap();
        let mut c = cfg(&sim);
        c.rx_source = IcomRxSource::If12k;
        let mut src = IcomNetSource::open(&c).expect("open");
        assert_eq!(src.sample_rate(), IF_OUT_RATE);
        assert_eq!(src.display_bandwidth(), None, "the IF path is not audio mode");

        let mut buf = vec![Complex32::default(); 4096];
        let mut got = Vec::new();
        wait_for("IF samples", || {
            let n = src.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            got.len() > 12_000
        });
        // Drop the filter's start-up transient.
        let got = &got[2_000..];
        let at = |hz: f64| goertzel(got, hz, IF_OUT_RATE);
        let wanted = at(3_000.0);
        assert!(
            wanted > 20.0 * at(-3_000.0),
            "the image must be gone: {wanted} vs {}",
            at(-3_000.0)
        );
        assert!(wanted > 20.0 * at(6_000.0), "energy is at +3 kHz, not elsewhere");
        assert!(wanted > 20.0 * at(0.0), "and not left at DC");
    }

    /// Complex energy at one frequency, positive or negative — the sign is the
    /// whole point of the test, so a real-valued Goertzel would not do.
    fn goertzel(x: &[Complex32], hz: f64, rate: f64) -> f64 {
        let w = std::f64::consts::TAU * hz / rate;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, s) in x.iter().enumerate() {
            let (sn, cs) = (-w * n as f64).sin_cos();
            re += f64::from(s.re) * cs - f64::from(s.im) * sn;
            im += f64::from(s.re) * sn + f64::from(s.im) * cs;
        }
        (re * re + im * im).sqrt() / x.len() as f64
    }

    #[test]
    fn the_radios_scope_reaches_the_full_band_lane() {
        let sim = Sim::start(SimOptions { freq_hz: 14_100_000.0, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");

        let mut bins = Vec::new();
        let mut span = None;
        let mut buf = vec![Complex32::default(); 1024];
        wait_for("a scope sweep", || {
            let _ = src.read(&mut buf);
            span = src.wide_spectrum_db(&mut bins);
            span.is_some()
        });
        let (center, width) = span.unwrap();
        assert_eq!(center, 14_100_000.0);
        // The simulator reports a ±50 kHz half-span; the width is the full one.
        assert_eq!(width, 100_000.0);
        assert_eq!(bins.len(), 475);
        // Mapped to a dB axis with the peak near the top of the scale.
        assert!(bins.iter().all(|&d| d <= 0.0));
        assert!(bins.iter().any(|&d| d > -10.0), "the planted peak survived the mapping");
    }

    /// What the operator was doing by hand: the sweeps stop — an enable lost on
    /// the way, the radio's scope screen closed — and the waterfall is dead
    /// until the session is rebuilt. The watchdog asks again instead, so the
    /// strip comes back on its own.
    #[test]
    fn a_scope_that_stops_sending_is_started_again() {
        let sim = Sim::start(SimOptions { freq_hz: 14_100_000.0, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");

        let mut bins = Vec::new();
        let mut buf = vec![Complex32::default(); 1024];
        let mut sweeps = |src: &mut IcomNetSource, bins: &mut Vec<f32>| {
            let _ = src.read(&mut buf);
            src.wide_spectrum_db(bins).is_some()
        };
        wait_for("the first scope sweep", || sweeps(&mut src, &mut bins));

        // The radio stops streaming and says nothing about it.
        sim.stall_scope();
        assert!(!sim.scope_streaming());

        // ...and the lane comes back without anybody reconnecting. The stall has
        // to be noticed first, so this waits longer than `SCOPE_STALL`.
        let deadline = Instant::now() + SCOPE_STALL + Duration::from_secs(4);
        let mut back = false;
        while Instant::now() < deadline && !back {
            back = sweeps(&mut src, &mut bins);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(back, "the scope never restarted");
        assert!(sim.scope_streaming(), "the radio was never asked to stream again");
        assert_eq!(bins.len(), 475);
    }

    /// The watchdog must not fire while the radio is transmitting: a scope that
    /// stops for the length of an over is a radio behaving normally, and asking
    /// it to stream mid-over would be two frames per retry for nothing.
    #[test]
    fn an_over_is_not_a_stalled_scope() {
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        src.scope_wanted = true;
        src.transmitting = true;
        src.last_sweep = Instant::now() - SCOPE_STALL * 4;
        src.last_scope_nudge = Instant::now() - SCOPE_RETRY_MAX;

        src.watch_scope();
        assert!(!src.scope_stalled, "an over was read as a fault");
        // And the clock was held, so the first quiet moment after the over is
        // not instantly a stall either.
        assert!(src.last_sweep.elapsed() < SCOPE_STALL);
    }

    #[test]
    fn switching_mode_sends_the_radio_a_set_mode() {
        // Field report, 2026-08-19: an IC-705 on the 12 kHz IF followed mode
        // changes made at the radio but not ones made in sdroxide. The stream
        // is not `audio_mode`, so the engine's "command the rig's mode" gate
        // never fired — see `IqSource::commands_rx_mode`.
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut c = cfg(&sim);
        c.rx_source = IcomRxSource::If12k;
        let mut src = IcomNetSource::open(&c).expect("open");
        assert!(src.commands_rx_mode(), "the radio in front of us owns its mode");

        src.set_control_mode(Mode::Lsb).unwrap();
        wait_for("a set-mode frame selecting LSB", || {
            sim.civ_frames().iter().any(|f| f.get(4) == Some(&0x06) && f.get(5) == Some(&0x00))
        });
    }

    /// Issue #150: an IC-705 on APRS transmitted, sounded like packet, and was
    /// decoded by nobody — and a rig the operator had put into FM-D by hand
    /// dropped back to plain FM the moment a beacon went out.
    ///
    /// The plain set-mode command *clears* the DATA switch, and this backend
    /// sent only that while the serial CAT path had always sent the pair. So
    /// every over went out through the microphone path, with the rig's speech
    /// processing and — on FM — its pre-emphasis, which tilts a Bell 202 tone
    /// pair about 6 dB. Structurally perfect, audibly normal, unreadable.
    #[test]
    fn a_digital_mode_is_commanded_with_the_data_switch_beside_it() {
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");

        // APRS: FM (0x05) *and* the DATA switch on.
        src.set_control_mode(Mode::Aprs).unwrap();
        wait_for("FM for APRS", || {
            sim.civ_frames().iter().any(|f| f.get(4) == Some(&0x06) && f.get(5) == Some(&0x05))
        });
        wait_for("the DATA switch turned on for APRS", || {
            sim.civ_frames()
                .iter()
                .any(|f| f.get(4) == Some(&0x1A) && f.get(5) == Some(&0x06) && f.get(6) == Some(&1))
        });

        // ...and a voice mode turns it off again, or a rig left in a data mode
        // by the last over would stay there.
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        src.set_control_mode(Mode::Nfm).unwrap();
        wait_for("the DATA switch turned off for plain FM", || {
            sim.civ_frames()
                .iter()
                .any(|f| f.get(4) == Some(&0x1A) && f.get(5) == Some(&0x06) && f.get(6) == Some(&0))
        });
    }

    /// Issue #119: a rig put in CW keys its own transmitter and ignores its
    /// modulator input, so with the keyer on the sound path (MCW) selecting CW
    /// must keep the radio in plain USB — the same mode every digital mode
    /// rides here. With the rig's own keyer, CW is still commanded as CW.
    #[test]
    fn cw_keyed_as_audio_keeps_the_radio_in_usb() {
        for (keying, sent, never) in
            [(CwKeying::Audio, 0x01u8, 0x03u8), (CwKeying::Cat, 0x03u8, 0x01u8)]
        {
            let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
            let mut c = cfg(&sim);
            c.cw_keying = keying;
            let mut src = IcomNetSource::open(&c).expect("open");
            src.set_control_mode(Mode::Cw).unwrap();
            wait_for("the set-mode frame", || {
                sim.civ_frames().iter().any(|f| f.get(4) == Some(&0x06) && f.get(5) == Some(&sent))
            });
            assert!(
                !sim.civ_frames()
                    .iter()
                    .any(|f| f.get(4) == Some(&0x06) && f.get(5) == Some(&never)),
                "{keying:?}: the wrong mode went to the radio"
            );
        }
    }

    #[test]
    fn a_poll_already_in_flight_does_not_drag_the_mode_back() {
        // The mode is polled every 200 ms, so a read issued just before the
        // operator switched comes back carrying the old mode. The simulator
        // never actually changes mode — it answers every read with USB — which
        // is the worst case: without the guard the app would be pulled straight
        // back off the LSB it had just been put in.
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        // Let the opening read of the mode go by first.
        wait_for("the radio's opening mode report", || {
            src.poll_control().iter().any(|u| matches!(u, ControlUpdate::Mode(_)))
        });

        src.set_control_mode(Mode::Lsb).unwrap();
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            for u in src.poll_control() {
                assert!(
                    !matches!(u, ControlUpdate::Mode(_)),
                    "a stale USB report was let through while LSB was settling"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // But only for a bounded while: a radio that will not go where it was
        // told still gets the last word, or the two stay out of step for good.
        wait_for("the radio winning after the settle window", || {
            src.poll_control().iter().any(|u| matches!(u, ControlUpdate::Mode(Mode::Usb)))
        });
    }

    #[test]
    fn tuning_sends_the_radio_a_set_frequency() {
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        src.set_center_hz(21_074_000.0).unwrap();
        wait_for("a set-frequency frame", || {
            sim.civ_frames().iter().any(|f| {
                f.get(4) == Some(&0x05)
                    && civ::decode_freq(&f[5..f.len() - 1]) == Some(21_074_000.0)
            })
        });
    }

    #[test]
    fn a_known_model_gets_its_modulation_input_switched_to_lan() {
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let _src = IcomNetSource::open(&cfg(&sim)).expect("open");
        // 1A 05 00 84 05 and 1A 05 00 85 05 — DATA OFF MOD and DATA MOD to LAN.
        wait_for("the modulation-input writes", || {
            let frames = sim.civ_frames();
            let seen = |item: u8| {
                frames.iter().any(|f| {
                    f.len() >= 10 && f[4] == 0x1A && f[5] == 0x05 && f[7] == item && f[8] == 0x05
                })
            };
            seen(0x84) && seen(0x85)
        });
    }

    #[test]
    fn an_ic705_gets_its_own_menu_numbers_and_its_own_lan_value() {
        // Field report, 2026-08-19: an IC-705 on WiFi came up with both "menu
        // numbering unknown" notes. Its Connectors block is a hundred items
        // further on than the MK2's, and its modulation source is `03` (WLAN),
        // not `05` (LAN) — writing the MK2's numbers here would have hit
        // something else entirely.
        let sim = Sim::start(SimOptions {
            civ_address: 0xA4,
            radio_name: "IC-705".into(),
            scope: false,
            ..Default::default()
        })
        .unwrap();
        let mut c = cfg(&sim);
        c.rx_source = IcomRxSource::If12k;
        let src = IcomNetSource::open(&c).expect("open");
        assert_eq!(src.open_status(), None, "nothing left for the operator to do by hand");

        let menu = |hi: u8, lo: u8, value: u8| {
            sim.civ_frames().iter().any(|f| {
                f.len() >= 10
                    && f[4] == 0x1A
                    && f[5] == 0x05
                    && f[6] == hi
                    && f[7] == lo
                    && f[8] == value
            })
        };
        wait_for("the IC-705 menu writes", || {
            // WLAN AF/IF Output > Output Select = IF, and both MOD inputs to WLAN.
            menu(0x01, 0x14, 0x01) && menu(0x01, 0x18, 0x03) && menu(0x01, 0x19, 0x03)
        });
        // And never the USB port's copy of the same setting, one item block
        // earlier — that would leave the network stream on AF.
        assert!(!menu(0x01, 0x09, 0x01), "the USB output select is not ours to write");
    }

    #[test]
    fn the_scope_span_is_commanded_rather_than_left_wherever_the_radio_had_it() {
        let sim = Sim::start(SimOptions { civ_address: 0xA4, ..Default::default() }).unwrap();
        let mut c = cfg(&sim);
        c.scope_span = sdroxide_types::IcomScopeSpan::Khz500;
        let _src = IcomNetSource::open(&c).expect("open");
        wait_for("centre mode and a ±500 kHz span", || {
            let frames = sim.civ_frames();
            let centre = frames
                .iter()
                .any(|f| f.len() >= 8 && f[4] == 0x27 && f[5] == 0x14 && f[6..8] == [0x00, 0x00]);
            let span = frames.iter().any(|f| {
                f.len() >= 12
                    && f[4] == 0x27
                    && f[5] == 0x15
                    && f[6] == 0x00
                    && civ::decode_freq(&f[7..12]) == Some(500_000.0)
            });
            centre && span
        });
    }

    #[test]
    fn leaving_the_span_to_the_radio_touches_neither_setting() {
        let sim = Sim::start(SimOptions { civ_address: 0xA4, ..Default::default() }).unwrap();
        let mut c = cfg(&sim);
        c.scope_span = sdroxide_types::IcomScopeSpan::Radio;
        let mut src = IcomNetSource::open(&c).expect("open");
        // Wait for something the session does send, so "nothing was sent" is a
        // real observation rather than a race with the opening burst.
        let mut buf = vec![Complex32::default(); 1024];
        let mut bins = Vec::new();
        wait_for("a scope sweep", || {
            let _ = src.read(&mut buf);
            src.wide_spectrum_db(&mut bins).is_some()
        });
        assert!(
            !sim.civ_frames()
                .iter()
                .any(|f| f.len() >= 6 && f[4] == 0x27 && (f[5] == 0x14 || f[5] == 0x15)),
            "the operator asked for the radio's own span"
        );
    }

    #[test]
    fn an_unknown_model_says_what_the_operator_has_to_set_by_hand() {
        let sim = Sim::start(SimOptions {
            civ_address: 0xA2,
            radio_name: "IC-9700".into(),
            scope: false,
            ..Default::default()
        })
        .unwrap();
        let src = IcomNetSource::open(&cfg(&sim)).expect("open");
        let status = src.open_status().expect("a note about the menu");
        assert!(status.contains("modulation input"), "{status}");
    }

    #[test]
    fn asking_for_the_if_at_a_rate_that_cannot_carry_it_falls_back_and_says_so() {
        let sim =
            Sim::start(SimOptions { sample_rate: 24_000, scope: false, ..Default::default() })
                .unwrap();
        let mut c = cfg(&sim);
        c.rx_source = IcomRxSource::If12k;
        c.sample_rate_hz = 24_000;
        let src = IcomNetSource::open(&c).expect("open");
        assert_eq!(src.sample_rate(), 24_000.0);
        assert!(src.open_status().unwrap().contains("12 kHz IF needs a 48 kHz"));
    }

    #[test]
    fn transmitting_keys_the_radio_and_unkeying_releases_it() {
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        let ptt = |on: u8| {
            sim.civ_frames()
                .iter()
                .any(|f| f.len() >= 7 && f[4] == 0x1C && f[5] == 0x00 && f[6] == on)
        };
        src.tx_begin(14_074_000.0, 48_000.0).unwrap();
        wait_for("PTT on", || ptt(1));
        src.tx_end().unwrap();
        wait_for("PTT off", || ptt(0));
    }

    #[test]
    fn a_dead_session_asks_to_be_reopened() {
        let sim = Sim::start(SimOptions { scope: false, ..Default::default() }).unwrap();
        let mut src = IcomNetSource::open(&cfg(&sim)).expect("open");
        assert!(!src.needs_reopen());
        src.release();
        assert!(src.needs_reopen());
    }
}
