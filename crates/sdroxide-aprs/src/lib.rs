//! APRS — the Automatic Packet Reporting System, as a codec.
//!
//! What travels on the air is an AX.25 UI frame; what is *APRS* is the
//! information field inside it. This crate is that field and nothing else:
//! bytes in, a [`AprsData`] out, and the other way round for the four things a
//! station transmits (a position, a status, a message and an acknowledgement).
//! The framing above it belongs to `sdroxide-ax25` and the modem below it to
//! `sdroxide-dsp`.
//!
//! # Why the addresses come in too
//!
//! [`parse`] is given the source and destination callsigns as well as the
//! information field, because one of the formats needs them. Mic-E — the
//! compressed format every Kenwood and Yaesu APRS radio transmits — puts half
//! the latitude, the north/south flag, the east/west flag and the message
//! bits in the *destination address*, which is otherwise unused on an APRS
//! channel. A parser given only the information field decodes a Mic-E position
//! at the wrong longitude, in the wrong hemisphere, and cannot tell.
//!
//! # Sources
//!
//! Every format here is written from the APRS Protocol Reference 1.0.1 —
//! chapter and section cited at each — with the changes in addenda 1.1 and 1.2
//! where they apply. Nothing is ported from an existing decoder.

#![forbid(unsafe_code)]

mod message;
mod mice;
mod path;
mod position;
mod telemetry;
mod weather;

use sdroxide_types::{AprsPosition, AprsSymbol, AprsWeather};

pub use message::{Message, MessageKind, encode_ack, encode_message, encode_rej, encode_status};
pub use mice::MicEStatus;
pub use path::{hop_count, parse_path, path_advice};
pub use position::{
    Position, Timestamp, encode_compressed_position, encode_object, encode_position,
    encode_position_ts,
};
pub use telemetry::Telemetry;

/// Why an information field could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AprsError {
    #[error("empty information field")]
    Empty,
    /// The first byte is a data type identifier the standard does not define,
    /// or one this codec does not implement. Carried rather than swallowed:
    /// the traffic view shows it, which is how an operator finds out that a
    /// station on their channel is sending something nobody reads.
    #[error("unhandled data type '{0}'")]
    UnknownType(char),
    #[error("{0}")]
    Malformed(&'static str),
}

type Result<T> = std::result::Result<T, AprsError>;

/// One decoded information field.
#[derive(Debug, Clone, PartialEq)]
pub enum AprsData {
    /// A station reporting where it is.
    Position(Box<Position>),
    /// A station reporting where *something else* is — a net control point, a
    /// storm, an event — under a name of its own. Killable by whoever put it
    /// there.
    Object { name: String, live: bool, pos: Box<Position> },
    /// An object with no timestamp, for things that neither move nor expire.
    Item { name: String, live: bool, pos: Box<Position> },
    /// A message, an acknowledgement or a rejection.
    Message(Message),
    /// A status report: one line of free text about the station itself.
    Status(String),
    /// Weather with no position attached — what a weather station that has
    /// already said where it is sends between position reports.
    Weather(Box<AprsWeather>),
    /// Telemetry: five analogue channels and eight bits.
    Telemetry(Box<Telemetry>),
    /// A query addressed to the channel or to us.
    Query(String),
    /// A Maidenhead grid report — the oldest and least precise position
    /// format, and still what some beacons send.
    Grid { grid: String, symbol: AprsSymbol, comment: String },
}

impl AprsData {
    /// A word for the traffic view: what this frame turned out to be.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            AprsData::Position(p) if p.weather.is_some() => "weather",
            AprsData::Position(_) => "position",
            AprsData::Object { .. } => "object",
            AprsData::Item { .. } => "item",
            AprsData::Message(m) => m.kind.label(),
            AprsData::Status(_) => "status",
            AprsData::Weather(_) => "weather",
            AprsData::Telemetry(_) => "telemetry",
            AprsData::Query(_) => "query",
            AprsData::Grid { .. } => "grid",
        }
    }

    /// The position this frame carries, if any.
    #[must_use]
    pub fn position(&self) -> Option<&Position> {
        match self {
            AprsData::Position(p)
            | AprsData::Object { pos: p, .. }
            | AprsData::Item { pos: p, .. } => Some(p),
            _ => None,
        }
    }
}

/// Read one information field.
///
/// `dest` is the AX.25 destination callsign, which Mic-E needs and every other
/// format ignores. `info` is the field itself, verbatim — it is *not* text: a
/// Mic-E field is binary, and several formats carry bytes above 0x7f in the
/// comment.
///
/// # Errors
///
/// [`AprsError::UnknownType`] for a data type identifier this codec does not
/// handle, and [`AprsError::Malformed`] where the identifier is one it does and
/// the field behind it will not parse.
pub fn parse(dest: &str, info: &[u8]) -> Result<AprsData> {
    let Some(&first) = info.first() else {
        return Err(AprsError::Empty);
    };
    let rest = &info[1..];
    match first {
        // 5.1 — position, no timestamp. `=` additionally says the station
        // accepts messages; `!` says it does not.
        b'!' | b'=' => {
            let mut p = position::parse_position(rest, false)?;
            p.messaging = first == b'=';
            Ok(AprsData::Position(Box::new(p)))
        }
        // 5.2 — position with timestamp. `@` accepts messages, `/` does not.
        b'/' | b'@' => {
            let mut p = position::parse_position(rest, true)?;
            p.messaging = first == b'@';
            Ok(AprsData::Position(Box::new(p)))
        }
        // 11 — object report: a nine-character name, a live/killed flag, a
        // timestamp, then an ordinary position.
        b';' => position::parse_object(rest),
        // 11.1 — item report: a name of 3 to 9 characters terminated by the
        // live/killed flag, then a position with no timestamp.
        b')' => position::parse_item(rest),
        // 14 — message, acknowledgement, rejection or bulletin.
        b':' => message::parse(rest).map(AprsData::Message),
        // 16 — status report.
        b'>' => Ok(AprsData::Status(printable(rest).trim_end().to_string())),
        // 12 — positionless weather report.
        b'_' => weather::parse_positionless(rest).map(|w| AprsData::Weather(Box::new(w))),
        // 13 — telemetry.
        b'T' => telemetry::parse(rest).map(|t| AprsData::Telemetry(Box::new(t))),
        // 15 — general query.
        b'?' => Ok(AprsData::Query(printable(rest).trim_end().to_string())),
        // 6 — Mic-E, in all four of the data type identifiers that have been
        // used for it. 0x1c/0x1d are the "current"/"old" forms the original
        // TH-D7 firmware sent; `'` and '`' are what everything since sends.
        0x1c | 0x1d | b'\'' | b'`' => mice::parse(dest, rest),
        // 17 — Maidenhead grid, in the two forms that exist.
        b'[' => Ok(grid_report(rest)),
        _ => Err(AprsError::UnknownType(first as char)),
    }
}

/// A `[` grid report: `[GRID]comment`, the locator in the brackets.
fn grid_report(rest: &[u8]) -> AprsData {
    let text = printable(rest);
    let (grid, comment) = match text.find(']') {
        Some(i) => (text[..i].to_string(), text[i + 1..].trim().to_string()),
        None => (text.trim().to_string(), String::new()),
    };
    AprsData::Grid { grid, symbol: AprsSymbol::default(), comment }
}

/// Bytes as text for display.
///
/// Lossy on purpose and never `from_utf8`: comment fields carry whatever the
/// sending station's character set was, and a decoder that refused the frame
/// over one high byte would lose a position report for the sake of one
/// mangled degree sign.
pub(crate) fn printable(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace(['\r', '\n', '\0'], "").to_string()
}

/// The centre of the square a position with `ambiguity` blanked digits could
/// be anywhere in, and how wide that square is in degrees of latitude.
///
/// One blanked digit is a tenth of a minute, two a whole minute, three ten
/// minutes, four a degree. Interface fact rather than an internal: the map
/// draws the square.
#[must_use]
pub fn ambiguity_span_deg(ambiguity: u8) -> f64 {
    match ambiguity {
        1 => 0.1 / 60.0,
        2 => 1.0 / 60.0,
        3 => 10.0 / 60.0,
        4 => 1.0,
        _ => 0.0,
    }
}

/// Where a Maidenhead locator puts a station, as a position with the
/// ambiguity that a locator actually has.
///
/// A six-character locator is a square about 4.6 by 9.3 km; calling that a
/// point would be the same lie as ignoring position ambiguity, so it comes
/// back flagged.
#[must_use]
pub fn position_from_grid(grid: &str) -> Option<AprsPosition> {
    let (lat, lon) = sdroxide_types::grid_to_latlon(grid)?;
    // A four-character locator is a degree of latitude tall, a six-character
    // one two and a half minutes.
    let ambiguity = if grid.trim().len() >= 6 { 3 } else { 4 };
    Some(AprsPosition { lat, lon, ambiguity })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_field_is_an_error_not_a_position() {
        assert_eq!(parse("APRS", b""), Err(AprsError::Empty));
    }

    /// A data type identifier nobody implements has to come back naming
    /// itself. The traffic view prints it, and that is how an operator learns
    /// their channel carries something this build does not read.
    #[test]
    fn an_unknown_type_names_itself() {
        assert_eq!(parse("APRS", b"%something"), Err(AprsError::UnknownType('%')));
    }
}
