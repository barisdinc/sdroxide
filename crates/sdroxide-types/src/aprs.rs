//! APRS: what a decoded station, a message and the panel's view of the channel
//! look like once they are off the air.
//!
//! The codec that produces these lives in `sdroxide-aprs`; this is the shape
//! they travel in, so a remote client draws the same map as the machine with
//! the radio.
//!
//! # Symbols
//!
//! Every APRS station says what it *is* in two characters — a table and a
//! code — and that pair is what a map draws an icon from. The pair is kept
//! verbatim in [`AprsSymbol`] rather than being reduced to a picture at the
//! decoder, because the alternate table's first character doubles as an
//! *overlay*: `S` over the digipeater symbol is a digipeater that also runs an
//! I-gate, and the letter is drawn on top of the icon rather than replacing
//! it. [`AprsSymbolKind`] is the reduction, done here so that every client
//! agrees on which icon a symbol means.

use serde::{Deserialize, Serialize};

/// Stations kept on the map. A busy metropolitan channel carries a few hundred
/// distinct stations in an hour; past that the oldest are dropped.
pub const APRS_STATION_MAX: usize = 400;

/// Messages kept in the pane, both directions together.
pub const APRS_MESSAGE_MAX: usize = 300;

/// Positions kept per station, so a moving one draws a trail behind it.
pub const APRS_TRACK_MAX: usize = 64;

/// Raw frames kept for the traffic view.
pub const APRS_TRAFFIC_MAX: usize = 200;

/// How long an unacknowledged message is retried before it is given up on.
/// Five is what the protocol's own recommendation works out to at the usual
/// thirty-second spacing.
pub const APRS_MSG_RETRIES: u8 = 5;

/// One APRS symbol, exactly as it travelled.
///
/// `table` is `/` for the primary table, `\` for the alternate one, or — and
/// this is the case that catches people — any of `0`–`9` and `A`–`Z`, which
/// means *the alternate table with that character drawn over the icon*. The
/// overlay is how a symbol says something the 190-entry table has no room
/// for: which network a digipeater belongs to, which agency an emergency
/// vehicle is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AprsSymbol {
    pub table: char,
    pub code: char,
}

impl Default for AprsSymbol {
    /// The house — what a station that has never said otherwise is assumed to
    /// be, and what most fixed stations actually are.
    fn default() -> Self {
        AprsSymbol { table: '/', code: '-' }
    }
}

impl AprsSymbol {
    #[must_use]
    pub fn new(table: char, code: char) -> Self {
        AprsSymbol { table, code }
    }

    /// The overlay character, when the table position carries one.
    #[must_use]
    pub fn overlay(self) -> Option<char> {
        match self.table {
            '/' | '\\' => None,
            c if c.is_ascii_alphanumeric() => Some(c),
            _ => None,
        }
    }

    /// Which icon this symbol asks for.
    #[must_use]
    pub fn kind(self) -> AprsSymbolKind {
        let idx = match self.code as u32 {
            c @ 0x21..=0x7e => (c - 0x21) as usize,
            // Outside the printable range the symbol is corrupt, not exotic.
            _ => return AprsSymbolKind::Unknown,
        };
        // Anything that is not the primary table is the alternate one: an
        // overlay character selects the alternate table's symbol *and* draws
        // itself on top, so it must not fall through to the primary table's
        // meaning for the same code.
        if self.table == '/' { PRIMARY[idx] } else { ALTERNATE[idx] }
    }

    /// The two characters as they would be written down.
    #[must_use]
    pub fn text(self) -> String {
        format!("{}{}", self.table, self.code)
    }
}

/// The icon a symbol resolves to.
///
/// A reduction of the 190 table entries onto the pictures actually worth
/// drawing at map size: several codes share one — the primary table's jeep,
/// truck stop and 18-wheeler are all a truck at twelve pixels — and the ones
/// with no picture of their own land on a generic shape rather than on
/// nothing, so an unrecognised station is still a station on the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AprsSymbolKind {
    Unknown,
    Dot,
    Circle,
    Box,
    Star,
    Triangle,
    X,
    House,
    Hospital,
    School,
    Restaurant,
    Church,
    Parking,
    Campground,
    Shelter,
    Lighthouse,
    Firehouse,
    PoliceStation,
    Digipeater,
    Igate,
    Node,
    Antenna,
    Yagi,
    Dish,
    Server,
    Computer,
    Phone,
    Car,
    Truck,
    Van,
    Bus,
    Motorcycle,
    Bicycle,
    Rv,
    Train,
    Ambulance,
    FireTruck,
    Police,
    Tractor,
    Aircraft,
    AircraftLarge,
    Helicopter,
    Balloon,
    Glider,
    Rocket,
    Satellite,
    Boat,
    Yacht,
    Person,
    Emergency,
    RedCross,
    Fire,
    Eyeball,
    WxStation,
    Rain,
    Snow,
    Thunderstorm,
    Hurricane,
    Tornado,
    Cloudy,
    Sunny,
}

impl AprsSymbolKind {
    /// A name for the tooltip and the station list.
    #[must_use]
    pub fn label(self) -> &'static str {
        use AprsSymbolKind as K;
        match self {
            K::Unknown => "station",
            K::Dot => "position",
            K::Circle => "circle",
            K::Box => "box",
            K::Star => "star",
            K::Triangle => "triangle",
            K::X => "unknown position",
            K::House => "home station",
            K::Hospital => "hospital",
            K::School => "school",
            K::Restaurant => "restaurant",
            K::Church => "church",
            K::Parking => "parking",
            K::Campground => "campsite",
            K::Shelter => "shelter",
            K::Lighthouse => "lighthouse",
            K::Firehouse => "fire station",
            K::PoliceStation => "police station",
            K::Digipeater => "digipeater",
            K::Igate => "I-gate",
            K::Node => "network node",
            K::Antenna => "repeater",
            K::Yagi => "beam antenna",
            K::Dish => "dish antenna",
            K::Server => "server",
            K::Computer => "computer",
            K::Phone => "telephone",
            K::Car => "car",
            K::Truck => "truck",
            K::Van => "van",
            K::Bus => "bus",
            K::Motorcycle => "motorcycle",
            K::Bicycle => "bicycle",
            K::Rv => "motorhome",
            K::Train => "train",
            K::Ambulance => "ambulance",
            K::FireTruck => "fire engine",
            K::Police => "police",
            K::Tractor => "farm vehicle",
            K::Aircraft => "light aircraft",
            K::AircraftLarge => "aircraft",
            K::Helicopter => "helicopter",
            K::Balloon => "balloon",
            K::Glider => "glider",
            K::Rocket => "rocket",
            K::Satellite => "satellite",
            K::Boat => "boat",
            K::Yacht => "yacht",
            K::Person => "person",
            K::Emergency => "emergency",
            K::RedCross => "aid station",
            K::Fire => "fire",
            K::Eyeball => "eyeball",
            K::WxStation => "weather station",
            K::Rain => "rain",
            K::Snow => "snow",
            K::Thunderstorm => "thunderstorm",
            K::Hurricane => "tropical storm",
            K::Tornado => "tornado",
            K::Cloudy => "cloud",
            K::Sunny => "clear",
        }
    }

    /// True for the symbols that are a state of the weather rather than a
    /// station — the alternate table is half meteorology, and those markers
    /// belong under the stations on the map rather than over them.
    #[must_use]
    pub fn is_weather(self) -> bool {
        use AprsSymbolKind as K;
        matches!(
            self,
            K::WxStation
                | K::Rain
                | K::Snow
                | K::Thunderstorm
                | K::Hurricane
                | K::Tornado
                | K::Cloudy
                | K::Sunny
        )
    }

    /// True for the symbols that move, so the map draws a trail behind them.
    #[must_use]
    pub fn is_mobile(self) -> bool {
        use AprsSymbolKind as K;
        matches!(
            self,
            K::Car
                | K::Truck
                | K::Van
                | K::Bus
                | K::Motorcycle
                | K::Bicycle
                | K::Rv
                | K::Train
                | K::Ambulance
                | K::FireTruck
                | K::Police
                | K::Tractor
                | K::Aircraft
                | K::AircraftLarge
                | K::Helicopter
                | K::Balloon
                | K::Glider
                | K::Rocket
                | K::Satellite
                | K::Boat
                | K::Yacht
                | K::Person
        )
    }
}

use AprsSymbolKind as K;

/// The primary table (`/`), indexed by `code - 0x21`.
///
/// Taken from the symbol table in the APRS 1.0.1 protocol reference. Where
/// several codes are one picture at map size they share a kind; where the
/// table has nothing but a reservation they are [`AprsSymbolKind::Unknown`].
static PRIMARY: [AprsSymbolKind; 94] = [
    K::PoliceStation, // ! police station
    K::Unknown,       // " reserved
    K::Digipeater,    // # digipeater
    K::Phone,         // $ telephone
    K::Server,        // % DX cluster
    K::Igate,         // & HF gateway
    K::Aircraft,      // ' small aircraft
    K::Dish,          // ( mobile satellite station
    K::Person,        // ) wheelchair
    K::Motorcycle,    // * snowmobile
    K::RedCross,      // + Red Cross
    K::Campground,    // , Boy Scout
    K::House,         // - house QTH (VHF)
    K::X,             // . unknown position
    K::Dot,           // / red dot
    K::Circle,        // 0 circle
    K::Circle,        // 1
    K::Circle,        // 2
    K::Circle,        // 3
    K::Circle,        // 4
    K::Circle,        // 5
    K::Circle,        // 6
    K::Circle,        // 7
    K::Circle,        // 8
    K::Circle,        // 9
    K::Fire,          // : fire
    K::Campground,    // ; campground / portable
    K::Motorcycle,    // < motorcycle
    K::Train,         // = railway engine
    K::Car,           // > car
    K::Server,        // ? file server
    K::Hurricane,     // @ hurricane predicted path
    K::RedCross,      // A aid station
    K::Server,        // B BBS
    K::Boat,          // C canoe
    K::Unknown,       // D reserved
    K::Eyeball,       // E eyeball
    K::Tractor,       // F farm vehicle (tractor)
    K::Box,           // G grid square
    K::Box,           // H hotel
    K::Node,          // I TCP/IP network station
    K::Unknown,       // J reserved
    K::School,        // K school
    K::Computer,      // L PC user
    K::Computer,      // M MacAPRS
    K::Star,          // N NTS station
    K::Balloon,       // O balloon
    K::Police,        // P police
    K::Unknown,       // Q reserved
    K::Rv,            // R recreational vehicle
    K::Satellite,     // S space shuttle
    K::Computer,      // T SSTV
    K::Bus,           // U bus
    K::Dish,          // V ATV
    K::WxStation,     // W national weather service site
    K::Helicopter,    // X helicopter
    K::Yacht,         // Y yacht (sail boat)
    K::Computer,      // Z WinAPRS
    K::Person,        // [ jogger / human
    K::Yagi,          // \ triangle DF station
    K::Box,           // ] PBBS / mail
    K::AircraftLarge, // ^ large aircraft
    K::WxStation,     // _ weather station
    K::Dish,          // ` dish antenna
    K::Ambulance,     // a ambulance
    K::Bicycle,       // b bicycle
    K::Emergency,     // c incident command post
    K::Firehouse,     // d fire station
    K::Person,        // e horse / equestrian
    K::FireTruck,     // f fire truck
    K::Glider,        // g glider
    K::Hospital,      // h hospital
    K::Triangle,      // i IOTA (islands on the air)
    K::Truck,         // j jeep
    K::Truck,         // k truck
    K::Computer,      // l laptop
    K::Antenna,       // m Mic-E repeater
    K::Node,          // n node
    K::Emergency,     // o emergency operations centre
    K::Person,        // p rover / dog
    K::Box,           // q grid square (shown above 128 km)
    K::Antenna,       // r antenna / repeater
    K::Boat,          // s ship / power boat
    K::Truck,         // t truck stop
    K::Truck,         // u truck (18-wheeler)
    K::Van,           // v van
    K::Circle,        // w water station
    K::Computer,      // x X / Unix
    K::Yagi,          // y yagi at QTH
    K::Shelter,       // z shelter
    K::Cloudy,        // { fog
    K::Node,          // | TNC stream switch
    K::Unknown,       // } reserved
    K::Node,          // ~ TNC stream switch
];

/// The alternate table (`\`), and every overlay position with it.
static ALTERNATE: [AprsSymbolKind; 94] = [
    K::Emergency,     // ! emergency
    K::Unknown,       // " reserved
    K::Digipeater,    // # numbered digipeater
    K::Box,           // $ bank or ATM
    K::Unknown,       // % reserved
    K::Igate,         // & gateway station
    K::Emergency,     // ' crash site
    K::Cloudy,        // ( cloudy
    K::Unknown,       // ) reserved
    K::Snow,          // * snow
    K::Church,        // + church
    K::Campground,    // , Girl Scout
    K::House,         // - house (HF operation)
    K::X,             // . ambiguous
    K::Dot,           // / waypoint destination
    K::Circle,        // 0 circle overlay
    K::Circle,        // 1
    K::Circle,        // 2
    K::Circle,        // 3
    K::Circle,        // 4
    K::Circle,        // 5
    K::Circle,        // 6
    K::Circle,        // 7
    K::Circle,        // 8
    K::Circle,        // 9
    K::Snow,          // : hail
    K::Campground,    // ; park / picnic area
    K::Thunderstorm,  // < advisory (gale flag)
    K::Node,          // = APRStt
    K::Car,           // > overlaid car
    K::Box,           // ? information kiosk
    K::Rain,          // @ rain
    K::Box,           // A numbered box
    K::Cloudy,        // B blowing dust
    K::Boat,          // C coast guard
    K::Rain,          // D drizzle
    K::Fire,          // E smoke
    K::Rain,          // F freezing rain
    K::Snow,          // G snow shower
    K::Cloudy,        // H haze
    K::Rain,          // I rain shower
    K::Thunderstorm,  // J lightning
    K::Computer,      // K Kenwood
    K::Lighthouse,    // L lighthouse
    K::Antenna,       // M MARS
    K::Boat,          // N navigation buoy
    K::Rocket,        // O rocket
    K::Parking,       // P parking
    K::Emergency,     // Q earthquake
    K::Restaurant,    // R restaurant
    K::Satellite,     // S satellite
    K::Thunderstorm,  // T thunderstorm
    K::Sunny,         // U sunny
    K::Antenna,       // V VORTAC navigation aid
    K::WxStation,     // W national weather service site
    K::Hospital,      // X pharmacy
    K::Unknown,       // Y reserved
    K::Unknown,       // Z reserved
    K::Cloudy,        // [ wall cloud
    K::Unknown,       // \ reserved
    K::Unknown,       // ] reserved
    K::AircraftLarge, // ^ aircraft
    K::WxStation,     // _ weather site
    K::Rain,          // ` rain
    K::Star,          // a aurora / ARRL
    K::Snow,          // b blowing snow
    K::Triangle,      // c civil defence triangle
    K::Server,        // d DX spot
    K::Snow,          // e sleet
    K::Tornado,       // f funnel cloud
    K::Thunderstorm,  // g gale flags
    K::Box,           // h store
    K::Box,           // i point of interest
    K::Triangle,      // j work zone
    K::Truck,         // k SUV / special vehicle
    K::Box,           // l area outline
    K::Box,           // m value sign
    K::Triangle,      // n triangle overlay
    K::Dot,           // o small circle
    K::Cloudy,        // p partly cloudy
    K::Unknown,       // q reserved
    K::Box,           // r restrooms
    K::Boat,          // s overlaid ship / boat
    K::Tornado,       // t tornado
    K::Truck,         // u overlaid truck
    K::Van,           // v overlaid van
    K::Rain,          // w flooding
    K::X,             // x wreck or obstruction
    K::Star,          // y Skywarn
    K::Shelter,       // z overlaid shelter
    K::Cloudy,        // { fog
    K::Node,          // | TNC stream switch
    K::Unknown,       // } reserved
    K::Node,          // ~ TNC stream switch
];

/// A position off the air.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AprsPosition {
    pub lat: f64,
    pub lon: f64,
    /// Digits of longitude/latitude the sender blanked out, 0–4. A station
    /// that reports to the nearest ten minutes is saying "somewhere in this
    /// square", and a map that drew it as a point would be inventing
    /// precision the sender deliberately withheld.
    pub ambiguity: u8,
}

/// What produced a station entry: it announced itself, or somebody announced
/// it on its behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AprsEntryKind {
    /// A station reporting its own position.
    #[default]
    Station,
    /// An *object*: something a station is reporting the position of — a net
    /// control point, a storm, an event. It can be killed by whoever put it
    /// there, which a station cannot.
    Object,
    /// An *item*: an object with no timestamp, for things that do not move
    /// and do not expire.
    Item,
}

/// The weather half of a report, in the units the panel shows.
///
/// Every field optional because every field is: a station sends the ones its
/// hardware has, and the difference between "zero rain" and "no rain gauge"
/// matters to anyone reading it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AprsWeather {
    /// Wind direction the wind is coming *from*, degrees true.
    pub wind_dir_deg: Option<u16>,
    pub wind_speed_ms: Option<f32>,
    pub wind_gust_ms: Option<f32>,
    pub temp_c: Option<f32>,
    /// Rain in the last hour, millimetres.
    pub rain_1h_mm: Option<f32>,
    /// Rain in the last 24 hours, millimetres.
    pub rain_24h_mm: Option<f32>,
    /// Rain since local midnight, millimetres.
    pub rain_midnight_mm: Option<f32>,
    pub humidity_pct: Option<u8>,
    /// Barometric pressure, hectopascals.
    pub pressure_hpa: Option<f32>,
}

impl AprsWeather {
    /// True when nothing was actually reported, so the panel can leave the
    /// row out rather than drawing a line of dashes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == AprsWeather::default()
    }
}

/// One station on the map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AprsStation {
    /// Callsign with SSID, as it appeared — or the object's name, which is
    /// nine free-form characters and need not be a callsign at all.
    pub name: String,
    /// Who put it there, when this is an object or an item.
    pub reported_by: String,
    pub entry: AprsEntryKind,
    pub symbol: AprsSymbol,
    pub pos: Option<AprsPosition>,
    /// Where it has been, oldest first, capped at [`APRS_TRACK_MAX`].
    pub track: Vec<(f64, f64)>,
    /// Course over ground, degrees true.
    pub course_deg: Option<u16>,
    /// Speed over ground, knots — the unit the protocol carries.
    pub speed_kn: Option<f32>,
    /// Altitude in metres.
    pub altitude_m: Option<f64>,
    /// The comment the position carried.
    pub comment: String,
    /// The last status report, which is a separate packet type.
    pub status: String,
    pub weather: Option<AprsWeather>,
    /// Unix seconds when it was last heard.
    pub last_heard: i64,
    /// Frames heard from it since the panel was cleared.
    pub packets: u32,
    /// The digipeater path of the last frame, in order.
    pub via: Vec<String>,
    /// The last frame reached us without being repeated. Worth its own field:
    /// on a channel where everything is digipeated, the stations you hear
    /// direct are the ones actually within range.
    pub direct: bool,
    /// An object the reporting station has killed. Kept rather than dropped so
    /// the map can grey it out — an object vanishing without trace looks like
    /// a receiver problem.
    pub killed: bool,
}

/// What a message is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AprsMsgState {
    /// Somebody sent it to us.
    #[default]
    Received,
    /// Ours, waiting for the channel.
    Queued,
    /// Ours, on the air, waiting for an acknowledgement.
    Sent,
    /// Ours, acknowledged by the far end.
    Acked,
    /// Ours, explicitly rejected by the far end.
    Rejected,
    /// Ours, retried to exhaustion with no answer.
    Failed,
}

/// One message, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AprsMessage {
    /// Unix seconds.
    pub at: i64,
    pub from: String,
    pub to: String,
    pub text: String,
    /// The message number, when it asked to be acknowledged. Messages sent
    /// without one are announcements: nobody answers them and nothing retries.
    pub id: String,
    pub state: AprsMsgState,
    /// Transmissions so far, for a message of ours still being retried.
    pub tries: u8,
}

/// One frame as the traffic view prints it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AprsTraffic {
    pub at: i64,
    pub from: String,
    pub to: String,
    pub via: Vec<String>,
    /// The information field, printable.
    pub info: String,
    /// What the codec made of it — "position", "message", "weather", "status",
    /// or the reason it made nothing.
    pub kind: String,
    pub sent: bool,
}

/// What an APRS station is doing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AprsStatus {
    /// The channel is busy — a modem-level carrier detect. CSMA will not key
    /// while this is set.
    pub dcd: bool,
    /// Smoothed receive level, 0..1, for the meter.
    pub level: f32,
    /// Every station heard, newest activity last.
    pub stations: Vec<AprsStation>,
    /// Messages both ways, oldest first.
    pub messages: Vec<AprsMessage>,
    /// The raw frame log.
    pub traffic: Vec<AprsTraffic>,
    /// Frames that arrived and failed their check sequence.
    pub bad_frames: u32,
    /// Frames heard that were not APRS at all — a connected-mode session
    /// sharing the channel, most often. Counted rather than logged: they are
    /// somebody else's traffic and there is nothing to show.
    pub non_aprs: u32,
    /// Where our own beacon says we are, once there is one.
    pub my_pos: Option<AprsPosition>,
    /// Seconds until the next beacon. `None` when beaconing is off.
    pub next_beacon_s: Option<u32>,
    /// Frames waiting for the channel.
    pub tx_queue: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tables are indexed by `code - 0x21` and have to cover `!`
    /// through `~` exactly. An off-by-one here would not fail to compile — it
    /// would quietly draw every station the wrong icon.
    #[test]
    fn the_symbol_tables_cover_the_printable_range() {
        assert_eq!(PRIMARY.len(), 0x7e - 0x21 + 1);
        assert_eq!(ALTERNATE.len(), PRIMARY.len());
        assert_eq!(AprsSymbol::new('/', '!').kind(), AprsSymbolKind::PoliceStation);
        assert_eq!(AprsSymbol::new('/', '~').kind(), AprsSymbolKind::Node);
        assert_eq!(AprsSymbol::new('\\', '!').kind(), AprsSymbolKind::Emergency);
        assert_eq!(AprsSymbol::new('\\', '~').kind(), AprsSymbolKind::Node);
    }

    /// The car and the house: the two symbols most of a channel actually is.
    #[test]
    fn the_common_symbols_resolve() {
        assert_eq!(AprsSymbol::new('/', '>').kind(), AprsSymbolKind::Car);
        assert_eq!(AprsSymbol::new('/', '-').kind(), AprsSymbolKind::House);
        assert_eq!(AprsSymbol::new('/', '#').kind(), AprsSymbolKind::Digipeater);
        assert_eq!(AprsSymbol::new('/', '_').kind(), AprsSymbolKind::WxStation);
    }

    /// An overlay selects the *alternate* table and draws its character on
    /// top. The trap is falling through to the primary table for the same
    /// code: `S#` is a digipeater either way, but `Ss` is a boat on the
    /// alternate table and a power boat on the primary — and `L\` picks out
    /// the difference properly.
    #[test]
    fn an_overlay_reads_the_alternate_table() {
        let s = AprsSymbol::new('S', '#');
        assert_eq!(s.overlay(), Some('S'));
        assert_eq!(s.kind(), AprsSymbolKind::Digipeater);
        // `L` on the alternate table is a lighthouse; on the primary it is a
        // PC user. An overlay must land on the former.
        assert_eq!(AprsSymbol::new('7', 'L').kind(), AprsSymbolKind::Lighthouse);
        assert_eq!(AprsSymbol::new('/', 'L').kind(), AprsSymbolKind::Computer);
        // The two real tables carry no overlay.
        assert_eq!(AprsSymbol::new('/', '>').overlay(), None);
        assert_eq!(AprsSymbol::new('\\', '>').overlay(), None);
    }

    /// A corrupt symbol must not index outside the table.
    #[test]
    fn an_out_of_range_code_is_unknown() {
        assert_eq!(AprsSymbol::new('/', '\u{1}').kind(), AprsSymbolKind::Unknown);
        assert_eq!(AprsSymbol::new('/', 'é').kind(), AprsSymbolKind::Unknown);
    }
}
