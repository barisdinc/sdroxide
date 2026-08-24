//! The AX.25 packet codec: every frame type, parsed and serialised.
//!
//! Vendored from rax25 `src/lib.rs`; see `vendor/rax25/PROVENANCE.md`. Changes
//! from upstream:
//!
//! * `anyhow` becomes [`crate::Ax25Error`], so a caller can tell a truncated
//!   frame from an unparseable one;
//! * the `USE_FCS` branches are gone. Upstream had them compiled out because a
//!   KISS TNC does the FCS itself; here [`crate::hdlc`] owns it, checks it, and
//!   strips it before a frame reaches this module. A frame arriving here has
//!   already passed.
//!
//! Section numbers in the comments are the AX.25 specification's, kept from
//! upstream — they are the most useful thing in the file.

use crate::Ax25Error;
use crate::addr::Addr;

type Result<T, E = Ax25Error> = std::result::Result<T, E>;

/// An AX.25 packet, of any type.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub(crate) src: Addr,
    pub(crate) dst: Addr,
    pub(crate) digipeater: Vec<Addr>,
    pub(crate) rr_extseq: bool,
    pub(crate) command_response: bool,
    pub(crate) command_response_la: bool,
    pub(crate) rr_dist1: bool,
    #[allow(clippy::struct_field_names)]
    pub(crate) packet_type: PacketType,
}

/// All packet types.
#[derive(Clone, Debug, PartialEq)]
pub enum PacketType {
    Sabm(Sabm),
    Sabme(Sabme),
    Ua(Ua),
    Dm(Dm),
    Disc(Disc),
    Iframe(Iframe),
    Rr(Rr),
    Rnr(Rnr),
    Rej(Rej),
    Srej(Srej),
    Frmr(Frmr),
    Xid(Xid),
    Ui(Ui),
    Test(Test),
}

/// SABM - Set Asynchronous Balanced Mode (4.3.3.1, page 23)
#[derive(Clone, Debug, PartialEq)]
pub struct Sabm {
    pub poll: bool,
}

/// SAMBE - Set Asynchronous Balanced Mode Extended (4.3.3.2, page 23)
#[derive(Clone, Debug, PartialEq)]
pub struct Sabme {
    pub poll: bool,
}

/// RR - Receiver Ready (4.3.2.1, page 21)
///
/// Basically an ACK.
#[derive(Debug, PartialEq, Clone)]
pub struct Rr {
    pub poll: bool,
    pub nr: u8,
}

/// REJ - Reject (4.3.2.3, page 21)
///
/// Unclear why this is even needed. Couldn't RR with NR older than last sent
/// be equally eager to retransmit?
#[derive(Debug, PartialEq, Clone)]
pub struct Rej {
    pub poll: bool,
    pub nr: u8,
}

/// SREJ - Selective reject (4.3.2.4, page 21)
///
/// Request retransmissions of a single iframe.
#[derive(Debug, PartialEq, Clone)]
pub struct Srej {
    pub poll: bool,
    pub nr: u8,
}

/// FRMR - A deprecated error signaling (4.3.3.9, page 28)
///
/// The AX.25 2.2 spec deprecates this, and says to not generate these frames. But
/// it does specify what to do when receiving one.
#[derive(Debug, PartialEq, Clone)]
pub struct Frmr {
    pub poll: bool,
}

/// Test - Test frame (4.3.3.8, page 28)
///
/// It's ping, basically. The payload is mirrorred back, or (if there's not
/// enough room to store the payload), an empty response is returned.
///
/// The intended use of the poll flag here is unclear.
#[derive(Debug, PartialEq, Clone)]
pub struct Test {
    pub poll: bool,
    pub payload: Vec<u8>,
}

/// XID - Exchange Identification (4.3.3.7, page 24)
///
/// ISO 8885 exchange of capabilities, like extended sequence numbers,
/// max IFRAME size ("MTU"), and lots of other stuff.
///
/// TODO: Currently not implemented.
#[derive(Debug, PartialEq, Clone)]
pub struct Xid {
    pub poll: bool,
}

/// RNR - Receiver Not Ready (4.3.2.2, page 21)
///
/// Like RR, but asks the sender to not send more data for now.
/// The TCP version of this would be a closed receiver window.
#[derive(Debug, PartialEq, Clone)]
pub struct Rnr {
    pub poll: bool,
    pub nr: u8,
}

/// UA - Unnumbered Ack (4.3.3.4, page 23)
///
/// Acknowledge of things that don't have sequence numbers. Like SABM(E)
/// and DISC.
///
/// The equivalent of both the replying FIN and the SYN|ACK in TCP.
/// Probably not a good idea to use the same message for these two very
/// different events, since it's more "yeah, whatever, I hear you", but
/// not acknowledging if you heard "let's go" or "close down".
#[derive(Debug, PartialEq, Clone)]
pub struct Ua {
    pub poll: bool,
}

/// IFRAME - Information Frame (4.3.1, page 19)
///
/// Carries information. Obviously. Really, this could probably have been
/// merged with RR/RNR, even if it means empty payload. That's what
/// TCP does.
#[derive(Clone, Debug, PartialEq)]
pub struct Iframe {
    pub nr: u8,
    pub ns: u8,
    pub poll: bool,
    pub pid: u8,
    pub payload: Vec<u8>,
}

/// UI - Unnumbered Information (4.3.3.6, page 24)
///
/// Information frames outside of the sequential data flow.
/// Can be used whether a connection is established or not.
/// Disconnected UIs power APRS.
///
/// APRS doesn't use "push" for ACKs, but when unicasted
/// it could. A DM should be returned when push is set.
#[derive(Clone, Debug, PartialEq)]
pub struct Ui {
    pub pid: u8,
    pub push: bool,
    pub payload: Vec<u8>,
}

/// DM - Disconnected Mode (4.3.3.5, page 23)
///
/// The reply if the incoming packet implies a connection is active, or if
/// a SABM(E) was received and nothing was ready to receive it.
///
/// Basically a TCP RST.
#[derive(Clone, Debug, PartialEq)]
pub struct Dm {
    pub poll: bool,
}

/// DISC - Disconnect (4.3.3.3, page 23)
///
/// End the connection. A DISC is acked with a UA packet, which seems
/// silly. Replying with DISC would make more sense, but hey ho.
#[derive(Clone, Debug, PartialEq)]
pub struct Disc {
    pub poll: bool,
}

// Unnumbered frames. Ending in 11.
#[allow(clippy::unusual_byte_groupings)]
const CONTROL_SABM: u8 = 0b001_0_11_11;
#[allow(clippy::unusual_byte_groupings)]
const CONTROL_SABME: u8 = 0b011_0_11_11;
#[allow(clippy::unusual_byte_groupings)]
const CONTROL_UI: u8 = 0b000_0_00_11;
#[allow(clippy::unusual_byte_groupings)]
const CONTROL_DISC: u8 = 0b010_0_00_11;
#[allow(clippy::unusual_byte_groupings)]
const CONTROL_DM: u8 = 0b0000_1111;
const CONTROL_UA: u8 = 0b0110_0011;
const CONTROL_TEST: u8 = 0b1110_0011;
const CONTROL_XID: u8 = 0b1010_1111;
const CONTROL_FRMR: u8 = 0b1000_0111;

// Supervisor frames. Ending in 01.
const CONTROL_RR: u8 = 0b0000_0001;
const CONTROL_RNR: u8 = 0b0000_0101;
const CONTROL_REJ: u8 = 0b0000_1001;
const CONTROL_SREJ: u8 = 0b0000_1101;

// Iframes end in 0.
const CONTROL_IFRAME: u8 = 0b0000_0000;

// Masks.
const CONTROL_POLL: u8 = 0b0001_0000;
const NR_MASK: u8 = 0b1110_0000;
const TYPE_MASK: u8 = 0b0000_0011;
const NO_L3: u8 = 0xF0;

impl Packet {
    /// Construct a UI frame.
    // TODO: allow setting all the other fields.
    #[must_use]
    pub fn ui<T: Into<Vec<u8>>>(src: Addr, dst: Addr, payload: T) -> Self {
        Self {
            src,
            dst,
            digipeater: vec![],
            rr_extseq: false,
            command_response: false,
            command_response_la: true,
            rr_dist1: false,
            packet_type: PacketType::Ui(Ui { pid: 0xf0, push: false, payload: payload.into() }),
        }
    }
    /// Construct a UI frame with a digipeater path.
    ///
    /// Upstream's [`Packet::ui`] builds a direct frame, which is all a
    /// connected-mode station needs — a link is negotiated with a fixed path
    /// and the state machine carries it. APRS is the opposite: the path is
    /// chosen per transmission, it is the only thing deciding how far the
    /// frame goes, and it is what `WIDE1-1,WIDE2-1` means.
    #[must_use]
    pub fn ui_via<T: Into<Vec<u8>>>(src: Addr, dst: Addr, via: Vec<Addr>, payload: T) -> Self {
        let mut p = Self::ui(src, dst, payload);
        p.digipeater = via;
        // Sent as a command, which is what every APRS station on the channel
        // sends: destination C bit set, source C bit clear. `Packet::ui` builds
        // the other pairing — a *response* — which is right for a connected
        // session answering an I-frame and wrong for a broadcast nobody asked
        // for. The bits are one each and no decoder here cares, but a frame
        // that does not look like everyone else's is a frame some digipeater
        // firmware is entitled to ignore.
        p.command_response = true;
        p.command_response_la = false;
        p
    }

    /// Get the packet type.
    #[must_use]
    pub fn packet_type(&self) -> &PacketType {
        &self.packet_type
    }

    /// Who sent it.
    ///
    /// Upstream exposes only `packet_type`, because its callers already know
    /// the addresses — they are the ones that built the connection. A monitor
    /// does not: it is reading other people's traffic, and the addresses are
    /// most of what it has to show. Added here rather than making the fields
    /// public, so the struct keeps the shape upstream gave it.
    #[must_use]
    pub fn src(&self) -> &Addr {
        &self.src
    }

    /// Who it was addressed to.
    #[must_use]
    pub fn dst(&self) -> &Addr {
        &self.dst
    }

    /// The digipeater path, in order. Empty for a direct frame.
    #[must_use]
    pub fn digipeaters(&self) -> &[Addr] {
        &self.digipeater
    }

    /// The command/response bit. Several state-machine events need it, and it
    /// is not recoverable from the frame type alone.
    #[must_use]
    pub fn command_response(&self) -> bool {
        self.command_response
    }
    /// Serialize a packet, either as standard mod-8, or extended mod-128.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn serialize(&self, ext: bool) -> Vec<u8> {
        let mut ret = Vec::with_capacity(
            14 + 2
                + if let PacketType::Iframe(s) = &self.packet_type {
                    s.payload.len() + 1
                } else {
                    0
                },
        );
        ret.extend(self.dst.serialize(false, self.command_response, self.rr_dist1, false));
        assert_ne!(self.command_response, self.command_response_la);
        ret.extend(self.src.serialize(
            self.digipeater.is_empty(),
            self.command_response_la,
            self.rr_extseq, // Setting this bit for extseq seems to be a de facto standard.
            false,
        ));

        {
            let l = self.digipeater.len();
            for (n, digi) in self.digipeater.iter().enumerate() {
                ret.extend(digi.serialize(
                    n == l - 1, // last repeater?
                    false,      // "has been repeated"
                    false,
                    false,
                ));
            }
        }

        match &self.packet_type {
            // U frames. Control always one byte.
            PacketType::Sabm(s) => {
                if ext {
                    ret.push(CONTROL_SABME | if s.poll { CONTROL_POLL } else { 0 });
                } else {
                    ret.push(CONTROL_SABM | if s.poll { CONTROL_POLL } else { 0 });
                }
            }
            PacketType::Sabme(s) => ret.push(CONTROL_SABME | if s.poll { CONTROL_POLL } else { 0 }),
            PacketType::Ua(s) => ret.push(CONTROL_UA | if s.poll { CONTROL_POLL } else { 0 }),
            PacketType::Disc(disc) => {
                ret.push(CONTROL_DISC | if disc.poll { CONTROL_POLL } else { 0 });
            }
            PacketType::Dm(s) => ret.push(CONTROL_DM | if s.poll { CONTROL_POLL } else { 0 }),
            // TODO: FRMR data too.
            PacketType::Frmr(s) => ret.push(CONTROL_FRMR | if s.poll { CONTROL_POLL } else { 0 }),
            PacketType::Ui(s) => {
                ret.push(CONTROL_UI | if s.push { CONTROL_POLL } else { 0 });
                ret.push(s.pid);
                ret.extend(&s.payload);
            }
            // TODO: XID data too.
            PacketType::Xid(s) => ret.push(CONTROL_XID | if s.poll { CONTROL_POLL } else { 0 }),
            PacketType::Test(s) => {
                ret.push(CONTROL_TEST | if s.poll { CONTROL_POLL } else { 0 });
                ret.extend(&s.payload);
            }

            // S frames.
            PacketType::Rr(s) => {
                if ext {
                    ret.push(CONTROL_RR);
                    ret.push((s.nr << 1) & 0xFE | u8::from(s.poll));
                } else {
                    ret.push(
                        CONTROL_RR
                            | if s.poll { CONTROL_POLL } else { 0 }
                            | ((s.nr << 5) & NR_MASK),
                    );
                }
            }
            PacketType::Rnr(s) => {
                if ext {
                    ret.push(CONTROL_RNR);
                    ret.push((s.nr << 1) & 0xFE | u8::from(s.poll));
                } else {
                    ret.push(
                        CONTROL_RNR
                            | if s.poll { CONTROL_POLL } else { 0 }
                            | ((s.nr << 5) & NR_MASK),
                    );
                }
            }
            PacketType::Rej(s) => {
                if ext {
                    ret.push(CONTROL_REJ);
                    ret.push((s.nr << 1) & 0xFE | u8::from(s.poll));
                } else {
                    ret.push(CONTROL_REJ | if s.poll { CONTROL_POLL } else { 0 });
                }
            }
            PacketType::Srej(s) => {
                if ext {
                    ret.push(CONTROL_SREJ);
                    ret.push((s.nr << 1) & 0xFE | u8::from(s.poll));
                } else {
                    ret.push(CONTROL_SREJ | if s.poll { CONTROL_POLL } else { 0 });
                }
            }
            PacketType::Iframe(iframe) => {
                if ext {
                    ret.push(CONTROL_IFRAME | ((iframe.ns << 1) & 0xFE));
                    ret.push((iframe.nr << 1) & 0xFE | u8::from(iframe.poll));
                } else {
                    ret.push(
                        CONTROL_IFRAME
                            | if iframe.poll { CONTROL_POLL } else { 0 }
                            | ((iframe.nr << 5) & 0b1110_0000)
                            | ((iframe.ns << 1) & 0b0000_1110),
                    );
                }
                ret.push(iframe.pid);
                ret.extend(&iframe.payload);
            }
        }
        ret
    }

    /// Parse packet from bytes.
    ///
    /// A packet with sequence numbers in it (S and I frames) cannot be parsed
    /// without knowing if it's extended or not, because the control field is
    /// either one or two bytes.
    ///
    /// Ideally you already know, because you sent, received, or at least saw
    /// the SABM (not extended) or SABME (extended).
    ///
    /// The Linux stack uses the `rbit_ext` reserved bit in the source address
    /// to indicate extended mode. This is the used by the other Linux tooling
    /// like `axlisten`. But it's nonstandard.
    ///
    /// In theory other heuristics can be provided, to try to brute force the
    /// mode. But really, it's best if you know ahead of time.
    ///
    /// This code supports using the Linux bit, by providing `None` as `ext`, as
    /// opposed to `Some(bool)`.
    pub fn parse(bytes: &[u8], ext: Option<bool>) -> Result<Self> {
        // The FCS is `crate::hdlc`'s business and is already gone by here.
        if bytes.len() < 15 {
            return Err(Ax25Error::Short(bytes.len()));
        }
        let dst = Addr::parse(&bytes[0..7])?;
        let src = Addr::parse(&bytes[7..14])?;

        let ext = match ext {
            Some(v) => v,
            None => src.rbit_ext,
        };

        let mut digipeater = Vec::new();
        let mut pos = 14;
        let mut more_addresses = !src.lowbit;
        while more_addresses {
            let digi = Addr::parse(&bytes[pos..pos + 7])?;
            more_addresses = !digi.lowbit;
            pos += 7;
            digipeater.push(digi);
        }

        let rest = &bytes[pos..];
        let control1 = rest[0];
        let (poll, nr, ns, bytes) = {
            if !ext || control1 & TYPE_MASK == 3 {
                // NOTE: ns/nr will be nonsense for U frames.
                // ns will be nonsense for S frames.
                (
                    control1 & CONTROL_POLL == CONTROL_POLL,
                    (control1 >> 5) & 7,
                    (control1 >> 1) & 7,
                    &rest[1..],
                )
            } else {
                if rest.len() < 2 {
                    return Err(Ax25Error::Parse(
                        "AX.25 in ext mode, but S/U frame is too short".into(),
                    ));
                }
                let control2 = rest[1];
                (control2 & 1 == 1, (control2 >> 1) & 127, (control1 >> 1) & 127, &rest[2..])
            }
        };
        Ok(Packet {
            src: src.clone(),
            dst: dst.clone(),
            command_response: dst.highbit,
            command_response_la: src.highbit,
            rr_dist1: dst.rbit_ext,
            rr_extseq: ext,
            digipeater,
            packet_type: match control1 & TYPE_MASK {
                // I frames. Second control byte, with NR and NS.
                // TODO: confirm pid is NO_L3
                0 | 2 => PacketType::Iframe(Iframe {
                    ns,
                    nr,
                    poll,
                    pid: NO_L3,
                    payload: bytes[1..].to_vec(),
                }),
                // S frames. Second control byte, with NR.
                1 => match control1 & !NR_MASK & !CONTROL_POLL {
                    CONTROL_RR => PacketType::Rr(Rr { poll, nr }),
                    CONTROL_RNR => PacketType::Rnr(Rnr { poll, nr }),
                    CONTROL_REJ => PacketType::Rej(Rej { poll, nr }),
                    CONTROL_SREJ => PacketType::Srej(Srej { poll, nr }),
                    _ => panic!("Impossible logic error: {control1} failed to be supervisor"),
                },
                // U frames. No second control byte.
                3 => match !CONTROL_POLL & control1 {
                    CONTROL_SABME => PacketType::Sabme(Sabme { poll }),
                    CONTROL_SABM => PacketType::Sabm(Sabm { poll }),
                    CONTROL_UA => PacketType::Ua(Ua { poll }),
                    CONTROL_DISC => PacketType::Disc(Disc { poll }),
                    CONTROL_DM => PacketType::Dm(Dm { poll }),
                    CONTROL_FRMR => PacketType::Frmr(Frmr { poll }),
                    CONTROL_UI => PacketType::Ui(Ui {
                        push: poll,
                        pid: bytes[0],
                        payload: bytes[1..].to_vec(),
                    }),
                    CONTROL_XID => PacketType::Xid(Xid { poll }),
                    CONTROL_TEST => PacketType::Test(Test { poll, payload: bytes.to_vec() }),
                    c => todo!("Control {c:b} not implemented"),
                },
                _ => panic!("Logic error: {control1} & 3 > 3"),
            },
        })
    }
}

/// Hub packet serializer/deserializer.
///
/// Hub reads and writes packets. Normally to a KISS serial port. But
/// ideally something more clevel with priority queues and mux-capability.
///
/// Then again that more clever system could just be freestanding, and expose
/// KISS as an interface to it.
pub trait Hub {
    /// Send frame. May block.
    ///
    /// The provided frame must be a complete AX.25 frame, without FEND or
    /// escaping.
    fn send(&mut self, frame: &[u8]) -> Result<()>;

    /// Try receiving a frame.
    ///
    /// Ok(None) means timeout.
    fn recv_timeout(&mut self, timeout: std::time::Duration) -> Result<Option<Vec<u8>>>;

    /// Clone a kisser.
    /// All packets get delivered to all clones.
    fn clone(&self) -> Box<dyn Hub>;
}
