//! Network spot domain types, shared by the native engine, the wire protocol,
//! and the UI (native + WASM). Pure data + serde — the actual feed clients
//! (DX cluster telnet, POTA/SOTA HTTP, PSK Reporter) live in the native
//! `sdroxide-net` crate. Modelled on [`crate::SkimmerSpot`] so the panadapter
//! overlay and world map reuse the same rendering path.

use serde::{Deserialize, Serialize};

use crate::Mode;

/// Where a spot came from. Drives the overlay colour and the map marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SpotKind {
    /// A DX-cluster telnet node (`DX de …`).
    DxCluster,
    /// Parks On The Air activator spot.
    Pota,
    /// Summits On The Air activator spot.
    Sota,
    /// A PSK Reporter reception report (someone heard us / a monitored call).
    PskReporter,
    /// A station connected to FreeDV Reporter (<https://qso.freedv.org/>).
    /// Appended last so the postcard discriminants above never move.
    FreeDv,
    /// A longwave/shortwave broadcast station from [`crate::broadcast`].
    ///
    /// Unlike every other kind this one is synthesised in the client from a
    /// bundled table rather than reported by a feed, so it never travels over
    /// the wire — but it shares `Spot` so the overlay, the list and the map
    /// render it through one path. Appended last, as above.
    Broadcast,
}

impl SpotKind {
    /// Every kind, in the order the SPOTS window's filter chips, the settings
    /// colour pickers and [`SpotKind::index`] all use. Anything indexed by kind
    /// — the filter array, [`crate::UiSettings::spot_colors`] — is this wide.
    pub const ALL: [SpotKind; 6] = [
        SpotKind::DxCluster,
        SpotKind::Pota,
        SpotKind::Sota,
        SpotKind::PskReporter,
        SpotKind::FreeDv,
        SpotKind::Broadcast,
    ];

    /// How many kinds there are, i.e. the width of every per-kind array.
    pub const COUNT: usize = SpotKind::ALL.len();

    /// This kind's position in [`SpotKind::ALL`].
    pub fn index(self) -> usize {
        match self {
            SpotKind::DxCluster => 0,
            SpotKind::Pota => 1,
            SpotKind::Sota => 2,
            SpotKind::PskReporter => 3,
            SpotKind::FreeDv => 4,
            SpotKind::Broadcast => 5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SpotKind::DxCluster => "DX",
            SpotKind::Pota => "POTA",
            SpotKind::Sota => "SOTA",
            SpotKind::PskReporter => "PSK",
            SpotKind::FreeDv => "FREEDV",
            SpotKind::Broadcast => "BC",
        }
    }

    /// The stock per-kind RGB tint for the overlay/marker (r, g, b).
    ///
    /// Where a label starts, not where it has to stay: the operator can retint
    /// any kind from the UI settings tab, and what they chose is kept in
    /// [`crate::UiSettings::spot_colors`]. Clients read that, not this.
    pub fn default_color(self) -> (u8, u8, u8) {
        match self {
            SpotKind::DxCluster => (120, 220, 255),   // cyan
            SpotKind::Pota => (120, 230, 140),        // green
            SpotKind::Sota => (255, 190, 90),         // amber
            SpotKind::PskReporter => (210, 150, 255), // violet
            SpotKind::FreeDv => (255, 130, 160),      // pink
            // Yellow-green. This used to be the same burnt orange the band-plan
            // overlay shades broadcast allocations with, so that a label matched
            // the band it sat in — but a broadcast box is *text*, and at that
            // saturation on the near-black waterfall it read as brown and was
            // hard to make out (issue #145). The band shading keeps the orange;
            // the label takes the brightest hue the other five leave free, which
            // is the gap between SOTA's amber and POTA's green.
            SpotKind::Broadcast => (190, 240, 120),
        }
    }
}

/// One spot from a network feed: a station heard/announced at a frequency, with
/// enough context to place a clickable marker on the panadapter and world map
/// and to pre-fill a log entry on click.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spot {
    /// Stable id so the UI can merge/update a marker in place and fade it out.
    /// Derived deterministically from (kind, call, rounded freq) by the feed.
    pub id: u64,
    pub kind: SpotKind,
    /// Absolute RF frequency of the spotted station (Hz).
    pub freq_hz: f64,
    /// The spotted (DX / activator / monitored) callsign, uppercased.
    pub call: String,
    /// Who reported it (cluster spotter, or the receiving station for PSK).
    pub spotter: String,
    /// Operating mode as announced ("FT8", "CW", "SSB", …); may be empty.
    pub mode: String,
    /// Free-text: cluster comment, park/summit name, etc.
    pub comment: String,
    /// POTA park ("K-1234") or SOTA summit ("W7W/…") reference, if any.
    pub reference: Option<String>,
    /// Maidenhead grid, if the feed provides one.
    pub grid: Option<String>,
    /// (lat, lon) in degrees for the world map, if known.
    pub loc: Option<(f64, f64)>,
    /// Spot time (unix seconds).
    pub when_utc: i64,
    /// SNR in dB (PSK Reporter), if any.
    pub snr_db: Option<i16>,
}

impl Spot {
    /// Best-effort radio [`Mode`] for click-to-tune from the spot's mode string.
    /// `SSB`/`PHONE` resolve to LSB below 10 MHz and USB above (ham convention);
    /// unknown data modes fall back to USB.
    pub fn radio_mode(&self) -> Option<Mode> {
        let m = self.mode.trim().to_ascii_uppercase();
        match m.as_str() {
            "" => None,
            "SSB" | "PHONE" | "USB/LSB" => {
                Some(if self.freq_hz < 10_000_000.0 { Mode::Lsb } else { Mode::Usb })
            }
            "DATA" | "DIGI" | "DIGITAL" | "MFSK" | "JT65" | "JS8" | "FT8CALL" => Some(Mode::Usb),
            other => other.parse::<Mode>().ok(),
        }
    }

    /// A stable id from the spot's identity, so re-reports of the same station
    /// on (nearly) the same frequency update one marker rather than piling up.
    /// Frequency is bucketed to 100 Hz.
    pub fn make_id(kind: SpotKind, call: &str, freq_hz: f64) -> u64 {
        // FNV-1a over (kind, call, freq bucket) — deterministic, no hashing deps.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        mix(&[kind as u8]);
        mix(call.as_bytes());
        let bucket = (freq_hz / 100.0).round() as i64;
        mix(&bucket.to_le_bytes());
        h
    }
}
