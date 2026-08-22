//! `PacketController` — AX.25 packet radio, on HF and on VHF/UHF.
//!
//! Receive is complete: modem, deframer, codec and the monitor an operator
//! watches. Transmit, CSMA and connected mode follow.
//!
//! # Why one controller for two modes
//!
//! [`Mode::Packet`] (VHF/UHF, FM) and [`Mode::PacketHf`] (HF, sideband) are the
//! same link layer over different radios. The waveform differs — Bell 202 tones
//! at 1200, a shaped scrambled baseband at 9600, 200 Hz-shift AFSK at 300 — but
//! above the bit stream every one of them is HDLC and AX.25, so the split lives
//! in the modem this holds, not in the controller or the protocol.
//!
//! The `Mode` split exists because the *radio* differs: FM against sideband
//! decides the filter, the modulator, the mode commanded over CAT and where the
//! band plan puts it. Those are per-`Mode` constants throughout the tree, and
//! one variant with a config field would have to thread that field into every
//! one of them.
//!
//! # Two rules measured rather than assumed
//!
//! 1. **The modem must not hear its own transmission.** Full-duplex hardware
//!    keeps receiving through an over, and in audio mode `on_rx_audio` is fed
//!    unconditionally. For a keyboard mode that is cosmetic; for AX.25 it is
//!    fatal, because the link layer would see its own I-frames and acknowledge
//!    itself. Hence [`PacketController::keyed`].
//! 2. **A CSMA slot timer may never be clocked from [`DigiEngine::poll`].**
//!    `poll` runs once per source block while receiving — measured at 341 ms on
//!    a sound-card rig with the real buffer size, against the 10 ms a slot
//!    wants. Slot countdown and DCD hold are therefore counted on the audio
//!    clock inside `on_rx_audio`. See
//!    `crates/sdroxide-radio/tests/tx_turnaround.rs` for the measurement.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sdroxide_ax25::{
    Addr, Deframer, Discard, Framer, Packet, PacketType, PortEndpoint, PortEvent, PortRequest,
    state,
};
use sdroxide_dsp::{AFSK_TX_PEAK, AfskProfile, AfskRx, AfskTx, G3RUH_TX_PEAK, G3ruhRx, G3ruhTx};
use sdroxide_types::{
    DigiConfig, DigiStatus, Mode, PACKET_HEARD_MAX, PacketBaud, PacketHeard, PacketStatus, QsoStep,
    TranscriptLine,
};

use crate::DigiEngine;
use crate::controller::DigiAction;

/// The modem for the configured speed. Both arms emit line levels; everything
/// above them is shared.
enum Modem {
    Afsk(Box<AfskRx>),
    G3ruh(Box<G3ruhRx>),
}

impl Modem {
    fn new(baud: PacketBaud, rate: f64) -> Modem {
        match baud {
            PacketBaud::Hf300 => Modem::Afsk(Box::new(AfskRx::new(rate, AfskProfile::Hf300))),
            PacketBaud::Vhf1200 => Modem::Afsk(Box::new(AfskRx::new(rate, AfskProfile::Vhf1200))),
            PacketBaud::Vhf9600 => Modem::G3ruh(Box::new(G3ruhRx::new(rate))),
        }
    }

    fn process(&mut self, audio: &[f32], out: &mut Vec<bool>) {
        match self {
            Modem::Afsk(m) => m.process(audio, out),
            Modem::G3ruh(m) => m.process(audio, out),
        }
    }

    /// How convincingly a signal is present, 0..1.
    fn separation(&self) -> f32 {
        match self {
            Modem::Afsk(m) => m.separation(),
            Modem::G3ruh(m) => m.separation(),
        }
    }

    fn magnitude(&self) -> f32 {
        match self {
            Modem::Afsk(m) => m.magnitude(),
            Modem::G3ruh(m) => m.magnitude(),
        }
    }
}

/// The transmit half, matching [`Modem`].
enum TxModem {
    Afsk(Box<AfskTx>),
    G3ruh(Box<G3ruhTx>),
}

impl TxModem {
    fn new(baud: PacketBaud, rate: f64) -> TxModem {
        match baud {
            PacketBaud::Hf300 => TxModem::Afsk(Box::new(AfskTx::new(rate, AfskProfile::Hf300))),
            PacketBaud::Vhf1200 => TxModem::Afsk(Box::new(AfskTx::new(rate, AfskProfile::Vhf1200))),
            PacketBaud::Vhf9600 => TxModem::G3ruh(Box::new(G3ruhTx::new(rate))),
        }
    }

    fn push_bits(&mut self, bits: &[bool]) {
        match self {
            TxModem::Afsk(m) => m.push_bits(bits),
            TxModem::G3ruh(m) => m.push_bits(bits),
        }
    }

    fn next_block(&mut self, out: &mut [f32]) -> usize {
        match self {
            TxModem::Afsk(m) => m.next_block(out),
            TxModem::G3ruh(m) => m.next_block(out),
        }
    }

    fn idle(&self) -> bool {
        match self {
            TxModem::Afsk(m) => m.idle(),
            TxModem::G3ruh(m) => m.idle(),
        }
    }

    /// The loudest sample this modem produces. The two differ: the AFSK tone is
    /// exactly its half-scale amplitude, while the 9600 baseband rings past its
    /// symbol level wherever the shaping filter meets a run of transitions.
    fn peak(&self) -> f32 {
        match self {
            TxModem::Afsk(_) => AFSK_TX_PEAK,
            TxModem::G3ruh(_) => G3RUH_TX_PEAK,
        }
    }
}

/// Above this the channel counts as busy, and CSMA will not key.
///
/// Well clear of the idle-noise figure — an unmodulated channel sits near zero,
/// because the two tone branches see the same noise — and well below what a
/// real signal produces, so it does not need tuning per band.
const DCD_THRESHOLD: f32 = 0.35;

/// The speed the modem is built for, given the mode and the operator's choice.
///
/// HF has exactly one speed, so a stale `Vhf9600` left in the config from a VHF
/// session must not follow the operator to 40 metres and produce a receiver
/// that decodes nothing without ever saying why.
fn baud_for(mode: Mode, cfg: &DigiConfig) -> PacketBaud {
    match mode {
        Mode::PacketHf => PacketBaud::Hf300,
        _ => match cfg.packet_baud {
            PacketBaud::Hf300 => PacketBaud::Vhf1200,
            b => b,
        },
    }
}

pub struct PacketController {
    mode: Mode,
    cfg: DigiConfig,
    tap_rate: f64,

    baud: PacketBaud,
    modem: Modem,
    deframer: Deframer,

    /// True while our own transmitter is on the air. Rule 1 above: the modem is
    /// deaf by construction for as long as this is set, whatever the hardware
    /// hands us.
    keyed: bool,

    tx_modem: TxModem,
    /// Frames waiting for the channel, oldest first.
    pending: VecDeque<Vec<u8>>,
    /// Set by CSMA once the channel is clear and the dice have been thrown;
    /// `poll` turns it into a key-up request.
    want_tx: bool,
    /// Samples left in the current CSMA slot. Counted here, on the audio clock,
    /// and never in `poll` — see rule 2.
    slot_left: u32,
    /// Samples in one slot at the current tap rate.
    slot_samples: u32,
    /// When the next beacon is due. `None` disables it.
    next_beacon: Option<SystemTime>,

    /// The connected-mode link, once something has asked for one.
    link: Option<Link>,
    /// Frames heard on the air, waiting for a KISS host to collect them.
    /// Capped, because a host that stops reading must not grow this forever.
    air_frames: Vec<Vec<u8>>,

    heard: Vec<PacketHeard>,
    bad_frames: u32,
    dcd: bool,

    levels: Vec<bool>,
    frames: Vec<Vec<u8>>,

    queued: Vec<DigiAction>,
    status_dirty: bool,
    last_status: Option<SystemTime>,
}

impl PacketController {
    pub fn new(mode: Mode, cfg: DigiConfig, tap_rate: f64) -> Self {
        let baud = baud_for(mode, &cfg);
        let cfg_slot = cfg.clone();
        PacketController {
            mode,
            cfg,
            tap_rate,
            baud,
            modem: Modem::new(baud, tap_rate),
            deframer: Deframer::new(),
            keyed: false,
            tx_modem: TxModem::new(baud, tap_rate),
            pending: VecDeque::new(),
            want_tx: false,
            // A full slot, not zero. Zero means the first audio block after a
            // frame is queued keys immediately, without the wait that stops two
            // stations pouncing together — and the window for it is exactly the
            // case of queueing something before any audio has arrived, which is
            // what a beacon at startup does.
            slot_left: slot_samples(&cfg_slot, tap_rate),
            slot_samples: slot_samples(&cfg_slot, tap_rate),
            next_beacon: None,
            link: None,
            air_frames: Vec::new(),
            heard: Vec::new(),
            bad_frames: 0,
            dcd: false,
            levels: Vec::new(),
            frames: Vec::new(),
            queued: Vec::new(),
            status_dirty: true,
            last_status: None,
        }
    }

    /// Rebuild the modem, after a speed change or a new tap rate.
    fn rebuild(&mut self) {
        self.baud = baud_for(self.mode, &self.cfg);
        self.modem = Modem::new(self.baud, self.tap_rate);
        self.tx_modem = TxModem::new(self.baud, self.tap_rate);
        self.deframer = Deframer::new();
        self.slot_samples = slot_samples(&self.cfg, self.tap_rate);
        self.status_dirty = true;
    }

    /// Queue a frame for the next clear slot.
    ///
    /// Nothing is sent from here — CSMA decides when, on the audio clock. That
    /// separation is the point: a caller that could transmit directly would be
    /// able to key on top of another station.
    pub fn queue_frame(&mut self, frame: Vec<u8>) {
        self.pending.push_back(frame);
        self.status_dirty = true;
    }

    /// Our own address, or `None` when the operator has not set one.
    ///
    /// A packet station with no callsign must not transmit — it would be an
    /// unidentified signal, which is illegal everywhere. Every transmit path
    /// goes through this and gives up quietly when it returns `None`.
    fn mycall(&self) -> Option<Addr> {
        let call = self.cfg.packet_mycall.trim();
        if call.is_empty() {
            return None;
        }
        Addr::new(call).ok()
    }

    /// Give the controller the engine end of a link port.
    ///
    /// The engine makes the pair and holds the other end, rather than the
    /// controller handing one out: a mode change destroys the controller, and
    /// the port's lifetime has to be something the engine can reason about.
    pub fn attach_port(&mut self, port: PortEndpoint) {
        let mut link = Link::new(port);
        link.data.set_accept_incoming(self.cfg.packet_accept_incoming);
        self.link = Some(link);
        self.status_dirty = true;
    }

    /// Drive the link: whatever the session asked for, plus the timers.
    fn poll_link(&mut self) {
        let Some(link) = self.link.as_mut() else { return };
        let mut frames = Vec::new();

        for req in link.port.take_requests() {
            match req {
                PortRequest::Connect { peer, ext, .. } => {
                    link.handle(&state::Event::Connect { addr: peer, ext }, &mut frames);
                }
                PortRequest::Data(d) => link.handle(&state::Event::Data(d), &mut frames),
                PortRequest::Disconnect => link.handle(&state::Event::Disconnect, &mut frames),
            }
        }

        // Timers. These belong here rather than on the audio clock: T1 and T3
        // are seconds, and `poll` runs often enough for seconds even at the
        // 341 ms worst case. The CSMA slot clock is the one that cannot live
        // here — see rule 2.
        if link.data.t1_expired() {
            link.handle(&state::Event::T1, &mut frames);
        }
        if link.data.t3_expired() {
            link.handle(&state::Event::T3, &mut frames);
        }

        // Announce the link coming up exactly once.
        //
        // `is_state_connected` and not `data.peer().is_some()`: the peer is
        // recorded the moment a connect is *requested*, so testing it announces
        // a link that is still waiting for its UA — and the session then writes
        // into a machine that refuses with "writing data while not connected".
        let up = link.state.is_state_connected();
        if up && !link.announced {
            link.announced = true;
            link.port.emit(PortEvent::Connected);
        } else if !up && link.announced {
            link.announced = false;
            link.port.emit(PortEvent::Disconnected);
        }

        for f in frames {
            self.queue_frame(f);
        }
    }

    /// Hand a received frame to the link, if one is open and it is addressed
    /// to us.
    fn feed_link(&mut self, p: &Packet) {
        let Some(link) = self.link.as_mut() else { return };
        // Compare the **callsigns**, not the addresses.
        //
        // `Addr` carries four more bits than the call — command/response,
        // end-of-address, and two reserved — and they differ between a frame
        // off the air and a freshly parsed `Addr::new`. Comparing whole
        // addresses therefore never matches, and the symptom is a station that
        // hears a SABM addressed to it, files it in the monitor, and never
        // answers. Silent, and indistinguishable from a dead receiver.
        if p.dst().call() != link.port.cfg.me.call() {
            // Somebody else's traffic. It belongs in the monitor, which has
            // already had it, and nowhere near our sequence numbers.
            return;
        }
        let mut frames = Vec::new();
        let cr = p.command_response();
        match p.packet_type() {
            PacketType::Sabm(f) => {
                link.handle(&state::Event::Sabm(f.clone(), p.src().clone()), &mut frames)
            }
            PacketType::Sabme(f) => {
                link.handle(&state::Event::Sabme(f.clone(), p.src().clone()), &mut frames)
            }
            PacketType::Ua(f) => link.handle(&state::Event::Ua(f.clone()), &mut frames),
            PacketType::Dm(f) => link.handle(&state::Event::Dm(f.clone()), &mut frames),
            PacketType::Disc(f) => link.handle(&state::Event::Disc(f.clone()), &mut frames),
            PacketType::Iframe(f) => link.handle(&state::Event::Iframe(f.clone(), cr), &mut frames),
            PacketType::Rr(f) => link.handle(&state::Event::Rr(f.clone(), cr), &mut frames),
            PacketType::Rnr(f) => link.handle(&state::Event::Rnr(f.clone()), &mut frames),
            PacketType::Rej(f) => link.handle(&state::Event::Rej(f.clone()), &mut frames),
            PacketType::Srej(f) => link.handle(&state::Event::Srej(f.clone()), &mut frames),
            PacketType::Frmr(f) => link.handle(&state::Event::Frmr(f.clone()), &mut frames),
            PacketType::Xid(f) => link.handle(&state::Event::Xid(f.clone(), cr), &mut frames),
            PacketType::Test(f) => link.handle(&state::Event::Test(f.clone(), cr), &mut frames),
            // UI frames are monitor traffic; they carry no link state.
            PacketType::Ui(_) => {}
        }
        for f in frames {
            self.queue_frame(f);
        }
    }

    /// Frames heard since the last call, for a KISS host.
    ///
    /// Drained rather than pushed: the engine already polls this controller, and
    /// a controller that reached out to a socket would have to know one exists.
    pub fn take_air_frames(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.air_frames)
    }

    /// Transmit a frame on a KISS host's behalf.
    ///
    /// It goes through CSMA like everything else — a host cannot key the radio
    /// directly, only ask. The frame is used verbatim: KISS hands over address
    /// through information and the FCS is ours to add, exactly as a TNC's
    /// firmware would.
    pub fn send_from_host(&mut self, frame: Vec<u8>) {
        self.queue_frame(frame);
    }

    /// Send a beacon now, on the operator's command, rather than waiting for
    /// the timer. Still subject to CSMA — nothing here bypasses the channel.
    pub fn queue_beacon_now(&mut self) {
        self.queue_beacon();
    }

    /// Queue an UNPROTO beacon carrying the operator's text.
    fn queue_beacon(&mut self) {
        let (Some(me), Some(dest)) = (self.mycall(), Addr::new("BEACON").ok()) else {
            return;
        };
        let text = self.cfg.packet_beacon_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let p = Packet::ui(me, dest, text.into_bytes());
        self.queue_frame(p.serialize(false));
    }

    /// One CSMA step, called once per received sample block.
    ///
    /// Standard KISS p-persistence: while the channel is busy the slot timer is
    /// held at zero, so the countdown only begins once the channel clears; each
    /// time a slot elapses, transmit with probability `persist/256`, otherwise
    /// wait another slot. The randomness is what stops two stations that were
    /// both waiting out the same transmission from colliding the instant it
    /// ends.
    fn csma(&mut self, samples: usize) {
        if self.pending.is_empty() || self.keyed || self.want_tx {
            self.slot_left = self.slot_samples;
            return;
        }
        if self.dcd {
            // Busy. Restart the countdown so we wait a full slot after it
            // clears rather than pouncing on the first quiet sample.
            self.slot_left = self.slot_samples;
            return;
        }
        let n = samples as u32;
        if self.slot_left > n {
            self.slot_left -= n;
            return;
        }
        self.slot_left = self.slot_samples;
        if roll() < self.cfg.packet_persist {
            self.want_tx = true;
            self.status_dirty = true;
        }
    }

    /// Render everything queued into the transmit modem: flags for TXDELAY,
    /// the frames, then a short tail.
    ///
    /// Frames share a single flag between them rather than each paying its own
    /// preamble — on a link with several frames outstanding that is the
    /// difference between one over and several.
    fn build_over(&mut self) {
        let bits_per_flag = 8.0;
        let baud = self.baud.baud();
        let flags =
            |ms: u16| ((ms as f64 / 1000.0) * baud / bits_per_flag).ceil().max(1.0) as usize;

        let mut framer = Framer::new();
        framer.push_flags(flags(self.cfg.packet_txdelay_ms));
        let mut sent = Vec::new();
        while let Some(f) = self.pending.pop_front() {
            framer.push_frame(&f);
            framer.push_flags(1);
            sent.push(f);
        }
        framer.push_flags(flags(self.cfg.packet_txtail_ms));
        let bits = framer.take();
        self.tx_modem.push_bits(&bits);

        // Our own traffic belongs in the monitor too: an operator watching a
        // session needs both halves of it, and a beacon that never appears
        // looks exactly like a beacon that was never sent.
        for f in sent {
            self.note_frame(&f, true);
        }
    }

    fn note(&mut self, entry: PacketHeard) {
        if self.heard.len() >= PACKET_HEARD_MAX {
            self.heard.remove(0);
        }
        self.heard.push(entry);
        self.status_dirty = true;
    }

    /// Turn a verified frame into a monitor line.
    ///
    /// A frame that fails to parse is still recorded rather than dropped
    /// silently: it passed a 16-bit FCS, so it is almost certainly a real frame
    /// of a kind the codec does not handle, and an operator staring at a quiet
    /// pane deserves to know the difference between "nothing is being heard"
    /// and "something is being heard and not understood".
    fn on_frame(&mut self, bytes: &[u8]) {
        self.note_frame(bytes, false);
    }

    fn note_frame(&mut self, bytes: &[u8], sent: bool) {
        let at =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        // `None` means "work out the sequence-number width from the reserved
        // bit in the source address", rather than assuming mod-8.
        //
        // A monitor is reading connections it did not join, so it cannot know:
        // an S or I frame has a one-byte control field under mod-8 and a
        // two-byte one under mod-128, and guessing wrong shifts everything
        // after it. The reserved bit is what the Linux stack and `axlisten` use
        // to signal the difference. It is not in the standard, so a mod-128
        // station that does not set it is still mis-read — but nothing short of
        // having seen the SABME can do better, and assuming mod-8 outright is
        // wrong strictly more often.
        match Packet::parse(bytes, None) {
            Ok(p) => {
                self.feed_link(&p);
                let (kind, text) = describe(&p);
                let entry = PacketHeard {
                    at,
                    from: p.src().call().to_string(),
                    to: p.dst().call().to_string(),
                    via: p.digipeaters().iter().map(|a| a.call().to_string()).collect(),
                    kind,
                    text,
                    sent,
                };
                self.note(entry);
            }
            Err(e) => {
                tracing::debug!("packet frame passed its FCS but would not parse: {e}");
                self.note(PacketHeard {
                    at,
                    from: String::new(),
                    to: String::new(),
                    via: Vec::new(),
                    kind: "?".into(),
                    text: format!("unparsed, {} bytes", bytes.len()),
                    sent,
                });
            }
        }
    }

    fn packet_status(&self) -> PacketStatus {
        PacketStatus {
            baud: self.baud,
            dcd: self.dcd,
            level: self.modem.magnitude().clamp(0.0, 1.0),
            heard: self.heard.clone(),
            bad_frames: self.bad_frames,
            link: self
                .link
                .as_ref()
                .filter(|l| l.state.is_state_connected())
                .and_then(|l| l.data.peer().map(|p| p.call().to_string())),
        }
    }

    fn digi_status(&self) -> DigiStatus {
        DigiStatus {
            mode: self.mode,
            step: QsoStep::Idle,
            dx_call: None,
            dx_grid: None,
            tx_next: false,
            tx_pending_msg: None,
            audio_hz: self.audio_hz(),
            tx_even: false,
            transmitting: self.keyed,
            tx_watchdog: false,
            transcript: Vec::<TranscriptLine>::new(),
            config: self.cfg.clone(),
            text_rx: String::new(),
            tx_sent: 0,
            fsq_heard: Vec::new(),
            fsq_messages: Vec::new(),
            rade: None,
            packet: Some(self.packet_status()),
            js8: None,
            fox_queue: Vec::new(),
            call_queue: Vec::new(),
            clock_offset_s: None,
            cw: None,
            wspr: None,
            qso: None,
        }
    }
}

/// A connected-mode link: the vendored state machine plus the port it serves.
///
/// The machine is pure — events in, actions out, no I/O and no clock of its
/// own — so everything about *when* lives here.
struct Link {
    state: Box<dyn state::State>,
    data: state::Data,
    port: PortEndpoint,
    /// Whether the session has been told the link is up, so `Connected` is
    /// emitted exactly once.
    announced: bool,
}

impl Link {
    fn new(port: PortEndpoint) -> Link {
        Link {
            state: state::new(),
            data: state::Data::new(port.cfg.me.clone()),
            port,
            announced: false,
        }
    }

    /// Feed one event and collect what falls out: frames to transmit, and
    /// anything the session needs to hear.
    fn handle(&mut self, ev: &state::Event, out: &mut Vec<Vec<u8>>) {
        let (next, returns) = state::handle(&*self.state, &mut self.data, ev);
        if let Some(next) = next {
            self.state = next;
        }
        let ext = self.data.ext();
        for r in returns {
            match &r {
                state::ReturnEvent::Data(state::Res::Some(d)) => {
                    self.port.emit(PortEvent::Data(d.clone()));
                }
                state::ReturnEvent::Data(state::Res::EOF) => {
                    self.port.emit(PortEvent::Disconnected);
                }
                state::ReturnEvent::Data(state::Res::None) => {}
                state::ReturnEvent::DlError(e) => {
                    // A link-layer error is not automatically fatal — most are
                    // recoverable and the machine says so by carrying on. Only
                    // the retry-exhaustion one ends the session, and the machine
                    // signals that by going back to Disconnected, which the
                    // caller notices.
                    tracing::debug!("AX.25 link error: {e}");
                }
                state::ReturnEvent::Packet(_) => {}
            }
            if let Some(frame) = r.serialize(ext) {
                out.push(frame);
            }
        }
    }
}

/// Samples in one CSMA slot at `rate`.
fn slot_samples(cfg: &DigiConfig, rate: f64) -> u32 {
    ((cfg.packet_slottime_ms.max(1) as f64 / 1000.0) * rate).round().max(1.0) as u32
}

/// A random byte for the persistence test.
///
/// Falls back to the clock when there is no entropy source. That is weaker than
/// it looks but not dangerous here: the worst case is two stations picking the
/// same slot and colliding, which the link layer already has to survive.
fn roll() -> u8 {
    let mut b = [0u8; 1];
    if getrandom::fill(&mut b).is_err() {
        let nanos =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        return (nanos >> 7) as u8;
    }
    b[0]
}

/// The frame type as a monitor prints it, and whatever text it carried.
fn describe(p: &Packet) -> (String, String) {
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim_end_matches('\n').to_string();
    match p.packet_type() {
        PacketType::Ui(f) => ("UI".into(), text(&f.payload)),
        PacketType::Iframe(f) => (format!("I{}{}", f.ns, f.nr), text(&f.payload)),
        PacketType::Sabm(_) => ("SABM".into(), String::new()),
        PacketType::Sabme(_) => ("SABME".into(), String::new()),
        PacketType::Ua(_) => ("UA".into(), String::new()),
        PacketType::Dm(_) => ("DM".into(), String::new()),
        PacketType::Disc(_) => ("DISC".into(), String::new()),
        PacketType::Rr(f) => (format!("RR{}", f.nr), String::new()),
        PacketType::Rnr(f) => (format!("RNR{}", f.nr), String::new()),
        PacketType::Rej(f) => (format!("REJ{}", f.nr), String::new()),
        PacketType::Srej(f) => (format!("SREJ{}", f.nr), String::new()),
        PacketType::Frmr(_) => ("FRMR".into(), String::new()),
        PacketType::Xid(_) => ("XID".into(), String::new()),
        PacketType::Test(f) => ("TEST".into(), text(&f.payload)),
    }
}

impl DigiEngine for PacketController {
    fn mode(&self) -> Mode {
        self.mode
    }

    /// Rule 1: never while our own transmitter is up.
    ///
    /// The gate is here rather than in the engine because the engine has good
    /// reasons to keep the receive chain running through an over — a full-duplex
    /// front end genuinely is receiving, and the panadapter wants it. It is this
    /// mode that must not listen, so this is where the refusal belongs.
    fn on_rx_audio(&mut self, tap: &[f32]) {
        if self.keyed {
            return;
        }
        let mut levels = std::mem::take(&mut self.levels);
        levels.clear();
        self.modem.process(tap, &mut levels);

        let mut frames = std::mem::take(&mut self.frames);
        frames.clear();
        for lvl in levels.drain(..) {
            self.deframer.push_level(lvl, &mut frames);
        }
        for f in frames.drain(..) {
            // Keep a copy for any KISS host before the codec gets an opinion:
            // a host is entitled to frames we cannot parse, which is most of
            // the point of offering the modem rather than the decoder.
            if self.air_frames.len() < PACKET_HEARD_MAX {
                self.air_frames.push(f.clone());
            }
            self.on_frame(&f);
        }
        for d in self.deframer.take_discards() {
            if d == Discard::BadFcs {
                self.bad_frames = self.bad_frames.saturating_add(1);
                self.status_dirty = true;
            }
        }

        // Carrier detect and the CSMA slot clock, both on the audio clock and
        // never on `poll` — see rule 2.
        let dcd = self.modem.separation() > DCD_THRESHOLD;
        if dcd != self.dcd {
            self.dcd = dcd;
            self.status_dirty = true;
        }
        self.csma(tap.len());

        self.levels = levels;
        self.frames = frames;
    }

    fn poll(&mut self, now_in: SystemTime, _dial_hz: f64) -> Vec<DigiAction> {
        let mut actions = std::mem::take(&mut self.queued);

        self.poll_link();

        // Beacon. Scheduled here rather than on the audio clock because minutes
        // are exactly what `poll`'s cadence is good for, and because a beacon
        // that slips a few hundred milliseconds is a beacon nobody notices.
        let every = self.cfg.packet_beacon_minutes;
        if every == 0 || self.cfg.packet_beacon_text.trim().is_empty() {
            self.next_beacon = None;
        } else {
            match self.next_beacon {
                // Wait one interval before the first one: keying the moment the
                // operator selects the mode is startling, and the settings they
                // are about to type are not in yet.
                None => self.next_beacon = Some(now_in + Duration::from_secs(every as u64 * 60)),
                Some(at) if now_in >= at => {
                    self.queue_beacon();
                    self.next_beacon = Some(now_in + Duration::from_secs(every as u64 * 60));
                }
                Some(_) => {}
            }
        }

        // CSMA has cleared us to transmit: key up through the engine's normal
        // PTT path so the station interlock and the band rails apply. If they
        // refuse, `abort_tx` puts the frames back on the queue rather than
        // dropping them.
        if self.want_tx && !self.keyed {
            self.want_tx = false;
            self.keyed = true;
            self.build_over();
            self.status_dirty = true;
            actions.push(DigiAction::KeyTx);
        }

        let now = SystemTime::now();
        let due = self
            .last_status
            .map(|t| now.duration_since(t).map(|d| d.as_secs_f32() > 0.2).unwrap_or(true))
            .unwrap_or(true);
        if self.status_dirty || due {
            self.status_dirty = false;
            self.last_status = Some(now);
            actions.push(DigiAction::Status(self.digi_status()));
        }
        actions
    }

    fn tx_burst_active(&self) -> bool {
        self.keyed
    }

    /// Whichever modem this speed uses, its own headroom — the shaped 9600
    /// baseband keeps more of it than the 1200 tone does, and declaring the
    /// tone's figure for both would drive the shaping peaks into the limiter
    /// and close the eye.
    fn tx_peak(&self) -> f32 {
        self.tx_modem.peak()
    }

    fn fill_tx_block(&mut self, out: &mut [f32]) -> bool {
        let n = self.tx_modem.next_block(out);
        // Anything the modem did not fill is silence, and a block it filled
        // nothing of means the over is played out. Returning `true` ends it:
        // holding a packet channel open with dead carrier is the one thing a
        // packet station must never do.
        if n == 0 {
            out.fill(0.0);
            return true;
        }
        self.tx_modem.idle() && n < out.len()
    }

    fn on_burst_done(&mut self) {
        self.keyed = false;
        self.status_dirty = true;
    }

    fn abort(&mut self) {
        self.keyed = false;
        self.want_tx = false;
        self.pending.clear();
        self.tx_modem = TxModem::new(self.baud, self.tap_rate);
        self.status_dirty = true;
    }

    /// The safety rails refused the key-up, or the operator stopped the over.
    ///
    /// The queue is left alone. A refused key is usually temporary — another
    /// radio holds the station interlock — and throwing the frames away would
    /// turn a moment's contention into lost traffic. CSMA will try again.
    fn abort_tx(&mut self) {
        self.keyed = false;
        self.want_tx = false;
        self.tx_modem = TxModem::new(self.baud, self.tap_rate);
        self.status_dirty = true;
    }

    fn set_config(&mut self, cfg: DigiConfig) {
        let was = self.baud;
        self.cfg = cfg;
        if let Some(l) = self.link.as_mut() {
            l.data.set_accept_incoming(self.cfg.packet_accept_incoming);
        }
        if baud_for(self.mode, &self.cfg) != was {
            self.rebuild();
        }
        self.status_dirty = true;
    }

    /// The operator's audio-frequency control does not apply: on FM the signal
    /// is centred on the dial, and on HF the tone pair is fixed by the standard
    /// rather than by the waterfall cursor.
    fn set_audio_hz(&mut self, _hz: f32) {}

    fn audio_hz(&self) -> f32 {
        match self.baud {
            // Centred on the carrier: there is no audio offset to report.
            _ if self.mode == Mode::Packet => 0.0,
            PacketBaud::Hf300 => AfskProfile::Hf300.centre_hz() as f32,
            _ => AfskProfile::Vhf1200.centre_hz() as f32,
        }
    }

    fn packet_beacon_now(&mut self) {
        self.queue_beacon();
    }

    fn packet_send_frame(&mut self, frame: Vec<u8>) {
        self.send_from_host(frame);
    }

    fn packet_take_air_frames(&mut self) -> Vec<Vec<u8>> {
        self.take_air_frames()
    }

    /// Empty the monitor. The bad-frame counter goes with it — it is the
    /// monitor's own tally of what it could not read, and a count left over
    /// from a cleared page describes traffic that is no longer on it. The link
    /// is untouched: a connected station stays connected.
    fn clear_rx(&mut self) {
        self.heard.clear();
        self.bad_frames = 0;
        self.status_dirty = true;
    }

    fn status(&self) -> DigiStatus {
        self.digi_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 1, pinned from the first commit rather than after the first
    /// mysterious self-acknowledgement on the air. The moment a transmitter
    /// lands behind this gate the bug it prevents is silent.
    #[test]
    fn a_keyed_station_does_not_listen_to_itself() {
        let mut c = PacketController::new(Mode::Packet, DigiConfig::default(), 48_000.0);
        c.keyed = true;
        c.on_rx_audio(&[0.5; 480]);
        assert!(c.heard.is_empty(), "a keyed station decoded something");
        assert!(!c.dcd, "a keyed station must not read its own signal as a busy channel");
    }

    /// An idle packet station holds the channel for nobody.
    #[test]
    fn an_empty_over_ends_immediately() {
        let mut c = PacketController::new(Mode::Packet, DigiConfig::default(), 48_000.0);
        let mut block = [1.0f32; 480];
        assert!(c.fill_tx_block(&mut block), "an over with nothing to send must end");
        assert!(block.iter().all(|s| *s == 0.0), "silence, not stale audio");
    }

    #[test]
    fn each_packet_mode_reports_its_own_name() {
        for mode in [Mode::Packet, Mode::PacketHf] {
            let c = PacketController::new(mode, DigiConfig::default(), 48_000.0);
            assert_eq!(c.mode(), mode);
            assert_eq!(c.status().mode, mode);
        }
    }

    /// HF runs at one speed. A `Vhf9600` left in the config from a VHF session
    /// must not follow the operator to 40 metres and produce a receiver that
    /// decodes nothing without ever saying why.
    #[test]
    fn hf_ignores_a_vhf_speed_left_in_the_config() {
        let cfg = DigiConfig { packet_baud: PacketBaud::Vhf9600, ..Default::default() };
        let c = PacketController::new(Mode::PacketHf, cfg, 48_000.0);
        assert_eq!(c.baud, PacketBaud::Hf300);
        assert_eq!(c.status().packet.unwrap().baud, PacketBaud::Hf300);
    }

    /// ...and the reverse: HF's speed is meaningless on VHF.
    #[test]
    fn vhf_falls_back_when_the_config_says_hf() {
        let cfg = DigiConfig { packet_baud: PacketBaud::Hf300, ..Default::default() };
        let c = PacketController::new(Mode::Packet, cfg, 48_000.0);
        assert_eq!(c.baud, PacketBaud::Vhf1200);
    }

    /// A station with no callsign must not transmit. Unidentified transmission
    /// is illegal everywhere, and an empty `packet_mycall` is the state the
    /// config ships in — so this is the default path, not an edge case.
    #[test]
    fn a_station_with_no_callsign_never_beacons() {
        let cfg = DigiConfig {
            packet_beacon_text: "sdroxide test".into(),
            packet_beacon_minutes: 1,
            packet_mycall: String::new(),
            ..Default::default()
        };
        let mut c = PacketController::new(Mode::Packet, cfg, 48_000.0);
        c.queue_beacon();
        assert!(c.pending.is_empty(), "beaconed without a callsign");
    }

    /// ...and with one, the beacon is a real frame addressed from that call.
    #[test]
    fn a_beacon_carries_the_operators_call_and_text() {
        let cfg = DigiConfig {
            packet_beacon_text: "sdroxide test".into(),
            packet_beacon_minutes: 1,
            packet_mycall: "OE3JJS-10".into(),
            ..Default::default()
        };
        let mut c = PacketController::new(Mode::Packet, cfg, 48_000.0);
        c.queue_beacon();
        let frame = c.pending.front().expect("no beacon queued");
        let p = Packet::parse(frame, None).expect("beacon does not parse");
        assert_eq!(p.src().call(), "OE3JJS-10");
        assert_eq!(p.dst().call(), "BEACON");
        match p.packet_type() {
            PacketType::Ui(ui) => assert_eq!(ui.payload, b"sdroxide test"),
            other => panic!("a beacon must be a UI frame, got {other:?}"),
        }
    }

    /// CSMA must not key while another station is transmitting. This is the
    /// whole reason the slot clock exists, and the failure mode is doubling on
    /// somebody mid-frame.
    #[test]
    fn csma_holds_off_while_the_channel_is_busy() {
        let cfg = DigiConfig { packet_persist: 255, ..Default::default() };
        let mut c = PacketController::new(Mode::Packet, cfg, 48_000.0);
        c.queue_frame(vec![0u8; 20]);
        c.dcd = true;
        // Far longer than a slot: with persistence at maximum it would fire
        // instantly if the busy check were missing.
        for _ in 0..50 {
            c.csma(4800);
        }
        assert!(!c.want_tx, "keyed on top of another station");

        c.dcd = false;
        for _ in 0..50 {
            c.csma(4800);
        }
        assert!(c.want_tx, "never keyed on a clear channel");
    }

    /// A slot must actually elapse. With persistence at maximum and a clear
    /// channel it is tempting to key immediately, but the wait is what keeps
    /// two stations that were both holding off from colliding the instant the
    /// channel clears.
    #[test]
    fn csma_waits_at_least_one_slot() {
        let cfg = DigiConfig { packet_persist: 255, packet_slottime_ms: 100, ..Default::default() };
        let mut c = PacketController::new(Mode::Packet, cfg, 48_000.0);
        c.queue_frame(vec![0u8; 20]);
        // 100 ms at 48 kHz is 4800 samples; one short block is not a slot.
        c.csma(480);
        assert!(!c.want_tx, "keyed before a slot had elapsed");
    }

    /// A refused key-up must not lose the traffic. Another radio holding the
    /// station interlock is temporary; the frames should still go when it lets
    /// go.
    #[test]
    fn a_refused_key_keeps_the_queue() {
        let mut c = PacketController::new(Mode::Packet, DigiConfig::default(), 48_000.0);
        c.queue_frame(vec![0u8; 20]);
        c.want_tx = true;
        c.abort_tx();
        assert_eq!(c.pending.len(), 1, "a refused key-up threw the frame away");
    }

    /// Changing speed rebuilds the modem. Without this the operator picks 9600,
    /// nothing decodes, and there is no clue as to why.
    #[test]
    fn a_speed_change_rebuilds_the_modem() {
        let mut c = PacketController::new(Mode::Packet, DigiConfig::default(), 48_000.0);
        assert_eq!(c.baud, PacketBaud::Vhf1200);
        c.set_config(DigiConfig { packet_baud: PacketBaud::Vhf9600, ..Default::default() });
        assert_eq!(c.baud, PacketBaud::Vhf9600);
        assert!(matches!(c.modem, Modem::G3ruh(_)));
    }
}
