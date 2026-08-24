//! Positions: the three ways a station says where it is, plus the objects and
//! items it says it about something else.
//!
//! Reference 1.0.1 chapter 6 (data formats), 8 (compressed), 9 (altitude and
//! the comment extensions), 11 (objects and items); the `!DAO!` precision
//! extension is from addendum 1.2.

use sdroxide_types::{AprsPosition, AprsSymbol, AprsWeather};

use crate::{AprsData, AprsError, Result, printable, weather};

/// A timestamp as the protocol carries it.
///
/// Not converted to a Unix time here: this crate has no clock, and the
/// conversion needs one — a report stamped `211245z` is the 21st of *some*
/// month, and only a receiver that knows today's date can say which. The
/// caller stamps arrival time from its own clock and keeps this for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    /// Day of the month, when the form carried one.
    pub day: Option<u8>,
    pub hour: u8,
    pub minute: u8,
    pub second: Option<u8>,
    /// UTC rather than the sender's local time.
    pub zulu: bool,
}

/// A position report, however it was encoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub pos: AprsPosition,
    pub symbol: AprsSymbol,
    /// Course over ground, degrees true.
    pub course_deg: Option<u16>,
    /// Speed over ground, in knots — the unit the protocol carries. Converting
    /// it here would throw away the exactness of the small integers it is
    /// actually sent as.
    pub speed_kn: Option<f32>,
    pub altitude_m: Option<f64>,
    /// The radio range a station claims, in kilometres — the `RNG` extension,
    /// and the compressed format's `{` course/speed replacement.
    pub range_km: Option<f64>,
    /// Everything after the position and its extension.
    pub comment: String,
    /// Present when the symbol says this is a weather station.
    pub weather: Option<AprsWeather>,
    pub timestamp: Option<Timestamp>,
    /// The station accepts messages (`=`/`@` rather than `!`//`).
    pub messaging: bool,
    /// The Mic-E message the operator has selected on the radio's front panel
    /// — "En Route", "Committed", "EMERGENCY". Only that format carries one.
    pub mice: Option<crate::MicEStatus>,
}

impl Position {
    fn at(pos: AprsPosition, symbol: AprsSymbol) -> Position {
        Position {
            pos,
            symbol,
            course_deg: None,
            speed_kn: None,
            altitude_m: None,
            range_km: None,
            comment: String::new(),
            weather: None,
            timestamp: None,
            messaging: false,
            mice: None,
        }
    }
}

/// Parse a position report body — everything after the data type identifier.
pub(crate) fn parse_position(rest: &[u8], timestamped: bool) -> Result<Position> {
    let (ts, rest) = if timestamped {
        if rest.len() < 7 {
            return Err(AprsError::Malformed("truncated timestamp"));
        }
        (parse_timestamp(&rest[..7]), &rest[7..])
    } else {
        (None, rest)
    };
    let mut p = parse_position_body(rest)?;
    p.timestamp = ts;
    Ok(p)
}

/// The position itself plus whatever follows it, in either encoding.
///
/// Which encoding is decided by the first byte, and it is unambiguous: an
/// uncompressed report starts with the first digit of the latitude, and a
/// compressed one starts with its symbol table character, which is never a
/// digit.
fn parse_position_body(rest: &[u8]) -> Result<Position> {
    match rest.first() {
        None => Err(AprsError::Malformed("no position")),
        Some(c) if c.is_ascii_digit() => parse_uncompressed(rest),
        Some(_) => parse_compressed(rest),
    }
}

/// `DDMM.hhN/DDDMM.hhW$` — 19 bytes, then the extension and the comment.
fn parse_uncompressed(rest: &[u8]) -> Result<Position> {
    if rest.len() < 19 {
        return Err(AprsError::Malformed("truncated uncompressed position"));
    }
    let (lat, ambiguity) = parse_lat(&rest[0..8])?;
    let table = rest[8] as char;
    let lon = parse_lon(&rest[9..18])?;
    let code = rest[18] as char;
    let mut p = Position::at(AprsPosition { lat, lon, ambiguity }, AprsSymbol::new(table, code));
    finish(&mut p, &rest[19..]);
    Ok(p)
}

/// `DDMM.hh` plus a hemisphere character, with trailing digits possibly
/// blanked out to say "somewhere in this square".
///
/// The blanks are the whole subtlety. A station that reports `4903.5 N` is
/// giving a tenth of a minute of slop, one that reports `49  .  N` a whole
/// degree, and reading a blank as a zero would put it up to half a degree
/// south of where it said it was — silently, and only on the stations that
/// deliberately fuzzed themselves.
fn parse_lat(b: &[u8]) -> Result<(f64, u8)> {
    let s = std::str::from_utf8(&b[..7]).map_err(|_| AprsError::Malformed("latitude"))?;
    let deg: f64 = s[0..2].parse().map_err(|_| AprsError::Malformed("latitude degrees"))?;
    let (min, ambiguity) = parse_minutes(&s[2..7])?;
    let mut lat = deg + min / 60.0;
    match b[7] {
        b'N' | b'n' => {}
        b'S' | b's' => lat = -lat,
        _ => return Err(AprsError::Malformed("latitude hemisphere")),
    }
    if !(-90.0..=90.0).contains(&lat) {
        return Err(AprsError::Malformed("latitude out of range"));
    }
    Ok((lat, ambiguity))
}

/// `DDDMM.hh` plus a hemisphere character.
fn parse_lon(b: &[u8]) -> Result<f64> {
    let s = std::str::from_utf8(&b[..8]).map_err(|_| AprsError::Malformed("longitude"))?;
    let deg: f64 = s[0..3].parse().map_err(|_| AprsError::Malformed("longitude degrees"))?;
    let (min, _) = parse_minutes(&s[3..8])?;
    let mut lon = deg + min / 60.0;
    match b[8] {
        b'E' | b'e' => {}
        b'W' | b'w' => lon = -lon,
        _ => return Err(AprsError::Malformed("longitude hemisphere")),
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err(AprsError::Malformed("longitude out of range"));
    }
    Ok(lon)
}

/// `MM.hh`, blanks allowed from the right. Returns the *centre* of the
/// ambiguous span and how many digits were blanked.
fn parse_minutes(s: &str) -> Result<(f64, u8)> {
    let c: Vec<char> = s.chars().collect();
    if c.len() != 5 || c[2] != '.' {
        return Err(AprsError::Malformed("minutes"));
    }
    let digits = [c[0], c[1], c[3], c[4]];
    // Blanks only ever come off the right-hand end, so counting them is enough
    // and the count is the ambiguity.
    let blanks = digits.iter().rev().take_while(|d| **d == ' ' || **d == '.').count() as u8;
    let mut value = 0.0f64;
    let weights = [10.0, 1.0, 0.1, 0.01];
    for (d, w) in digits.iter().zip(weights) {
        if *d == ' ' {
            continue;
        }
        let v = d.to_digit(10).ok_or(AprsError::Malformed("minutes"))? as f64;
        value += v * w;
    }
    // Half the blanked span, so an ambiguous report lands in the middle of
    // the square it describes rather than in its bottom-left corner.
    let span = crate::ambiguity_span_deg(blanks) * 60.0;
    Ok((value + span / 2.0, blanks))
}

/// The 13-byte compressed form: table, four base-91 latitude bytes, four
/// longitude bytes, symbol code, two course/speed bytes and a type byte.
fn parse_compressed(rest: &[u8]) -> Result<Position> {
    if rest.len() < 13 {
        return Err(AprsError::Malformed("truncated compressed position"));
    }
    let table = rest[0] as char;
    let y = base91(&rest[1..5]).ok_or(AprsError::Malformed("compressed latitude"))?;
    let x = base91(&rest[5..9]).ok_or(AprsError::Malformed("compressed longitude"))?;
    let code = rest[9] as char;
    let lat = 90.0 - y / 380_926.0;
    let lon = -180.0 + x / 190_463.0;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(AprsError::Malformed("compressed position out of range"));
    }
    // An overlay in the compressed form is carried as `a`–`j` rather than as
    // the digit itself: the table byte has to stay distinguishable from a
    // latitude byte, and the digits are already spoken for.
    let table = match table {
        'a'..='j' => char::from(b'0' + (table as u8 - b'a')),
        t => t,
    };
    let mut p = Position::at(AprsPosition { lat, lon, ambiguity: 0 }, AprsSymbol::new(table, code));
    let (c, s, t) = (rest[10], rest[11], rest[12]);
    if c != b' ' {
        if c == b'{' {
            // Radio range rather than movement: 2 × 1.08^s miles.
            let miles = 2.0 * 1.08f64.powi(i32::from(s.saturating_sub(33)));
            p.range_km = Some(miles * 1.609_344);
        } else if (t.saturating_sub(33)) & 0b0001_1000 == 0b0001_0000 {
            // The type byte says the fix came from a GGA sentence, which means
            // these two bytes are an altitude in feet rather than a course.
            let n = f64::from(c.saturating_sub(33)) * 91.0 + f64::from(s.saturating_sub(33));
            p.altitude_m = Some(1.002f64.powf(n) * 0.3048);
        } else {
            p.course_deg = Some(u16::from(c.saturating_sub(33)) * 4 % 360);
            p.speed_kn = Some(1.08f32.powi(i32::from(s.saturating_sub(33))) - 1.0);
        }
    }
    finish(&mut p, &rest[13..]);
    Ok(p)
}

/// Four base-91 bytes as a number.
fn base91(b: &[u8]) -> Option<f64> {
    let mut v = 0f64;
    for &c in b {
        if !(33..=124).contains(&c) {
            return None;
        }
        v = v * 91.0 + f64::from(c - 33);
    }
    Some(v)
}

/// The comment: its extensions, its weather, its altitude, and what is left.
fn finish(p: &mut Position, tail: &[u8]) {
    // A weather station's "comment" is not a comment at all — the symbol code
    // `_` means every byte after the position is a weather report, starting
    // with the wind in the course/speed field's place.
    if p.symbol.code == '_' {
        let (w, rest) = weather::parse_in_comment(tail);
        p.weather = Some(w);
        p.comment = printable(rest).trim().to_string();
        return;
    }

    let mut tail = tail;
    // Chapter 7: one optional seven-byte extension, immediately after the
    // symbol. Only these four exist, and each is recognisable on sight.
    if tail.len() >= 7 {
        let ext = &tail[..7];
        if ext[3] == b'/' && ext[..3].iter().all(u8::is_ascii_digit) {
            // `CSE/SPD` — course in degrees, speed in knots. A course of 000
            // means "unknown", which is not the same as due north.
            let course: u16 = printable(&ext[..3]).parse().unwrap_or(0);
            if course > 0 {
                p.course_deg = Some(course % 360);
            }
            if let Ok(spd) = printable(&ext[4..7]).parse::<f32>() {
                p.speed_kn = Some(spd);
            }
            tail = &tail[7..];
        } else if ext.starts_with(b"PHG") {
            // Power-height-gain. The useful half is the height, which gives a
            // range; the panel shows the digits themselves.
            tail = &tail[7..];
        } else if ext.starts_with(b"RNG") {
            if let Ok(miles) = printable(&ext[3..7]).parse::<f64>() {
                p.range_km = Some(miles * 1.609_344);
            }
            tail = &tail[7..];
        } else if ext.starts_with(b"DFS") {
            tail = &tail[7..];
        }
    }

    let mut comment = printable(tail);
    // Chapter 9: an altitude may appear anywhere in the comment as `/A=nnnnnn`
    // in feet. Six digits, and negative altitudes are written as a six-digit
    // number by the same convention GPS receivers use, so a leading `-` is
    // not part of it.
    if let Some(i) = comment.find("/A=") {
        let digits: String = comment[i + 3..].chars().take(6).collect();
        if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(feet) = digits.parse::<f64>() {
                p.altitude_m = Some(feet * 0.3048);
            }
            comment.replace_range(i..i + 9, "");
        }
    }
    apply_dao(p, &mut comment);
    p.comment = comment.trim().to_string();
}

/// The `!DAO!` datum/precision extension (addendum 1.2).
///
/// It adds a third digit to the minutes, which is about two metres — the
/// difference between a car parked on one side of a street and the other.
/// Two forms: an upper-case datum character means the two bytes are ASCII
/// digits of thousandths of a minute, a lower-case one means they are base-91
/// hundredths.
fn apply_dao(p: &mut Position, comment: &mut String) {
    // Every `!`, not just the first: an operator's own exclamation mark ahead
    // of the extension would otherwise hide it. Measured in characters, and
    // the byte span to cut worked out from them — a comment may carry
    // whatever character set the sending station used.
    let mut found = None;
    for (i, _) in comment.match_indices('!') {
        let b: Vec<char> = comment[i..].chars().take(5).collect();
        if b.len() == 5 && b[4] == '!' {
            let end = i + b.iter().map(|c| c.len_utf8()).sum::<usize>();
            found = Some((i, end, b));
            break;
        }
    }
    let Some((i, end, b)) = found else { return };
    let (d, a, o) = (b[1], b[2], b[3]);
    let (dlat, dlon) = if d.is_ascii_uppercase() {
        let (da, dob) = (a.to_digit(10), o.to_digit(10));
        match (da, dob) {
            (Some(da), Some(dob)) => (f64::from(da) * 0.001, f64::from(dob) * 0.001),
            _ => return,
        }
    } else if d.is_ascii_lowercase() {
        let val = |c: char| {
            let v = c as u32;
            (33..=126).contains(&v).then(|| f64::from(v - 33) / 91.0 * 0.01)
        };
        match (val(a), val(o)) {
            (Some(va), Some(vo)) => (va, vo),
            _ => return,
        }
    } else {
        return;
    };
    // The extra precision extends the number away from zero, so it follows
    // the hemisphere rather than always adding.
    p.pos.lat += if p.pos.lat < 0.0 { -dlat } else { dlat } / 60.0;
    p.pos.lon += if p.pos.lon < 0.0 { -dlon } else { dlon } / 60.0;
    comment.replace_range(i..end, "");
}

/// `DDHHMMz`, `DDHHMM/` or `HHMMSSh`.
fn parse_timestamp(b: &[u8]) -> Option<Timestamp> {
    let s = std::str::from_utf8(b).ok()?;
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u8>().ok();
    match s.as_bytes()[6] {
        b'z' | b'Z' => Some(Timestamp {
            day: n(0..2),
            hour: n(2..4)?,
            minute: n(4..6)?,
            second: None,
            zulu: true,
        }),
        b'/' => Some(Timestamp {
            day: n(0..2),
            hour: n(2..4)?,
            minute: n(4..6)?,
            second: None,
            zulu: false,
        }),
        b'h' | b'H' => Some(Timestamp {
            day: None,
            hour: n(0..2)?,
            minute: n(2..4)?,
            second: n(4..6),
            zulu: true,
        }),
        _ => None,
    }
}

/// `;NAME_____*DDHHMMz` then a position (chapter 11).
pub(crate) fn parse_object(rest: &[u8]) -> Result<AprsData> {
    if rest.len() < 17 {
        return Err(AprsError::Malformed("truncated object"));
    }
    // Exactly nine characters, space-padded — an object name is a fixed field,
    // unlike an item's.
    let name = printable(&rest[..9]).trim().to_string();
    let live = match rest[9] {
        b'*' => true,
        b'_' => false,
        _ => return Err(AprsError::Malformed("object live/killed flag")),
    };
    let ts = parse_timestamp(&rest[10..17]);
    let mut pos = parse_position_body(&rest[17..])?;
    pos.timestamp = ts;
    Ok(AprsData::Object { name, live, pos: Box::new(pos) })
}

/// `)NAME!` or `)NAME_` then a position (chapter 11.1). The name is 3 to 9
/// characters and is terminated by the flag rather than padded to a width.
pub(crate) fn parse_item(rest: &[u8]) -> Result<AprsData> {
    let end = rest
        .iter()
        .take(10)
        .position(|&c| c == b'!' || c == b'_')
        .ok_or(AprsError::Malformed("item name has no terminator"))?;
    if !(3..=9).contains(&end) {
        return Err(AprsError::Malformed("item name length"));
    }
    let name = printable(&rest[..end]).trim().to_string();
    let live = rest[end] == b'!';
    let pos = parse_position_body(&rest[end + 1..])?;
    Ok(AprsData::Item { name, live, pos: Box::new(pos) })
}

// ── Encoding ──────────────────────────────────────────────────────────────

/// `DDMM.hhN`.
fn fmt_lat(lat: f64) -> String {
    let hemi = if lat < 0.0 { 'S' } else { 'N' };
    let a = lat.abs().min(89.999_99);
    let deg = a.trunc();
    let min = (a - deg) * 60.0;
    format!("{:02.0}{:05.2}{hemi}", deg, min)
}

/// `DDDMM.hhW`.
fn fmt_lon(lon: f64) -> String {
    let hemi = if lon < 0.0 { 'W' } else { 'E' };
    let a = lon.abs().min(179.999_99);
    let deg = a.trunc();
    let min = (a - deg) * 60.0;
    format!("{:03.0}{:05.2}{hemi}", deg, min)
}

/// An uncompressed position report with no timestamp.
///
/// `messaging` picks the data type identifier, and it is not cosmetic: a
/// station that sends `!` is telling the channel not to send it messages, and
/// well-behaved clients grey out the reply button.
#[must_use]
pub fn encode_position(
    pos: AprsPosition,
    symbol: AprsSymbol,
    comment: &str,
    messaging: bool,
) -> String {
    format!(
        "{}{}{}{}{}{}",
        if messaging { '=' } else { '!' },
        fmt_lat(pos.lat),
        symbol.table,
        fmt_lon(pos.lon),
        symbol.code,
        clean_comment(comment)
    )
}

/// The same with a `DDHHMMz` timestamp, which the caller supplies from its own
/// clock.
#[must_use]
pub fn encode_position_ts(
    pos: AprsPosition,
    symbol: AprsSymbol,
    comment: &str,
    messaging: bool,
    (day, hour, minute): (u8, u8, u8),
) -> String {
    format!(
        "{}{day:02}{hour:02}{minute:02}z{}{}{}{}{}",
        if messaging { '@' } else { '/' },
        fmt_lat(pos.lat),
        symbol.table,
        fmt_lon(pos.lon),
        symbol.code,
        clean_comment(comment)
    )
}

/// The 13-byte compressed form — a third of the air time of the uncompressed
/// one, and more precise, which is why it is what a beacon should send.
#[must_use]
pub fn encode_compressed_position(
    pos: AprsPosition,
    symbol: AprsSymbol,
    comment: &str,
    messaging: bool,
) -> String {
    let mut s = String::with_capacity(20);
    s.push(if messaging { '=' } else { '!' });
    // An overlay travels as `a`–`j` here so the table byte can never be
    // mistaken for the first byte of an uncompressed latitude.
    s.push(match symbol.table {
        c @ '0'..='9' => char::from(b'a' + (c as u8 - b'0')),
        c => c,
    });
    let y = ((90.0 - pos.lat) * 380_926.0).round().clamp(0.0, 91.0f64.powi(4) - 1.0) as u32;
    let x = ((180.0 + pos.lon) * 190_463.0).round().clamp(0.0, 91.0f64.powi(4) - 1.0) as u32;
    push_base91(&mut s, y);
    push_base91(&mut s, x);
    s.push(symbol.code);
    // No course, no speed, and the type byte says so: `sT` of `space space`
    // with a current, other-source, compressed-origin type.
    s.push(' ');
    s.push(' ');
    s.push('!');
    s.push_str(&clean_comment(comment));
    s
}

/// An object report, live or killed.
#[must_use]
pub fn encode_object(
    name: &str,
    live: bool,
    pos: AprsPosition,
    symbol: AprsSymbol,
    comment: &str,
    (day, hour, minute): (u8, u8, u8),
) -> String {
    // Exactly nine characters: an object name is a fixed-width field, and a
    // short one that is not padded shifts everything after it.
    let mut n: String =
        name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(9).collect();
    while n.chars().count() < 9 {
        n.push(' ');
    }
    format!(
        ";{n}{}{day:02}{hour:02}{minute:02}z{}{}{}{}{}",
        if live { '*' } else { '_' },
        fmt_lat(pos.lat),
        symbol.table,
        fmt_lon(pos.lon),
        symbol.code,
        clean_comment(comment)
    )
}

/// Four base-91 digits, most significant first.
fn push_base91(s: &mut String, mut v: u32) {
    let mut d = [0u8; 4];
    for i in (0..4).rev() {
        d[i] = (v % 91) as u8;
        v /= 91;
    }
    for b in d {
        s.push(char::from(b + 33));
    }
}

/// A comment as it may go on the air: printable ASCII only, and never long
/// enough to burst the frame.
///
/// `|` and `~` are excluded as well as the control characters: both are TNC
/// stream-switch characters, and one in a comment can put a receiving TNC into
/// a mode its operator has to power-cycle it out of.
fn clean_comment(c: &str) -> String {
    c.chars()
        .filter(|&ch| ch.is_ascii_graphic() && ch != '|' && ch != '~' || ch == ' ')
        .take(43)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from the protocol reference itself (chapter 6): a station
    /// in New Jersey, on the primary table's house symbol.
    #[test]
    fn the_reference_uncompressed_example_decodes() {
        let d = crate::parse("APRS", b"!4903.50N/07201.75W-Test 001234").unwrap();
        let p = d.position().unwrap();
        assert!((p.pos.lat - 49.058_333).abs() < 1e-5, "{}", p.pos.lat);
        assert!((p.pos.lon + 72.029_166).abs() < 1e-5, "{}", p.pos.lon);
        assert_eq!(p.pos.ambiguity, 0);
        assert_eq!(p.symbol, AprsSymbol::new('/', '-'));
        assert_eq!(p.comment, "Test 001234");
        assert!(!p.messaging, "`!` says the station takes no messages");
    }

    /// Ambiguity is the field that goes wrong silently. Each blanked digit has
    /// to widen the square *and* move the point to its centre.
    #[test]
    fn blanked_digits_widen_the_square_and_centre_the_point() {
        // 4903.5_ — a tenth of a minute of slop, centred half of that up.
        let d = crate::parse("APRS", b"!4903.5 N/07201.75W-").unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.pos.ambiguity, 1);
        assert!((p.pos.lat - (49.0 + 3.55 / 60.0)).abs() < 1e-9, "{}", p.pos.lat);
        // A whole degree of slop: 49xx -> centred on 49°30'.
        let d = crate::parse("APRS", b"!49  .  N/07201.75W-").unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.pos.ambiguity, 4);
        assert!((p.pos.lat - 49.5).abs() < 1e-9, "{}", p.pos.lat);
    }

    /// The compressed position built from the format's own arithmetic: four
    /// base-91 digits of `(90 - lat) x 380926` and `(180 + lon) x 190463`.
    /// `5L!!` is 40.5 degrees down from the north pole exactly — 380926 is
    /// four times 95231.5, so half a degree lands on a whole count — and
    /// `<*e7` is 107.25 east of the antimeridian to within one count, which
    /// is the closest the format can get to a quarter degree. Both come out
    /// on those numbers if and only if the two scaling constants are right,
    /// which is what makes them worth pinning.
    #[test]
    fn a_compressed_position_decodes_on_the_format_s_own_arithmetic() {
        let d = crate::parse("APRS", b"!/5L!!<*e7> sT").unwrap();
        let p = d.position().unwrap();
        assert!((p.pos.lat - 49.5).abs() < 1e-6, "{}", p.pos.lat);
        // One count of longitude is 1/190463 of a degree, about 60 cm.
        assert!((p.pos.lon + 72.75).abs() < 1e-5, "{}", p.pos.lon);
        assert_eq!(p.symbol, AprsSymbol::new('/', '>'));
        // A space in the first of the two extension bytes means the sender
        // put nothing there. Reading it as base-91 would give this parked car
        // a course of 340 degrees and a speed of a hundred knots.
        assert_eq!(p.course_deg, None);
        assert_eq!(p.speed_kn, None);
    }

    /// The same position with the course/speed bytes filled in: course is the
    /// byte times four degrees, speed is 1.08 to the byte's power minus one,
    /// and the type byte says which of the three meanings the pair has.
    #[test]
    fn the_compressed_course_speed_bytes_decode() {
        let d = crate::parse("APRS", b"!/5L!!<*e7>7P[").unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.course_deg, Some(88), "(0x37 - 33) * 4");
        assert!(p.speed_kn.is_some_and(|s| (s - 36.2).abs() < 0.1), "{:?}", p.speed_kn);
        assert_eq!(p.altitude_m, None);
    }

    /// The type byte's bit 4 changes what the same two bytes mean: with it
    /// set they are an altitude in feet, not a course and a speed. A decoder
    /// that ignores it puts an airliner on the ground doing 300 knots on a
    /// heading it never flew.
    #[test]
    fn the_type_byte_switches_the_pair_to_an_altitude() {
        // `1` on the type byte is 0x31 - 33 = 16, which is the GGA pattern.
        let d = crate::parse("APRS", b"!/5L!!<*e7>S]1").unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.course_deg, None);
        assert_eq!(p.speed_kn, None);
        assert!(p.altitude_m.is_some_and(|a| a > 3000.0), "{:?}", p.altitude_m);
    }

    /// Course/speed and altitude, the two extensions that actually appear.
    #[test]
    fn the_course_speed_extension_and_an_altitude_are_lifted_out_of_the_comment() {
        let d = crate::parse("APRS", b"!4903.50N/07201.75W>088/036/A=001234Rolling").unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.course_deg, Some(88));
        assert_eq!(p.speed_kn, Some(36.0));
        assert!(p.altitude_m.is_some_and(|a| (a - 376.1).abs() < 0.5), "{:?}", p.altitude_m);
        assert_eq!(p.comment, "Rolling", "the extensions must not be left in the comment");
    }

    /// A course of `000` means "not known", not "due north". Reading it as a
    /// heading points every stationary car on the map at the pole.
    #[test]
    fn a_zero_course_is_no_course() {
        let d = crate::parse("APRS", b"!4903.50N/07201.75W>000/000parked").unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.course_deg, None);
        assert_eq!(p.speed_kn, Some(0.0));
    }

    /// Round trip through our own encoder, which is what the beacon sends.
    #[test]
    fn an_encoded_position_reads_back_as_itself() {
        let pos = AprsPosition { lat: 48.208_8, lon: 16.372_1, ambiguity: 0 };
        let sym = AprsSymbol::new('/', '-');
        let f = encode_position(pos, sym, "sdroxide", true);
        let d = crate::parse("APRS", f.as_bytes()).unwrap();
        let p = d.position().unwrap();
        assert!(p.messaging);
        assert_eq!(p.symbol, sym);
        assert_eq!(p.comment, "sdroxide");
        // The uncompressed form carries hundredths of a minute, so agreement
        // to about ten metres is all there is to agree to.
        assert!((p.pos.lat - pos.lat).abs() < 1e-4, "{} vs {}", p.pos.lat, pos.lat);
        assert!((p.pos.lon - pos.lon).abs() < 1e-4, "{} vs {}", p.pos.lon, pos.lon);
    }

    /// The compressed encoder is the one a beacon should use: same test, and
    /// tighter, because the format is more precise than the one it replaces.
    #[test]
    fn an_encoded_compressed_position_reads_back_as_itself() {
        let pos = AprsPosition { lat: -33.868_8, lon: 151.209_3, ambiguity: 0 };
        let sym = AprsSymbol::new('/', '>');
        let f = encode_compressed_position(pos, sym, "mobile", false);
        assert_eq!(f.len(), 1 + 13 + 6);
        let d = crate::parse("APRS", f.as_bytes()).unwrap();
        let p = d.position().unwrap();
        assert_eq!(p.symbol, sym);
        assert_eq!(p.comment, "mobile");
        assert!((p.pos.lat - pos.lat).abs() < 1e-5, "{} vs {}", p.pos.lat, pos.lat);
        assert!((p.pos.lon - pos.lon).abs() < 1e-5, "{} vs {}", p.pos.lon, pos.lon);
    }

    /// A compressed overlay travels as a letter and has to come back a digit,
    /// or every overlaid digipeater on the map is drawn with a stray `f`.
    #[test]
    fn a_compressed_overlay_round_trips_as_a_digit() {
        let pos = AprsPosition { lat: 10.0, lon: 20.0, ambiguity: 0 };
        let sym = AprsSymbol::new('7', '#');
        let f = encode_compressed_position(pos, sym, "", false);
        assert_eq!(f.as_bytes()[1], b'h', "overlay 7 travels as 'a'+7");
        let d = crate::parse("APRS", f.as_bytes()).unwrap();
        assert_eq!(d.position().unwrap().symbol, sym);
    }

    /// An object is a fixed nine-character name; a short one has to be padded
    /// or every field behind it shifts.
    #[test]
    fn an_object_round_trips_with_a_short_name() {
        let pos = AprsPosition { lat: 51.5, lon: -0.1, ambiguity: 0 };
        let f = encode_object("NET", true, pos, AprsSymbol::new('/', 'r'), "80m net", (21, 12, 45));
        let d = crate::parse("APRS", f.as_bytes()).unwrap();
        match d {
            AprsData::Object { name, live, pos: p } => {
                assert_eq!(name, "NET");
                assert!(live);
                assert_eq!(p.comment, "80m net");
                assert_eq!(p.timestamp.unwrap().day, Some(21));
            }
            other => panic!("{other:?}"),
        }
    }

    /// An item's name is terminated, not padded — the one structural
    /// difference from an object, and getting it wrong shifts the position.
    #[test]
    fn an_item_takes_its_name_up_to_the_terminator() {
        let d = crate::parse("APRS", b")AID!4903.50N/07201.75Wa").unwrap();
        match d {
            AprsData::Item { name, live, pos } => {
                assert_eq!(name, "AID");
                assert!(live);
                assert_eq!(pos.symbol.code, 'a');
            }
            other => panic!("{other:?}"),
        }
    }

    /// `!DAO!` is two more metres of precision hiding in the comment. It has
    /// to be applied *and* removed — left in place it is four bytes of noise
    /// on every line of the station list.
    #[test]
    fn the_dao_extension_is_applied_and_stripped() {
        let plain = crate::parse("APRS", b"!4903.50N/07201.75W-hi").unwrap();
        let dao = crate::parse("APRS", b"!4903.50N/07201.75W-hi!W55!").unwrap();
        let (a, b) = (plain.position().unwrap(), dao.position().unwrap());
        assert_eq!(b.comment, "hi");
        // Five thousandths of a minute further from the equator and further
        // from Greenwich: this station is north and *west*, so the longitude
        // moves negative.
        assert!((b.pos.lat - a.pos.lat - 0.005 / 60.0).abs() < 1e-12);
        assert!((b.pos.lon - a.pos.lon + 0.005 / 60.0).abs() < 1e-12);
    }

    /// In the southern and western hemispheres the extra precision has to go
    /// the other way, or every DAO station in Australia jumps north-east.
    #[test]
    fn dao_precision_follows_the_hemisphere() {
        let plain = crate::parse("APRS", b"!3352.10S/15112.60E>").unwrap();
        let dao = crate::parse("APRS", b"!3352.10S/15112.60E>!W99!").unwrap();
        let (a, b) = (plain.position().unwrap(), dao.position().unwrap());
        assert!(b.pos.lat < a.pos.lat, "south is more negative");
        assert!(b.pos.lon > a.pos.lon, "east is more positive");
    }

    /// The same character-boundary trap as Mic-E's altitude: a comment with a
    /// non-ASCII byte in it, and a `!DAO!` behind it.
    #[test]
    fn a_non_ascii_comment_does_not_split_a_character_around_dao() {
        let mut info: Vec<u8> = b"!4903.50N/07201.75W-25".to_vec();
        info.extend_from_slice(&[0xb0]); // a latin-1 degree sign
        info.extend_from_slice(b"C!W55!");
        let d = crate::parse("APRS", &info).unwrap();
        let p = d.position().unwrap();
        assert!(p.comment.ends_with('C'), "{:?}", p.comment);
        assert!(!p.comment.contains("!W55!"), "the extension must be stripped: {:?}", p.comment);
    }

    /// A truncated frame must be an error, never a position at (0, 0) — the
    /// Gulf of Guinea is where every unvalidated APRS map has a cluster.
    #[test]
    fn a_truncated_position_is_refused() {
        assert!(crate::parse("APRS", b"!4903.50N/072").is_err());
        assert!(crate::parse("APRS", b"!/5L!!<*").is_err());
        assert!(crate::parse("APRS", b"@2107").is_err());
    }
}
