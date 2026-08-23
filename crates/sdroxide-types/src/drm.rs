//! Digital Radio Mondiale receive status.
//!
//! What the decoder knows about the multiplex it is listening to, as one
//! latest-wins snapshot. Unlike [`crate::RdsData`] nothing here is a delta: DRM
//! carries a service label and a scrolling text message that are simply
//! *current*, and a set of sync lights whose whole value is that they show the
//! present state of each stage.

use serde::{Deserialize, Serialize};

/// How one stage of the receive chain is doing, in the four states Dream's own
/// indicators use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DrmSync {
    /// Nothing arriving at this stage at all.
    #[default]
    Absent,
    /// Arriving, but failing its CRC — the stage is locked onto something wrong.
    CrcError,
    /// Arriving with errors the FEC could not fully repair.
    DataError,
    /// Good.
    Ok,
}

impl DrmSync {
    /// Dream reports these as a plain 0–3; anything else is treated as absent
    /// rather than trusted.
    pub fn from_raw(v: i32) -> Self {
        match v {
            1 => DrmSync::CrcError,
            2 => DrmSync::DataError,
            3 => DrmSync::Ok,
            _ => DrmSync::Absent,
        }
    }

    pub fn is_ok(self) -> bool {
        self == DrmSync::Ok
    }

    /// Single-character indicator, as Dream's console display draws it.
    pub fn glyph(self) -> char {
        match self {
            DrmSync::Absent => '-',
            DrmSync::CrcError => 'x',
            DrmSync::DataError => '!',
            DrmSync::Ok => '•',
        }
    }
}

/// DRM robustness mode: how much guard interval the transmission spends on
/// multipath, from A (a ground-wave channel, most capacity) to D (a badly
/// scattered sky-wave path, least).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrmRobustness {
    A,
    B,
    C,
    D,
    E,
}

impl DrmRobustness {
    pub fn from_raw(v: i32) -> Option<Self> {
        match v {
            0 => Some(DrmRobustness::A),
            1 => Some(DrmRobustness::B),
            2 => Some(DrmRobustness::C),
            3 => Some(DrmRobustness::D),
            4 => Some(DrmRobustness::E),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DrmRobustness::A => "A",
            DrmRobustness::B => "B",
            DrmRobustness::C => "C",
            DrmRobustness::D => "D",
            DrmRobustness::E => "E",
        }
    }
}

/// The six channel widths DRM30/DRM+ are allowed to occupy, in kHz. 9 and
/// 10 kHz — one broadcast channel raster — are what nearly everything on the
/// air actually uses.
pub fn spectrum_occupancy_khz(raw: i32) -> Option<f32> {
    match raw {
        0 => Some(4.5),
        1 => Some(5.0),
        2 => Some(9.0),
        3 => Some(10.0),
        4 => Some(18.0),
        5 => Some(20.0),
        _ => None,
    }
}

/// Audio coding of the selected service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrmCodec {
    Aac,
    /// Speech codecs from the original standard, both long since withdrawn.
    Celp,
    Hvxc,
    /// xHE-AAC (USAC), what most surviving broadcasters moved to.
    XheAac,
    /// Dream's own extension, not part of the DRM standard.
    Opus,
    Unknown,
}

impl DrmCodec {
    /// The order in `CAudioParam::EAudCod`.
    pub fn from_raw(v: i32) -> Self {
        match v {
            0 => DrmCodec::Aac,
            1 => DrmCodec::Celp,
            2 => DrmCodec::Hvxc,
            3 => DrmCodec::XheAac,
            4 => DrmCodec::Opus,
            _ => DrmCodec::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DrmCodec::Aac => "AAC",
            DrmCodec::Celp => "CELP",
            DrmCodec::Hvxc => "HVXC",
            DrmCodec::XheAac => "xHE-AAC",
            DrmCodec::Opus => "Opus",
            DrmCodec::Unknown => "?",
        }
    }
}

/// The broadcaster's own clock, when the multiplex carries one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
}

/// One service of the multiplex — in practice the one being listened to.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DrmService {
    /// The station's name for itself, up to 16 characters.
    #[serde(default)]
    pub label: String,
    /// The scrolling text message the audio stream carries alongside the sound.
    #[serde(default)]
    pub text: String,
    /// ISO country and ISO 639 language codes, when signalled.
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub service_id: u32,
    #[serde(default)]
    pub bitrate_kbps: f32,
    #[serde(default)]
    pub codec: Option<DrmCodec>,
    #[serde(default)]
    pub stereo: bool,
}

/// Everything the DRM decoder knows right now.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DrmStatus {
    /// The five stages of the receive chain, in the order they lock: the
    /// sample-rate/IO interface, time synchronisation, frame synchronisation,
    /// then the two decoded channels — FAC (which says what the transmission
    /// is) and SDC (which says what the services are) — and finally the audio.
    #[serde(default)]
    pub io: DrmSync,
    #[serde(default)]
    pub time_sync: DrmSync,
    #[serde(default)]
    pub frame_sync: DrmSync,
    #[serde(default)]
    pub fac: DrmSync,
    #[serde(default)]
    pub sdc: DrmSync,
    #[serde(default)]
    pub audio: DrmSync,

    /// Acquisition has finished and the receiver believes it has a signal.
    /// Everything below is only meaningful while this holds.
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub snr_db: f32,
    #[serde(default)]
    pub if_level_db: f32,
    /// Weighted and plain modulation error ratio of the main service channel.
    #[serde(default)]
    pub wmer_db: f32,
    #[serde(default)]
    pub mer_db: f32,
    /// Where the DRM carrier actually sits inside the decoder's own 48 kHz
    /// window, which is not where the dial is — see the mode's I.F. offset.
    #[serde(default)]
    pub dc_offset_hz: f32,
    /// Residual sample-clock error against the transmitter, in Hz.
    #[serde(default)]
    pub sample_offset_hz: f32,
    /// Doppler spread and delay spread of the path, when the channel estimator
    /// has enough to say.
    #[serde(default)]
    pub doppler_hz: Option<f32>,
    #[serde(default)]
    pub delay_ms: f32,

    #[serde(default)]
    pub robustness: Option<DrmRobustness>,
    #[serde(default)]
    pub bandwidth_khz: Option<f32>,
    /// Two seconds of time interleaving rather than 400 ms: better against
    /// fading, worse to acquire.
    #[serde(default)]
    pub interleaver_long: bool,
    /// Protection levels of the two multiplex parts.
    #[serde(default)]
    pub protection_a: u8,
    #[serde(default)]
    pub protection_b: u8,

    #[serde(default)]
    pub audio_services: u8,
    #[serde(default)]
    pub data_services: u8,
    /// Which service of the multiplex is being decoded, 0-based.
    #[serde(default)]
    pub current_service: u8,
    #[serde(default)]
    pub service: DrmService,
    #[serde(default)]
    pub time: Option<DrmTime>,
}

impl DrmStatus {
    /// The receiver is decoding audio, not merely holding sync on a carrier.
    pub fn decoding(&self) -> bool {
        self.locked && self.fac.is_ok() && self.audio != DrmSync::Absent
    }

    /// A one-line summary for a status bar: the label if the multiplex has
    /// named itself, else how far the chain has got.
    pub fn summary(&self) -> String {
        if !self.service.label.is_empty() {
            return self.service.label.clone();
        }
        if self.locked {
            "acquiring service".to_string()
        } else if self.time_sync.is_ok() {
            "syncing".to_string()
        } else {
            "no signal".to_string()
        }
    }
}
