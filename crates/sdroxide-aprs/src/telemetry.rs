//! Telemetry (chapter 13): five analogue channels and eight bits, with a
//! sequence number so a receiver can tell a repeat from a new reading.
//!
//! The parameter names, units and scaling coefficients that make the numbers
//! mean something arrive separately, as messages ([`crate::MessageKind`]'s
//! `TelemetryDef`), because they change rarely and the readings do not.

use crate::{AprsError, Result, printable};

/// One telemetry report.
#[derive(Debug, Clone, PartialEq)]
pub struct Telemetry {
    /// The sequence number as sent — usually three digits, and `MIC` on the
    /// frames a Mic-E radio's own telemetry uses.
    pub seq: String,
    /// The five analogue channels, raw. Scaling them needs the `EQNS.`
    /// definition, which is a different frame.
    pub analog: [Option<f32>; 5],
    pub digital: [bool; 8],
    pub comment: String,
}

/// `T#SSS,111,222,333,444,555,00000000` — the `T` has already been eaten.
pub(crate) fn parse(rest: &[u8]) -> Result<Telemetry> {
    let s = printable(rest);
    let s = s.strip_prefix('#').unwrap_or(&s);
    let mut parts = s.split(',');
    let seq = parts.next().unwrap_or("").trim().to_string();
    if seq.is_empty() {
        return Err(AprsError::Malformed("telemetry sequence"));
    }
    let mut analog = [None; 5];
    for slot in &mut analog {
        *slot = parts.next().and_then(|p| p.trim().parse::<f32>().ok());
    }
    let mut digital = [false; 8];
    let bits = parts.next().unwrap_or("");
    let mut comment = String::new();
    // `char_indices`, not `enumerate`: the tail is sliced by byte, and a
    // comment with one non-ASCII character in it would otherwise be cut in the
    // middle of it.
    for (n, (i, c)) in bits.char_indices().enumerate() {
        match c {
            '0' | '1' if n < 8 => digital[n] = c == '1',
            // Anything past the eight bits is the comment, which the format
            // allows to run on without a separator.
            _ => {
                comment = bits[i..].trim().to_string();
                break;
            }
        }
    }
    let tail: Vec<&str> = parts.collect();
    if !tail.is_empty() {
        if !comment.is_empty() {
            comment.push(',');
        }
        comment.push_str(&tail.join(","));
    }
    Ok(Telemetry { seq, analog, digital, comment: comment.trim().to_string() })
}

#[cfg(test)]
mod tests {
    use crate::{AprsData, parse};

    #[test]
    fn the_reference_telemetry_example_decodes() {
        let AprsData::Telemetry(t) = parse("APRS", b"T#005,199,000,255,073,123,01101001").unwrap()
        else {
            panic!()
        };
        assert_eq!(t.seq, "005");
        assert_eq!(t.analog[0], Some(199.0));
        assert_eq!(t.analog[4], Some(123.0));
        assert_eq!(t.digital, [false, true, true, false, true, false, false, true]);
    }

    /// A station that sends fewer than five channels must leave the rest
    /// absent rather than filling them with zeroes.
    #[test]
    fn missing_channels_stay_missing() {
        let AprsData::Telemetry(t) = parse("APRS", b"T#012,100,200").unwrap() else { panic!() };
        assert_eq!(t.analog[1], Some(200.0));
        assert_eq!(t.analog[2], None);
    }
}
