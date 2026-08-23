//! The SmartSDR client: two threads behind one [`FlexHandle`].
//!
//! - **Control** owns the TCP socket on port 4992. It runs the connect
//!   handshake, sends sequenced commands, matches replies to them, and turns
//!   status lines into [`FlexUpdate`]s.
//! - **Data** owns a UDP socket the radio streams VITA-49 packets to. It pushes
//!   DAX IQ into the RX ring, drains the TX ring into DAX audio packets, and
//!   tracks meters.
//!
//! # Why DAX IQ and not the panadapter
//!
//! A FLEX will hand out either finished FFT bins plus demodulated audio (what
//! SmartSDR itself renders) or raw complex baseband over a DAX IQ stream.
//! sdroxide has its own DDC, demodulators, waterfall, skimmer and digital
//! modes; feeding them radio-side FFT bins would leave all of that unused and
//! limit reception to whatever the radio already decided to demodulate. So the
//! IQ stream is the data path, exactly as for TCI and HPSDR, and the radio's own
//! slice follows our dial rather than the other way round.
//!
//! The cost is bandwidth: DAX IQ tops out at 192 kHz, so that is this backend's
//! widest span.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_types::{Mode, TxTelemetry};

use crate::protocol::{self as p, Line};
use crate::trace::Trace;

/// How long the TCP connect and the version/handle exchange may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// How long a command may wait for its `R` reply before we give up on it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// Keepalive cadence. The radio drops a client that goes quiet.
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// DAX IQ channel we claim. Channel 1 of 4; a second sdroxide against the same
/// radio would need a different one, which is why it is a field on the config
/// rather than a constant here.
pub const DEFAULT_IQ_CHANNEL: u32 = 1;

/// IQ sample rates a FLEX will deliver over DAX.
pub const IQ_RATES: [f64; 4] = [24_000.0, 48_000.0, 96_000.0, 192_000.0];

#[derive(Debug, thiserror::Error)]
pub enum FlexError {
    #[error("{0}")]
    Msg(String),
}

fn err(s: impl Into<String>) -> FlexError {
    FlexError::Msg(s.into())
}

/// A change the radio reported that the engine should adopt.
#[derive(Debug, Clone)]
pub enum FlexUpdate {
    /// The operator moved the slice's frequency on the radio.
    Freq(f64),
    /// The operator changed the slice's mode on the radio.
    Mode(Mode),
    /// RF power, as a 0..1 fraction of the radio's `rfpower` percentage.
    TxDrive(f32),
    /// TUNE power, as a 0..1 fraction of `tunepower`.
    TuneDrive(f32),
}

/// Control messages to the control thread.
enum Ctrl {
    SetCenter(f64),
    SetIf(f64),
    SetMode(Mode),
    TxOn(f64),
    TxOff,
    SetDrive(f64),
    SetTuneDrive(f64),
    /// Fire-and-forget command, for anything the typed API doesn't cover.
    Raw(String),
    Shutdown,
}

/// Radio identity learned during the handshake.
#[derive(Debug, Clone, Default)]
pub struct RadioIdent {
    pub version: String,
    pub model: String,
    pub nickname: String,
    pub callsign: String,
    /// The client handle the radio assigned us.
    pub handle: u32,
}

impl RadioIdent {
    /// Fill in the fields carried by the `radio` status object, which arrives in
    /// the burst answering `client gui` rather than in the `V`/`H` handshake.
    fn absorb_deferred(&mut self, lines: &[Line]) {
        for line in lines {
            let Line::Status { object, kvs, .. } = line else { continue };
            if object != "radio" {
                continue;
            }
            let take = |k: &str, into: &mut String| {
                if let Some(v) = kvs.get(k)
                    && !v.is_empty()
                {
                    *into = v.clone();
                }
            };
            take("model", &mut self.model);
            take("nickname", &mut self.nickname);
            take("callsign", &mut self.callsign);
        }
    }

    /// A human-readable name for the radio: its nickname, its model, or a
    /// generic label — whichever it actually told us.
    pub fn display_name(&self) -> String {
        match (self.nickname.is_empty(), self.model.is_empty()) {
            (false, false) => format!("{} ({})", self.nickname, self.model),
            (false, true) => self.nickname.clone(),
            (true, false) => self.model.clone(),
            (true, true) => "FlexRadio".to_string(),
        }
    }
}

/// A live connection to a FlexRadio. Dropping it tears the session down.
pub struct FlexHandle {
    ctrl: Sender<Ctrl>,
    rx: Consumer<f32>,
    tx: Producer<f32>,
    updates: Receiver<FlexUpdate>,
    telem: Receiver<TxTelemetry>,
    joins: Vec<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    /// The IQ rate the radio is actually sending, read off the packet class
    /// codes. Starts at the requested rate and corrects itself if the radio
    /// disagrees.
    actual_rate: Arc<AtomicU32>,
    pub ident: RadioIdent,
    pub requested_rate_hz: f64,
    pub trace: Trace,
}

impl FlexHandle {
    /// Connect to a radio, run the handshake, bring up the DAX IQ stream and
    /// spawn both threads.
    pub fn connect(
        address: &str,
        iq_rate_hz: f64,
        iq_channel: u32,
        station: &str,
    ) -> Result<FlexHandle, FlexError> {
        let trace = Trace::new();
        let (host, port) = split_addr(address)?;
        trace.note(format!("connecting to {host}:{port}"));

        let sockaddr = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| err(format!("resolve {host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| err(format!("no address for {host}:{port}")))?;
        let stream = TcpStream::connect_timeout(&sockaddr, CONNECT_TIMEOUT)
            .map_err(|e| err(format!("connect {host}:{port}: {e}")))?;
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| err(e.to_string()))?;

        let writer = stream.try_clone().map_err(|e| err(e.to_string()))?;
        let mut reader = LineReader::new(stream, trace.clone());
        // Status the radio volunteers during the handshake. Handed to the
        // control thread once it exists, so nothing said before it started is
        // lost — see [`Wire::request`].
        let mut deferred: Vec<Line> = Vec::new();

        // ── Handshake ───────────────────────────────────────────────────────
        // The radio speaks first: `V<version>`, then `H<handle>`. Nothing we
        // send before the handle arrives can be attributed to us.
        let mut ident = RadioIdent::default();
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while ident.handle == 0 {
            if Instant::now() > deadline {
                return Err(err("timed out waiting for the radio's version/handle lines"));
            }
            match reader.next_line()? {
                Some(Line::Version(v)) => ident.version = v,
                Some(Line::Handle(h)) => ident.handle = h,
                Some(other) => deferred.push(other),
                None => {}
            }
        }
        trace.note(format!("handle=0x{:08X} version={}", ident.handle, ident.version));

        let mut wire = Wire { out: writer, seq: 1, trace: trace.clone() };

        // Bind the UDP socket before telling the radio where to stream: the
        // port has to exist before `client udpport` names it, or the radio's
        // first packets land nowhere.
        let udp = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .map_err(|e| err(format!("bind VITA-49 UDP socket: {e}")))?;
        let udp_port = udp.local_addr().map_err(|e| err(e.to_string()))?.port();
        // Short, because this read is also what paces the TX audio pump: a long
        // timeout would starve the radio's transmit buffer whenever RX packets
        // stop arriving, which is exactly what a half-duplex radio does on
        // key-down.
        udp.set_read_timeout(Some(Duration::from_millis(2))).ok();
        trace.note(format!("VITA-49 UDP port {udp_port}"));

        // Identify, register as a GUI client, then subscribe. The order is the
        // radio's protocol state machine, not a preference: `client program`
        // establishes who we are before `client gui` claims a GUI slot, and
        // subscriptions before registration are refused.
        wire.send("client program sdroxide")?;
        let gui = wire.request(
            &mut reader,
            &mut deferred,
            &format!("client gui {}", gui_client_id(station)),
        )?;
        if gui.code != 0 {
            return Err(err(describe_gui_failure(gui.code, &gui.body)));
        }
        wire.send(&format!("client station {station}"))?;
        wire.send("keepalive enable")?;
        wire.send(&format!("client udpport {udp_port}"))?;
        for topic in ["slice", "pan", "tx", "meter", "radio", "daxiq", "dax", "atu"] {
            wire.send(&format!("sub {topic} all"))?;
        }

        // ── Bring up the data path ──────────────────────────────────────────
        // A DAX IQ stream is centred on a panadapter, so one has to exist and
        // be bound to the channel before the stream will carry anything useful.
        let pan_id = create_panadapter(&mut wire, &mut reader, &mut deferred, iq_rate_hz)?;
        wire.send(&format!("display pan set {pan_id} daxiq_channel={iq_channel}"))?;

        let create = wire.request(
            &mut reader,
            &mut deferred,
            &format!("stream create type=dax_iq daxiq_channel={iq_channel}"),
        )?;
        if create.code != 0 {
            return Err(err(format!(
                "the radio refused a DAX IQ stream (code 0x{:08X}{}). \
                 DAX IQ channel {iq_channel} may be in use by another client.",
                create.code,
                if create.body.is_empty() { String::new() } else { format!(": {}", create.body) }
            )));
        }
        let iq_stream = p::parse_hex_id(create.body.trim())
            .ok_or_else(|| err(format!("unparseable DAX IQ stream id {:?}", create.body)))?;
        trace.note(format!("dax_iq stream 0x{iq_stream:08X} channel {iq_channel}"));

        // The radio always creates the stream at 48 kHz and only moves to the
        // requested rate on this second command, so it is sent even when the
        // two agree — a no-op there, and the only way to 192 kHz otherwise.
        let rate = iq_rate_hz.round() as u32;
        wire.send(&format!("stream set {} daxiq_rate={rate}", p::hex_id(iq_stream)))?;

        // The `radio` status arrived in the burst that answered `client gui`, so
        // it is sitting in `deferred` rather than in `ident`. Read it here: the
        // model decides which frequency ranges the caller advertises (only some
        // FLEX models have a 2 m receiver), and it is what the UI labels the
        // connection with.
        ident.absorb_deferred(&deferred);

        // ── Rings and threads ───────────────────────────────────────────────
        // Half a second of IQ: deep enough to ride out a scheduling hiccup on
        // the reader, shallow enough that a stalled consumer shows up as a gap
        // rather than as half a minute of stale audio.
        let rx_cap = ((iq_rate_hz * 2.0 * 0.5) as usize).next_power_of_two().max(1 << 16);
        let (rx_prod, rx_cons) = RingBuffer::<f32>::new(rx_cap);
        let (tx_prod, tx_cons) = RingBuffer::<f32>::new(1 << 15);
        let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
        let (upd_tx, upd_rx) = crossbeam_channel::unbounded();
        let (telem_tx, telem_rx) = crossbeam_channel::unbounded();

        let alive = Arc::new(AtomicBool::new(true));
        let actual_rate = Arc::new(AtomicU32::new(rate));
        let meters = Arc::new(Mutex::new(MeterTable::default()));
        let tx_stream = Arc::new(AtomicU32::new(0));

        let radio_udp = SocketAddr::new(sockaddr.ip(), port);

        let data = DataThread {
            udp,
            radio_udp,
            radio_udp_confirmed: false,
            rx: rx_prod,
            tx: tx_cons,
            iq_stream,
            tx_stream: Arc::clone(&tx_stream),
            telem: telem_tx,
            meters: Arc::clone(&meters),
            trace: trace.clone(),
            actual_rate: Arc::clone(&actual_rate),
            alive: Arc::clone(&alive),
            last_count: HashMap::new(),
            tx_count: 0,
            keyed: false,
            tx_started: None,
            tx_frames: 0,
        };
        let control = ControlThread {
            wire,
            reader,
            deferred,
            ctrl: ctrl_rx,
            updates: upd_tx,
            meters,
            trace: trace.clone(),
            iq_stream,
            tx_stream,
            pan_id,
            center: 0.0,
            if_hz: 0.0,
            rx_if: 0.0,
            slice: None,
            mode: String::new(),
            keyed: false,
            cmd_at: None,
            rfpower: None,
            tunepower: None,
            alive: Arc::clone(&alive),
        };

        // Data first, so `Drop` joins it first: it exits when the control
        // thread's teardown clears `alive`, which is the order that lets the
        // radio's streams be released before either thread goes away.
        let joins = vec![
            spawn("sdroxide-flex-data", move || data.run())?,
            spawn("sdroxide-flex-ctl", move || control.run())?,
        ];

        Ok(FlexHandle {
            ctrl: ctrl_tx,
            rx: rx_cons,
            tx: tx_prod,
            updates: upd_rx,
            telem: telem_rx,
            joins,
            alive,
            actual_rate,
            ident,
            requested_rate_hz: iq_rate_hz,
            trace,
        })
    }

    /// Whether both threads are still running. They stop when the radio closes
    /// the TCP session or a socket read fails, after which only a fresh
    /// [`Self::connect`] revives the link.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The IQ rate the radio is actually delivering, from the packet class
    /// codes. Differs from the requested rate while a `daxiq_rate` change is
    /// settling — and permanently, if the radio refused it.
    pub fn actual_rate_hz(&self) -> f64 {
        self.actual_rate.load(Ordering::Relaxed) as f64
    }

    /// Move the IQ centre (the panadapter the DAX stream follows).
    pub fn set_center(&self, hz: f64) {
        let _ = self.ctrl.send(Ctrl::SetCenter(hz));
    }
    /// Keep the radio's own slice `hz` above the IQ centre, so its display and
    /// any second client track our dial.
    pub fn set_if_offset(&self, hz: f64) {
        let _ = self.ctrl.send(Ctrl::SetIf(hz));
    }
    pub fn set_mode(&self, mode: Mode) {
        let _ = self.ctrl.send(Ctrl::SetMode(mode));
    }
    /// Begin transmitting at `tx_freq_hz`. Returns the TX audio rate.
    pub fn tx_begin(&self, tx_freq_hz: f64) -> f64 {
        let _ = self.ctrl.send(Ctrl::TxOn(tx_freq_hz));
        p::TX_RATE_HZ as f64
    }
    pub fn tx_end(&self) {
        let _ = self.ctrl.send(Ctrl::TxOff);
    }
    /// RF power as a 0..1 fraction.
    pub fn set_drive(&self, frac: f64) {
        let _ = self.ctrl.send(Ctrl::SetDrive(frac));
    }
    /// TUNE power as a 0..1 fraction.
    pub fn set_tune_drive(&self, frac: f64) {
        let _ = self.ctrl.send(Ctrl::SetTuneDrive(frac));
    }
    /// Send an arbitrary command. Nothing waits for its reply.
    pub fn send_raw(&self, command: impl Into<String>) {
        let _ = self.ctrl.send(Ctrl::Raw(command.into()));
    }

    /// Push mono 24 kHz TX audio, with bounded back-pressure.
    pub fn tx_write(&mut self, audio: &[f32]) {
        for &v in audio {
            let mut val = v;
            let mut tries = 0u32;
            loop {
                match self.tx.push(val) {
                    Ok(()) => break,
                    Err(rtrb::PushError::Full(x)) => {
                        // The radio drains at 24 kHz; if it has stopped, give up
                        // rather than blocking the engine's TX thread forever.
                        if tries > 2000 {
                            return;
                        }
                        tries += 1;
                        val = x;
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
            }
        }
    }

    /// TX audio still queued for the radio, in samples.
    pub fn tx_pending(&self) -> usize {
        self.tx.buffer().capacity() - self.tx.slots()
    }

    /// Drain interleaved I,Q floats from the RX ring (always an even count).
    pub fn rx_read(&mut self, out: &mut [f32]) -> usize {
        let take = self.rx.slots().min(out.len()) & !1;
        let mut n = 0;
        while n < take {
            match self.rx.pop() {
                Ok(v) => {
                    out[n] = v;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        n
    }

    /// Drop whatever the network thread queued in the RX ring. The radio
    /// keeps streaming I/Q for the whole over, so `rx_read` would otherwise
    /// replay a stale backlog after `tx_end`.
    pub fn discard_pending_rx(&mut self) {
        while self.rx.pop().is_ok() {}
    }

    /// Drain changes the operator made on the radio.
    pub fn poll_updates(&self) -> Vec<FlexUpdate> {
        self.updates.try_iter().collect()
    }

    /// The most recent TX telemetry, or `None` if nothing new arrived.
    pub fn poll_telemetry(&self) -> Option<TxTelemetry> {
        self.telem.try_iter().last()
    }
}

impl Drop for FlexHandle {
    fn drop(&mut self) {
        // Only the shutdown message — *not* `alive = false`. Clearing the flag
        // here races the control thread, which checks it at the top of its loop:
        // lose that race and the thread returns before running `teardown`,
        // leaving the radio still streaming DAX IQ to a closed port and its DAX
        // channel claimed against the next session. The control thread clears
        // the flag itself once teardown has actually gone out, which is also
        // what stops the data thread.
        let _ = self.ctrl.send(Ctrl::Shutdown);
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, FlexError> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map_err(|e| err(format!("spawn {name}: {e}")))
}

// ─── Command plumbing ────────────────────────────────────────────────────────

/// A reply to one command.
#[derive(Debug, Clone)]
struct Reply {
    code: u32,
    body: String,
}

/// The write half of the control socket, with the command sequence counter.
struct Wire {
    out: TcpStream,
    seq: u32,
    trace: Trace,
}

impl Wire {
    /// Send a command without waiting for its reply.
    fn send(&mut self, command: &str) -> Result<u32, FlexError> {
        let seq = self.seq;
        self.seq += 1;
        let line = p::build_command(seq, command);
        self.trace.tx_line(&line);
        self.out.write_all(line.as_bytes()).map_err(|e| err(format!("write: {e}")))?;
        self.out.flush().ok();
        Ok(seq)
    }

    /// Send a command and read until its reply arrives, collecting every other
    /// line into `deferred` on the way past.
    ///
    /// Deferring rather than dropping is load-bearing. A radio answers `client
    /// gui` and each `sub` with a burst of status objects, and the very first of
    /// those is the `slice` status that tells us which slice we own. Discarding
    /// non-matching lines here left the session with no slice to tune — the
    /// panadapter followed the dial and the radio's own VFO never moved.
    ///
    /// Only used during the handshake, where each answer decides what to send
    /// next. Once the threads are running, replies arrive through the control
    /// thread's normal read loop — a blocking round trip per command there would
    /// serialise the whole session behind the network.
    fn request(
        &mut self,
        reader: &mut LineReader,
        deferred: &mut Vec<Line>,
        command: &str,
    ) -> Result<Reply, FlexError> {
        let seq = self.send(command)?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err(err(format!("no reply to {command:?} within {REPLY_TIMEOUT:?}")));
            }
            match reader.next_line()? {
                Some(Line::Response { seq: s, code, body }) if s == seq => {
                    return Ok(Reply { code, body });
                }
                Some(other) => deferred.push(other),
                None => {}
            }
        }
    }
}

/// Assembles `\n`-terminated control lines from a socket with a read timeout.
///
/// `BufReader::read_line` cannot be used here. On a timeout it returns an error
/// *after* having consumed and appended whatever bytes had arrived, so a line
/// split across the timeout boundary is silently lost — and the radio's status
/// burst, which is exactly the traffic that arrives in bursts, is what gets
/// lost. Owning the byte buffer means a partial line simply waits for its tail.
struct LineReader {
    sock: TcpStream,
    buf: Vec<u8>,
    /// Lines assembled but not yet handed out.
    ready: VecDeque<String>,
    trace: Trace,
}

/// Cap on the line-assembly buffer. Control lines are tens to hundreds of
/// bytes; a peer that dribbles bytes without ever sending `\n` would otherwise
/// grow this until the process runs out of memory.
const MAX_LINE_BUFFER: usize = 4 * 1024 * 1024;

impl LineReader {
    fn new(sock: TcpStream, trace: Trace) -> LineReader {
        LineReader { sock, buf: Vec::with_capacity(8192), ready: VecDeque::new(), trace }
    }

    /// The next complete line, or `None` if none has arrived yet.
    fn next_line(&mut self) -> Result<Option<Line>, FlexError> {
        if let Some(line) = self.ready.pop_front() {
            self.trace.rx_line(&line);
            return Ok(Some(p::parse_line(&line)));
        }

        let mut chunk = [0u8; 8192];
        match self.sock.read(&mut chunk) {
            Ok(0) => return Err(err("the radio closed the connection")),
            Ok(n) => {
                if self.buf.len() + n > MAX_LINE_BUFFER {
                    return Err(err("control line buffer overflowed — malformed peer"));
                }
                self.buf.extend_from_slice(&chunk[..n]);
                while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.buf.drain(..=nl).collect();
                    self.ready.push_back(String::from_utf8_lossy(&line).trim_end().to_string());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(err(format!("read: {e}"))),
        }

        match self.ready.pop_front() {
            Some(line) => {
                self.trace.rx_line(&line);
                Ok(Some(p::parse_line(&line)))
            }
            None => Ok(None),
        }
    }
}

/// Turn a `client gui` rejection into something an operator can act on.
///
/// The bare hex code is the single most common thing in a Flex bug report and
/// the single least useful, because the interesting failures all mean "somebody
/// else has this radio" in one way or another.
fn describe_gui_failure(code: u32, body: &str) -> String {
    let hint = match code {
        0xF300_0001 => {
            " — the radio already has a GUI client and multiFLEX is off. \
             Disconnect the other client, or enable multiFLEX on the radio."
        }
        0x5000_2C00..=0x5000_2CFF => " — the radio rejected this client's identity.",
        _ => "",
    };
    let detail = if body.trim().is_empty() { String::new() } else { format!(": {}", body.trim()) };
    format!("the radio refused GUI client registration (code 0x{code:08X}{detail}){hint}")
}

/// A stable per-station client id.
///
/// The radio remembers a GUI client by this id and restores its slices and
/// panadapters on reconnect, so it must not change between runs — but two
/// stations sharing one would collide on a multiFLEX radio. Deriving it from the
/// station name gives both properties without persisting anything.
fn gui_client_id(station: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in station.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // A UUID-shaped id, which is what the radio expects, seeded deterministically.
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (h >> 32) as u32,
        (h >> 16) as u16,
        h & 0xFFF,
        (h >> 12) & 0xFFF,
        h & 0xFFFF_FFFF_FFFF
    )
}

/// Create a panadapter for our IQ stream to follow, and set its span to the IQ
/// bandwidth so the radio's own display shows what we are actually receiving.
///
/// `display panafall create` makes a panadapter *and* its waterfall; the reply
/// body is `<pan id>,<waterfall id>`.
fn create_panadapter(
    wire: &mut Wire,
    reader: &mut LineReader,
    deferred: &mut Vec<Line>,
    iq_rate_hz: f64,
) -> Result<String, FlexError> {
    let reply = wire.request(reader, deferred, "display panafall create x=100 y=100")?;
    if reply.code != 0 {
        return Err(err(format!(
            "the radio refused a panadapter (code 0x{:08X}: {})",
            reply.code,
            reply.body.trim()
        )));
    }
    let pan = reply.body.trim().split(',').next().unwrap_or("").trim().to_string();
    if pan.is_empty() {
        return Err(err(format!("unparseable panafall create reply {:?}", reply.body)));
    }
    let pan_id = if pan.starts_with("0x") { pan } else { format!("0x{pan}") };
    // The radio speaks MHz here, as it does for every frequency in the protocol.
    wire.send(&format!("display pan set {pan_id} bandwidth={:.6}", iq_rate_hz / 1e6))?;
    Ok(pan_id)
}

// ─── Meters ──────────────────────────────────────────────────────────────────

/// Meter definitions, keyed by the numeric id the data packets use.
///
/// Ids are assigned per session and differ between firmware and radio families,
/// so a meter is identified by its `(source, name)` and the id is only ever a
/// lookup key learned from the definition status.
#[derive(Debug, Default)]
struct MeterTable {
    defs: HashMap<u16, MeterDef>,
}

#[derive(Debug, Clone, Default)]
struct MeterDef {
    name: String,
    unit: String,
}

impl MeterTable {
    /// Absorb a `meter` status line. Definitions arrive as `<id>.<field>=<value>`
    /// with `#` separators, several meters to a line.
    fn absorb(&mut self, kvs: &p::Kvs) {
        for (k, v) in kvs {
            let Some((id_s, field)) = k.split_once('.') else { continue };
            let Ok(id) = id_s.parse::<u16>() else { continue };
            let e = self.defs.entry(id).or_default();
            match field {
                "nam" => e.name = v.to_ascii_uppercase(),
                "unit" => e.unit = v.clone(),
                _ => {}
            }
        }
    }

    /// Scale a raw reading and return `(name, value)` when the meter is known.
    fn resolve(&self, m: p::RawMeter) -> Option<(&str, f32)> {
        let d = self.defs.get(&m.id)?;
        if d.name.is_empty() {
            return None;
        }
        Some((d.name.as_str(), p::scale_meter(m.raw, &d.unit)))
    }
}

// ─── Control thread ──────────────────────────────────────────────────────────

struct ControlThread {
    wire: Wire,
    reader: LineReader,
    /// Lines the connect handshake read past on its way to a reply.
    deferred: Vec<Line>,
    ctrl: Receiver<Ctrl>,
    updates: Sender<FlexUpdate>,
    meters: Arc<Mutex<MeterTable>>,
    trace: Trace,
    iq_stream: u32,
    /// The `dax_tx` stream, created lazily on the first key-down and shared with
    /// the data thread, which needs the id to address its audio packets.
    tx_stream: Arc<AtomicU32>,
    pan_id: String,
    center: f64,
    /// Where the radio's slice currently sits relative to the centre.
    if_hz: f64,
    /// The receive offset, restored when leaving TX.
    rx_if: f64,
    /// The slice index we drive, learned from the first slice status.
    slice: Option<u32>,
    mode: String,
    keyed: bool,
    /// When we last commanded a frequency. The radio echoes every change back,
    /// and during a waterfall drag those echoes are stale by the time they
    /// arrive — following them would fight the drag, so they are ignored for a
    /// moment after each of our own commands.
    cmd_at: Option<Instant>,
    rfpower: Option<u32>,
    tunepower: Option<u32>,
    alive: Arc<AtomicBool>,
}

/// How long an echoed frequency counts as ours rather than as an operator move.
const ECHO_WINDOW: Duration = Duration::from_millis(300);

impl ControlThread {
    fn run(mut self) {
        // Replay what the handshake read past — the slice status among it, which
        // is how we learn which slice is ours.
        for line in std::mem::take(&mut self.deferred) {
            self.on_line(line);
        }

        let mut last_ping = Instant::now();
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return;
            }
            let mut stop = false;
            while let Ok(msg) = self.ctrl.try_recv() {
                if self.handle_ctrl(msg) {
                    stop = true;
                    break;
                }
            }
            if stop {
                self.teardown();
                return;
            }

            match self.reader.next_line() {
                Ok(Some(line)) => self.on_line(line),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(target: "smartsdr", "control link lost: {e}");
                    self.trace.note(format!("control link lost: {e}"));
                    self.alive.store(false, Ordering::Relaxed);
                    return;
                }
            }

            if last_ping.elapsed() >= PING_INTERVAL {
                last_ping = Instant::now();
                if self.wire.send("ping").is_err() {
                    self.alive.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
    }

    /// Returns `true` when the thread should stop.
    fn handle_ctrl(&mut self, msg: Ctrl) -> bool {
        match msg {
            Ctrl::SetCenter(hz) => {
                self.center = hz;
                let _ = self.wire.send(&format!(
                    "display pan set {} center={:.6}",
                    self.pan_id,
                    hz / 1e6
                ));
                self.retune_slice(self.rx_if);
            }
            Ctrl::SetIf(hz) => {
                self.rx_if = hz;
                if !self.keyed && (hz - self.if_hz).abs() > 0.5 {
                    self.retune_slice(hz);
                }
            }
            Ctrl::SetMode(m) => {
                self.mode = mode_to_flex(m).to_string();
                if let Some(s) = self.slice {
                    let _ = self.wire.send(&format!("slice set {s} mode={}", self.mode));
                }
            }
            Ctrl::TxOn(tx_freq) => {
                self.retune_slice(tx_freq - self.center);
                if let Some(s) = self.slice {
                    // Nominate our slice as the transmitter's, or the radio
                    // keys on whichever slice it last had.
                    let _ = self.wire.send(&format!("slice set {s} tx=1"));
                }
                self.ensure_tx_stream();
                // Take the modulator input from DAX rather than the mic jacks.
                let _ = self.wire.send("transmit set dax=1");
                let _ = self.wire.send("xmit 1");
                self.keyed = true;
                self.trace.note(format!("TX on at {tx_freq:.0} Hz"));
            }
            Ctrl::TxOff => {
                let _ = self.wire.send("xmit 0");
                // Hand the modulator back to the mic, or the operator's next
                // local transmission is silent.
                let _ = self.wire.send("transmit set dax=0");
                self.keyed = false;
                self.retune_slice(self.rx_if);
                self.trace.note("TX off");
            }
            Ctrl::SetDrive(frac) => {
                let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as u32;
                self.rfpower = Some(pct);
                let _ = self.wire.send(&format!("transmit set rfpower={pct}"));
            }
            Ctrl::SetTuneDrive(frac) => {
                let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as u32;
                self.tunepower = Some(pct);
                let _ = self.wire.send(&format!("transmit set tunepower={pct}"));
            }
            Ctrl::Raw(cmd) => {
                let _ = self.wire.send(&cmd);
            }
            Ctrl::Shutdown => return true,
        }
        false
    }

    /// Put the radio's slice `off` Hz from the IQ centre.
    fn retune_slice(&mut self, off: f64) {
        self.if_hz = off;
        self.cmd_at = Some(Instant::now());
        if let Some(s) = self.slice {
            // `autopan=0`: the slice must not drag the panadapter with it, or
            // moving the dial inside the span would move the IQ centre too and
            // the offset would never converge.
            let _ = self
                .wire
                .send(&format!("slice tune {s} {:.6} autopan=0", (self.center + off) / 1e6));
        }
    }

    /// Create the DAX TX audio stream if we don't have one yet.
    fn ensure_tx_stream(&mut self) {
        if self.tx_stream.load(Ordering::Relaxed) != 0 {
            return;
        }
        // Fire-and-forget: the id arrives as a `stream` status, which the read
        // loop adopts. Blocking here would stall the key-down behind a round
        // trip, and the first packets would be dropped by the radio anyway
        // until it has the stream.
        let _ = self.wire.send("stream create type=dax_tx");
    }

    fn teardown(&mut self) {
        if self.keyed {
            let _ = self.wire.send("xmit 0");
            let _ = self.wire.send("transmit set dax=0");
        }
        let tx = self.tx_stream.load(Ordering::Relaxed);
        if tx != 0 {
            let _ = self.wire.send(&format!("stream remove {}", p::hex_id(tx)));
        }
        let _ = self.wire.send(&format!("stream remove {}", p::hex_id(self.iq_stream)));
        let _ = self.wire.send(&format!("display panafall remove {}", self.pan_id));
        // Give the radio a moment to act on the teardown before the socket
        // closes under it; a hard close leaves the streams orphaned and still
        // transmitting to a port nobody is listening on.
        std::thread::sleep(Duration::from_millis(120));
        self.alive.store(false, Ordering::Relaxed);
        self.trace.note("session closed");
    }

    fn on_line(&mut self, line: Line) {
        match line {
            Line::Status { object, kvs, .. } => self.on_status(&object, &kvs),
            Line::Message { severity, text } => {
                self.trace.note(format!("radio message [{severity:?}] {text}"));
                match severity {
                    crate::protocol::Severity::Error | crate::protocol::Severity::Fatal => {
                        tracing::warn!(target: "smartsdr", ?severity, %text, "radio message");
                    }
                    _ => tracing::debug!(target: "smartsdr", ?severity, %text, "radio message"),
                }
            }
            Line::Response { seq, code, body } if code != 0 => {
                self.trace.note(format!("command {seq} failed: 0x{code:08X} {body}"));
                tracing::warn!(
                    target: "smartsdr",
                    seq,
                    code = format!("0x{code:08X}"),
                    %body,
                    "command rejected"
                );
            }
            _ => {}
        }
    }

    fn on_status(&mut self, object: &str, kvs: &p::Kvs) {
        let mut words = object.split_whitespace();
        match words.next() {
            Some("slice") => {
                let Some(idx) = words.next().and_then(|s| s.parse::<u32>().ok()) else { return };
                // Adopt the first slice we see as ours: a fresh GUI session gets
                // one slice, and claiming it beats creating a second.
                if self.slice.is_none() {
                    self.slice = Some(idx);
                    self.trace.note(format!("adopted slice {idx}"));
                }
                if self.slice != Some(idx) {
                    return;
                }
                if let Some(f) = kvs.get("RF_frequency").and_then(|v| v.parse::<f64>().ok()) {
                    let hz = f * 1e6;
                    let ours = self.cmd_at.is_some_and(|t| t.elapsed() < ECHO_WINDOW);
                    // `center` is still 0 for the first status of a session, so
                    // this path also does the initial adoption: sdroxide opens
                    // on whatever the radio is already tuned to, rather than
                    // yanking a running station off frequency to whatever was
                    // in its own config.
                    let expected = self.center + self.if_hz;
                    if !ours && (hz - expected).abs() > 1.0 {
                        // The operator turned the radio's dial. Adopt it as our
                        // centre — the engine will re-derive its own offset.
                        self.center = hz;
                        self.if_hz = 0.0;
                        self.rx_if = 0.0;
                        let _ = self.updates.send(FlexUpdate::Freq(hz));
                    }
                }
                if let Some(m) = kvs.get("mode") {
                    let ml = m.to_ascii_uppercase();
                    if ml != self.mode
                        && let Some(mode) = flex_to_mode(&ml)
                    {
                        self.mode = ml;
                        let _ = self.updates.send(FlexUpdate::Mode(mode));
                    }
                }
            }
            Some("transmit") => {
                // Echoes of our own commands carry the value we already know,
                // so only a real change is passed on.
                if let Some(v) = kvs.get("rfpower").and_then(|v| v.parse::<u32>().ok())
                    && self.rfpower != Some(v)
                {
                    self.rfpower = Some(v);
                    let _ = self.updates.send(FlexUpdate::TxDrive(v as f32 / 100.0));
                }
                if let Some(v) = kvs.get("tunepower").and_then(|v| v.parse::<u32>().ok())
                    && self.tunepower != Some(v)
                {
                    self.tunepower = Some(v);
                    let _ = self.updates.send(FlexUpdate::TuneDrive(v as f32 / 100.0));
                }
            }
            Some("meter") => {
                self.meters.lock().unwrap_or_else(|e| e.into_inner()).absorb(kvs);
            }
            Some("stream") => {
                // `stream 0x…  type=dax_tx …` — adopt the TX stream id the
                // radio assigned to the create we fired off at key-down.
                let id = words.next().and_then(p::parse_hex_id);
                if let (Some(id), Some(ty)) = (id, kvs.get("type"))
                    && ty == "dax_tx"
                    && self.tx_stream.swap(id, Ordering::Relaxed) != id
                {
                    {
                        self.trace.note(format!("dax_tx stream 0x{id:08X}"));
                        // Claim the transmit direction; without this the radio
                        // accepts the packets and ignores them.
                        let _ = self.wire.send(&format!("stream set {} tx=1", p::hex_id(id)));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Map an sdroxide mode to the radio's spelling.
///
/// Only the radio's *filter and sideband* are being set here — the digital
/// modes are demodulated in sdroxide from the IQ, so every one of them asks the
/// radio for DIGU/DIGL, exactly as the TCI backend does. Nothing about the
/// radio's own decoding is engaged.
fn mode_to_flex(m: Mode) -> &'static str {
    match m {
        Mode::Lsb => "LSB",
        Mode::Cw => "CW",
        Mode::Am | Mode::Drm => "AM",
        Mode::Sam => "SAM",
        Mode::Dsb => "DSB",
        // The radio has one FM; wideband FM is ours to do from the IQ.
        Mode::Nfm | Mode::Wfm | Mode::Rifp | Mode::Packet => "FM",
        Mode::Digl => "DIGL",
        Mode::Digu
        | Mode::Ft8
        | Mode::Ft4
        | Mode::Ft2
        | Mode::Js8
        | Mode::Wspr
        | Mode::Psk
        | Mode::Rtty
        | Mode::Olivia
        | Mode::Thor
        | Mode::Fsq
        | Mode::Hell
        | Mode::PacketHf
        | Mode::Rade => "DIGU",
        Mode::Usb | Mode::Sstv | Mode::Wefax | Mode::RfPaint | Mode::Spec => "USB",
    }
}

/// Map the radio's mode spelling to ours, or `None` for one we don't model.
fn flex_to_mode(s: &str) -> Option<Mode> {
    Some(match s {
        "LSB" => Mode::Lsb,
        "USB" => Mode::Usb,
        "CW" | "CWL" | "CWU" => Mode::Cw,
        "AM" => Mode::Am,
        "SAM" => Mode::Sam,
        "FM" | "NFM" => Mode::Nfm,
        "DFM" => Mode::Wfm,
        "DSB" => Mode::Dsb,
        "DIGU" | "RTTY" => Mode::Digu,
        "DIGL" => Mode::Digl,
        _ => return None,
    })
}

// ─── Data thread ─────────────────────────────────────────────────────────────

struct DataThread {
    udp: UdpSocket,
    /// Where DAX TX audio goes.
    ///
    /// Starts as the radio's command port, which is where a FLEX listens — 4992
    /// is both its TCP and its UDP port. It is then corrected to wherever the
    /// radio's own IQ packets actually came from, which is the same place on
    /// real hardware but not behind a NAT, a port-forward, or a simulator that
    /// could not claim 4992. Sending TX audio to an assumed port that nothing is
    /// listening on fails completely silently: UDP reports nothing, the radio
    /// keys up, and the operator transmits dead air.
    radio_udp: SocketAddr,
    /// Whether [`Self::radio_udp`] has been confirmed against a received packet.
    radio_udp_confirmed: bool,
    rx: Producer<f32>,
    tx: Consumer<f32>,
    iq_stream: u32,
    tx_stream: Arc<AtomicU32>,
    telem: Sender<TxTelemetry>,
    meters: Arc<Mutex<MeterTable>>,
    trace: Trace,
    actual_rate: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
    /// Last VITA packet counter per stream, for loss detection.
    last_count: HashMap<u32, u8>,
    tx_count: u8,
    keyed: bool,
    /// When the current transmission started, and how many audio frames have
    /// gone out since — the clock TX packets are paced against.
    tx_started: Option<Instant>,
    tx_frames: u64,
}

/// How far ahead of the wall clock the radio's TX buffer is kept. Four packets
/// is ~21 ms: enough that a late block from the engine does not leave the radio
/// with nothing to play, short enough not to add audible latency.
const TX_LEAD_PACKETS: usize = 4;

/// Most TX packets one loop turn may emit, so a long stall produces a catch-up
/// rather than a burst the radio would only drop.
const TX_MAX_PACKETS_PER_TURN: u32 = 16;

impl DataThread {
    fn run(mut self) {
        // Comfortably over a 192 kHz IQ packet; the radio keeps them inside the
        // path MTU it was told about, but a jumbo-frame LAN can carry more.
        let mut buf = vec![0u8; 16 * 1024];
        let mut iq = Vec::with_capacity(4096);
        let mut raw_meters = Vec::with_capacity(64);
        let mut fwd_w = None;
        let mut swr = None;

        // The radio only starts streaming once it has seen traffic from our UDP
        // port on some paths (routed networks, and firmware that learns the
        // return path from the first datagram). One priming packet costs
        // nothing and removes a class of "connected but no spectrum" report.
        let _ = self.udp.send_to(&[0u8; 4], self.radio_udp);

        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return;
            }

            match self.udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let Some(h) = p::parse_vita_header(&buf[..n]) else { continue };
                    let lost = match self.last_count.insert(h.stream_id, h.count) {
                        Some(prev) => p::count_gap(prev, h.count),
                        None => 0,
                    };
                    self.trace.packet(h.stream_id, h.class_code, n, lost);
                    let payload = p::vita_payload(&buf[..n], &h);

                    if h.stream_id == self.iq_stream {
                        // Learn where the radio really is. Keyed on our own IQ
                        // stream id, so an unrelated datagram cannot redirect
                        // our transmit audio somewhere else.
                        if !self.radio_udp_confirmed {
                            self.radio_udp_confirmed = true;
                            if from != self.radio_udp {
                                self.trace.note(format!(
                                    "radio streams from {from}, not {} — TX audio follows",
                                    self.radio_udp
                                ));
                                tracing::debug!(
                                    target: "smartsdr::vita",
                                    %from,
                                    assumed = %self.radio_udp,
                                    "adopting the radio's actual UDP endpoint"
                                );
                                self.radio_udp = from;
                            }
                        }
                        if let Some(rate) = p::dax_iq_rate(h.class_code) {
                            self.actual_rate.store(rate, Ordering::Relaxed);
                        }
                        iq.clear();
                        p::decode_dax_iq(payload, &mut iq);
                        for &s in &iq {
                            // A full ring means the engine is not keeping up;
                            // dropping the newest sample keeps the I/Q phase
                            // aligned, which overwriting would not.
                            if self.rx.push(s).is_err() {
                                break;
                            }
                        }
                    } else if h.class_code == p::pcc::METER {
                        raw_meters.clear();
                        p::decode_meters(payload, &mut raw_meters);
                        let table = self.meters.lock().unwrap_or_else(|e| e.into_inner());
                        let mut changed = false;
                        for m in &raw_meters {
                            match table.resolve(*m) {
                                Some(("FWDPWR", v)) => {
                                    fwd_w = Some(v);
                                    changed = true;
                                }
                                // Below 1.0 is unphysical: a reading the radio
                                // hasn't computed yet, not a match.
                                Some(("SWR", v)) if (1.0..=100.0).contains(&v) => {
                                    swr = Some(v);
                                    changed = true;
                                }
                                _ => {}
                            }
                        }
                        drop(table);
                        if changed && self.keyed {
                            let _ =
                                self.telem.send(TxTelemetry { fwd_w, swr, alc: None, po: None });
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    tracing::warn!(target: "smartsdr", "VITA-49 socket error: {e}");
                    self.trace.note(format!("VITA-49 socket error: {e}"));
                    self.alive.store(false, Ordering::Relaxed);
                    return;
                }
            }

            // Drain whatever TX audio the engine has queued. `keyed` follows the
            // ring rather than the control thread's PTT state: audio in the ring
            // is the only thing that should produce packets, and it is the
            // engine that decides when there is any.
            let pending = self.tx.slots();
            if pending >= p::TX_FRAMES_PER_PACKET {
                if !self.keyed {
                    self.keyed = true;
                    self.tx_started = Some(Instant::now());
                    self.tx_frames = 0;
                    fwd_w = None;
                    swr = None;
                }
                self.pump_tx_audio();
            } else if self.keyed && pending == 0 {
                self.keyed = false;
                self.tx_started = None;
                // Clear the meter on unkey rather than leaving the last SWR
                // reading frozen on the display.
                let _ = self.telem.send(TxTelemetry::default());
            }
        }
    }

    /// Send however many TX packets real time has come due for.
    ///
    /// The pacing is wall-clock and not "one packet per loop", because the loop
    /// turns over at whatever rate RX packets arrive: at 24 kHz IQ that is only
    /// ~190 Hz, which is barely one packet's worth of audio per turn, and when
    /// the radio mutes its receiver on key-down it can drop to the socket
    /// timeout — 20 packets a second against 187 needed. Under-feeding the
    /// radio's buffer is not silence, it is a chopped transmission.
    ///
    /// [`TX_LEAD_PACKETS`] of standing lead absorbs a late block from the
    /// engine; without it, every scheduling hiccup would punch a hole in the
    /// audio. The engine hands us bursts faster than real time, so the ring —
    /// not the network — is what holds the surplus.
    fn pump_tx_audio(&mut self) {
        let Some(started) = self.tx_started else { return };
        let elapsed = started.elapsed().as_secs_f64();
        let due = (elapsed * p::TX_RATE_HZ as f64) as u64
            + (TX_LEAD_PACKETS * p::TX_FRAMES_PER_PACKET) as u64;
        // Bound the work per turn so a long stall cannot produce an unbounded
        // burst that the radio would only drop.
        let mut budget = TX_MAX_PACKETS_PER_TURN;
        while self.tx_frames + p::TX_FRAMES_PER_PACKET as u64 <= due
            && self.tx.slots() > 0
            && budget > 0
        {
            budget -= 1;
            if !self.send_tx_audio() {
                break;
            }
        }
    }

    /// Send one TX packet. Returns whether anything went out.
    fn send_tx_audio(&mut self) -> bool {
        let stream = self.tx_stream.load(Ordering::Relaxed);
        if stream == 0 {
            // The radio hasn't given us a TX stream yet. Hold the audio: the
            // engine's first burst arrives faster than real time, so a short
            // wait costs latency, not samples.
            return false;
        }
        let mut frame = [0f32; p::TX_FRAMES_PER_PACKET];
        let mut n = 0;
        while n < frame.len() {
            match self.tx.pop() {
                Ok(v) => {
                    frame[n] = v;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        if n == 0 {
            return false;
        }
        // Zero-pad a short final packet rather than sending a ragged one: the
        // radio plays whatever length it is given, and a stream of odd-sized
        // packets makes the frame accounting below drift against the clock.
        let pkt = p::build_dax_tx_audio(stream, self.tx_count, &frame);
        self.tx_count = (self.tx_count + 1) & 0x0F;
        self.tx_frames += p::TX_FRAMES_PER_PACKET as u64;
        if let Err(e) = self.udp.send_to(&pkt, self.radio_udp) {
            tracing::warn!(target: "smartsdr", "DAX TX send failed: {e}");
            return false;
        }
        true
    }
}

// ─── Address parsing ─────────────────────────────────────────────────────────

/// Split `host[:port]` into its parts, defaulting to the radio's port.
pub fn split_addr(address: &str) -> Result<(String, u16), FlexError> {
    let a = address.trim().trim_end_matches('/');
    if a.is_empty() {
        return Err(err("no radio address configured"));
    }
    match a.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse().map_err(|_| err(format!("invalid port in {address:?}")))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((a.to_string(), p::RADIO_PORT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_default_to_the_radio_port() {
        assert_eq!(split_addr("192.168.1.50").unwrap(), ("192.168.1.50".into(), 4992));
        assert_eq!(split_addr("192.168.1.50:5000").unwrap(), ("192.168.1.50".into(), 5000));
        assert_eq!(split_addr(" flex.local/ ").unwrap(), ("flex.local".into(), 4992));
        assert!(split_addr("").is_err());
        assert!(split_addr("host:notaport").is_err());
    }

    #[test]
    fn gui_client_ids_are_stable_and_distinct() {
        // Stable across runs: the radio restores a known client's slices.
        assert_eq!(gui_client_id("Shack"), gui_client_id("Shack"));
        // Distinct per station: two clients sharing one would collide on a
        // multiFLEX radio.
        assert_ne!(gui_client_id("Shack"), gui_client_id("Mobile"));
        let id = gui_client_id("Shack");
        assert_eq!(id.len(), 36);
        assert_eq!(id.split('-').count(), 5);
    }

    #[test]
    fn gui_rejection_explains_the_multiflex_case() {
        let m = describe_gui_failure(0xF300_0001, "");
        assert!(m.contains("multiFLEX"));
        assert!(m.contains("0xF3000001"));
        // An unknown code still reports the body rather than swallowing it.
        assert!(describe_gui_failure(0x1234, "some detail").contains("some detail"));
    }

    #[test]
    fn modes_round_trip_through_the_radio_spelling() {
        for m in [Mode::Lsb, Mode::Usb, Mode::Cw, Mode::Am, Mode::Sam, Mode::Dsb, Mode::Digu] {
            assert_eq!(flex_to_mode(mode_to_flex(m)), Some(m));
        }
        // FM collapses: the radio has one FM, we have two.
        assert_eq!(flex_to_mode(mode_to_flex(Mode::Nfm)), Some(Mode::Nfm));
        assert_eq!(mode_to_flex(Mode::Wfm), "FM");
        // Every sdroxide digital mode asks the radio only for a sideband —
        // the decoding is ours, from the IQ.
        assert_eq!(mode_to_flex(Mode::Ft8), "DIGU");
        assert_eq!(mode_to_flex(Mode::Rtty), "DIGU");
        assert_eq!(flex_to_mode("WHAT"), None);
    }

    #[test]
    fn every_mode_maps_to_something_the_radio_accepts() {
        // The match is exhaustive by construction; this guards the spellings
        // themselves, since an unknown one is silently ignored by the radio.
        const ACCEPTED: [&str; 8] = ["LSB", "USB", "CW", "AM", "SAM", "DSB", "FM", "DIGU"];
        for m in Mode::ALL {
            let s = mode_to_flex(m);
            assert!(
                ACCEPTED.contains(&s) || s == "DIGL",
                "mode {m:?} maps to unknown radio mode {s:?}"
            );
        }
    }

    #[test]
    fn meter_table_resolves_by_learned_id() {
        let mut t = MeterTable::default();
        t.absorb(&p::parse_kvs_with("12.src=TX-#12.num=0#12.nam=SWR#12.unit=SWR", '#'));
        t.absorb(&p::parse_kvs_with("13.src=TX-#13.nam=FWDPWR#13.unit=dBm", '#'));

        assert_eq!(t.resolve(p::RawMeter { id: 12, raw: 192 }), Some(("SWR", 1.5)));
        assert_eq!(t.resolve(p::RawMeter { id: 13, raw: 5120 }), Some(("FWDPWR", 40.0)));
        // An id we never saw a definition for is not guessed at.
        assert_eq!(t.resolve(p::RawMeter { id: 99, raw: 1 }), None);
    }
}
