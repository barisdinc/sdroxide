//! Mic-E (chapter 10) — the compressed position format every commercial APRS
//! radio transmits, and the one a decoder cannot read from the information
//! field alone.
//!
//! Mic-E splits a position across the frame. The information field holds the
//! longitude, the course, the speed and the symbol; the *destination address*
//! — seven bytes of the AX.25 header that an APRS frame otherwise wastes on
//! the word `APRS` — holds the whole latitude, the north/south flag, the
//! east/west flag, a 100-degree longitude offset and three message bits. The
//! two halves are useless apart: a receiver that reads only the information
//! field gets a longitude in the wrong hemisphere, offset by up to a hundred
//! degrees, with no latitude at all.
//!
//! Nothing here encodes. A beacon has the compressed format
//! ([`crate::encode_compressed_position`]), which is a byte or two longer,
//! carries the same information, and does not need the destination address —
//! which on transmit is where the software's own identifier belongs.

use sdroxide_types::{AprsPosition, AprsSymbol};

use crate::position::Position;
use crate::{AprsData, AprsError, Result, printable};

/// The three message bits, as the operator set them on the radio's front
/// panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicEStatus {
    OffDuty,
    EnRoute,
    InService,
    Returning,
    Committed,
    Special,
    Priority,
    /// The one code that means the same thing in both tables, and the one
    /// worth surfacing: a station sending it has pressed the emergency button.
    Emergency,
    /// One of the seven codes whose meaning is agreed locally rather than by
    /// the standard.
    Custom(u8),
}

impl MicEStatus {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MicEStatus::OffDuty => "Off Duty",
            MicEStatus::EnRoute => "En Route",
            MicEStatus::InService => "In Service",
            MicEStatus::Returning => "Returning",
            MicEStatus::Committed => "Committed",
            MicEStatus::Special => "Special",
            MicEStatus::Priority => "Priority",
            MicEStatus::Emergency => "EMERGENCY",
            MicEStatus::Custom(0) => "Custom 0",
            MicEStatus::Custom(1) => "Custom 1",
            MicEStatus::Custom(2) => "Custom 2",
            MicEStatus::Custom(3) => "Custom 3",
            MicEStatus::Custom(4) => "Custom 4",
            MicEStatus::Custom(5) => "Custom 5",
            MicEStatus::Custom(_) => "Custom 6",
        }
    }

    /// True for the code that is an actual call for help, so a panel can
    /// colour it. Both tables agree on this one.
    #[must_use]
    pub fn is_emergency(self) -> bool {
        self == MicEStatus::Emergency
    }
}

/// One character of the destination address, decoded.
struct DestChar {
    /// The latitude digit, or `None` where the sender blanked it out.
    digit: Option<u8>,
    /// The message bit this character carries.
    bit: bool,
    /// The character came from the `P`–`Z` range, which is what sets the
    /// north / west / plus-100 flags.
    high: bool,
    /// The character came from the `A`–`K` range, which says the message code
    /// is a locally agreed one rather than a standard one.
    custom: bool,
}

/// Decode one destination-address character.
///
/// Three overlapping alphabets in one field, which is why this is a function
/// rather than an expression: `0`–`9` are a digit with every flag clear,
/// `A`–`K` are the same digits again with the custom bit set (and `K` a
/// blank), `L` is a blank with nothing set, and `P`–`Z` are the digits a third
/// time with the flag bit set (`Z` a blank).
fn dest_char(c: u8) -> Option<DestChar> {
    match c {
        b'0'..=b'9' => {
            Some(DestChar { digit: Some(c - b'0'), bit: false, high: false, custom: false })
        }
        b'A'..=b'J' => {
            Some(DestChar { digit: Some(c - b'A'), bit: true, high: false, custom: true })
        }
        b'K' => Some(DestChar { digit: None, bit: true, high: false, custom: true }),
        b'L' => Some(DestChar { digit: None, bit: false, high: false, custom: false }),
        b'P'..=b'Y' => {
            Some(DestChar { digit: Some(c - b'P'), bit: true, high: true, custom: false })
        }
        b'Z' => Some(DestChar { digit: None, bit: true, high: true, custom: false }),
        _ => None,
    }
}

/// Parse a Mic-E frame. `dest` is the AX.25 destination callsign; `rest` is
/// the information field with its data type identifier already removed.
pub(crate) fn parse(dest: &str, rest: &[u8]) -> Result<AprsData> {
    // The SSID of the destination address carries the generic digipeater path
    // in some Mic-E dialects; the latitude is in the callsign part alone.
    let dest = dest.split('-').next().unwrap_or(dest).as_bytes();
    if dest.len() < 6 {
        return Err(AprsError::Malformed("Mic-E destination too short"));
    }
    if rest.len() < 8 {
        return Err(AprsError::Malformed("truncated Mic-E frame"));
    }

    let mut d = Vec::with_capacity(6);
    for &c in &dest[..6] {
        d.push(dest_char(c).ok_or(AprsError::Malformed("Mic-E destination character"))?);
    }

    // ── Latitude, entirely from the destination address ──
    //
    // Blanks come off the right, exactly as in an uncompressed report, and
    // mean the same thing.
    let ambiguity = d.iter().rev().take_while(|c| c.digit.is_none()).count() as u8;
    if ambiguity > 4 {
        return Err(AprsError::Malformed("Mic-E latitude is entirely blank"));
    }
    let digits: Vec<f64> = d.iter().map(|c| c.digit.unwrap_or(0).into()).collect();
    let deg = digits[0] * 10.0 + digits[1];
    let min = digits[2] * 10.0 + digits[3] + digits[4] / 10.0 + digits[5] / 100.0;
    // The centre of the ambiguous square, like everywhere else.
    let min = min + crate::ambiguity_span_deg(ambiguity) * 60.0 / 2.0;
    let mut lat = deg + min / 60.0;
    let north = d[3].high;
    if !north {
        lat = -lat;
    }
    if !(-90.0..=90.0).contains(&lat) {
        return Err(AprsError::Malformed("Mic-E latitude out of range"));
    }

    // ── Longitude, from the information field plus one flag ──
    let mut lon_deg = i32::from(rest[0]) - 28;
    if d[4].high {
        lon_deg += 100;
    }
    // Two folded ranges: the encoding cannot represent 180–189 and 190–199
    // directly, so it wraps them into values that would otherwise be illegal.
    if (180..=189).contains(&lon_deg) {
        lon_deg -= 80;
    } else if (190..=199).contains(&lon_deg) {
        lon_deg -= 190;
    }
    if !(0..=179).contains(&lon_deg) {
        return Err(AprsError::Malformed("Mic-E longitude degrees out of range"));
    }
    let mut lon_min = i32::from(rest[1]) - 28;
    if lon_min >= 60 {
        lon_min -= 60;
    }
    let lon_hun = i32::from(rest[2]) - 28;
    if !(0..=59).contains(&lon_min) || !(0..=99).contains(&lon_hun) {
        return Err(AprsError::Malformed("Mic-E longitude minutes out of range"));
    }
    let mut lon = f64::from(lon_deg) + (f64::from(lon_min) + f64::from(lon_hun) / 100.0) / 60.0;
    let west = d[5].high;
    if west {
        lon = -lon;
    }

    // ── Speed and course, split across three bytes on a decimal boundary ──
    let sp = i32::from(rest[3]) - 28;
    let dc = i32::from(rest[4]) - 28;
    let se = i32::from(rest[5]) - 28;
    let mut speed = sp * 10 + dc / 10;
    let mut course = (dc % 10) * 100 + se;
    if speed >= 800 {
        speed -= 800;
    }
    if course >= 400 {
        course -= 400;
    }

    let symbol = AprsSymbol::new(rest[7] as char, rest[6] as char);
    let mut p = Position {
        pos: AprsPosition { lat, lon, ambiguity },
        symbol,
        // A course of zero is "unknown" here too, and for the same reason.
        course_deg: (course > 0 && course < 360).then_some(course as u16),
        speed_kn: (speed >= 0).then_some(speed as f32),
        altitude_m: None,
        range_km: None,
        comment: String::new(),
        weather: None,
        timestamp: None,
        // Mic-E radios are messaging radios; the format has no bit to say
        // otherwise.
        messaging: true,
        mice: Some(message_code(&d)),
    };

    let mut tail = printable(&rest[8..]);
    // Altitude: three base-91 digits and a `}`, metres above a datum 10 km
    // below sea level. Usually first, but not required to be.
    //
    // Sliced by *character* rather than by byte offset: the status text is
    // whatever the sending station's character set was, and one non-ASCII byte
    // ahead of the altitude would otherwise cut the string mid-character.
    if let Some(brace) = tail.char_indices().position(|(_, c)| c == '}')
        && brace >= 3
    {
        let chars: Vec<(usize, char)> = tail.char_indices().collect();
        let b: Vec<u32> = chars[brace - 3..brace].iter().map(|&(_, c)| c as u32).collect();
        if b.iter().all(|c| (33..=126).contains(c)) {
            let v = (b[0] - 33) * 91 * 91 + (b[1] - 33) * 91 + (b[2] - 33);
            p.altitude_m = Some(f64::from(v) - 10_000.0);
            let (start, _) = chars[brace - 3];
            let end = chars[brace].0 + chars[brace].1.len_utf8();
            tail.replace_range(start..end, "");
        }
    }
    // The leading character of the status text is a manufacturer's identifier
    // — `>` a TH-D7, `]` a TM-D700, `` ` `` an Alinco or a Yaesu — and is not
    // part of what the operator typed.
    let tail = tail.trim_start_matches(['>', ']', '`', '\'']);
    p.comment = tail.trim().to_string();
    Ok(AprsData::Position(Box::new(p)))
}

/// The three message bits, read against whichever of the two tables the
/// destination characters selected.
fn message_code(d: &[DestChar]) -> MicEStatus {
    let bits = u8::from(d[0].bit) << 2 | u8::from(d[1].bit) << 1 | u8::from(d[2].bit);
    let custom = d[..3].iter().any(|c| c.custom);
    // All three bits clear is an emergency in both tables — the one code the
    // custom set does not get to redefine.
    if bits == 0 {
        return MicEStatus::Emergency;
    }
    if custom {
        // 111 is Custom-0 and the codes count downwards from there.
        return MicEStatus::Custom(7 - bits);
    }
    match bits {
        7 => MicEStatus::OffDuty,
        6 => MicEStatus::EnRoute,
        5 => MicEStatus::InService,
        4 => MicEStatus::Returning,
        3 => MicEStatus::Committed,
        2 => MicEStatus::Special,
        _ => MicEStatus::Priority,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse as parse_info;

    /// A jeep at 33°25.64'N 112°07.44'W, doing 20 knots on 251°, En Route,
    /// with the symbol `/j`.
    ///
    /// The bytes are built here from the *encoder* half of the format — which
    /// is written down separately from the decoding rules this module
    /// implements, so the two agreeing is a real check rather than a circular
    /// one. Every one of the format's awkward corners is in this single frame:
    ///
    /// - the longitude's hundred-degree offset lives in the destination
    ///   address, so `(` is 12 on the wire and 112 on the map;
    /// - a longitude minute below ten has 60 added to keep the byte
    ///   printable, so 7 minutes travels as `_`;
    /// - the speed has 800 added for the same reason, so 20 knots travels as
    ///   an apparent 820;
    /// - and the course's hundreds digit has 4 added, so 251 travels as 651.
    #[test]
    fn a_constructed_mic_e_frame_decodes_through_every_offset() {
        let AprsData::Position(p) = parse_info("SS2UVT", b"`(_Hn\"Oj/").unwrap() else {
            panic!("not a position")
        };
        assert!((p.pos.lat - 33.427_333).abs() < 1e-5, "{}", p.pos.lat);
        assert!((p.pos.lon + 112.124).abs() < 1e-5, "{}", p.pos.lon);
        assert_eq!(p.course_deg, Some(251));
        assert_eq!(p.speed_kn, Some(20.0));
        assert_eq!(p.mice, Some(MicEStatus::EnRoute));
        assert_eq!(p.symbol, AprsSymbol::new('/', 'j'));
    }

    /// The half a decoder given only the information field gets wrong. The
    /// same information field under a destination whose flags are all clear
    /// has to land in the southern and eastern hemispheres, a hundred degrees
    /// away.
    #[test]
    fn the_destination_address_decides_the_hemisphere() {
        let north = parse_info("SS2UVT", b"`(_Hn\"Oj/").unwrap();
        // The same latitude digits with every flag character replaced by its
        // plain digit: south, east, and no hundred-degree offset.
        let south = parse_info("SS2564", b"`(_Hn\"Oj/").unwrap();
        let (n, s) = (north.position().unwrap(), south.position().unwrap());
        assert!(n.pos.lat > 0.0 && s.pos.lat < 0.0, "north/south flag");
        assert!(n.pos.lon < 0.0 && s.pos.lon > 0.0, "east/west flag");
        // And the 100-degree offset the fifth character carries.
        assert!((n.pos.lon.abs() - s.pos.lon.abs() - 100.0).abs() < 1e-6);
    }

    /// The emergency code is the one worth getting right, and it is the same
    /// in both tables.
    #[test]
    fn all_bits_clear_is_an_emergency_in_either_table() {
        // `LLL` — blanks with every message bit clear.
        let d: Vec<DestChar> = "LLL456".bytes().map(|c| dest_char(c).unwrap()).collect();
        assert_eq!(message_code(&d), MicEStatus::Emergency);
        // `000` — digits, also all bits clear.
        let d: Vec<DestChar> = "000456".bytes().map(|c| dest_char(c).unwrap()).collect();
        assert_eq!(message_code(&d), MicEStatus::Emergency);
    }

    /// `A`–`K` select the custom table; `P`–`Z` the standard one. Reading the
    /// wrong table turns "Committed" into "Custom 4" and back.
    #[test]
    fn the_alphabet_used_picks_the_message_table() {
        let std: Vec<DestChar> = "PPP456".bytes().map(|c| dest_char(c).unwrap()).collect();
        assert_eq!(message_code(&std), MicEStatus::OffDuty);
        let cus: Vec<DestChar> = "AAA456".bytes().map(|c| dest_char(c).unwrap()).collect();
        assert_eq!(message_code(&cus), MicEStatus::Custom(0));
    }

    /// A destination whose characters are outside all three alphabets is not
    /// a Mic-E frame at all. `APRS` in the destination — which is what every
    /// non-Mic-E station sends — must be refused rather than decoded into a
    /// position somewhere in the Atlantic.
    #[test]
    fn a_non_mic_e_destination_is_refused() {
        assert!(parse_info("APRS", b"`(_Hn\"Oj/").is_err());
    }

    /// The altitude rides in the status text as three base-91 digits and a
    /// brace, and has to come out of the comment as well as into the field.
    #[test]
    fn the_altitude_is_lifted_out_of_the_status_text() {
        let AprsData::Position(p) = parse_info("SS2UVT", b"`(_Hn\"Oj/\"4T}hello").unwrap() else {
            panic!()
        };
        // "4T} — (34-33)*8281 + (52-33)*91 + (84-33) = 8281 + 1729 + 51
        assert!(p.altitude_m.is_some_and(|a| (a - 61.0).abs() < 1.0), "{:?}", p.altitude_m);
        assert_eq!(p.comment, "hello");
    }

    /// A comment carrying a byte the sender's character set produced, ahead of
    /// the altitude. The altitude is found by counting *characters* back from
    /// the brace, so a multi-byte one before it must not slice the string in
    /// half — which in Rust is a panic, on a receiver, from one frame off the
    /// air.
    #[test]
    fn a_non_ascii_status_text_does_not_split_a_character() {
        // 0xb0 is a degree sign in latin-1, which is what a lot of weather
        // comments carry; it becomes a three-byte replacement character.
        let mut info: Vec<u8> = b"`(_Hn\"Oj/".to_vec();
        info.extend_from_slice(&[0xb0]);
        info.extend_from_slice(b"\"4T}hi");
        let AprsData::Position(p) = parse_info("SS2UVT", &info).unwrap() else { panic!() };
        assert!(p.altitude_m.is_some_and(|a| (a - 61.0).abs() < 1.0), "{:?}", p.altitude_m);
        assert!(p.comment.ends_with("hi"), "{:?}", p.comment);
    }

    #[test]
    fn a_truncated_mic_e_frame_is_refused() {
        assert!(parse_info("SS2UVT", b"`(_Hn").is_err());
    }
}
