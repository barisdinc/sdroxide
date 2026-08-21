//! flrig over XML-RPC — the other daemon, for a rig flrig already drives well.
//!
//! Like [`crate::rigctld`] this is not a radio but a program already driving
//! one, reached over a socket. The difference is what stands behind the
//! interface: flrig carries its own per-model driver for each transceiver, and
//! on a fair number of rigs its handling of the transmit power and the receive
//! bandwidth is the more faithful of the two daemons — which is precisely when
//! an operator reaches for this family. It also shares the rig: flrig's own
//! panel and every other program pointed at it stay live alongside this end.
//!
//! What is reached: the frequency, the mode (by the rig's own mode *names*,
//! learned from `rig.get_modes` at open), PTT both ways, the transmit power in
//! whole watts against the maximum the rig reports, the receive bandwidth
//! (`rig.set_bandwidth` snaps to the nearest filter the driver has), the
//! S-meter — which flrig hands over already in dBm — and the SWR and power-out
//! faces while transmitting. CW goes through flrig's `cwio` keyer, a DTR/RTS
//! line on a serial port configured *inside flrig*, not the rig's internal
//! keyer: until that port is set up there, `rig.cwio_text` keys nothing. What
//! is not there at all: no RIT/XIT clear (flrig's interface has only split),
//! and no antenna switching.
//!
//! # The correlation problem
//!
//! An XML-RPC `methodResponse` carries a value and nothing else — nothing on
//! the wire names the request it answers, so a pipelined `rig.get_vfo` and
//! `rig.get_smeter` are indistinguishable coming back. Hamlib's extended
//! protocol solved this with an echo header; XML-RPC has none to offer. What
//! it does guarantee is *order*: one response per request, on one connection,
//! in the order the requests went in. So this file keeps a queue of the
//! methods actually written — fed by [`Protocol::wrote`], never at
//! frame-generation time, because a generated frame is not a written one (the
//! driver computes mode frames purely for comparison and dedups others away)
//! — and reads each complete HTTP response against the head of that queue.
//! [`Protocol::link_opened`] drops the queue with the connection it describes.
//!
//! # One question at a time
//!
//! Order is only worth correlating on if every request is answered, and
//! flrig's server does not manage that. It is XmlRpc++ 0.8, one `select` loop
//! on one thread shared by every client, and it reads its socket with
//! `nbRead`, which drains *everything* waiting there into one buffer. It finds
//! the first request in that buffer, answers it, and then clears the buffer
//! whole (`_header = ""; _request = "";` in `writeResponse`). A second request
//! that arrived while the first was being served is therefore discarded — no
//! response, no fault, no error, no close — and a client correlating by order
//! spends the rest of the session reading every answer against the wrong
//! question: the dial takes the S-meter's number, the transmit read takes a
//! frequency and reports an over nobody keyed.
//!
//! Nothing makes that likelier than transmitting: `rig.set_ptt` sits in
//! flrig's server thread for up to a second waiting for the radio to confirm
//! the change, and everything sent meanwhile piles up to be thrown away.
//!
//! So this profile reports what it has outstanding ([`Protocol::in_flight`])
//! and the driver holds the next frame until the answer is in. Hamlib's flrig
//! backend arrived at the same place from the other direction — it flushes its
//! socket before every command, with the comment "appears we can lose sync if
//! we don't clear things out".
//!
//! # The wire
//!
//! Requests are `POST` with `Content-Type: text/xml`, HTTP/1.1 and no
//! `Connection: close` — flrig's embedded XmlRpc server keeps a 1.1 connection
//! open unless told otherwise, so the one long-lived stream the driver holds
//! is exactly what it expects. Responses are framed by `Content-Length`, which
//! flrig always sends.

use std::collections::VecDeque;

use crate::{CatUpdate, Protocol, interp};
use sdroxide_types::Mode;
use tracing::{debug, info, warn};

/// Watts assumed at full scale until `rig.get_maxpwr` has answered. The common
/// case, and wrong only for the moments before the open sequence's reply lands
/// — the same assumption the ASCII families make for good.
const DEFAULT_MAX_W: f32 = 100.0;

/// The lowest dial a reply to `rig.get_vfo` may claim and still be believed.
///
/// Not a limit on where a radio can tune: it is the line between a frequency
/// and a *meter reading*, which is what lands here when the correlation has
/// slipped — flrig's S-meter, SWR and power faces are all 0–100, and 7 on the
/// dial is what an operator sees. No transceiver flrig drives, nor any general
/// coverage receiver in its list, tunes below a kilohertz, so nothing real is
/// refused by this and everything below it is proof the answers have lost
/// their place. Zero is not: a VFO flrig has not read yet reads back as one.
const MIN_DIAL_HZ: f64 = 1000.0;

/// The largest number a `rig.get_ptt` reply may carry and still be read as a
/// transmit state rather than as somebody else's answer. flrig sends 0 or 1;
/// the room above that is for a driver with its own idea of "keyed", which
/// costs one reading if it is wrong. A frequency is five figures and up.
const MAX_PTT_STATE: i64 = 9;

/// flrig's SWR face, raw 0–100 → ratio. The face is drawn with marks at 1,
/// 1.5, 2, 3 and >5, evenly spaced — but what a given transceiver driver puts
/// on it is that driver's business, so this table is APPROXIMATE and worth
/// checking against a live rig before trusting the protection trip to it.
/// How many connections in a row may end in a lost step before the driver
/// stops remaking the link over it. See [`Flrig::lose_step`].
const GIVE_UP_AFTER: u8 = 3;

const SWR_CAL: [(f32, f32); 5] = [(0.0, 1.0), (25.0, 1.5), (50.0, 2.0), (75.0, 3.0), (100.0, 5.0)];

/// One request written and not yet answered — the kinds whose replies carry a
/// value this file interprets. Everything else (the setters, the cwio calls)
/// falls to `Other`: XML-RPC answers those too, and the reply still has to be
/// popped, but only a fault in it means anything.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sent {
    GetVfo,
    GetMode,
    GetModes,
    GetXcvr,
    GetMaxPwr,
    GetPower,
    GetDbm,
    GetSwr,
    GetPo,
    GetPtt,
    Other,
}

/// A request's name, for the log.
fn label(s: Sent) -> &'static str {
    match s {
        Sent::GetVfo => "frequency read",
        Sent::GetMode => "mode read",
        Sent::GetModes => "mode-list read",
        Sent::GetXcvr => "transceiver read",
        Sent::GetMaxPwr => "power-scale read",
        Sent::GetPower => "power read",
        Sent::GetDbm => "S-meter read",
        Sent::GetSwr => "SWR read",
        Sent::GetPo => "power-out read",
        Sent::GetPtt => "PTT read",
        Sent::Other => "command",
    }
}

/// One XML-RPC parameter, as the three types flrig's methods take.
enum Param<'a> {
    Int(i64),
    Double(f64),
    Str(&'a str),
}

pub struct Flrig {
    /// For the HTTP `Host:` header only — the connection itself is the
    /// driver's [`crate::Link`].
    host: String,
    /// Raw bytes of the response stream, framed by HTTP headers.
    buf: Vec<u8>,
    /// The methods written and not yet answered, in write order — the whole of
    /// the correlation story. Fed by [`Protocol::wrote`], drained one per
    /// complete response.
    pending: VecDeque<Sent>,
    /// The mode names the rig behind flrig actually has (`rig.get_modes`),
    /// uppercased. Decides which candidate name a mode goes out as; empty
    /// until the open sequence's answer lands, where the first candidate
    /// serves.
    modes: Vec<String>,
    /// Watts at the top of the rig's scale (`rig.get_maxpwr`), or the
    /// assumption until it answers.
    max_w: f32,
    /// The transceiver flrig says it is driving (`rig.get_xcvr`), once known —
    /// for the log, and for [`Protocol::mode_moves_dial`]: an Elecraft shifts
    /// its dial by the CW pitch on mode changes behind flrig exactly as on a
    /// direct link.
    xcvr: Option<String>,
    /// A fault or an HTTP error arrived since last asked — see
    /// [`Protocol::refused`].
    failed: bool,
    /// An answer arrived that cannot belong to the request it was read against
    /// — see [`Protocol::desynced`]. The driver holds one question at a time
    /// precisely so this cannot happen; it is here because the cost of being
    /// wrong about that is silent and lasts the whole session, and one
    /// reconnect is the whole of the cure.
    lost_step: bool,
    /// How many connections in a row have ended in one, counted across
    /// reconnects on purpose — see [`Flrig::lose_step`]. A cure that has been
    /// tried this often and not worked is not the cure.
    slips_in_a_row: u8,
}

impl Flrig {
    pub fn new(host: String) -> Self {
        Flrig {
            host,
            buf: Vec::new(),
            pending: VecDeque::new(),
            modes: Vec::new(),
            max_w: DEFAULT_MAX_W,
            xcvr: None,
            failed: false,
            lost_step: false,
            slips_in_a_row: 0,
        }
    }

    /// One complete HTTP request carrying one XML-RPC call.
    fn call(&self, method: &str, params: &[Param]) -> Vec<u8> {
        let mut body = String::with_capacity(160);
        body.push_str("<?xml version=\"1.0\"?><methodCall><methodName>");
        body.push_str(method);
        body.push_str("</methodName><params>");
        for p in params {
            body.push_str("<param><value>");
            match p {
                Param::Int(v) => body.push_str(&format!("<i4>{v}</i4>")),
                Param::Double(v) => body.push_str(&format!("<double>{v}</double>")),
                Param::Str(s) => {
                    body.push_str("<string>");
                    body.push_str(&xml_escape(s));
                    body.push_str("</string>");
                }
            }
            body.push_str("</value></param>");
        }
        body.push_str("</params></methodCall>");
        format!(
            "POST /RPC2 HTTP/1.1\r\nHost: {}\r\nUser-Agent: sdroxide\r\n\
             Content-Type: text/xml\r\nContent-Length: {}\r\n\r\n{}",
            self.host,
            body.len(),
            body
        )
        .into_bytes()
    }

    /// Interpret one response body against the request at the head of the
    /// queue.
    fn interpret(&mut self, sent: Sent, body: &str, out: &mut Vec<CatUpdate>) {
        let doc = match roxmltree::Document::parse(body) {
            Ok(d) => d,
            Err(e) => {
                // flrig emits clean, tiny documents; anything it would not
                // emit is safest read as nothing.
                debug!("flrig: unparseable reply to a {}: {e}", label(sent));
                return;
            }
        };
        if doc.descendants().any(|n| n.has_tag_name("fault")) {
            debug!("flrig: a {} answered a fault", label(sent));
            self.failed = true;
            return;
        }
        match sent {
            Sent::GetVfo => {
                // flrig carries the frequency as a *string* of hertz — always
                // a number, so anything else here is somebody else's answer.
                let raw = scalar(&doc).unwrap_or_default();
                match raw.parse::<f64>() {
                    Ok(hz) if hz.is_finite() && hz >= MIN_DIAL_HZ => {
                        // A frequency where a frequency was asked for is the
                        // one positive proof the two sides are in step.
                        self.slips_in_a_row = 0;
                        out.push(CatUpdate::Freq(hz.round()));
                    }
                    // A VFO flrig has not read yet reads back as zero: nothing
                    // to report, and nothing wrong. A *negative* one is not
                    // that — it is the S-meter's dBm in the dial's place.
                    Ok(0.0) => {}
                    // An empty value is flrig's way of having nothing to say —
                    // it is what every setter answers — and is not evidence of
                    // anything. Only a value that is a real answer to some
                    // *other* question is.
                    _ if raw.is_empty() => {}
                    _ => self.lose_step(&format!("a frequency read was answered {raw:?}")),
                }
            }
            Sent::GetMode => {
                if let Some(m) = scalar(&doc).as_deref().and_then(mode_from_name) {
                    out.push(CatUpdate::Mode(m));
                }
            }
            Sent::GetModes => {
                let mut names = strings(&doc);
                // Some vintages hand the list back as one `|`-separated
                // string rather than an array; both spellings are taken.
                if names.is_empty()
                    && let Some(s) = scalar(&doc)
                {
                    names = s.split('|').map(str::to_string).collect();
                }
                self.modes = names
                    .into_iter()
                    .map(|s| s.trim().to_ascii_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            Sent::GetXcvr => {
                if let Some(name) = scalar(&doc).filter(|s| !s.is_empty())
                    && self.xcvr.as_deref() != Some(name.as_str())
                {
                    info!("flrig is driving a {name}");
                    self.xcvr = Some(name);
                }
            }
            Sent::GetMaxPwr => {
                if let Some(w) = scalar(&doc).and_then(|v| v.parse::<f32>().ok())
                    && w > 0.0
                {
                    self.max_w = w;
                }
            }
            Sent::GetPower => {
                if let Some(w) = scalar(&doc).and_then(|v| v.parse::<f32>().ok()) {
                    out.push(CatUpdate::Power((w / self.max_w).clamp(0.0, 1.0)));
                }
            }
            Sent::GetDbm => {
                // Already decibel-milliwatts — the one meter flrig converts
                // itself. The window guards against a driver with no S-meter
                // answering something that is not a signal.
                if let Some(dbm) = scalar(&doc).and_then(|v| v.parse::<f32>().ok())
                    && (-200.0..=0.0).contains(&dbm)
                {
                    out.push(CatUpdate::Signal(dbm));
                }
            }
            Sent::GetSwr => {
                // The raw 0–100 face. Zero is a driver with nothing behind
                // the meter, not an SWR any antenna has ever had.
                if let Some(raw) = scalar(&doc).and_then(|v| v.parse::<f32>().ok())
                    && raw > 0.0
                {
                    out.push(CatUpdate::Swr(interp(&SWR_CAL, raw)));
                }
            }
            Sent::GetPo => {
                // What the driver's `get_power_out` reads — watts on most of
                // them, scaled here against the rig's own maximum.
                if let Some(w) = scalar(&doc).and_then(|v| v.parse::<f32>().ok())
                    && w >= 0.0
                {
                    out.push(CatUpdate::Po((w / self.max_w).clamp(0.0, 1.0)));
                }
            }
            Sent::GetPtt => {
                // Keyed or not, and every non-zero counts as keyed — the same
                // reading every other profile here takes of a transmit state.
                // What is refused is only what no transmit state could be: a
                // *frequency*, which parses as a perfectly good non-zero and
                // would otherwise be an over nobody keyed, blanking the S-meter
                // and refusing the operator's own next one. An odd small number
                // from some driver costs one reading; five figures is proof the
                // answers have slipped.
                let raw = scalar(&doc).unwrap_or_default();
                match raw.parse::<i64>() {
                    Ok(n) if n.abs() <= MAX_PTT_STATE => out.push(CatUpdate::Ptt(n != 0)),
                    // Nothing said is not something said wrong — see the
                    // frequency read above.
                    _ if raw.is_empty() => {}
                    _ => self.lose_step(&format!("a transmit read was answered {raw:?}")),
                }
            }
            Sent::Other => {}
        }
    }

    /// Note that an answer cannot belong to the request it was read against,
    /// and say so — once per connection, because what follows is a reconnect
    /// and this line is the only record of why.
    fn lose_step(&mut self, why: &str) {
        if !self.lost_step {
            if self.slips_in_a_row < GIVE_UP_AFTER {
                warn!(
                    "flrig and this end have lost step ({why}); remaking the link, because until \
                     it is remade every answer belongs to some other question"
                );
            } else {
                // Remaking the link cures a slip — one request lost, after
                // which the order is wrong for good. It cannot cure an answer
                // flrig gives every time, and a session that spends itself
                // reconnecting into the same reply is worse than one that
                // simply refuses that reply: the refusing is what keeps the
                // meter off the dial, and it goes on working.
                warn!(
                    "flrig has answered this way on {} connections in a row ({why}); leaving the \
                     link alone and refusing the answer instead",
                    self.slips_in_a_row
                );
            }
        }
        self.lost_step = true;
    }
}

/// XML character escaping for the three characters that matter in content.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// The first scalar value in a response — the typed child of the first
/// `<value>`, or its bare text where flrig sent it untyped (both are legal
/// XML-RPC and flrig uses both).
fn scalar(doc: &roxmltree::Document) -> Option<String> {
    let value = doc.descendants().find(|n| n.has_tag_name("value"))?;
    let text = match value.children().find(|c| c.is_element()) {
        Some(typed) => typed.text().unwrap_or(""),
        None => value.text().unwrap_or(""),
    };
    Some(text.trim().to_string())
}

/// Every string in an array response — the `<value>`s under `<data>`.
fn strings(doc: &roxmltree::Document) -> Vec<String> {
    doc.descendants()
        .filter(|n| n.has_tag_name("data"))
        .flat_map(|d| d.descendants().filter(|n| n.has_tag_name("value")))
        .map(|v| {
            match v.children().find(|c| c.is_element()) {
                Some(typed) => typed.text().unwrap_or(""),
                None => v.text().unwrap_or(""),
            }
            .trim()
            .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// The method name inside a frame this file built — what feeds the queue.
fn method_of(frame: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(frame).ok()?;
    let start = s.find("<methodName>")? + "<methodName>".len();
    let end = s[start..].find("</methodName>")? + start;
    Some(&s[start..end])
}

/// The names a mode may go out as, most specific first. flrig's mode names are
/// the *rig's* — an Elecraft says `DATA`, an Icom `USB-D`, a Yaesu `DATA-U` —
/// so each app mode carries the spellings seen across flrig's drivers and the
/// first one the rig actually has wins. Plain `USB`/`LSB` close the data lists
/// so a digital over never fails to go out for want of a data position.
fn candidates(m: Mode) -> &'static [&'static str] {
    match m {
        Mode::Lsb => &["LSB"],
        Mode::Usb | Mode::Spec | Mode::Sstv | Mode::Wefax | Mode::RfPaint => &["USB"],
        Mode::Cw => &["CW"],
        Mode::Am | Mode::Sam => &["AM"],
        Mode::Dsb => &["DSB"],
        Mode::Nfm => &["FM", "FM-N", "NFM"],
        // No plain-FM fallback here, unlike the data lists below: a rig with
        // no WFM position would report `FM` back, which reads as NFM and would
        // bounce the app out of a mode it is demodulating itself. Nothing
        // about listening needs the rig moved, so nothing goes out instead.
        Mode::Wfm => &["WFM", "FM-W"],
        // Data over FM rather than over a sideband: the carrier is the
        // signal's centre, not one edge of it.
        Mode::Rifp | Mode::Packet => &["PKT-FM", "PKTFM", "DATA-FM", "FM-D", "FM"],
        Mode::Digl => &["DATA-R", "DATA-L", "DIGL", "PKT-LSB", "PKTLSB", "PKT-L", "LSB-D", "LSB"],
        Mode::Digu
        | Mode::Ft8
        | Mode::Js8
        | Mode::Wspr
        | Mode::Ft4
        | Mode::Ft2
        | Mode::Psk
        | Mode::Rtty
        | Mode::Olivia
        | Mode::Thor
        | Mode::Fsq
        | Mode::Hell
        | Mode::PacketHf
        | Mode::Rade => {
            &["DATA", "DATA-U", "DIGU", "PKT-USB", "PKTUSB", "PKT-U", "USB-D", "DIG", "PKT", "USB"]
        }
    }
}

/// A mode name flrig reported → the app's mode.
///
/// Deliberately not the inverse of [`candidates`] over its whole range, on the
/// principle every profile here follows: a rig position that would be
/// commanded back as something *else* yields `None`, or the two sides spend
/// the session correcting each other. `RTTY`/`FSK` because sdroxide's RTTY is
/// its own modem in a data sideband; `AMS` because synchronous AM is this
/// side's own detector on the rig's plain AM; the FM-data positions because
/// two app modes (RIFP, packet) command them and neither can claim the read.
fn mode_from_name(s: &str) -> Option<Mode> {
    Some(match s.trim().to_ascii_uppercase().as_str() {
        "LSB" => Mode::Lsb,
        "USB" => Mode::Usb,
        // CW and its reverse are both CW to the app.
        "CW" | "CW-R" | "CWR" | "CW-L" | "CW-U" => Mode::Cw,
        "AM" | "AM-N" | "AMN" => Mode::Am,
        "DSB" => Mode::Dsb,
        "FM" | "FM-N" | "FMN" | "NFM" => Mode::Nfm,
        "WFM" | "FM-W" | "FMW" => Mode::Wfm,
        "DATA" | "DATA-U" | "DIGU" | "PKT-USB" | "PKTUSB" | "PKT-U" | "USB-D" | "DIG" | "PKT" => {
            Mode::Digu
        }
        "DATA-R" | "DATA-L" | "DIGL" | "PKT-LSB" | "PKTLSB" | "PKT-L" | "LSB-D" => Mode::Digl,
        _ => return None,
    })
}

impl Protocol for Flrig {
    fn set_freq(&mut self, hz: f64) -> Vec<u8> {
        self.call("rig.set_vfo", &[Param::Double(hz.round().max(0.0))])
    }

    fn set_mode(&mut self, m: Mode) -> Vec<u8> {
        let cands = candidates(m);
        let name = if self.modes.is_empty() {
            // The rig's list has not answered yet; the most specific spelling
            // is the best guess there is.
            Some(cands[0])
        } else {
            cands.iter().copied().find(|c| self.modes.iter().any(|have| have == c))
        };
        match name {
            Some(n) => self.call("rig.set_mode", &[Param::Str(n)]),
            // No position for this mode on this rig. An empty frame writes
            // nothing — leaving the radio where the operator put it, which is
            // a better answer than a guess.
            None => Vec::new(),
        }
    }

    fn ptt(&self, on: bool) -> Vec<u8> {
        self.call("rig.set_ptt", &[Param::Int(i64::from(on))])
    }

    fn poll_requests(&self) -> Vec<Vec<u8>> {
        vec![self.call("rig.get_vfo", &[]), self.call("rig.get_mode", &[])]
    }
    fn dial_requests(&self) -> Vec<Vec<u8>> {
        vec![self.call("rig.get_vfo", &[])]
    }

    fn tx_telemetry_requests(&self) -> Vec<Vec<u8>> {
        vec![self.call("rig.get_swrmeter", &[]), self.call("rig.get_pwrmeter", &[])]
    }

    fn rx_telemetry_requests(&self) -> Vec<Vec<u8>> {
        vec![self.call("rig.get_DBM", &[])]
    }

    fn tx_state_requests(&self) -> Vec<Vec<u8>> {
        vec![self.call("rig.get_ptt", &[])]
    }

    /// What the rig is, what it can do, what its modes are called — the
    /// answers everything after them is interpreted against, which is why they
    /// go out first.
    fn open_requests(&self) -> Vec<Vec<u8>> {
        vec![
            self.call("rig.get_xcvr", &[]),
            self.call("rig.get_maxpwr", &[]),
            self.call("rig.get_modes", &[]),
        ]
    }

    fn clear_offsets(&self) -> Vec<Vec<u8>> {
        // Split is the one offset flrig's interface can switch off. RIT and
        // XIT have no method at all — a rig left with RIT on offsets the dial
        // unseen, which the native profiles clear and this one cannot.
        vec![self.call("rig.set_split", &[Param::Int(0)])]
    }

    /// flrig's cwio queue takes text freely; the Elecraft-sized chunk keeps an
    /// abort responsive rather than fitting any buffer.
    fn cw_chunk_len(&self) -> usize {
        24
    }
    fn send_cw(&mut self, text: &str) -> Vec<Vec<u8>> {
        // `cwio_text` only queues; `cwio_send 1` is what starts the keying —
        // both verified against flrig's server source. Repeating the start on
        // a keyer already sending is a no-op there.
        vec![
            self.call("rig.cwio_text", &[Param::Str(text)]),
            self.call("rig.cwio_send", &[Param::Int(1)]),
        ]
    }
    fn abort_cw(&mut self) -> Vec<Vec<u8>> {
        vec![self.call("rig.cwio_send", &[Param::Int(0)])]
    }
    fn set_cw_wpm(&mut self, wpm: f32) -> Vec<Vec<u8>> {
        vec![self.call("rig.cwio_set_wpm", &[Param::Int(wpm.round().max(1.0) as i64)])]
    }

    fn set_filter(&mut self, _mode: Mode, lo_hz: f32, hi_hz: f32) -> Vec<Vec<u8>> {
        // flrig takes a width and snaps to the nearest bandwidth its driver
        // has — the per-model filter table this profile does not carry, kept
        // on flrig's side of the socket.
        let width = (hi_hz - lo_hz).abs().round().max(50.0) as i64;
        vec![self.call("rig.set_bandwidth", &[Param::Int(width)])]
    }
    fn commands_filter(&self) -> bool {
        true
    }

    fn set_power(&mut self, frac: f32) -> Vec<Vec<u8>> {
        // Whole watts — flrig's power is an integer, so the finest step this
        // family has is 1 W and nothing below it can be asked for. A KX3's
        // 0.1 W settings are out of reach here; the native Elecraft profile
        // has them.
        let w = (frac.clamp(0.0, 1.0) * self.max_w).round().max(1.0) as i64;
        vec![self.call("rig.set_power", &[Param::Int(w)])]
    }
    fn read_power(&self) -> Vec<Vec<u8>> {
        vec![self.call("rig.get_power", &[])]
    }
    fn commands_power(&self) -> bool {
        true
    }

    fn mode_moves_dial(&self) -> bool {
        // An Elecraft set to shift its VFO with the mode does so behind flrig
        // exactly as on a direct link. Which rig this is arrives with the open
        // sequence's `rig.get_xcvr`, so the answer sharpens once it lands.
        self.xcvr.as_deref().is_some_and(|x| {
            let x = x.trim().to_ascii_uppercase();
            ["K2", "K3", "K4", "KX"].iter().any(|p| x.starts_with(p))
        })
    }

    fn refused(&mut self) -> bool {
        std::mem::take(&mut self.failed)
    }

    /// The requests written and not yet answered — see the module note on why
    /// this profile is never allowed more than one.
    fn in_flight(&self) -> usize {
        self.pending.len()
    }

    fn desynced(&mut self) -> bool {
        if !std::mem::take(&mut self.lost_step) {
            return false;
        }
        self.slips_in_a_row = self.slips_in_a_row.saturating_add(1);
        self.slips_in_a_row <= GIVE_UP_AFTER
    }

    fn parse(&mut self, buf: &mut Vec<u8>) -> Vec<CatUpdate> {
        self.buf.extend_from_slice(buf);
        buf.clear();
        let mut out = Vec::new();
        loop {
            // Frame by the HTTP head: everything up to the blank line, then
            // exactly `Content-Length` bytes of body.
            let Some(head_end) = find(&self.buf, b"\r\n\r\n") else { break };
            let head = String::from_utf8_lossy(&self.buf[..head_end]).into_owned();
            let (status, content_length) = parse_head(&head);
            let Some(len) = content_length else {
                // A head this file cannot frame by is a stream it cannot stay
                // synchronised with. Declare the desync: everything held about
                // bytes in flight describes nothing now, and the reconnect (or
                // the next well-formed response) starts clean.
                warn!("flrig: response without a Content-Length; resynchronising");
                self.buf.clear();
                self.pending.clear();
                break;
            };
            let total = head_end + 4 + len;
            if self.buf.len() < total {
                break;
            }
            let body = String::from_utf8_lossy(&self.buf[head_end + 4..total]).into_owned();
            self.buf.drain(..total);
            let Some(sent) = self.pending.pop_front() else {
                // A response nobody is waiting for: one answer too many, which
                // puts every later one a place ahead of its question exactly
                // as a lost request puts them a place behind.
                self.lose_step("an answer arrived for a question nobody asked");
                continue;
            };
            if status != Some(200) {
                debug!("flrig: HTTP {status:?} answering a {}", label(sent));
                self.failed = true;
                continue;
            }
            self.interpret(sent, &body, &mut out);
        }
        out
    }

    fn wrote(&mut self, frame: &[u8]) {
        let Some(method) = method_of(frame) else { return };
        self.pending.push_back(match method {
            "rig.get_vfo" => Sent::GetVfo,
            "rig.get_mode" => Sent::GetMode,
            "rig.get_modes" => Sent::GetModes,
            "rig.get_xcvr" => Sent::GetXcvr,
            "rig.get_maxpwr" => Sent::GetMaxPwr,
            "rig.get_power" => Sent::GetPower,
            "rig.get_DBM" => Sent::GetDbm,
            "rig.get_swrmeter" => Sent::GetSwr,
            "rig.get_pwrmeter" => Sent::GetPo,
            "rig.get_ptt" => Sent::GetPtt,
            _ => Sent::Other,
        });
    }

    fn link_opened(&mut self) {
        // The queue and the half-read reply describe a connection that no
        // longer exists. What the rig *is* — its model, its scale, its mode
        // names — is still true and stays.
        self.buf.clear();
        self.pending.clear();
        self.failed = false;
        self.lost_step = false;
    }
}

/// First position of `needle` in `haystack`, byte-wise.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The status code and `Content-Length` of an HTTP response head.
fn parse_head(head: &str) -> (Option<u16>, Option<usize>) {
    let mut lines = head.lines();
    let status = lines
        .next()
        .filter(|l| l.starts_with("HTTP/"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok());
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().ok();
        }
    }
    (status, content_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flrig() -> Flrig {
        Flrig::new("127.0.0.1:12345".into())
    }

    /// Wrap `body` in the HTTP response flrig would send it in and feed it to
    /// the parser.
    fn respond(f: &mut Flrig, body: &str) -> Vec<CatUpdate> {
        respond_status(f, 200, body)
    }

    fn respond_status(f: &mut Flrig, status: u16, body: &str) -> Vec<CatUpdate> {
        let mut buf = format!(
            "HTTP/1.1 {status} OK\r\nServer: XMLRPC++ 0.8\r\nContent-Type: text/xml\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        f.parse(&mut buf)
    }

    /// An ordinary single-value `methodResponse` around `value`, which
    /// includes its own type tag (or none — both are legal).
    fn ok(value: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><methodResponse><params><param><value>{value}</value>\
             </param></params></methodResponse>"
        )
    }

    fn text(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    /// Build a request for `method` and record it as written, the way the
    /// driver's `write_frame` would.
    fn ask(f: &mut Flrig, method: &str) {
        let frame = f.call(method, &[]);
        f.wrote(&frame);
    }

    #[test]
    fn requests_are_http_posts_that_keep_the_connection_open() {
        let mut f = flrig();
        let frame = text(f.set_freq(14_074_000.0));
        let (head, body) = frame.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /RPC2 HTTP/1.1\r\n"));
        assert!(head.contains("Host: 127.0.0.1:12345"));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        // No `Connection: close`: flrig keeps a 1.1 connection open unless
        // told otherwise, and the one long-lived stream depends on that.
        assert!(!head.contains("Connection"));
        assert!(body.contains("<methodName>rig.set_vfo</methodName>"));
        assert!(body.contains("<double>14074000</double>"));
    }

    #[test]
    fn a_reply_is_read_against_the_request_at_the_head_of_the_queue() {
        let mut f = flrig();
        // The poll writes two requests; nothing in either reply names them.
        for req in f.poll_requests() {
            f.wrote(&req);
        }
        assert_eq!(respond(&mut f, &ok("14074000")), vec![CatUpdate::Freq(14_074_000.0)]);
        assert_eq!(respond(&mut f, &ok("<string>USB</string>")), vec![CatUpdate::Mode(Mode::Usb)]);
    }

    /// The regression the write-fed queue exists for. The driver computes mode
    /// frames purely to compare against [`crate::ModeMemory`] and dedups
    /// others away entirely — a queue fed at generation time would count those
    /// never-written frames and read every later reply one slot off.
    #[test]
    fn a_generated_but_unwritten_frame_does_not_shift_the_queue() {
        let mut f = flrig();
        // Generated for comparison, never written — so never `wrote`.
        let _ = f.set_mode(Mode::Cw);
        ask(&mut f, "rig.get_vfo");
        assert_eq!(respond(&mut f, &ok("7030000")), vec![CatUpdate::Freq(7_030_000.0)]);
    }

    #[test]
    fn replies_split_across_reads_are_reassembled() {
        let mut f = flrig();
        ask(&mut f, "rig.get_vfo");
        let body = ok("7030000");
        let full = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let bytes = full.as_bytes();
        // In three arbitrary pieces: mid-head, mid-body, the rest.
        let mut part = bytes[..9].to_vec();
        assert!(f.parse(&mut part).is_empty());
        let mut part = bytes[9..60].to_vec();
        assert!(f.parse(&mut part).is_empty());
        let mut part = bytes[60..].to_vec();
        assert_eq!(f.parse(&mut part), vec![CatUpdate::Freq(7_030_000.0)]);
    }

    #[test]
    fn the_mode_goes_out_by_the_name_this_rig_actually_has() {
        let mut f = flrig();
        // Before the list answers, the most specific spelling is the guess.
        assert!(text(f.set_mode(Mode::Digu)).contains("<string>DATA</string>"));
        // A KX3's list arrives (flrig's Elecraft driver's own names).
        ask(&mut f, "rig.get_modes");
        respond(
            &mut f,
            "<?xml version=\"1.0\"?><methodResponse><params><param><value><array><data>\
             <value>LSB</value><value>USB</value><value>CW</value><value>FM</value>\
             <value>AM</value><value>DATA</value><value>CW-R</value><value>DATA-R</value>\
             </data></array></value></param></params></methodResponse>",
        );
        assert!(text(f.set_mode(Mode::Digu)).contains("<string>DATA</string>"));
        assert!(text(f.set_mode(Mode::Digl)).contains("<string>DATA-R</string>"));
        // No WFM on a KX3: an empty frame, which writes nothing — not a guess.
        assert!(f.set_mode(Mode::Wfm).is_empty());
        // An Icom's list instead: the same app mode goes out as its spelling.
        respond_modes(&mut f, &["LSB", "USB", "AM", "CW", "RTTY", "FM", "USB-D", "LSB-D"]);
        assert!(text(f.set_mode(Mode::Digu)).contains("<string>USB-D</string>"));
        assert!(text(f.set_mode(Mode::Digl)).contains("<string>LSB-D</string>"));
    }

    fn respond_modes(f: &mut Flrig, names: &[&str]) {
        ask(f, "rig.get_modes");
        let values: String = names.iter().map(|n| format!("<value>{n}</value>")).collect();
        respond(
            f,
            &format!(
                "<?xml version=\"1.0\"?><methodResponse><params><param><value><array>\
                 <data>{values}</data></array></value></param></params></methodResponse>"
            ),
        );
    }

    #[test]
    fn a_pipe_separated_mode_list_is_taken_too() {
        let mut f = flrig();
        ask(&mut f, "rig.get_modes");
        respond(&mut f, &ok("<string>LSB|USB|CW|DATA</string>"));
        assert_eq!(f.modes, vec!["LSB", "USB", "CW", "DATA"]);
    }

    #[test]
    fn the_power_scale_is_the_rigs_own() {
        let mut f = flrig();
        // Until the rig says otherwise, 100 W is assumed.
        assert!(text(f.set_power(0.5).remove(0)).contains("<i4>50</i4>"));
        // A KX3 answers 15 W.
        ask(&mut f, "rig.get_maxpwr");
        respond(&mut f, &ok("<i4>15</i4>"));
        ask(&mut f, "rig.get_power");
        let up = respond(&mut f, &ok("<i4>10</i4>"));
        match up.as_slice() {
            [CatUpdate::Power(frac)] => assert!((frac - 10.0 / 15.0).abs() < 1e-6),
            other => panic!("expected a power adoption, got {other:?}"),
        }
        // And the slider's fraction goes back out in the rig's watts.
        assert!(text(f.set_power(0.5).remove(0)).contains("<i4>8</i4>"));
    }

    #[test]
    fn the_meters_read_on_their_own_scales() {
        let mut f = flrig();
        // The S-meter arrives already in dBm.
        ask(&mut f, "rig.get_DBM");
        assert_eq!(respond(&mut f, &ok("-73")), vec![CatUpdate::Signal(-73.0)]);
        // SWR is the 0-100 face; half scale is 2:1 on the assumed marks.
        ask(&mut f, "rig.get_swrmeter");
        assert_eq!(respond(&mut f, &ok("50")), vec![CatUpdate::Swr(2.0)]);
        // Zero is a driver with nothing behind the meter, not a reading.
        ask(&mut f, "rig.get_swrmeter");
        assert!(respond(&mut f, &ok("0")).is_empty());
        // Power-out in watts, as a fraction of the maximum.
        ask(&mut f, "rig.get_maxpwr");
        respond(&mut f, &ok("<i4>100</i4>"));
        ask(&mut f, "rig.get_pwrmeter");
        assert_eq!(respond(&mut f, &ok("25")), vec![CatUpdate::Po(0.25)]);
    }

    #[test]
    fn a_fault_is_noted_and_reported_once() {
        let mut f = flrig();
        assert!(!f.refused());
        ask(&mut f, "rig.set_ptt");
        assert!(
            respond(
                &mut f,
                "<?xml version=\"1.0\"?><methodResponse><fault><value><struct>\
                 <member><name>faultCode</name><value><i4>-1</i4></value></member>\
                 <member><name>faultString</name><value>nope</value></member>\
                 </struct></value></fault></methodResponse>",
            )
            .is_empty()
        );
        assert!(f.refused(), "the daemon said the command did not take");
        assert!(!f.refused(), "and it is reported exactly once");
        // An HTTP error is a refusal too.
        ask(&mut f, "rig.set_ptt");
        respond_status(&mut f, 500, &ok("<i4>0</i4>"));
        assert!(f.refused());
    }

    #[test]
    fn only_the_positions_that_round_trip_are_followed() {
        let mut f = flrig();
        // RTTY: sdroxide's is its own modem in a data sideband, and would be
        // commanded back as a data position — so it is not followed.
        ask(&mut f, "rig.get_mode");
        assert!(respond(&mut f, &ok("<string>RTTY</string>")).is_empty());
        // Synchronous AM is this side's own detector on the rig's plain AM.
        ask(&mut f, "rig.get_mode");
        assert!(respond(&mut f, &ok("<string>AMS</string>")).is_empty());
        // Every mode this profile commands by default has to read back as
        // itself.
        for m in [
            Mode::Lsb,
            Mode::Usb,
            Mode::Cw,
            Mode::Am,
            Mode::Dsb,
            Mode::Nfm,
            Mode::Wfm,
            Mode::Digl,
            Mode::Digu,
        ] {
            let name = candidates(m)[0];
            ask(&mut f, "rig.get_mode");
            assert_eq!(
                respond(&mut f, &ok(&format!("<string>{name}</string>"))),
                vec![CatUpdate::Mode(m)],
                "{m:?} goes out as {name} and read back as something else"
            );
        }
    }

    #[test]
    fn a_new_connection_starts_with_nothing_in_flight() {
        let mut f = flrig();
        ask(&mut f, "rig.get_vfo");
        ask(&mut f, "rig.get_mode");
        // Half a response arrives, then the link drops.
        let mut part = b"HTTP/1.1 200 OK\r\nContent-Le".to_vec();
        assert!(f.parse(&mut part).is_empty());
        f.link_opened();
        // The fresh connection's first exchange correlates cleanly.
        ask(&mut f, "rig.get_vfo");
        assert_eq!(respond(&mut f, &ok("14074000")), vec![CatUpdate::Freq(14_074_000.0)]);
    }

    #[test]
    fn what_the_rig_is_arrives_with_the_open_sequence() {
        let mut f = flrig();
        assert!(!f.mode_moves_dial(), "nothing is claimed before the rig is known");
        for req in f.open_requests() {
            f.wrote(&req);
        }
        respond(&mut f, &ok("<string>KX3</string>"));
        respond(&mut f, &ok("<i4>15</i4>"));
        respond(
            &mut f,
            "<?xml version=\"1.0\"?><methodResponse><params><param><value><array><data>\
             <value>LSB</value><value>USB</value></data></array></value></param></params>\
             </methodResponse>",
        );
        // An Elecraft shifts its dial with the mode behind flrig exactly as on
        // a direct link.
        assert!(f.mode_moves_dial());
        assert_eq!(f.max_w, 15.0);
        assert_eq!(f.modes, vec!["LSB", "USB"]);
    }

    #[test]
    fn cw_goes_out_as_queue_then_start() {
        let mut f = flrig();
        // `cwio_text` only queues — without the `cwio_send 1` behind it the
        // message would sit in flrig unsent, verified against its source.
        let frames: Vec<String> = f.send_cw("CQ CQ").into_iter().map(text).collect();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains("<methodName>rig.cwio_text</methodName>"));
        assert!(frames[0].contains("<string>CQ CQ</string>"));
        assert!(frames[1].contains("<methodName>rig.cwio_send</methodName>"));
        assert!(frames[1].contains("<i4>1</i4>"));
        let abort = text(f.abort_cw().remove(0));
        assert!(abort.contains("<methodName>rig.cwio_send</methodName>"));
        assert!(abort.contains("<i4>0</i4>"));
    }

    #[test]
    fn the_filter_goes_out_as_a_width_for_flrig_to_snap() {
        let mut f = flrig();
        let frame = text(f.set_filter(Mode::Cw, 300.0, 800.0).remove(0));
        assert!(frame.contains("<methodName>rig.set_bandwidth</methodName>"));
        assert!(frame.contains("<i4>500</i4>"));
    }

    /// The failure users hit: flrig's S-meter, SWR and power faces are all
    /// 0-100, and one of those numbers read as a dial is a radio apparently
    /// tuned to 7 Hz. It cannot be believed, and it cannot be quietly dropped
    /// either — an answer in the wrong place means every later one is too.
    #[test]
    fn a_meter_reading_is_not_a_dial() {
        let mut f = flrig();
        ask(&mut f, "rig.get_vfo");
        assert!(respond(&mut f, &ok("7")).is_empty(), "7 Hz is a meter, not a frequency");
        assert!(f.desynced(), "and being handed one is proof the answers have slipped");
        assert!(!f.desynced(), "reported exactly once");
    }

    /// The other half of the same slip, and the more expensive one: a
    /// frequency parses perfectly well as a transmit state, and every non-zero
    /// number would be an over. That blanks the S-meter and refuses the
    /// operator's own next key-down.
    #[test]
    fn a_frequency_is_not_a_transmit_state() {
        let mut f = flrig();
        ask(&mut f, "rig.get_ptt");
        assert!(respond(&mut f, &ok("<i4>14074000</i4>")).is_empty());
        assert!(f.desynced());
        // Both of the answers this read really has still read.
        for (raw, on) in [("<i4>0</i4>", false), ("<i4>1</i4>", true)] {
            ask(&mut f, "rig.get_ptt");
            assert_eq!(respond(&mut f, &ok(raw)), vec![CatUpdate::Ptt(on)]);
        }
        assert!(!f.desynced(), "and nothing about those is a slip");
    }

    /// A driver with its own idea of "keyed" costs one reading, not a link:
    /// any small number is a transmit state, and only a number no transmit
    /// state could be — a frequency — is a slip.
    #[test]
    fn an_odd_transmit_state_is_still_a_transmit_state() {
        let mut f = flrig();
        ask(&mut f, "rig.get_ptt");
        assert_eq!(respond(&mut f, &ok("<i4>2</i4>")), vec![CatUpdate::Ptt(true)]);
        assert!(!f.desynced(), "an odd number is odd, not proof of anything");
    }

    /// Remaking the link cures a slip — one request lost, after which the order
    /// is wrong for good. It cannot cure an answer flrig gives every time, and
    /// a session spent reconnecting into the same reply is worse than one that
    /// simply refuses it: the refusing is what keeps a meter reading off the
    /// dial, and it goes on working.
    #[test]
    fn a_slip_that_survives_every_reconnect_stops_being_reconnected_over() {
        let mut f = flrig();
        for attempt in 1..=GIVE_UP_AFTER {
            ask(&mut f, "rig.get_vfo");
            assert!(respond(&mut f, &ok("7")).is_empty());
            assert!(f.desynced(), "attempt {attempt} is still worth a fresh connection");
            f.link_opened();
        }
        ask(&mut f, "rig.get_vfo");
        assert!(respond(&mut f, &ok("7")).is_empty(), "and the answer is still refused");
        assert!(!f.desynced(), "but the link is left alone now");
    }

    /// ...and a link that comes back working starts that count again, so a
    /// second genuine slip an hour later still gets its reconnect.
    #[test]
    fn an_answer_in_its_right_place_clears_the_count() {
        let mut f = flrig();
        for _ in 0..GIVE_UP_AFTER {
            ask(&mut f, "rig.get_vfo");
            respond(&mut f, &ok("7"));
            assert!(f.desynced());
            f.link_opened();
        }
        ask(&mut f, "rig.get_vfo");
        assert_eq!(respond(&mut f, &ok("14074000")), vec![CatUpdate::Freq(14_074_000.0)]);
        ask(&mut f, "rig.get_vfo");
        assert!(respond(&mut f, &ok("7")).is_empty());
        assert!(f.desynced(), "a working link earns the next slip its reconnect");
    }

    /// A VFO flrig has not read yet answers zero, and an flrig with nothing
    /// to say answers an empty value. Neither is a slip: reconnecting over an
    /// flrig sitting there with no radio attached would be a loop.
    #[test]
    fn a_dial_flrig_has_not_read_is_not_a_slip() {
        let mut f = flrig();
        for raw in ["0", ""] {
            ask(&mut f, "rig.get_vfo");
            assert!(respond(&mut f, &ok(raw)).is_empty());
            assert!(!f.desynced(), "{raw:?} is flrig having nothing to report");
        }
        ask(&mut f, "rig.get_ptt");
        assert!(respond(&mut f, &ok("")).is_empty());
        assert!(!f.desynced());
    }

    /// What the driver holds its next frame on — see the module note. The
    /// count is the queue: one per request written, one off per answer read.
    #[test]
    fn what_is_outstanding_is_what_has_not_been_answered() {
        let mut f = flrig();
        assert_eq!(f.in_flight(), 0);
        for req in f.poll_requests() {
            f.wrote(&req);
        }
        assert_eq!(f.in_flight(), 2);
        respond(&mut f, &ok("14074000"));
        assert_eq!(f.in_flight(), 1);
        respond(&mut f, &ok("USB"));
        assert_eq!(f.in_flight(), 0);
        // An answer with nothing outstanding is a slip of its own: one too
        // many puts every later answer a place ahead of its question.
        respond(&mut f, &ok("14074000"));
        assert!(f.desynced());
    }

    #[test]
    fn text_in_a_cw_message_is_xml_escaped() {
        let mut f = flrig();
        let frame = text(f.send_cw("R R <K>").remove(0));
        assert!(frame.contains("<string>R R &lt;K&gt;</string>"));
    }
}
