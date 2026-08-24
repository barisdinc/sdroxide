//! Weather reports (chapter 12), in the two places they appear: folded into a
//! position report whose symbol is the weather station, and standing alone in
//! a `_` frame from a station that has already said where it is.
//!
//! # Units
//!
//! The wire units are the ones a 1990s American weather station produced —
//! miles per hour, degrees Fahrenheit, hundredths of an inch of rain, tenths
//! of a hectopascal — and they are converted here rather than at the panel, so
//! there is one place where the conversion can be wrong.

use sdroxide_types::AprsWeather;

use crate::{AprsError, Result};

const MPH_TO_MS: f32 = 0.447_04;
const HUNDREDTH_INCH_TO_MM: f32 = 0.254;

/// Read the weather out of a position report's comment: the wind first, in
/// the course/speed field's place, then the keyed fields.
///
/// Returns what is left, which is the station's actual comment — most weather
/// stations put their software's name there.
pub(crate) fn parse_in_comment(tail: &[u8]) -> (AprsWeather, &[u8]) {
    let mut w = AprsWeather::default();
    let mut rest = tail;
    // `ddd/sss` — the same seven bytes a moving station uses for course and
    // speed, reused for wind direction and wind speed.
    if rest.len() >= 7 && rest[3] == b'/' {
        w.wind_dir_deg = num(&rest[0..3]).map(|v| (v as u16) % 360);
        w.wind_speed_ms = num(&rest[4..7]).map(|v| v * MPH_TO_MS);
        rest = &rest[7..];
    }
    let used = fields(rest, &mut w);
    (w, &rest[used..])
}

/// A `_` frame: an eight-digit `MMDDHHMM` timestamp, then the same fields with
/// the wind carried under its own keys.
pub(crate) fn parse_positionless(rest: &[u8]) -> Result<AprsWeather> {
    if rest.len() < 8 {
        return Err(AprsError::Malformed("truncated weather report"));
    }
    let mut w = AprsWeather::default();
    fields(&rest[8..], &mut w);
    if w == AprsWeather::default() {
        return Err(AprsError::Malformed("weather report carried no readings"));
    }
    Ok(w)
}

/// Scan the keyed fields, returning how many bytes were consumed.
///
/// Each is a letter and a fixed number of digits, and a field whose digits are
/// dots or spaces means "this station has no such sensor" — which is not the
/// same as a reading of zero and must not become one.
fn fields(b: &[u8], w: &mut AprsWeather) -> usize {
    let mut i = 0;
    // `s` is wind speed here but snowfall later in the same report, and the
    // two are told apart only by which came first. Once the wind is known, a
    // later `s` is snow and is skipped rather than overwriting it.
    let mut wind_seen = w.wind_speed_ms.is_some();
    while i < b.len() {
        let key = b[i];
        let width = match key {
            b'c' | b's' | b'g' | b't' | b'r' | b'p' | b'P' | b'L' | b'l' => 3,
            b'h' => 2,
            b'b' => 5,
            b'#' => 3,
            // Anything else is the start of the comment.
            _ => break,
        };
        if i + 1 + width > b.len() {
            break;
        }
        let field = &b[i + 1..i + 1 + width];
        // A field has to be digits (or the dots/spaces that mean "absent").
        // Anything else means this letter was the first character of the
        // comment and happened to look like a key.
        if !field.iter().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b' ' | b'-')) {
            break;
        }
        let v = num(field);
        match key {
            b'c' => w.wind_dir_deg = v.map(|x| (x as u16) % 360),
            b's' if !wind_seen => {
                w.wind_speed_ms = v.map(|x| x * MPH_TO_MS);
                wind_seen = true;
            }
            b's' => {}
            b'g' => w.wind_gust_ms = v.map(|x| x * MPH_TO_MS),
            b't' => w.temp_c = v.map(|f| (f - 32.0) * 5.0 / 9.0),
            b'r' => w.rain_1h_mm = v.map(|x| x * HUNDREDTH_INCH_TO_MM),
            b'p' => w.rain_24h_mm = v.map(|x| x * HUNDREDTH_INCH_TO_MM),
            b'P' => w.rain_midnight_mm = v.map(|x| x * HUNDREDTH_INCH_TO_MM),
            // Humidity is sent modulo 100, so `00` is a saturated atmosphere
            // rather than a desert.
            b'h' => w.humidity_pct = v.map(|x| if x == 0.0 { 100 } else { x as u8 }),
            b'b' => w.pressure_hpa = v.map(|x| x / 10.0),
            _ => {}
        }
        i += 1 + width;
    }
    i
}

/// A fixed-width numeric field, or `None` where the station left it blank.
///
/// A leading `-` is part of the number: temperatures below zero are sent as
/// `t-05`, which is three characters like every other temperature.
fn num(b: &[u8]) -> Option<f32> {
    let s = std::str::from_utf8(b).ok()?.trim();
    if s.is_empty() || s.contains('.') || s == "-" {
        return None;
    }
    s.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use crate::{AprsData, parse};

    /// The reference's own complete weather report.
    #[test]
    fn a_weather_position_decodes_every_sensor() {
        let f = b"!4903.50N/07201.75W_220/004g005t077r000p000P000h50b09900wRSW";
        let AprsData::Position(p) = parse("APRS", f).unwrap() else { panic!() };
        let w = p.weather.expect("the `_` symbol means this is a weather report");
        assert_eq!(w.wind_dir_deg, Some(220));
        assert!(w.wind_speed_ms.is_some_and(|v| (v - 1.788).abs() < 0.01), "{:?}", w.wind_speed_ms);
        assert!(w.wind_gust_ms.is_some_and(|v| (v - 2.235).abs() < 0.01));
        assert!(w.temp_c.is_some_and(|v| (v - 25.0).abs() < 0.01), "{:?}", w.temp_c);
        assert_eq!(w.rain_1h_mm, Some(0.0));
        assert_eq!(w.humidity_pct, Some(50));
        assert!(w.pressure_hpa.is_some_and(|v| (v - 990.0).abs() < 0.01), "{:?}", w.pressure_hpa);
        // What is left is the station's own note, not the weather.
        assert_eq!(p.comment, "wRSW");
    }

    /// `h00` is 100% relative humidity — the field is sent modulo 100, and
    /// reading it literally turns fog into a desert.
    #[test]
    fn zero_humidity_means_saturated() {
        let f = b"!4903.50N/07201.75W_000/000h00";
        let AprsData::Position(p) = parse("APRS", f).unwrap() else { panic!() };
        assert_eq!(p.weather.unwrap().humidity_pct, Some(100));
    }

    /// A missing sensor is not a reading of zero. A station with no
    /// thermometer sends `t...`, and a map that drew that as 0°F would put a
    /// hard frost on it in July.
    #[test]
    fn a_blank_field_is_absent_rather_than_zero() {
        let f = b"!4903.50N/07201.75W_220/004t...r000";
        let AprsData::Position(p) = parse("APRS", f).unwrap() else { panic!() };
        let w = p.weather.unwrap();
        assert_eq!(w.temp_c, None);
        assert_eq!(w.rain_1h_mm, Some(0.0));
    }

    /// Below freezing the temperature is negative and still three characters.
    #[test]
    fn a_negative_temperature_decodes() {
        let f = b"!4903.50N/07201.75W_000/000t-05";
        let AprsData::Position(p) = parse("APRS", f).unwrap() else { panic!() };
        assert!(p.weather.unwrap().temp_c.is_some_and(|t| (t + 20.56).abs() < 0.05));
    }

    /// The positionless form, from a station that has already said where it
    /// is.
    #[test]
    fn a_positionless_report_decodes() {
        let AprsData::Weather(w) = parse("APRS", b"_10090556c220s004g005t077b09900").unwrap()
        else {
            panic!()
        };
        assert_eq!(w.wind_dir_deg, Some(220));
        assert!(w.temp_c.is_some_and(|v| (v - 25.0).abs() < 0.01));
    }
}
