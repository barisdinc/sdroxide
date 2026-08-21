//! Turning an rtl_433 decode into the same [`Decoded`] the native protocols
//! produce, so both lanes land in one device table with one set of units.
//!
//! rtl_433 reports whatever each device actually measures, in whatever unit its
//! decoder was written against — `temperature_F` from an American sensor,
//! `wind_avg_km_h` from a European one. The device table is in SI, so the
//! conversion happens here rather than in the panel, where two rows of the same
//! quantity would otherwise be in different units.
//!
//! Anything not recognised as a physical quantity is not thrown away: it goes to
//! the report's key/value lane, which is where the native decoders already put
//! battery state and message types.

use sdroxide_types::{IsmProtocol, IsmQuantity, IsmReading};

use super::sys::{Kv, Value};
use crate::proto::Decoded;

/// A mapped decode, plus the two things about the *reception* rather than the
/// device that the caller needs.
pub struct Mapped {
    pub decoded: Decoded,
    /// Where rtl_433 says it heard this, when it says. Falls back to the window
    /// centre, which is what puts the row on the waterfall.
    pub freq_hz: f64,
    pub snr_db: Option<f32>,
}

/// Keys that identify the device rather than describe a reading.
const IDENTITY_KEYS: &[&str] = &["model", "id", "channel", "subtype", "type"];

/// Keys that describe the reception, not the device. The panel has columns for
/// these already, or they are noise in a device row.
const RECEPTION_KEYS: &[&str] =
    &["time", "protocol", "rssi", "snr", "noise", "freq", "freq1", "freq2", "mod", "mic", "tag"];

/// Keys holding the raw frame.
const RAW_KEYS: &[&str] = &["data", "code", "codes", "raw", "raw_message"];

/// Map one rtl_433 event.
///
/// `None` when the event carries no `model`: that is rtl_433's own report data
/// or an analyzer line rather than a device decode, and a device table row with
/// no device is worse than nothing.
pub fn map_event(kvs: &[Kv], window_center_hz: f64) -> Option<Mapped> {
    let get = |name: &str| kvs.iter().find(|kv| kv.key == name).map(|kv| &kv.value);

    let model = match get("model") {
        Some(Value::Str(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return None,
    };

    // Identity: the model plus whatever the device calls itself. rtl_433 reports
    // `id` as an int on most decoders and a string on a few, and `channel` is
    // the switch on the back of a sensor — two units of the same model are told
    // apart by it, so it belongs in the identity rather than the readings.
    let mut device = match get("id") {
        Some(v) => v.to_display(),
        None => String::new(),
    };
    if let Some(ch) = get("channel") {
        let ch = ch.to_display();
        if !ch.is_empty() {
            if device.is_empty() {
                device = format!("ch{ch}");
            } else {
                device.push('/');
                device.push_str(&ch);
            }
        }
    }
    if device.is_empty() {
        // Some remotes report nothing but a button code. Still a device, and
        // still worth one stable row.
        device = "—".to_string();
    }

    let mut readings: Vec<IsmReading> = Vec::new();
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut raw_hex = String::new();
    let mut freq_hz = None;
    let mut freq_pair: (Option<f64>, Option<f64>) = (None, None);
    let mut snr_db = None;

    for kv in kvs {
        let key = kv.key.as_str();

        if IDENTITY_KEYS.contains(&key) {
            // `subtype`/`type` are not identity in rtl_433's sense but they do
            // distinguish what a multi-purpose decoder heard, so they read
            // better beside the device than as a reading.
            if matches!(key, "subtype" | "type") {
                extra.push((key.to_string(), kv.value.to_display()));
            }
            continue;
        }

        if RAW_KEYS.contains(&key) {
            if raw_hex.is_empty() {
                raw_hex = kv.value.to_display();
            }
            continue;
        }

        if RECEPTION_KEYS.contains(&key) {
            match key {
                "freq" => freq_hz = kv.value.as_f64().map(mhz_to_hz),
                "freq1" => freq_pair.0 = kv.value.as_f64().map(mhz_to_hz),
                "freq2" => freq_pair.1 = kv.value.as_f64().map(mhz_to_hz),
                "snr" => snr_db = kv.value.as_f64().map(|v| v as f32),
                _ => {}
            }
            continue;
        }

        match quantity_for(key, &kv.value) {
            Some((quantity, value)) => readings.push(IsmReading { quantity, value }),
            None => extra.push((pretty_key(key), display_value(key, &kv.value))),
        }
    }

    // An FSK decoder reports the two tone frequencies rather than one centre.
    if freq_hz.is_none() {
        freq_hz = match freq_pair {
            (Some(a), Some(b)) => Some((a + b) / 2.0),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
    }

    Some(Mapped {
        decoded: Decoded {
            protocol: IsmProtocol::Rtl433,
            model: Some(model),
            device,
            readings,
            extra,
            // rtl_433 decodes what it can read; a decoder that reached a valid
            // frame has not hit an encrypted payload the way the native metering
            // protocols do.
            encrypted: false,
            raw_hex,
        },
        freq_hz: freq_hz.unwrap_or(window_center_hz),
        snr_db,
    })
}

fn mhz_to_hz(v: f64) -> f64 {
    v * 1e6
}

/// Recognise a reading and put it in SI.
///
/// Returns `None` for anything that is not one of the quantities the device
/// table can draw, which sends it to the key/value lane instead.
fn quantity_for(key: &str, value: &Value) -> Option<(IsmQuantity, f64)> {
    let v = value.as_f64()?;
    let q = match key {
        "temperature_C" => (IsmQuantity::TempC, v),
        "temperature_F" => (IsmQuantity::TempC, (v - 32.0) / 1.8),
        // A few decoders report more than one probe.
        k if k.starts_with("temperature_") && k.ends_with("_C") => (IsmQuantity::TempC, v),
        "humidity" => (IsmQuantity::HumidityPct, v),
        "moisture" => (IsmQuantity::SoilMoisturePct, v),

        "wind_avg_m_s" => (IsmQuantity::WindAvgMs, v),
        "wind_avg_km_h" => (IsmQuantity::WindAvgMs, v / 3.6),
        "wind_avg_mi_h" => (IsmQuantity::WindAvgMs, v * 0.447_04),
        "wind_max_m_s" => (IsmQuantity::WindGustMs, v),
        "wind_max_km_h" => (IsmQuantity::WindGustMs, v / 3.6),
        "wind_max_mi_h" => (IsmQuantity::WindGustMs, v * 0.447_04),
        "wind_dir_deg" => (IsmQuantity::WindDirDeg, v),

        "rain_mm" | "rain_rate_mm_h" => (IsmQuantity::RainMm, v),
        "rain_in" | "rain_rate_in_h" => (IsmQuantity::RainMm, v * 25.4),

        "pressure_hPa" => (IsmQuantity::PressureHpa, v),
        "pressure_kPa" => (IsmQuantity::PressureHpa, v * 10.0),
        "pressure_PSI" => (IsmQuantity::PressureHpa, v * 68.947_57),

        "uv" | "uvi" | "uv_index" => (IsmQuantity::UvIndex, v),
        "light_lux" | "lux" => (IsmQuantity::LuxLx, v),

        "battery_V" => (IsmQuantity::BatteryVolts, v),
        "battery_mV" => (IsmQuantity::BatteryVolts, v / 1000.0),
        // `battery_ok` is a flag, not a percentage: 1 or 0. It goes to the
        // key/value lane by way of display_value below.
        "battery_level" | "battery_pct" => (IsmQuantity::BatteryPct, v),

        "storm_dist_km" | "storm_dist" => (IsmQuantity::LightningKm, v),
        "strike_count" | "strike_distance" if key == "strike_count" => {
            (IsmQuantity::StrikeCount, v)
        }
        "strike_distance" => (IsmQuantity::LightningKm, v),

        "power_W" => (IsmQuantity::PowerW, v),
        "power_kW" => (IsmQuantity::PowerW, v * 1000.0),
        "energy_kWh" => (IsmQuantity::EnergyKwh, v),
        "energy_Wh" => (IsmQuantity::EnergyKwh, v / 1000.0),
        "voltage_V" => (IsmQuantity::VoltageV, v),
        "voltage_mV" => (IsmQuantity::VoltageV, v / 1000.0),
        "current_A" => (IsmQuantity::CurrentA, v),
        "current_mA" => (IsmQuantity::CurrentA, v / 1000.0),

        _ => return None,
    };
    Some(q)
}

/// rtl_433's key names carry their unit as a suffix, which is redundant once the
/// value is in the table. Strip it and make the rest readable.
fn pretty_key(key: &str) -> String {
    let base = key
        .strip_suffix("_C")
        .or_else(|| key.strip_suffix("_F"))
        .or_else(|| key.strip_suffix("_hPa"))
        .unwrap_or(key);
    base.replace('_', " ")
}

/// Render a value for the key/value lane, spelling out the flags that rtl_433
/// reports as 0 or 1.
fn display_value(key: &str, value: &Value) -> String {
    if key == "battery_ok" {
        return match value.as_f64() {
            Some(v) if v >= 0.5 => "ok".to_string(),
            Some(_) => "low".to_string(),
            None => value.to_display(),
        };
    }
    if let Some(v) = value.as_f64() {
        // Flags reported as ints read better as words in a row of readings.
        if matches!(
            key,
            "battery" | "test" | "tamper" | "alarm" | "learn" | "maybe_battery" | "button"
        ) && (v == 0.0 || v == 1.0)
        {
            return if v == 1.0 { "yes".into() } else { "no".into() };
        }
    }
    value.to_display()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(k: &str, v: i64) -> Kv {
        Kv { key: k.into(), value: Value::Int(v) }
    }
    fn dbl(k: &str, v: f64) -> Kv {
        Kv { key: k.into(), value: Value::Double(v) }
    }
    fn s(k: &str, v: &str) -> Kv {
        Kv { key: k.into(), value: Value::Str(v.into()) }
    }

    fn reading(m: &Mapped, q: IsmQuantity) -> Option<f64> {
        m.decoded.readings.iter().find(|r| r.quantity == q).map(|r| r.value)
    }
    fn extra<'a>(m: &'a Mapped, k: &str) -> Option<&'a str> {
        m.decoded.extra.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str())
    }

    /// Transcribed from a real Bresser 6-in-1 line — the device the native
    /// Bresser decoder was verified against, so both lanes can be compared on
    /// the same signal.
    #[test]
    fn maps_a_bresser_6in1() {
        let kvs = [
            s("model", "Bresser-6in1"),
            int("id", 143),
            int("channel", 0),
            int("battery_ok", 1),
            dbl("temperature_C", 21.4),
            int("humidity", 58),
            dbl("wind_max_m_s", 3.6),
            dbl("wind_avg_m_s", 1.8),
            dbl("wind_dir_deg", 270.0),
            dbl("rain_mm", 12.4),
            dbl("freq", 868.325),
            dbl("snr", 18.5),
            s("mic", "CRC"),
        ];
        let m = map_event(&kvs, 868_650_000.0).expect("has a model");
        assert_eq!(m.decoded.protocol, IsmProtocol::Rtl433);
        assert_eq!(m.decoded.model.as_deref(), Some("Bresser-6in1"));
        assert_eq!(m.decoded.device, "143/0");
        assert_eq!(reading(&m, IsmQuantity::TempC), Some(21.4));
        assert_eq!(reading(&m, IsmQuantity::HumidityPct), Some(58.0));
        assert_eq!(reading(&m, IsmQuantity::WindGustMs), Some(3.6));
        assert_eq!(reading(&m, IsmQuantity::RainMm), Some(12.4));
        assert_eq!(extra(&m, "battery ok"), Some("ok"));
        assert_eq!(m.freq_hz, 868_325_000.0);
        assert_eq!(m.snr_db, Some(18.5));
        // Reception metadata must not turn into device readings.
        assert!(extra(&m, "mic").is_none());
        assert!(extra(&m, "snr").is_none());
    }

    #[test]
    fn converts_imperial_units() {
        let kvs = [
            s("model", "Acurite-Tower"),
            int("id", 7),
            dbl("temperature_F", 68.0),
            dbl("wind_avg_mi_h", 10.0),
            dbl("rain_in", 1.0),
            dbl("pressure_kPa", 101.3),
        ];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert!((reading(&m, IsmQuantity::TempC).unwrap() - 20.0).abs() < 1e-9);
        assert!((reading(&m, IsmQuantity::WindAvgMs).unwrap() - 4.4704).abs() < 1e-9);
        assert!((reading(&m, IsmQuantity::RainMm).unwrap() - 25.4).abs() < 1e-9);
        assert!((reading(&m, IsmQuantity::PressureHpa).unwrap() - 1013.0).abs() < 1e-9);
    }

    #[test]
    fn maps_an_energy_meter() {
        let kvs = [
            s("model", "Efergy-e2CT"),
            int("id", 4231),
            dbl("power_W", 812.5),
            dbl("energy_kWh", 14311.25),
            dbl("current_A", 3.4),
            dbl("voltage_V", 230.0),
        ];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert_eq!(reading(&m, IsmQuantity::PowerW), Some(812.5));
        assert_eq!(reading(&m, IsmQuantity::EnergyKwh), Some(14311.25));
        assert_eq!(reading(&m, IsmQuantity::CurrentA), Some(3.4));
        assert_eq!(reading(&m, IsmQuantity::VoltageV), Some(230.0));
    }

    #[test]
    fn falls_back_to_the_window_centre_without_a_freq() {
        let kvs = [s("model", "Nexus-TH"), int("id", 21), dbl("temperature_C", 5.0)];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert_eq!(m.freq_hz, 433_920_000.0);
        assert_eq!(m.snr_db, None);
    }

    #[test]
    fn averages_the_two_fsk_tone_frequencies() {
        let kvs = [s("model", "X"), int("id", 1), dbl("freq1", 868.2), dbl("freq2", 868.4)];
        let m = map_event(&kvs, 868_650_000.0).unwrap();
        assert!((m.freq_hz - 868_300_000.0).abs() < 1.0);
    }

    #[test]
    fn an_event_without_a_model_is_not_a_device() {
        let kvs = [dbl("freq", 433.92), dbl("snr", 12.0)];
        assert!(map_event(&kvs, 433_920_000.0).is_none());
    }

    #[test]
    fn a_remote_with_no_id_still_gets_a_row() {
        let kvs = [s("model", "Generic-Remote"), s("data", "a9878c")];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert_eq!(m.decoded.device, "—");
        assert_eq!(m.decoded.raw_hex, "a9878c");
    }

    #[test]
    fn unknown_keys_are_kept_as_text() {
        let kvs = [
            s("model", "Thing"),
            int("id", 1),
            s("state", "OPEN"),
            int("tamper", 1),
            int("counter", 42),
        ];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert_eq!(extra(&m, "state"), Some("OPEN"));
        assert_eq!(extra(&m, "tamper"), Some("yes"));
        assert_eq!(extra(&m, "counter"), Some("42"));
    }

    #[test]
    fn a_low_battery_flag_reads_as_words() {
        let kvs = [s("model", "T"), int("id", 1), int("battery_ok", 0)];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert_eq!(extra(&m, "battery ok"), Some("low"));
    }

    #[test]
    fn a_string_id_still_identifies() {
        // A few decoders report id as text.
        let kvs = [s("model", "TPMS"), s("id", "3f7a91"), dbl("temperature_C", 30.0)];
        let m = map_event(&kvs, 433_920_000.0).unwrap();
        assert_eq!(m.decoded.device, "3f7a91");
    }
}
