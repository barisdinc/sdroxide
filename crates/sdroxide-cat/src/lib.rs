//! Serial CAT control for non-SoapySDR rigs (Icom CI-V / Yaesu / Xiegu).
//!
//! NATIVE ONLY — links `serialport`; must never be a dependency of any
//! wasm-targeted crate. The rest of the app talks to it only through the
//! opaque [`CatHandle`] (a background serial thread), so no serial types leak
//! into the engine or UI.

/// Icom CI-V framing and parsing. Public because the Icom LAN backend tunnels
/// the same protocol over UDP and must not carry a second copy of it.
pub mod civ;
/// ELAD framing. Public for the same reason [`civ`] is: the native ELAD backend
/// tunnels these commands through the FDM-DUO's USB interface and must not
/// carry a second copy of them.
pub mod elad;
mod elecraft;
mod flrig;
mod kenwood;
mod rigctld;
mod yaesu;

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use sdroxide_types::{
    CatConfig, CatFamily, CwKeying, DigiMode, LineState, Mode, ModeControl, Parity, PttMethod,
    StopBits, TxTelemetry,
};
use tracing::{info, warn};

/// How long after sdroxide keys or unkeys the rig's own transmit state is
/// ignored. Covers a read that was already in flight across the edge, and the
/// rig's own turnaround, without being long enough to miss a real over.
const PTT_SETTLE: Duration = Duration::from_millis(600);

/// How long a rig that has stopped answering the transmit read is still
/// believed to be transmitting. A rig cannot be left keyed on this side of the
/// link by a control port that has gone quiet: the S-meter would stay blanked
/// and every key-down would be refused, with nothing on screen to say why.
const RIG_TX_MAX_AGE: Duration = Duration::from_millis(2000);

/// The fastest the meters are ever asked for, whatever the poll rate is set to.
///
/// Five times a second is already faster than anyone reads a needle, and every
/// reading is a frame on the control port. On the radios where that port shares
/// an internal USB bus with the sound card — an IC-7300 is a four-port hub with
/// a CP2102 and a PCM2901 behind it, both at full speed — frames the operator
/// cannot see the benefit of are paid for in audio the operator can hear the
/// loss of.
const METER_FLOOR: Duration = Duration::from_millis(200);

/// How often the dial and mode are still read on a rig that reports its own
/// changes unasked (CI-V transceive).
///
/// This is a safety net, not the thing that follows the knob: a broadcast that
/// went missing — a collision on a shared bus, a buffer that overflowed — would
/// otherwise leave the readout on a frequency the radio left seconds ago, and
/// nothing else would ever notice. Three seconds is short enough that a missed
/// broadcast is a glitch rather than a wrong dial, and long enough that the
/// poll it replaces is gone as a source of traffic.
const PUSHED_POLL_PERIOD: Duration = Duration::from_secs(3);

/// A scope that has sent nothing for this long has stopped. Sweeps arrive
/// several times a second, so this is a silence, not a slow sweep — and it is
/// long enough to sit through a band change or a menu opened on the radio.
/// Mirrors the LAN backend's watchdog, which exists for the same reasons: the
/// enables are fire-and-forget, and several ordinary things stop the sweeps.
const SCOPE_STALL: Duration = Duration::from_secs(3);

/// How soon after a stall to ask the radio again, and the ceiling the interval
/// backs off to while it stays quiet. The enables are idempotent, but a rig
/// with no scope at all — or one whose CI-V USB port is still linked to
/// [REMOTE], so the waveform never reaches this side — must not be asked twice
/// a second forever on a link where every frame is bus time the audio pays for.
const SCOPE_RETRY: Duration = Duration::from_secs(2);
const SCOPE_RETRY_MAX: Duration = Duration::from_secs(30);

/// One finished sweep of the rig's own spectrum scope: what it covers, and its
/// amplitudes on the radio's 0..=160 scale.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeFrame {
    pub center_hz: f64,
    /// Full width, low edge to high edge.
    pub span_hz: f64,
    pub bins: Vec<u8>,
}

/// Whether this configuration streams the rig's scope over the serial link.
///
/// Three gates, all needed: the operator asked for it, the family is Icom (the
/// only serial dialect with `27 00`), and the link is fast enough to carry the
/// sweeps at all — below [`sdroxide_types::CAT_SCOPE_MIN_BAUD`] they would
/// bury every poll and PTT, so a slow link silently declines rather than
/// degrading the control channel. The caller surfaces the note for that case.
pub fn scope_active(cfg: &CatConfig) -> bool {
    cfg.scope
        && cfg.family == CatFamily::Icom
        && cfg.serial.baud >= sdroxide_types::CAT_SCOPE_MIN_BAUD
}

/// How many dial polls the mode rides along with one of.
///
/// The dial has to keep up with a hand on the VFO knob. The mode is a discrete
/// setting somebody changes a handful of times in an evening, and asking for it
/// at the same rate spends a frame every time on an answer that is the same one
/// it was last time.
const MODE_POLL_EVERY: u32 = 4;

/// The shortest the mode is ever asked for, whatever [`MODE_POLL_EVERY`] works
/// out to. A fast poll rate is somebody buying a responsive dial; it is not a
/// reason to interrogate a setting that has not moved.
const MODE_POLL_FLOOR: Duration = Duration::from_secs(1);

/// How often the rig is asked what *mode* it is in, at a given poll rate.
///
/// Never slower than the dial poll's own ceiling: at the bottom of the range
/// the two meet and the split stops mattering, which is the right place for it
/// to stop.
fn mode_poll_period(cfg: &CatConfig) -> Duration {
    (poll_period(cfg) * MODE_POLL_EVERY).clamp(MODE_POLL_FLOOR, Duration::from_secs(5))
}

/// How often the rig's meters are asked for while receiving, at a given poll
/// rate — the cadence the serial thread runs. Only the receive side: the
/// transmit meter is the SWR, which stays at [`METER_FLOOR`] whatever the
/// setting says.
fn meter_period(cfg: &CatConfig) -> Duration {
    poll_period(cfg).max(METER_FLOOR)
}

/// How long one of the rig's S-meter readings stands in for the next.
///
/// Three of the rig's own answers, and never less than a second and a half: an
/// ordinary gap between them is covered, and a link that has gone quiet is not
/// — a needle left standing at the last thing a dead port said is worse than no
/// needle. It has to follow the poll rate, because at a low one the gap between
/// two honest answers is longer than any fixed window would allow for and the
/// meter would blank between every pair of them; the floor is there so that
/// raising the poll rate cannot make the needle *more* twitchy than it was.
pub fn signal_max_age(cfg: &CatConfig) -> Duration {
    (meter_period(cfg) * 3).max(Duration::from_millis(1500))
}

/// How often the rig is asked what its dial and mode are, at a given poll rate.
///
/// Clamped at both ends: below 0.2 Hz the readout stops being a readout, and
/// above ~33 Hz there is no room left between frames anyway (see `FRAME_GAP`).
fn poll_period(cfg: &CatConfig) -> Duration {
    Duration::from_secs_f32((1.0 / cfg.poll_hz.max(0.2)).min(5.0))
}

/// A change the rig reported (external dial/mode movement) or that we read back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CatUpdate {
    Freq(f64),
    Mode(Mode),
    /// TX SWR reading (routed to the telemetry channel, not the control channel).
    Swr(f32),
    /// TX ALC reading, 0.0..=1.0 of the rig's own meter. Routed alongside
    /// [`CatUpdate::Swr`]. Distinct from the engine's own drive measurement,
    /// which is what SDRoxide SENDS rather than what the rig does about it.
    Alc(f32),
    /// TX power-output reading as a `0.0..=1.0` fraction of the rig's full
    /// scale (routed to the telemetry channel, like [`CatUpdate::Swr`]).
    Po(f32),
    /// RX S-meter reading in dBm, from the rig's own meter (routed to the
    /// signal channel, not the control channel).
    Signal(f32),
    /// The transmit power the rig is set to, as a `0..1` fraction of its
    /// maximum. Read once when the port opens, so the panel's Drive slider ends
    /// up where the radio's own power control already is.
    Power(f32),
    /// Which antenna socket the rig says it is receiving on, by the name that
    /// family gives the port (see [`Protocol::antennas`]).
    ///
    /// Read once when the port opens and never polled, so this is the radio's
    /// own setting being *adopted* — the same shape as [`Self::Power`], and for
    /// the same reason: the operator set it at the front panel, it survived the
    /// power cycle, and imposing a remembered one on top would move an antenna
    /// relay nobody asked to move.
    Antenna(&'static str),
    /// The rig is transmitting under its *own* control — a hand on the mic
    /// button, a foot switch, its VOX, its keyer. `true` is keyed.
    ///
    /// Only ever an over sdroxide did not ask for: an over we keyed is one the
    /// engine already knows about, and reporting it back would be an echo. See
    /// the suppression around `ptt_ours` in the serial thread.
    Ptt(bool),
}

/// Enumerate serial ports for the settings UI. USB-style ports (ttyACM/ttyUSB,
/// where CAT rigs like the X6100 appear) are listed first; the many legacy
/// `/dev/ttyS*` entries — which the non-libudev sysfs scan can't filter to
/// only present ones — sort to the end.
pub fn available_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    let rank = |p: &str| -> u8 {
        if p.contains("ttyACM") || p.contains("ttyUSB") {
            0
        } else if p.contains("ttyS") {
            2
        } else {
            1
        }
    };
    ports.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    ports
}

/// Per-family framing. `parse` consumes complete frames from a rolling buffer.
trait Protocol: Send {
    fn set_freq(&mut self, hz: f64) -> Vec<u8>;
    fn set_mode(&mut self, m: Mode) -> Vec<u8>;
    /// CAT-command PTT (only used when `PttMethod::Cat`).
    fn ptt(&self, on: bool) -> Vec<u8>;
    /// Frames that request the rig's current freq + mode — the whole poll.
    fn poll_requests(&self) -> Vec<Vec<u8>>;
    /// The part of [`Self::poll_requests`] that asks for the dial alone, sent
    /// on the polls the mode does not ride along with (see [`MODE_POLL_EVERY`]).
    ///
    /// Defaults to the whole poll, so a family added without a thought spared
    /// for this keeps asking for everything every time — the behaviour every
    /// family had before the split existed.
    fn dial_requests(&self) -> Vec<Vec<u8>> {
        self.poll_requests()
    }
    /// Frames requesting TX telemetry (SWR / power), polled only while keyed.
    /// Empty for families with no such read.
    fn tx_telemetry_requests(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Frames requesting the rig's S-meter, polled only while receiving.
    ///
    /// A CAT rig sends us audio it has already demodulated, filtered and
    /// levelled — there is no signal left on this side of the sound card to
    /// measure — so its own meter is the only S-meter the operator can be
    /// shown. Empty for families with no such read, which fall back to the
    /// level of the audio itself.
    fn rx_telemetry_requests(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Frames asking whether the rig is transmitting, polled while sdroxide is
    /// *not* the one keying it.
    ///
    /// This is how an over the operator started at the radio — mic button, foot
    /// switch, VOX, the rig's own keyer — becomes something sdroxide knows
    /// about, so the meter can show the SWR of it and nothing here tries to key
    /// on top of it. Empty for families with no such read, which simply never
    /// learn about an over they were not asked for.
    fn tx_state_requests(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Frames written once when the link opens, before anything else — for a
    /// profile that must ask what is on the other end before its other frames
    /// mean anything: the model, the power scale, the mode names. Empty — the
    /// default — for every family that needs no introduction.
    fn open_requests(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Frames that ask the rig to run its spectrum scope and stream the sweeps
    /// here — written when the link opens, and again by the watchdog whenever
    /// the sweeps stop. All idempotent, for exactly that reason. Empty — the
    /// default — for every family without a streamable scope, which is every
    /// family but Icom.
    fn scope_requests(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// The sweep [`Protocol::parse`] most recently completed, if one has since
    /// the last call. Sweeps carry no sequence and only the newest can be
    /// drawn, so this is a take of the latest rather than a queue.
    fn take_scope_sweep(&mut self) -> Option<ScopeFrame> {
        None
    }

    /// Frames that switch the rig's *own* RIT, XIT and split off, sent once
    /// when the port opens. sdroxide carries all three on the dial (the rig's
    /// dial is the only frequency control a CAT rig gives us), so anything the
    /// radio is still holding would add to ours unseen. Empty for families with
    /// no such command.
    fn clear_offsets(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Longest run of CW text this rig's keyer takes in one go, or 0 when it
    /// cannot be keyed from text at all. The caller sends no more than this per
    /// [`Protocol::send_cw`], so nothing has to be truncated on the way out.
    fn cw_chunk_len(&self) -> usize {
        0
    }
    /// Hand `text` to the rig's own keyer. The rig keys itself — this is *not*
    /// wrapped in PTT, and must not be: a transmitter already keyed by CAT is
    /// one the keyer cannot key.
    fn send_cw(&mut self, _text: &str) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Stop a message the rig is part way through sending. Empty for families
    /// with no such command — there, an abort can only stop the *next* chunk.
    fn abort_cw(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Set the rig keyer's speed. The rig keys at its own speed, so the panel's
    /// WPM has no effect on the air until it has been sent here.
    fn set_cw_wpm(&mut self, _wpm: f32) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Set the rig's own receive filter, from audio-band edges in Hz either
    /// side of the dial.
    ///
    /// The mode is passed because no family expresses a filter without one: the
    /// same 500 Hz is one index in CW and a different one in SSB, and some rigs
    /// take a width in CW but a pair of slope frequencies in SSB. What each
    /// family can express varies too, so a protocol that cannot say this
    /// particular passband on this particular rig returns nothing — leaving the
    /// radio's filter where the operator last put it, which is a far better
    /// answer than a guess at an index.
    fn set_filter(&mut self, _mode: Mode, _lo_hz: f32, _hi_hz: f32) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Whether [`Protocol::set_filter`] reaches this family at all.
    fn commands_filter(&self) -> bool {
        false
    }

    /// Set the transmitter's output power, as a `0..1` fraction of what the rig
    /// can do.
    ///
    /// This is the only transmit level a CAT rig has that means anything in
    /// *every* mode. The level of the audio we put into its sound card is not
    /// one: a rig in CW keys its own transmitter from its own keyer and never
    /// looks at the sound card, and one asked to TUNE holds a carrier the audio
    /// has no part in either. Empty for families with no such command, where
    /// there is nothing but the audio.
    fn set_power(&mut self, _frac: f32) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Frames asking what the rig's power is set to, sent once when the port
    /// opens so the panel can *adopt* the rig's own level instead of imposing a
    /// remembered one on it. Empty for families with no such read.
    fn read_power(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Whether [`Protocol::set_power`] reaches this family at all.
    fn commands_power(&self) -> bool {
        false
    }

    /// The antenna sockets this family can switch the *receiver* between, named
    /// as the rest of sdroxide names ports. Empty — the default — for the
    /// families with no such command, which is all of them but ELAD.
    ///
    /// Receive only, and deliberately: the one rig here with two sockets
    /// transmits out of the same one either way, so there is no transmit port
    /// to choose.
    fn antennas(&self) -> &'static [&'static str] {
        &[]
    }
    /// Put the receiver on `name`, which is one of [`Protocol::antennas`].
    fn set_antenna(&mut self, _name: &str) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Frames asking which socket the rig is on, sent once when the port opens
    /// so the panel adopts the radio's own setting (see
    /// [`CatUpdate::Antenna`]). Empty for families with no such read.
    fn read_antenna(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Whether a mode change can move this rig's *dial*.
    ///
    /// Most radios keep the VFO where it is when the mode changes. Some can be
    /// set not to: they shift the displayed frequency by the CW pitch when
    /// entering or leaving CW, so that zero-beat stays zero-beat, and the
    /// operator's 14.050 becomes 14.050 6 without anything having asked for it.
    /// A protocol that answers true has the frequency re-asserted behind every
    /// mode command it writes — which is what the radio's own documentation
    /// tells applications to do.
    fn mode_moves_dial(&self) -> bool {
        false
    }

    /// True when [`Protocol::parse`] learned something about the rig's framing
    /// that invalidates frames written before it — the Yaesu frequency-field
    /// width. Reported once, to whoever can re-issue the frame.
    fn reframed(&mut self) -> bool {
        false
    }

    /// True when [`Protocol::parse`] saw the rig refuse a command since the
    /// last call. Which command it refused is not in the answer — CI-V's "NG"
    /// carries nothing but itself — so this is only worth reporting where the
    /// caller knows what it just sent. Reported once, then cleared.
    fn refused(&mut self) -> bool {
        false
    }

    /// True while this rig is reporting its own dial and mode changes unasked —
    /// Icom's "CI-V transceive", which every other family here lacks.
    ///
    /// Deliberately proven in both directions rather than assumed or timed out.
    /// It is a setting in the radio's own menu, so nothing is claimed until a
    /// broadcast has actually arrived and parsed, and until then the poll runs
    /// exactly as it always did.
    ///
    /// A timeout is the wrong way to withdraw the claim: at idle — precisely
    /// when the traffic is worth saving — nothing changes, so no broadcasts
    /// arrive to keep it alive, and a timeout would put the poll back on every
    /// rig that simply had nothing to report. What actually disproves it is a
    /// *change* that arrived without one: the safety-net poll answering with a
    /// dial or a mode that nobody here commanded and no broadcast announced.
    /// The rig moved and did not say so, which is the whole of the question.
    fn pushes_updates(&self) -> bool {
        false
    }

    fn parse(&mut self, buf: &mut Vec<u8>) -> Vec<CatUpdate>;

    /// Called with every protocol-generated frame the moment it is actually
    /// written to the link — and only then. What a profile *generates* is not
    /// what goes out: `set_mode` is also called purely to compute a frame for
    /// comparison (see [`ModeMemory`]), and a frame the dedup discards was
    /// never sent. A profile whose replies do not name the request they answer
    /// — flrig's XML-RPC — correlates on this instead of on the reply.
    fn wrote(&mut self, _frame: &[u8]) {}

    /// The link has (re)opened. Anything a profile holds that describes bytes
    /// in flight — a half-read reply, a queue of requests awaiting answers —
    /// is about a connection that no longer exists and must be dropped here.
    /// The protocol object itself outlives reconnects on purpose (what it has
    /// *learned* about the rig is still true); this is only about the wire.
    fn link_opened(&mut self) {}
}

/// CI-V protocol (Icom + Xiegu). `radio` is the CI-V transceiver address.
struct Civ {
    radio: u8,
    /// The `1A` sub-command that switches DATA mode on this model, or `None`
    /// where the model has none — see [`civ::set_mode_frames`].
    data_sub: Option<u8>,
    /// The rig answered "NG" since this was last read (see
    /// [`Protocol::refused`]).
    nak: bool,
    /// The rig has broadcast a change nobody asked for — its transceive setting
    /// is on (see [`Protocol::pushes_updates`]).
    pushed: bool,
    /// When the last broadcast arrived, so a polled answer that disagrees with
    /// [`Self::seen_freq`] can be told from one that merely overtook a knob
    /// still being turned. Turning the VFO produces a broadcast per step, and a
    /// read issued in the middle of that comes back describing a dial that has
    /// already moved on — which is transceive working, not failing.
    last_push: Option<Instant>,
    /// The dial and mode this end has reason to believe the rig is on: whatever
    /// was last broadcast, answered, or commanded from here. A polled answer
    /// that disagrees with these is a change nobody was told about.
    seen_freq: Option<f64>,
    seen_mode: Option<u8>,
    /// Whether this session streams the rig's scope, and the half-span to
    /// command it to (`None` leaves the radio's own span alone). See
    /// [`scope_active`] for the gates.
    scope: bool,
    scope_half_span: Option<f64>,
    /// Reassembles the `27 00` sweeps, which arrive in ~11 fragments over
    /// serial where the LAN delivers them whole.
    scope_assembler: civ::ScopeAssembler,
    /// The newest finished sweep, until [`Protocol::take_scope_sweep`] takes it.
    scope_finished: Option<ScopeFrame>,
}

/// How long after a broadcast a disagreeing polled answer is put down to the
/// two having crossed on the wire rather than to transceive being off.
///
/// One round trip is all it takes, and a knob being turned produces broadcasts
/// far faster than that; a second is generous in the direction that costs
/// nothing, since guessing wrong here only means polling normally for a while.
const PUSH_CROSSED_WIRES: Duration = Duration::from_secs(1);

impl Civ {
    fn new(radio: u8, data_sub: Option<u8>) -> Civ {
        Civ {
            radio,
            data_sub,
            nak: false,
            pushed: false,
            last_push: None,
            seen_freq: None,
            seen_mode: None,
            scope: false,
            scope_half_span: None,
            scope_assembler: civ::ScopeAssembler::default(),
            scope_finished: None,
        }
    }

    /// Stream the rig's scope this session, sweeping `half_span` either side of
    /// the dial (`None` keeps whatever span the radio's own screen is on).
    fn with_scope(mut self, half_span: Option<f64>) -> Civ {
        self.scope = true;
        self.scope_half_span = half_span;
        self
    }

    /// Whether the rig turning up at `now`, where we believed `seen`, disproves
    /// the transceive claim.
    ///
    /// Only a *polled* answer can: a broadcast is the rig reporting itself,
    /// which is the claim holding. Only a disagreeing one can: an answer that
    /// matches what we already believed says nothing either way, and the safety
    /// net produces one of those every few seconds. And only outside
    /// [`PUSH_CROSSED_WIRES`], which is where a read and a broadcast that
    /// crossed on the wire live.
    fn moved_silently<T: PartialEq>(&self, seen: &Option<T>, now: &T, broadcast: bool) -> bool {
        !broadcast
            && self.pushed
            && seen.as_ref().is_some_and(|was| was != now)
            && self.last_push.is_none_or(|at| at.elapsed() > PUSH_CROSSED_WIRES)
    }
}

impl Protocol for Civ {
    fn set_freq(&mut self, hz: f64) -> Vec<u8> {
        // Where we have just put the rig is somewhere it got to without a
        // broadcast, and legitimately so — recording it here is what stops our
        // own tuning reading as the radio moving behind our back.
        self.seen_freq = Some(hz.round());
        civ::set_freq_frame(self.radio, hz)
    }
    fn set_mode(&mut self, m: Mode) -> Vec<u8> {
        self.seen_mode = Some(civ::mode_to_civ(m));
        civ::set_mode_frames(self.radio, m, self.data_sub).concat()
    }
    fn ptt(&self, on: bool) -> Vec<u8> {
        civ::ptt_frame(self.radio, on)
    }
    fn poll_requests(&self) -> Vec<Vec<u8>> {
        vec![civ::read_freq_frame(self.radio), civ::read_mode_frame(self.radio)]
    }
    fn dial_requests(&self) -> Vec<Vec<u8>> {
        vec![civ::read_freq_frame(self.radio)]
    }
    fn tx_telemetry_requests(&self) -> Vec<Vec<u8>> {
        // Three reads per telemetry tick. All are answered on the same command
        // and are told apart by their sub-command byte on the way back in.
        vec![
            civ::read_swr_frame(self.radio),
            civ::read_alc_frame(self.radio),
            civ::read_po_frame(self.radio),
        ]
    }
    fn tx_state_requests(&self) -> Vec<Vec<u8>> {
        vec![civ::read_ptt_frame(self.radio)]
    }
    fn rx_telemetry_requests(&self) -> Vec<Vec<u8>> {
        vec![civ::read_smeter_frame(self.radio)]
    }
    fn clear_offsets(&self) -> Vec<Vec<u8>> {
        civ::clear_offsets_frames(self.radio)
    }
    fn cw_chunk_len(&self) -> usize {
        civ::CW_MAX
    }
    fn send_cw(&mut self, text: &str) -> Vec<Vec<u8>> {
        civ::send_cw_frame(self.radio, text).into_iter().collect()
    }
    fn abort_cw(&mut self) -> Vec<Vec<u8>> {
        vec![civ::stop_cw_frame(self.radio)]
    }
    fn set_cw_wpm(&mut self, wpm: f32) -> Vec<Vec<u8>> {
        vec![civ::keyer_speed_frame(self.radio, wpm)]
    }
    fn set_filter(&mut self, mode: Mode, lo_hz: f32, hi_hz: f32) -> Vec<Vec<u8>> {
        civ::set_filter_frame(self.radio, mode, lo_hz, hi_hz).into_iter().collect()
    }
    fn commands_filter(&self) -> bool {
        true
    }
    fn set_power(&mut self, frac: f32) -> Vec<Vec<u8>> {
        vec![civ::set_power_frame(self.radio, frac)]
    }
    fn read_power(&self) -> Vec<Vec<u8>> {
        vec![civ::read_power_frame(self.radio)]
    }
    fn commands_power(&self) -> bool {
        true
    }
    /// The same enable sequence the LAN backend sends, because it is the same
    /// scope: run it, stream it here, and — when a span is chosen — put it in
    /// centre mode so it follows the dial, since a scope left in a fixed mode
    /// ignores the span command and sits on a slice of band the dial is not in.
    fn scope_requests(&self) -> Vec<Vec<u8>> {
        if !self.scope {
            return Vec::new();
        }
        let mut out =
            vec![civ::scope_on_frame(self.radio, true), civ::scope_output_frame(self.radio, true)];
        if let Some(half) = self.scope_half_span {
            out.push(civ::scope_mode_frame(self.radio, civ::ScopeMode::Center));
            out.push(civ::scope_span_frame(self.radio, half));
        }
        out
    }
    fn take_scope_sweep(&mut self) -> Option<ScopeFrame> {
        self.scope_finished.take()
    }
    fn refused(&mut self) -> bool {
        std::mem::take(&mut self.nak)
    }
    fn pushes_updates(&self) -> bool {
        self.pushed
    }
    fn parse(&mut self, buf: &mut Vec<u8>) -> Vec<CatUpdate> {
        let mut out = Vec::new();
        for reply in civ::parse_frames(buf) {
            // Ignore our own echoes (controller-sourced frames).
            if reply.from == civ::CONTROLLER_ADDR {
                continue;
            }
            match reply.cmd {
                // The transceive broadcasts (`0x00` frequency, `0x01` mode):
                // what a rig with that setting on sends, unasked, the moment
                // its dial or its mode moves. Same payloads as the answers to
                // the reads below, addressed to nobody in particular.
                //
                // These are only ever *from* the radio. `0x00` and `0x01` are
                // the controller's "set frequency" and "set mode" — but this
                // end sets with `0x05`/`0x06`, and anything from the controller
                // address was already skipped above as our own echo. The LAN
                // backend has folded the same pair together since it was
                // written (`IcomNetSource::on_reply`); this is the serial side
                // of the link catching up with it.
                0x00 | 0x03 => {
                    if let Some(hz) = civ::decode_freq(&reply.data) {
                        let broadcast = reply.cmd == 0x00;
                        self.pushed &= !self.moved_silently(&self.seen_freq, &hz, broadcast);
                        if broadcast {
                            self.pushed = true;
                            self.last_push = Some(Instant::now());
                        }
                        self.seen_freq = Some(hz);
                        out.push(CatUpdate::Freq(hz));
                    }
                }
                0x01 | 0x04 => {
                    if let Some(&b) = reply.data.first() {
                        let broadcast = reply.cmd == 0x01;
                        // Judged on the mode *byte*, not the app's `Mode`: two
                        // app modes share a byte on CI-V (USB and USB-DATA),
                        // and a rig that has moved between them has not moved
                        // as far as this command can see.
                        self.pushed &= !self.moved_silently(&self.seen_mode, &b, broadcast);
                        if broadcast {
                            self.pushed = true;
                            self.last_push = Some(Instant::now());
                        }
                        self.seen_mode = Some(b);
                        if let Some(m) = civ::civ_to_mode(b) {
                            out.push(CatUpdate::Mode(m));
                        }
                    }
                }
                // Level read (0x14): only the transmit power (0x0A) is asked
                // for, and only when the port opens.
                0x14 => {
                    if let Some(frac) = civ::parse_power_reply(&reply.data) {
                        out.push(CatUpdate::Power(frac));
                    }
                }
                // Meter read (0x15): while transmitting the SWR sub-meter
                // (0x12), the ALC one (0x13) and the power-output one (0x11);
                // while receiving the S-meter (0x02). The sub-command byte in
                // the reply says which arrived — nothing else does, since all
                // four are answered on the one command, which is why each
                // parser checks it and returns None otherwise.
                0x15 => {
                    if let Some(swr) = civ::parse_swr_reply(&reply.data) {
                        out.push(CatUpdate::Swr(swr));
                    } else if let Some(alc) = civ::parse_alc_reply(&reply.data) {
                        out.push(CatUpdate::Alc(alc));
                    } else if let Some(po) = civ::parse_po_reply(&reply.data) {
                        out.push(CatUpdate::Po(po));
                    } else if let Some(dbm) = civ::parse_smeter_reply(&reply.data) {
                        out.push(CatUpdate::Signal(dbm));
                    }
                }
                // "NG": the rig would not do what it was asked. Plenty of these
                // are expected — every sub-command a given model doesn't
                // implement answers this way — so it is only noted here, for a
                // caller that knows it just sent something that mattered.
                // Transceiver status (0x1C): sub-command 0x00 is the PTT
                // line. Asked for only while sdroxide is not keying, so an
                // answer of "transmitting" means the operator is on the mic.
                0x1C => {
                    if let Some(on) = civ::parse_ptt_reply(&reply.data) {
                        out.push(CatUpdate::Ptt(on));
                    }
                }
                // A scope sweep fragment (`27 00`), unsolicited once the
                // enables have taken. Over serial a sweep spans ~11 frames;
                // only a completed reassembly is worth surfacing, and only the
                // newest — see `Protocol::take_scope_sweep`.
                0x27 => {
                    if let Some((info, bins)) = civ::parse_scope_frame(&reply.data)
                        .and_then(|s| self.scope_assembler.push(s))
                        && !bins.is_empty()
                    {
                        self.scope_finished = Some(ScopeFrame {
                            center_hz: info.center_hz,
                            span_hz: info.span_hz,
                            bins,
                        });
                    }
                }
                civ::NG => self.nak = true,
                _ => {}
            }
        }
        out
    }
}

/// Output power Yaesu and Kenwood rigs are assumed to have at full scale, in
/// watts.
///
/// Both families' `PC` sets a *number of watts*, and neither has a command that
/// says how many the rig has — so a 0..1 fraction can only be turned into one
/// against an assumption. A hundred watts is what all but a handful of the rigs
/// these two dialects cover put out (the FT-891, FT-991A, FTDX10, FTDX101D,
/// FT-710, TS-590, TS-2000, TS-890 are all 100 W), and the exceptions are the
/// *bigger* ones — an FTDX101MP or a TS-480HX simply tops out at half the
/// slider. Erring low is the right way to be wrong about a transmitter.
///
/// The other two families need none of this. Icom's power level spans whatever
/// the radio has, so the fraction is the setting; and Elecraft, whose rigs run
/// from a 12 W KX2 to a 110 W K3, has an `OM` query that says which one is on
/// the other end of the cable.
const ASCII_FULL_POWER_W: f32 = 100.0;

/// Where to put the dial back after a mode change moved it, or `None` when
/// there is nothing to put back.
///
/// What we last *set* comes first: that is where the operator asked to be, and
/// it is right even if the rig has since shifted itself somewhere else. Only
/// when nothing has been set — a session that has done nothing but adopt the
/// radio's own dial — does the last frequency the rig *reported* stand in, and
/// it is the pre-shift one, because the reply carrying the shifted frequency
/// cannot have arrived before the mode command that caused it went out.
fn dial_to_restore(moves: bool, last_sent: Option<f64>, reported: Option<f64>) -> Option<f64> {
    moves.then(|| last_sent.or(reported)).flatten()
}

/// Piecewise-linear interpolation over a rising calibration table, clamped to
/// its ends.
///
/// Every meter a rig reports arrives as a number on a scale its manufacturer
/// chose and only ever published as a handful of points — "reading 130 is S9",
/// "reading 89 is 2:1" — so turning one into a decibel or a ratio is always
/// this: find the segment, interpolate, and refuse to extrapolate past either
/// end, where the curve is not just unknown but often not monotonic.
pub(crate) fn interp(cal: &[(f32, f32)], x: f32) -> f32 {
    if x <= cal[0].0 {
        return cal[0].1;
    }
    for w in cal.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if x <= x1 {
            return y0 + (y1 - y0) * (x - x0) / (x1 - x0);
        }
    }
    cal[cal.len() - 1].1
}

/// dBm of an S9 signal on HF, the reference every S-meter curve here is
/// expressed against.
pub(crate) const S9_DBM: f32 = -73.0;

/// `PC` — set output power (Yaesu "new CAT" and Kenwood alike). The three-digit
/// field is in watts, and the families' documented minimum is 5 W: a rig cannot
/// be asked for less, so the bottom of the slider is as low as it goes.
fn pc_set_frame(frac: f32) -> Vec<u8> {
    let w = (frac.clamp(0.0, 1.0) * ASCII_FULL_POWER_W).round().clamp(5.0, 999.0) as u32;
    format!("PC{w:03};").into_bytes()
}

/// `PC;` — ask what the power is set to.
fn pc_read_frame() -> Vec<u8> {
    b"PC;".to_vec()
}

/// The payload of a `PC` reply (the digits after `PC`) as a 0..1 fraction, or
/// `None` when it isn't a number.
fn pc_parse(rest: &str) -> Option<f32> {
    let w: u32 = rest.trim().parse().ok()?;
    Some((w as f32 / ASCII_FULL_POWER_W).clamp(0.0, 1.0))
}

fn make_protocol(cfg: &CatConfig) -> Box<dyn Protocol> {
    match cfg.family {
        // A Xiegu speaks the dialect but is not an Icom: none of the model
        // table applies to it, so it gets the plain mode command.
        CatFamily::Xiegu => Box::new(Civ::new(cfg.icom_radio_id, None)),
        CatFamily::Icom => {
            let civ = Civ::new(cfg.icom_radio_id, cfg.icom_model.data_mode_sub());
            Box::new(if scope_active(cfg) {
                civ.with_scope(cfg.scope_span.half_span_hz())
            } else {
                civ
            })
        }
        CatFamily::Yaesu => Box::new(yaesu::Yaesu::new()),
        CatFamily::Kenwood => Box::new(kenwood::Kenwood::new(cfg.kenwood_send)),
        CatFamily::Elecraft => Box::new(elecraft::Elecraft::new()),
        CatFamily::Elad => Box::new(elad::Elad::new(cfg.elad_tx_input)),
        CatFamily::Rigctld => Box::new(rigctld::Rigctld::new()),
        CatFamily::Flrig => Box::new(flrig::Flrig::new(cfg.flrig_addr.trim().to_string())),
    }
}

enum CatCmd {
    Freq(f64),
    Mode(Mode),
    Ptt(bool),
    /// Text for the rig's own keyer to send.
    Cw(String),
    /// Stop a message the rig is part way through.
    CwAbort,
    CwWpm(f32),
    /// Output power as a 0..1 fraction of the rig's maximum.
    Power(f32),
    /// The rig's own receive filter: the mode it applies to, and the audio-band
    /// edges in Hz.
    Filter(Mode, f32, f32),
    /// Which antenna socket to receive on, by name.
    Antenna(String),
    Stop,
}

/// Opaque handle to the running serial thread.
pub struct CatHandle {
    cmd_tx: Sender<CatCmd>,
    event_rx: Receiver<CatUpdate>,
    telem_rx: Receiver<TxTelemetry>,
    signal_rx: Receiver<f32>,
    /// The newest finished scope sweep, written by the serial thread and taken
    /// by [`CatHandle::take_scope_sweep`]. A slot rather than a channel because
    /// only the latest sweep can be drawn — one falling behind must overwrite,
    /// not queue.
    scope: std::sync::Arc<std::sync::Mutex<Option<ScopeFrame>>>,
    cw_chunk_len: usize,
    commands_power: bool,
    commands_filter: bool,
    antennas: &'static [&'static str],
}

impl CatHandle {
    pub fn set_freq(&self, hz: f64) {
        let _ = self.cmd_tx.send(CatCmd::Freq(hz));
    }
    pub fn set_mode(&self, m: Mode) {
        let _ = self.cmd_tx.send(CatCmd::Mode(m));
    }
    pub fn set_ptt(&self, on: bool) {
        let _ = self.cmd_tx.send(CatCmd::Ptt(on));
    }
    /// How much CW text this rig's keyer takes at a time, or `None` if it
    /// cannot be keyed from text.
    pub fn cw_chunk_len(&self) -> Option<usize> {
        (self.cw_chunk_len > 0).then_some(self.cw_chunk_len)
    }
    /// Hand `text` to the rig's keyer. No more than [`Self::cw_chunk_len`] at a
    /// time, and not again until the rig has finished the last lot — a rig part
    /// way through a message has nowhere to put a second one.
    pub fn send_cw(&self, text: String) {
        let _ = self.cmd_tx.send(CatCmd::Cw(text));
    }
    pub fn abort_cw(&self) {
        let _ = self.cmd_tx.send(CatCmd::CwAbort);
    }
    /// Set the rig keyer's speed — but only when the rig is the one doing the
    /// sending. A rig has one keyer and its paddle uses it too, so an operator
    /// who is not keying from here keeps their own speed.
    pub fn set_cw_wpm(&self, wpm: f32) {
        if self.cw_chunk_len == 0 {
            return;
        }
        let _ = self.cmd_tx.send(CatCmd::CwWpm(wpm));
    }
    /// Set the transmitter's output power, as a `0..1` fraction of what the rig
    /// can do. Silently ignored on a family with no power command, where the
    /// level of the audio going into the rig is the only control there is.
    pub fn set_power(&self, frac: f32) {
        if !self.commands_power {
            return;
        }
        let _ = self.cmd_tx.send(CatCmd::Power(frac));
    }
    /// Whether [`Self::set_power`] reaches this rig.
    pub fn commands_power(&self) -> bool {
        self.commands_power
    }
    /// Set the rig's own receive filter to the passband the operator chose,
    /// as audio-band edges in Hz. Silently ignored on a family — or a model —
    /// whose filter sdroxide cannot address.
    pub fn set_filter(&self, mode: Mode, lo_hz: f32, hi_hz: f32) {
        if !self.commands_filter {
            return;
        }
        let _ = self.cmd_tx.send(CatCmd::Filter(mode, lo_hz, hi_hz));
    }
    /// The antenna sockets this rig can put its receiver on, or empty where the
    /// family has no such command. What the caller publishes as
    /// `DeviceCaps::antennas_rx`.
    pub fn antennas(&self) -> &'static [&'static str] {
        self.antennas
    }
    /// Put the receiver on `name`, one of [`Self::antennas`]. Silently ignored
    /// on a rig with one socket, and on a name that rig has never heard of.
    pub fn set_antenna(&self, name: &str) {
        if !self.antennas.contains(&name) {
            return;
        }
        let _ = self.cmd_tx.send(CatCmd::Antenna(name.to_string()));
    }
    /// Non-blocking drain of rig-reported freq/mode changes.
    pub fn poll(&self) -> Vec<CatUpdate> {
        self.event_rx.try_iter().collect()
    }
    /// Latest TX telemetry (SWR) the rig reported, or `None` if nothing new
    /// arrived since the last call. A default (all-`None`) value is pushed when
    /// PTT drops, so the reading clears on unkey.
    pub fn poll_telemetry(&self) -> Option<TxTelemetry> {
        self.telem_rx.try_iter().last()
    }

    /// The rig's own S-meter in dBm, or `None` if nothing new arrived since the
    /// last call. Only rigs whose family has such a read report one; the rest
    /// never send here at all.
    pub fn poll_signal(&self) -> Option<f32> {
        self.signal_rx.try_iter().last()
    }

    /// The newest finished sweep of the rig's own spectrum scope, or `None`
    /// when nothing new has completed since the last take. Only an Icom with
    /// [`CatConfig::scope`] on and a fast enough link ever produces one — see
    /// [`scope_active`].
    pub fn take_scope_sweep(&self) -> Option<ScopeFrame> {
        self.scope.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl Drop for CatHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(CatCmd::Stop);
    }
}

/// Blocking one-shot query of the rig's current frequency + mode, used at
/// startup so the app adopts the radio's state instead of overwriting it.
/// Returns `None` if the port can't be opened or the rig doesn't answer.
pub fn query_once(cfg: &CatConfig) -> Option<(Option<f64>, Option<Mode>)> {
    let mut port = open_link(cfg).ok()?;
    let mut protocol = make_protocol(cfg);
    protocol.link_opened();
    for req in protocol.poll_requests() {
        let _ = port.write_all(&req);
        protocol.wrote(&req);
    }
    let _ = port.flush();
    let mut rx = Vec::new();
    let mut buf = [0u8; 128];
    let (mut freq, mut mode) = (None, None);
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline && (freq.is_none() || mode.is_none()) {
        if let Ok(n) = port.read(&mut buf) {
            if n > 0 {
                rx.extend_from_slice(&buf[..n]);
                for u in protocol.parse(&mut rx) {
                    match u {
                        CatUpdate::Freq(hz) => freq = Some(hz),
                        CatUpdate::Mode(m) => mode = Some(m),
                        // No meter, the power, or the transmit state is
                        // requested during the startup query.
                        CatUpdate::Swr(_)
                        | CatUpdate::Alc(_)
                        | CatUpdate::Po(_)
                        | CatUpdate::Signal(_)
                        | CatUpdate::Power(_)
                        | CatUpdate::Antenna(_)
                        | CatUpdate::Ptt(_) => {}
                    }
                }
            }
        }
    }
    (freq.is_some() || mode.is_some()).then_some((freq, mode))
}

/// Spawn the serial CAT thread from a persisted [`CatConfig`].
pub fn spawn(cfg: CatConfig) -> CatHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (telem_tx, telem_rx) = crossbeam_channel::unbounded();
    let (signal_tx, signal_rx) = crossbeam_channel::unbounded();
    // Asked of the framing before it goes to the thread, so the keyer can size
    // its chunks to the rig without reaching across the channel to find out.
    let cw_chunk_len =
        if cfg.cw_keying == CwKeying::Cat { make_protocol(&cfg).cw_chunk_len() } else { 0 };
    // Same reason: the engine asks whether this rig's power can be commanded
    // before it commands anything.
    let commands_power = make_protocol(&cfg).commands_power();
    let commands_filter = make_protocol(&cfg).commands_filter();
    // And the same again for the antenna sockets: the caps this device
    // publishes are built before a single frame has gone out, so the list has
    // to come from the framing rather than from the rig.
    let antennas = make_protocol(&cfg).antennas();
    let scope = std::sync::Arc::new(std::sync::Mutex::new(None));
    let scope_in = scope.clone();
    std::thread::Builder::new()
        .name("sdroxide-cat".into())
        .spawn(move || serial_thread(cfg, cmd_rx, event_tx, telem_tx, signal_tx, scope_in))
        .expect("spawn cat thread");
    CatHandle {
        cmd_tx,
        event_rx,
        telem_rx,
        signal_rx,
        scope,
        cw_chunk_len,
        commands_power,
        commands_filter,
        antennas,
    }
}

fn map_parity(p: Parity) -> serialport::Parity {
    match p {
        Parity::None => serialport::Parity::None,
        Parity::Even => serialport::Parity::Even,
        Parity::Odd => serialport::Parity::Odd,
    }
}
fn map_stop(s: StopBits) -> serialport::StopBits {
    match s {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}
fn map_data_bits(n: u8) -> serialport::DataBits {
    match n {
        5 => serialport::DataBits::Five,
        6 => serialport::DataBits::Six,
        7 => serialport::DataBits::Seven,
        _ => serialport::DataBits::Eight,
    }
}

/// The shortest gap left between two frames written to the rig.
///
/// A transceiver serves its control port with the same processor that runs the
/// radio, and it acts on one command at a time: a frame that arrives while the
/// rig is still working through the previous one can simply be missed, and
/// nothing on the wire says so. The case that matters is key-down, which asserts
/// the mode (and, with split or XIT, the transmit frequency) and then keys —
/// changing mode is among the slowest things a rig does, and a PTT lost behind
/// one is an over that never reaches the air while everything else about the
/// link looks healthy.
///
/// Only consecutive writes wait; a frame sent on its own goes out at once. The
/// whole traffic here is a handful of short frames a second, so this costs
/// nothing that can be noticed — and [`ModeMemory`] keeps the mode off the wire
/// entirely when the rig is already in it, which is what makes key-down a single
/// frame in the ordinary case.
const FRAME_GAP: Duration = Duration::from_millis(30);

/// Shortest gap between two output-power writes. A slider drag is worth one
/// frame every so often, not one per pixel; a hundred milliseconds is faster
/// than anyone can read a wattmeter and slow enough that the queue in front of
/// the next PTT stays empty.
const POWER_GAP: Duration = Duration::from_millis(100);

/// Shortest gap between two receive-filter writes. Longer than the power's: a
/// filter edge is dragged rather than nudged, several frames can go out per
/// change on some families, and nothing about a receive filter is urgent.
const FILTER_GAP: Duration = Duration::from_millis(250);

/// Write one frame, leaving at least [`FRAME_GAP`] since the last one went out.
/// Returns true on a write error — the caller's signal to reconnect.
///
/// The protocol is told about every frame that goes out ([`Protocol::wrote`])
/// — here, and nowhere else, so that "generated" and "written" cannot drift
/// apart on it. An empty frame is a profile saying "nothing to send" (an
/// unmappable mode, say) and writes nothing, waits for nothing, tells nothing.
fn write_frame(
    port: &mut dyn Link,
    protocol: &mut dyn Protocol,
    frame: &[u8],
    last_write: &mut Instant,
) -> bool {
    if frame.is_empty() {
        return false;
    }
    let since = last_write.elapsed();
    if since < FRAME_GAP {
        std::thread::sleep(FRAME_GAP - since);
    }
    let failed = port.write_all(frame).is_err();
    protocol.wrote(frame);
    *last_write = Instant::now();
    failed
}

/// Write an output-power level, unless it is the one the rig was last given.
/// Returns true on a write error — the caller's signal to reconnect.
///
/// Skipping a level already written is what keeps the assertion before every
/// key-down free: the rig is served one frame at a time, and a needless one in
/// front of a PTT delays the transmitter coming up.
fn write_power(
    port: &mut dyn Link,
    protocol: &mut dyn Protocol,
    frac: f32,
    last_sent: &mut Option<f32>,
    last_write: &mut Instant,
) -> bool {
    if *last_sent == Some(frac) {
        return false;
    }
    let mut failed = false;
    for f in protocol.set_power(frac) {
        failed |= write_frame(port, protocol, &f, last_write);
    }
    if !failed {
        *last_sent = Some(frac);
    }
    failed
}

/// What mode the rig is in, held as the frame that would put it there — either
/// because it was told so, or because it said so on its last poll.
///
/// Every key-down asserts the mode, so that an over cannot go out in whatever
/// the rig happens to have been left in. Asserting it is not free, though: the
/// rig acts on the command every time, which on an Icom also re-selects filter
/// 1 under an operator who chose another, and leaves the radio busy at exactly
/// the moment the PTT frame arrives behind it. So the command is only written
/// when it would actually change something.
///
/// What is compared is the frame, not the mode: two of the app's modes can be
/// one thing to a rig (DIGU rides on USB, and that is what goes on the wire),
/// and the rig can only report back the one it has.
#[derive(Default)]
struct ModeMemory(Option<Vec<u8>>);

impl ModeMemory {
    /// True when `frame` still needs sending — the rig is in some other mode,
    /// or has not said which. Records it as sent.
    fn needs(&mut self, frame: &[u8]) -> bool {
        if self.0.as_deref() == Some(frame) {
            return false;
        }
        self.0 = Some(frame.to_vec());
        true
    }

    /// The rig reported the mode `frame` would have set. That is where it is,
    /// whoever put it there — a mode the operator selected on the radio itself
    /// is one there is no need to command back onto it.
    fn reported(&mut self, frame: &[u8]) {
        if self.0.as_deref() != Some(frame) {
            self.0 = Some(frame.to_vec());
        }
    }
}

/// The link to the radio: a serial port, or a TCP connection to a `rigctld`.
///
/// Everything above this point is bytes in and bytes out, which is why the
/// network case fits at all — a rigctld speaks a line protocol over a socket
/// exactly as a transceiver speaks a frame protocol over a wire, and the
/// driver's rate limiting, coalescing and reconnection all mean the same thing
/// on both. The one thing that does not carry over is the pair of control
/// lines, which a socket simply does not have.
trait Link: Send {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    /// Drive RTS, where there is one. A network link has none, so a `PTT
    /// method` of RTS or DTR keys nothing there — see [`PttMethod`].
    fn set_rts(&mut self, _on: bool) {}
    fn set_dtr(&mut self, _on: bool) {}
}

impl Link for Box<dyn serialport::SerialPort> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(self, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(self, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(self)
    }
    fn set_rts(&mut self, on: bool) {
        let _ = self.write_request_to_send(on);
    }
    fn set_dtr(&mut self, on: bool) {
        let _ = self.write_data_terminal_ready(on);
    }
}

impl Link for std::net::TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(self, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(self, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(self)
    }
}

/// How long a read waits before giving the loop its turn back. The same on both
/// transports: the driver polls, so a read that blocks is a driver that cannot
/// write.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Open whichever link this configuration describes.
fn open_link(cfg: &CatConfig) -> std::io::Result<Box<dyn Link>> {
    if cfg.family.is_network() {
        let addr = match cfg.family {
            CatFamily::Flrig => cfg.flrig_addr.trim(),
            _ => cfg.rigctld_addr.trim(),
        };
        let stream = std::net::TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        // Every command here is a handful of bytes that wants to be on the wire
        // now: a key-down waiting on Nagle for another 40 ms is an over that
        // starts late.
        let _ = stream.set_nodelay(true);
        return Ok(Box::new(stream));
    }
    let s = &cfg.serial;
    let port = serialport::new(&s.path, s.baud)
        .data_bits(map_data_bits(s.data_bits))
        .parity(map_parity(s.parity))
        .stop_bits(map_stop(s.stop_bits))
        .timeout(READ_TIMEOUT)
        .open()
        .map_err(std::io::Error::other)?;
    Ok(Box::new(port))
}

/// What to call this link in the log — a port and a baud rate, or a host.
pub fn link_label(cfg: &CatConfig) -> String {
    match cfg.family {
        CatFamily::Rigctld => format!("rigctld at {}", cfg.rigctld_addr.trim()),
        CatFamily::Flrig => format!("flrig at {}", cfg.flrig_addr.trim()),
        _ => format!("{} at {} baud", cfg.serial.path, cfg.serial.baud),
    }
}

/// Apply a forced control-line level (ignored when `LineState::None`). If a
/// line is used for PTT, PTT owns it instead (handled in the loop).
fn apply_line(port: &mut dyn Link, forced: LineState, rts: bool) {
    let level = match forced {
        LineState::None => return,
        LineState::High => true,
        LineState::Low => false,
    };
    if rts { port.set_rts(level) } else { port.set_dtr(level) }
}

/// What mode to command the rig into for a given app mode. FT8/FT4 use the
/// separate `digi_mode` setting; every other mode obeys `mode_control`
/// (CAT = mirror the selected mode to the rig; Radio = don't touch it).
///
/// `digi_mode` is a choice between two *sidebands* — plain USB or the rig's
/// DATA-U position — so it only makes sense for a mode that rides a
/// sideband. The carrier-centred modes (RIFP, VHF packet) frequency-modulate
/// the carrier instead: sending them as USB puts the rig in the wrong
/// modulation entirely, and nothing downstream would say so. Those fall
/// through to `mode_control`, where each protocol's own map answers DATA-FM.
///
/// CW keyed as audio (`CwKeying::Audio`) rides the digi sideband too: a rig
/// put in CW keys its own transmitter and never modulates what arrives at its
/// sound card, so the keyed sidetone (MCW) only reaches the air from USB or
/// DATA-U. Commanding CW there was the issue #119 dead key — a Xiegu G90
/// switched out of U-D made no power at all.
fn commanded_mode(cfg: &CatConfig, app_mode: Mode) -> Option<Mode> {
    let rides_digi_sideband =
        (app_mode.is_digital() && !app_mode.is_sstv() && !app_mode.is_carrier_centered())
            || (app_mode == Mode::Cw && cfg.cw_keying == CwKeying::Audio);
    if rides_digi_sideband {
        return match cfg.digi_mode {
            DigiMode::Radio => None,
            DigiMode::Usb => Some(Mode::Usb),
            DigiMode::Data => Some(Mode::Digu),
        };
    }
    match cfg.mode_control {
        ModeControl::Cat => Some(app_mode),
        ModeControl::Radio => None,
    }
}

fn serial_thread(
    cfg: CatConfig,
    cmd_rx: Receiver<CatCmd>,
    event_tx: Sender<CatUpdate>,
    telem_tx: Sender<TxTelemetry>,
    signal_tx: Sender<f32>,
    scope_out: std::sync::Arc<std::sync::Mutex<Option<ScopeFrame>>>,
) {
    let mut protocol = make_protocol(&cfg);
    let poll_period = poll_period(&cfg);
    // The meters follow the poll rate too. They used to run at a fixed 5 Hz,
    // which made the setting a half-measure: an operator turning the control
    // traffic down to quieten a shared USB bus took away the dial poll and left
    // the meter poll — the same number of frames — running underneath it.
    //
    // Only while receiving. The transmit side stays at `METER_FLOOR` whatever
    // the setting says: that reading is the SWR, the protection trip counts on
    // it arriving, and it only runs for the length of an over.
    let rx_meter_period = meter_period(&cfg);
    // The mode rides only every `MODE_POLL_EVERY`th dial poll — see
    // `mode_poll_period`.
    let mode_period = mode_poll_period(&cfg);
    // What the log was last told about the transceive stand-down. Kept per
    // process rather than per connection: `protocol` — and so what it has
    // learned about this rig — outlives a reconnect.
    let mut announced_push = false;
    // The latest of each TX meter, because they arrive in separate replies and
    // the consumer keeps only the last message sent. See the send site.
    let mut last_swr: Option<f32> = None;
    let mut last_alc: Option<f32> = None;
    let mut last_po: Option<f32> = None;
    // See `commanded_mode` for the app-mode → rig-mode policy.
    let mode_cmd = |app_mode: Mode| -> Option<Mode> { commanded_mode(&cfg, app_mode) };

    loop {
        // (Re)open the port, retrying on failure.
        let mut port = match open_link(&cfg) {
            Ok(p) => {
                // The PTT method belongs in this line: a rig that answers every
                // read and still refuses to key is nearly always one being asked
                // to key some way it isn't set up for, and this is where that
                // shows.
                info!(
                    link = %link_label(&cfg),
                    family = cfg.family.label(),
                    ptt = cfg.ptt.label(),
                    "CAT link open"
                );
                p
            }
            Err(e) => {
                warn!(link = %link_label(&cfg), "CAT open failed: {e}");
                // Wait, but still honor a Stop.
                match cmd_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(CatCmd::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return;
                    }
                    _ => continue,
                }
            }
        };
        // Forced control lines (unless the line is the PTT method).
        if cfg.ptt != PttMethod::Rts {
            apply_line(&mut *port, cfg.serial.force_rts, true);
        }
        if cfg.ptt != PttMethod::Dtr {
            apply_line(&mut *port, cfg.serial.force_dtr, false);
        }
        // Deassert PTT line at start.
        match cfg.ptt {
            PttMethod::Rts => port.set_rts(false),
            PttMethod::Dtr => port.set_dtr(false),
            _ => {}
        }
        // When the last frame went out, so consecutive writes can be spaced
        // (see `FRAME_GAP`). Backdated: the first write waits for nothing.
        let mut last_write = Instant::now() - FRAME_GAP;
        // A fresh connection: whatever the protocol still holds about bytes in
        // flight on the old one is now about nothing.
        protocol.link_opened();
        // Introductions first, for the profile that needs them: the answers to
        // these (a power scale, a mode list) are what its later frames are
        // interpreted against, and write order is reply order.
        for f in protocol.open_requests() {
            write_frame(&mut *port, &mut *protocol, &f, &mut last_write);
        }
        // Don't force a mode on connect — adopt the rig's current mode (read via
        // `query_once`/poll); the app commands mode only when the operator picks one.
        // RIT/XIT/split are the exception: those we do own, so clear the rig's
        // own copies rather than let them offset us invisibly.
        for f in protocol.clear_offsets() {
            write_frame(&mut *port, &mut *protocol, &f, &mut last_write);
        }
        // The transmit power is the rig's, not ours: ask what it is set to and
        // let the panel adopt it, the same way the dial and the mode are
        // adopted. Imposing a remembered level instead would put an operator
        // who has never touched the slider on the air at whatever it defaults
        // to — on a radio they had already set the power on.
        for f in protocol.read_power() {
            write_frame(&mut *port, &mut *protocol, &f, &mut last_write);
        }
        // Which socket the receiver is on, asked the same once-per-connection
        // way and adopted the same way. The rig remembers it across power
        // cycles, so this is the only moment the panel can find out what the
        // operator left it on.
        for f in protocol.read_antenna() {
            write_frame(&mut *port, &mut *protocol, &f, &mut last_write);
        }
        // Start the rig's scope streaming, where this session wants it. The
        // enables are fire-and-forget — a rig without a scope answers NG and
        // that is the end of it — and the watchdog below re-sends them when
        // the sweeps stop, so a lost enable is a delay rather than a session
        // with no picture.
        let scope_wanted = {
            let reqs = protocol.scope_requests();
            for f in &reqs {
                write_frame(&mut *port, &mut *protocol, f, &mut last_write);
            }
            !reqs.is_empty()
        };
        let mut last_sweep = Instant::now();
        let mut next_scope_nudge = Instant::now() + SCOPE_STALL;
        let mut scope_retry = SCOPE_RETRY;

        let mut rx = Vec::with_capacity(256);
        let mut read_buf = [0u8; 256];
        let mut next_poll = Instant::now();
        // Backdated so the first poll of a connection carries the mode: the app
        // adopts the rig's mode rather than commanding one, and waiting a mode
        // period to find out what it is would leave the panel wrong meanwhile.
        let mut next_mode_poll = Instant::now();
        // Which meter is asked for depends on what the rig is doing: SWR while
        // keyed, S-meter while receiving.
        //
        // Started half a period behind the dial poll. Started together the two
        // fire together, and four frames go out back to back with nothing but
        // `FRAME_GAP` between them — the worst shape this traffic can have on a
        // radio whose control port shares a USB bus with its sound card, where
        // what the audio needs is the gaps rather than a lower average. Same
        // number of frames, spread out.
        //
        // Only the starting phase: both timers re-arm from the moment they
        // fire, so the two wander relative to each other over a long session.
        // Worth having anyway — the cost is one addition, and the worst it can
        // decay to is the lockstep it starts out avoiding.
        let mut next_meter = Instant::now() + rx_meter_period / 2;
        let mut ptt = false;
        // When a CAT key-down was last written, so the rig's refusal of one can
        // be told from the refusals its unimplemented sub-commands answer with.
        let mut ptt_written: Option<Instant> = None;
        let mut pending_freq: Option<f64> = None;
        let mut last_sent_freq: Option<f64> = None;
        let mut freq_deadline = Instant::now();
        // Output power, coalesced and rate-limited exactly as the frequency is:
        // dragging the Drive slider produces a command per pixel, and a rig
        // whose control port is served one frame at a time must not be handed
        // hundreds of them — the PTT behind that queue is the thing that would
        // suffer. Only a level that differs from the last one written goes out,
        // so the assertion before every key-down is free once it has settled.
        let mut pending_power: Option<f32> = None;
        let mut last_sent_power: Option<f32> = None;
        let mut power_deadline = Instant::now();
        // The rig's own receive filter, on the same rate limit and the same
        // only-on-change rule. Unlike the power it is never asserted before a
        // key-down: it is a receive setting, and the one moment it must not
        // compete for the bus is the moment the transmitter is coming up.
        let mut pending_filter: Option<(Mode, f32, f32)> = None;
        let mut last_sent_filter: Option<(Mode, f32, f32)> = None;
        let mut filter_deadline = Instant::now();
        // The socket this end has put the receiver on since the port opened, so
        // the opening read's answer can be told from the truth (see where it is
        // parsed). `None` until something is commanded, which is where a rig
        // nobody has switched stays.
        let mut last_sent_antenna: Option<&'static str> = None;
        let mut mode_memory = ModeMemory::default();
        // Only forward genuine changes so the engine isn't re-notified every poll.
        let mut emit_freq: Option<f64> = None;
        let mut emit_mode: Option<Mode> = None;
        // The rig's own transmit state as last reported upwards, and when its
        // last answer arrived. `None` = never answered, which is where a family
        // with no such read stays for good.
        let mut emit_rig_tx: Option<bool> = None;
        let mut last_tx_reply = Instant::now();
        // When sdroxide last keyed or unkeyed. A read already on the wire when
        // that happened comes back describing the other side of the edge, so
        // answers are ignored for a moment either side of one — otherwise every
        // unkey ends with one frame's worth of "the operator is on the mic".
        let mut ptt_edge = Instant::now() - PTT_SETTLE;

        let broke = 'io: loop {
            // Drain commands.
            loop {
                match cmd_rx.try_recv() {
                    Ok(CatCmd::Freq(hz)) => pending_freq = Some(hz), // coalesce
                    Ok(CatCmd::Mode(m)) => {
                        if let Some(mm) = mode_cmd(m) {
                            let f = protocol.set_mode(mm);
                            if mode_memory.needs(&f) {
                                if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                                    break 'io true;
                                }
                                // On a rig that shifts its VFO with the mode,
                                // that command has just moved the dial out from
                                // under the operator. Put it back — before the
                                // next poll reads the shifted frequency and
                                // walks the app's dial to it, and before any
                                // key-down, which is the moment being a pitch
                                // off frequency actually costs something.
                                if let Some(hz) = dial_to_restore(
                                    protocol.mode_moves_dial(),
                                    last_sent_freq,
                                    emit_freq,
                                ) {
                                    last_sent_freq = None; // past the dedup
                                    pending_freq = Some(hz);
                                    freq_deadline = Instant::now();
                                }
                            }
                        }
                    }
                    Ok(CatCmd::Ptt(on)) => {
                        // Key-down has to land at the level the engine asserted
                        // for it — the drive, or the tune level under TUNE — so
                        // the rate limit below is not allowed to leave that
                        // sitting in the queue while the transmitter comes up.
                        if on
                            && let Some(frac) = pending_power.take()
                            && write_power(
                                &mut *port,
                                &mut *protocol,
                                frac,
                                &mut last_sent_power,
                                &mut last_write,
                            )
                        {
                            break 'io true;
                        }
                        // Key-down has to land on the transmit frequency. With
                        // XIT or split the engine queues the transmit dial
                        // immediately before PTT, and the debounce below would
                        // otherwise let the first moment of the over go out
                        // where we were listening — so flush it first.
                        if on
                            && let Some(hz) = pending_freq.take()
                            && last_sent_freq != Some(hz)
                        {
                            let f = protocol.set_freq(hz);
                            if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                                break 'io true;
                            }
                            last_sent_freq = Some(hz);
                            emit_freq = Some(hz); // suppress the poll echo
                            freq_deadline = Instant::now() + Duration::from_millis(50);
                        }
                        let failed = match cfg.ptt {
                            PttMethod::Vox => false,
                            PttMethod::Rts => {
                                port.set_rts(on);
                                false
                            }
                            PttMethod::Dtr => {
                                port.set_dtr(on);
                                false
                            }
                            PttMethod::Cat => {
                                let f = protocol.ptt(on);
                                ptt_written = on.then(Instant::now);
                                write_frame(&mut *port, &mut *protocol, &f, &mut last_write)
                            }
                        };
                        if failed {
                            break 'io true;
                        }
                        ptt = on;
                        ptt_edge = Instant::now();
                        // Ask the meter that belongs to the new state straight
                        // away, rather than showing the other one's last reading
                        // for the rest of the current period.
                        next_meter = Instant::now();
                        if !on {
                            // Clear the readings so the meters drop on unkey.
                            // Both held values go too, or the next over would
                            // open carrying the last one from the previous one.
                            last_swr = None;
                            last_alc = None;
                            last_po = None;
                            // Clear the readings so the meters drop on unkey,
                            // here as well as at the receiver: a stale SWR held
                            // locally would be re-sent beside the next over's
                            // first PO reading and briefly look current.
                            let _ = telem_tx.send(TxTelemetry::default());
                        }
                    }
                    // CW the rig keys itself. Deliberately outside the PTT
                    // interlock above: the rig switches to transmit for the
                    // length of the message on its own, and asserting CAT PTT
                    // around it would hold a carrier the keyer cannot key.
                    Ok(CatCmd::Cw(text)) => {
                        // Nothing else asserts the level for this over: the rig
                        // keys itself, so there is no PTT here to hang it off —
                        // and CW is the mode where the rig's power is the only
                        // transmit control there is, the sound card having no
                        // part in it at all.
                        if let Some(frac) = pending_power.take()
                            && write_power(
                                &mut *port,
                                &mut *protocol,
                                frac,
                                &mut last_sent_power,
                                &mut last_write,
                            )
                        {
                            break 'io true;
                        }
                        for f in protocol.send_cw(&text) {
                            if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                                break 'io true;
                            }
                        }
                    }
                    Ok(CatCmd::CwAbort) => {
                        for f in protocol.abort_cw() {
                            if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                                break 'io true;
                            }
                        }
                    }
                    Ok(CatCmd::CwWpm(wpm)) => {
                        for f in protocol.set_cw_wpm(wpm) {
                            if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                                break 'io true;
                            }
                        }
                    }
                    // Coalesced exactly as the power is, and for the same
                    // reason: dragging a filter edge produces a command per
                    // pixel, and a rig served one frame at a time must not be
                    // handed hundreds of them.
                    Ok(CatCmd::Filter(m, lo, hi)) => pending_filter = Some((m, lo, hi)),
                    Ok(CatCmd::Power(frac)) => pending_power = Some(frac), // coalesce
                    // Not coalesced and not rate limited: an antenna is a click
                    // on a two-entry list, not a slider being dragged, and it
                    // is one frame either way.
                    Ok(CatCmd::Antenna(name)) => {
                        let frames = protocol.set_antenna(&name);
                        // A name the family does not have produces no frames,
                        // and must not be recorded as where the receiver is.
                        if !frames.is_empty()
                            && let Some(&port) = protocol.antennas().iter().find(|&&a| a == name)
                        {
                            last_sent_antenna = Some(port);
                        }
                        let mut failed = false;
                        for f in frames {
                            failed |= write_frame(&mut *port, &mut *protocol, &f, &mut last_write);
                        }
                        if failed {
                            break 'io true;
                        }
                    }
                    Ok(CatCmd::Stop) => return,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }

            // Rate-limited power write (only on change), on the same principle
            // as the frequency below it.
            if let Some(frac) = pending_power {
                let now = Instant::now();
                if now >= power_deadline {
                    pending_power = None;
                    power_deadline = now + POWER_GAP;
                    if write_power(
                        &mut *port,
                        &mut *protocol,
                        frac,
                        &mut last_sent_power,
                        &mut last_write,
                    ) {
                        break 'io true;
                    }
                }
            }

            // Rate-limited filter write, on the same principle — and skipped
            // entirely while transmitting, where a receive setting has nothing
            // to do and the bus belongs to the meter.
            if let Some(f) = pending_filter
                && !ptt
            {
                let now = Instant::now();
                if now >= filter_deadline {
                    pending_filter = None;
                    filter_deadline = now + FILTER_GAP;
                    if last_sent_filter != Some(f) {
                        let mut failed = false;
                        for frame in protocol.set_filter(f.0, f.1, f.2) {
                            failed |=
                                write_frame(&mut *port, &mut *protocol, &frame, &mut last_write);
                        }
                        if failed {
                            break 'io true;
                        }
                        last_sent_filter = Some(f);
                    }
                }
            }

            // Debounced frequency write (rate-limit to ~50 ms, only on change).
            if let Some(hz) = pending_freq {
                let now = Instant::now();
                if last_sent_freq != Some(hz) && now >= freq_deadline {
                    let f = protocol.set_freq(hz);
                    if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                        break 'io true;
                    }
                    last_sent_freq = Some(hz);
                    emit_freq = Some(hz); // suppress the poll echo of our own set
                    pending_freq = None;
                    freq_deadline = now + Duration::from_millis(50);
                }
            }

            // Poll the rig for external changes — unless it is already telling
            // us about them. A rig with CI-V transceive switched on broadcasts
            // its dial and its mode the moment either moves, which is both
            // faster than any poll and free, so the poll drops back to the
            // safety net that catches a broadcast gone missing.
            if Instant::now() >= next_poll {
                let pushes = protocol.pushes_updates();
                // Both edges are said out loud, because neither is visible from
                // anywhere else: the stand-down arms on the radio volunteering a
                // broadcast and disarms on it moving without one, both of which
                // happen whenever they happen, and each changes how much traffic
                // this thread puts on the wire. Without these lines, anyone
                // measuring the control traffic against audio dropouts cannot
                // tell which of the two rates they were measuring.
                if pushes != announced_push {
                    announced_push = pushes;
                    if pushes {
                        info!(
                            poll_s = PUSHED_POLL_PERIOD.as_secs(),
                            "the radio reports its own dial and mode (CI-V transceive is on); \
                             standing the dial poll down to a safety net"
                        );
                    } else {
                        info!(
                            poll_hz = cfg.poll_hz,
                            "the radio moved without reporting it (CI-V transceive is off); \
                             polling the dial at the configured rate again"
                        );
                    }
                }
                let period = if pushes { PUSHED_POLL_PERIOD } else { poll_period };
                next_poll = Instant::now() + period.max(poll_period);
                // The mode rides along every so often; the rest of the time the
                // poll is the dial on its own. Not while the rig is reporting
                // itself: that poll is already down to one every few seconds,
                // and splitting a frame off something that small buys nothing.
                let with_mode = pushes || Instant::now() >= next_mode_poll;
                if with_mode {
                    next_mode_poll = Instant::now() + mode_period;
                }
                let reqs =
                    if with_mode { protocol.poll_requests() } else { protocol.dial_requests() };
                for req in reqs {
                    if write_frame(&mut *port, &mut *protocol, &req, &mut last_write) {
                        break 'io true;
                    }
                }
            }

            // Poll the meter that applies right now: the SWR while keyed, the
            // rig's S-meter while receiving. Both ride the same command on CI-V
            // and only one of them is meaningful at a time, so they take turns
            // rather than sharing the bus.
            //
            // Which one applies is not only our own PTT: an over the operator
            // started at the radio is just as much a transmission, and the
            // meter that matters during it is the same one.
            //
            // The rate is the operator's while receiving and `METER_FLOOR`
            // while transmitting — the SWR feeds the protection trip, and an
            // over is short enough that its traffic is not what an afternoon of
            // receiving costs.
            if Instant::now() >= next_meter {
                let on_air = ptt || emit_rig_tx == Some(true);
                next_meter = Instant::now() + if on_air { METER_FLOOR } else { rx_meter_period };
                let mut reqs = if on_air {
                    protocol.tx_telemetry_requests()
                } else {
                    protocol.rx_telemetry_requests()
                };
                // ...and ask whether the rig has keyed itself. Not while we are
                // the ones keying it: the answer would be our own key-down
                // coming back, and the engine already knows about that.
                if !ptt {
                    reqs.extend(protocol.tx_state_requests());
                }
                for req in reqs {
                    if write_frame(&mut *port, &mut *protocol, &req, &mut last_write) {
                        break 'io true;
                    }
                }
            }

            // Start the scope again when its sweeps stop. Several ordinary
            // things stop them — the enable lost on the wire, the radio's own
            // scope screen closed, a menu opened — and nothing reports it, so
            // the strip would otherwise sit dead until a reconnect. The
            // enables are idempotent; the backoff keeps a rig that will never
            // sweep (no scope, or its CI-V USB port still linked to [REMOTE])
            // from being asked twice a second forever.
            if scope_wanted {
                let now = Instant::now();
                if ptt || emit_rig_tx == Some(true) {
                    // A rig does not sweep while it transmits, and an over is
                    // not a stall: hold the clock instead of nudging through it.
                    last_sweep = now;
                } else if now.duration_since(last_sweep) > SCOPE_STALL && now >= next_scope_nudge {
                    next_scope_nudge = now + scope_retry;
                    scope_retry = (scope_retry * 2).min(SCOPE_RETRY_MAX);
                    for f in protocol.scope_requests() {
                        if write_frame(&mut *port, &mut *protocol, &f, &mut last_write) {
                            break 'io true;
                        }
                    }
                }
            }

            // A rig that has stopped answering is not a rig that is still
            // transmitting. Without this, one lost reply at the wrong moment
            // would leave the app believing an over is in progress for as long
            // as the session lasts — S-meter blanked, transmit refused.
            if emit_rig_tx == Some(true) && last_tx_reply.elapsed() > RIG_TX_MAX_AGE {
                warn!("the radio stopped answering the transmit read; assuming it is receiving");
                emit_rig_tx = Some(false);
                let _ = event_tx.send(CatUpdate::Ptt(false));
            }

            // Read whatever arrived; parse and emit updates.
            match port.read(&mut read_buf) {
                Ok(0) => {}
                Ok(n) => {
                    rx.extend_from_slice(&read_buf[..n]);
                    let mut updates = protocol.parse(&mut rx);
                    if let Some(sweep) = protocol.take_scope_sweep() {
                        last_sweep = Instant::now();
                        scope_retry = SCOPE_RETRY;
                        *scope_out.lock().unwrap_or_else(|e| e.into_inner()) = Some(sweep);
                    }
                    // A reply can teach the framing how this particular rig
                    // addresses its frequency (see `Protocol::reframed`). What
                    // we sent before that was refused and the rig never moved,
                    // so the operator's last dial has to go out again — now in
                    // terms the rig accepts.
                    if protocol.reframed()
                        && let Some(hz) = last_sent_freq.take()
                    {
                        pending_freq = Some(hz);
                        freq_deadline = Instant::now();
                        // The frequency in this same batch is where the refused
                        // set left the rig — not somewhere the operator asked
                        // to be. Reporting it would walk the app's dial back to
                        // it for the moment before the re-issue lands.
                        updates.retain(|u| !matches!(u, CatUpdate::Freq(_)));
                    }
                    // A refusal on its own says nothing — rigs answer that way
                    // for every sub-command they don't have, and the offsets
                    // cleared at open collect a few. One arriving on the heels
                    // of a key-down is worth saying out loud: the operator is
                    // looking at a transmitter that did not key, with no other
                    // sign of why.
                    if protocol.refused()
                        && ptt_written.is_some_and(|t| t.elapsed() < Duration::from_millis(500))
                    {
                        ptt_written = None;
                        warn!(
                            "the radio refused a command at key-down — if it did not transmit, \
                             check its CI-V settings, or the PTT method in Settings → Radio"
                        );
                    }
                    for u in updates {
                        // The meters are telemetry, not control changes: they go
                        // to their own channels and skip the freq/mode dedup
                        // below — a reading that repeats is still current, and
                        // dropping it would freeze the meter.
                        // ⛔ SWR and ALC arrive as SEPARATE replies, and the
                        // consumer keeps only the LAST message
                        // (`poll_telemetry` is `try_iter().last()`). So sending
                        // one field at a time would make each reading blank the
                        // other, and the casualty would be the SWR guard: it
                        // reads `None` as "the rig has not said anything yet"
                        // and would sit at that forever, never tripping. Both
                        // are therefore held here and sent together, so every
                        // message carries the latest of each.
                        let send = |swr, alc, po| {
                            let _ = telem_tx.send(TxTelemetry { fwd_w: None, swr, alc, po });
                        };
                        if let CatUpdate::Swr(v) = u {
                            last_swr = Some(v);
                            send(last_swr, last_alc, last_po);
                            continue;
                        }
                        if let CatUpdate::Alc(v) = u {
                            last_alc = Some(v);
                            send(last_swr, last_alc, last_po);
                            continue;
                        }
                        if let CatUpdate::Po(v) = u {
                            last_po = Some(v);
                            send(last_swr, last_alc, last_po);
                            continue;
                        }
                        if let CatUpdate::Signal(dbm) = u {
                            let _ = signal_tx.send(dbm);
                            continue;
                        }
                        // The rig's own transmit state. Deduped like the dial
                        // (a level re-reported five times a second is not five
                        // key-downs), and dropped entirely across one of our
                        // own PTT edges, where the answer in hand describes
                        // whichever side of the edge the read was issued on.
                        if let CatUpdate::Ptt(on) = u {
                            last_tx_reply = Instant::now();
                            if ptt || ptt_edge.elapsed() < PTT_SETTLE {
                                // Still worth recording: this is what the poll
                                // above switches meters on, and after our own
                                // over it must not be left saying "keyed".
                                emit_rig_tx = Some(false);
                                continue;
                            }
                            if emit_rig_tx != Some(on) {
                                emit_rig_tx = Some(on);
                                let _ = event_tx.send(u);
                            }
                            continue;
                        }
                        // The power the rig reports goes straight out: it is
                        // asked for once per connection, and the engine adopts
                        // it without answering, so there is nothing here to
                        // dedup and no loop to break. Recording it as sent is
                        // what makes the adoption free — the engine asserts the
                        // level it adopted before the first key-down, and that
                        // is now a level the rig is already on.
                        if let CatUpdate::Power(frac) = u {
                            last_sent_power = Some(frac);
                            let _ = event_tx.send(u);
                            continue;
                        }
                        // The socket the rig says it is on. Forwarded whole —
                        // it is asked for once per connection, so there are no
                        // repeats to dedup — unless it disagrees with a socket
                        // this end has already commanded on this connection, in
                        // which case it is the answer to the opening read
                        // crossing that command on the wire. Adopting it there
                        // would put the panel back on the port the operator
                        // just left, and leave it disagreeing with the radio.
                        if let CatUpdate::Antenna(a) = u {
                            if last_sent_antenna.is_none_or(|w| w == a) {
                                let _ = event_tx.send(u);
                            }
                            continue;
                        }
                        // Forward only genuine changes (poll repeats otherwise).
                        let changed = match u {
                            CatUpdate::Freq(hz) => {
                                let c = emit_freq.map(|f| (f - hz).abs() >= 1.0).unwrap_or(true);
                                if c {
                                    emit_freq = Some(hz);
                                }
                                c
                            }
                            CatUpdate::Mode(m) => {
                                // Also where the rig's mode is learned: what it
                                // reports is the truth about what it is in, and
                                // anything that isn't what we last set means the
                                // next mode command has to go out for real.
                                mode_memory.reported(&protocol.set_mode(m));
                                let c = emit_mode != Some(m);
                                if c {
                                    emit_mode = Some(m);
                                }
                                c
                            }
                            // The meters, the power and the transmit state are
                            // handled above.
                            CatUpdate::Swr(_)
                            | CatUpdate::Alc(_)
                            | CatUpdate::Po(_)
                            | CatUpdate::Signal(_)
                            | CatUpdate::Power(_)
                            | CatUpdate::Antenna(_)
                            | CatUpdate::Ptt(_) => false,
                        };
                        if changed {
                            let _ = event_tx.send(u);
                        }
                    }
                }
                // A read that found nothing in its window. Which of the two
                // kinds arrives is the transport's business — a serial port
                // times out, a socket would block — and neither is an error.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => {
                    warn!("CAT read error: {e}");
                    break 'io true;
                }
            }

            std::thread::sleep(Duration::from_millis(5));
        };

        if broke {
            warn!("CAT link error; reconnecting");
            std::thread::sleep(Duration::from_secs(1));
        } else {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdroxide_types::CatFamily;

    /// `FE FE 00 94 00 …` — the rig telling the bus its dial has moved.
    fn broadcast_freq(hz: f64) -> Vec<u8> {
        let mut b = vec![0xFE, 0xFE, 0x00, 0x94, 0x00];
        b.extend_from_slice(&civ::encode_freq(hz));
        b.push(0xFD);
        b
    }

    /// `FE FE E0 94 03 …` — the rig answering the read this end just sent.
    fn polled_freq(hz: f64) -> Vec<u8> {
        let mut b = vec![0xFE, 0xFE, civ::CONTROLLER_ADDR, 0x94, 0x03];
        b.extend_from_slice(&civ::encode_freq(hz));
        b.push(0xFD);
        b
    }

    fn icom() -> Box<dyn Protocol> {
        make_protocol(&CatConfig {
            family: CatFamily::Icom,
            icom_radio_id: 0x94,
            icom_model: sdroxide_types::IcomModel::Ic7300,
            ..CatConfig::default()
        })
    }

    fn icom_with_scope(baud: u32) -> Box<dyn Protocol> {
        make_protocol(&CatConfig {
            family: CatFamily::Icom,
            icom_radio_id: 0x94,
            icom_model: sdroxide_types::IcomModel::Ic7300,
            scope: true,
            serial: sdroxide_types::SerialConfig { baud, ..Default::default() },
            ..CatConfig::default()
        })
    }

    /// One `27 00` fragment as the rig sends it over serial: division `div` of
    /// `divs`, with `body` carrying the wave information (division 1) or bins.
    fn sweep_fragment(div: u8, divs: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![0xFE, 0xFE, civ::CONTROLLER_ADDR, 0x94, 0x27, 0x00, 0x00, div, divs];
        b.extend_from_slice(body);
        b.push(0xFD);
        b
    }

    #[test]
    fn the_scope_is_asked_for_only_on_a_link_its_sweeps_fit_down() {
        // 115200 carries the sweeps; the enable sequence is the LAN backend's:
        // scope on, output on, centre mode, span.
        let fast = icom_with_scope(115_200);
        let reqs = fast.scope_requests();
        assert_eq!(reqs.len(), 4);
        assert_eq!(reqs[0], vec![0xFE, 0xFE, 0x94, 0xE0, 0x27, 0x10, 0x01, 0xFD]);
        assert_eq!(reqs[1], vec![0xFE, 0xFE, 0x94, 0xE0, 0x27, 0x11, 0x01, 0xFD]);
        assert_eq!(&reqs[2][4..7], &[0x27, 0x14, 0x00]);
        assert_eq!(&reqs[3][4..6], &[0x27, 0x15]);
        // 19200 cannot: the sweeps would bury every poll and PTT, so the box
        // being ticked must not put them on the wire.
        assert!(icom_with_scope(19_200).scope_requests().is_empty());
        // And a rig that was never asked sends nothing either.
        assert!(icom().scope_requests().is_empty());
    }

    #[test]
    fn a_fragmented_sweep_reassembles_and_is_taken_once() {
        let mut p = icom_with_scope(115_200);
        // Division 1 carries the wave information and no bins — centre mode,
        // dial at 14.074 MHz, ±50 kHz — exactly as the serial transport splits
        // a sweep the LAN would deliver whole.
        let mut info = vec![0x00];
        info.extend_from_slice(&civ::encode_freq(14_074_000.0));
        info.extend_from_slice(&civ::encode_freq(50_000.0));
        info.push(0x00);
        let mut buf = sweep_fragment(1, 3, &info);
        assert!(p.parse(&mut buf).is_empty());
        assert!(p.take_scope_sweep().is_none(), "half a sweep must not be drawn");

        let mut buf = sweep_fragment(2, 3, &[10u8; 200]);
        assert!(p.parse(&mut buf).is_empty());
        let mut buf = sweep_fragment(3, 3, &[20u8; 275]);
        assert!(p.parse(&mut buf).is_empty());

        let sweep = p.take_scope_sweep().expect("a finished sweep");
        assert_eq!(sweep.center_hz, 14_074_000.0);
        assert_eq!(sweep.span_hz, 100_000.0, "a centred half-span reads as the full width");
        assert_eq!(sweep.bins.len(), 475);
        assert!(sweep.bins[..200].iter().all(|&b| b == 10));
        assert!(sweep.bins[200..].iter().all(|&b| b == 20));
        // A take is a take: the same sweep must not be drawn twice.
        assert!(p.take_scope_sweep().is_none());
    }

    /// The rig's mode is asserted on every key-down. Writing it every time is
    /// what this guards against: the rig acts on each one — re-selecting its
    /// filter, and busy for as long as it takes — with the PTT frame right
    /// behind it.
    #[test]
    fn the_mode_is_only_commanded_when_it_would_change_something() {
        let mut p = icom();
        let mut m = ModeMemory::default();
        // Nothing is known about the rig yet, so the mode goes out.
        assert!(m.needs(&p.set_mode(Mode::Usb)));
        // Asserting the same mode again — every subsequent key-down — does not.
        assert!(!m.needs(&p.set_mode(Mode::Usb)));
        // DIGU is USB on the wire for this family, so it is not a change either.
        assert!(!m.needs(&p.set_mode(Mode::Digu)));
        // A mode that really is different is written.
        assert!(m.needs(&p.set_mode(Mode::Cw)));
    }

    #[test]
    fn what_the_rig_reports_is_where_the_rig_is() {
        let mut p = icom();
        let mut m = ModeMemory::default();
        assert!(m.needs(&p.set_mode(Mode::Cw)));
        // The operator turns the mode knob on the radio itself. The app follows
        // it there, and commanding it back onto a mode it is already in is
        // exactly the wasted write this avoids.
        m.reported(&p.set_mode(Mode::Lsb));
        assert!(!m.needs(&p.set_mode(Mode::Lsb)));
        // And a mode the rig is *not* in still goes out.
        assert!(m.needs(&p.set_mode(Mode::Usb)));
    }

    /// A mode change on an Elecraft can shift the dial by the CW pitch, so the
    /// frequency is re-asserted behind every mode command. Which frequency is
    /// the whole question: the operator's, not the rig's.
    #[test]
    fn the_dial_put_back_after_a_mode_change_is_the_one_that_was_asked_for() {
        // What we last set wins — that is where the operator asked to be.
        assert_eq!(
            dial_to_restore(true, Some(14_050_000.0), Some(14_050_600.0)),
            Some(14_050_000.0)
        );
        // With nothing set this session, the last frequency the rig reported
        // stands in. It is the pre-shift one: the reply carrying the shifted
        // frequency cannot have arrived before the mode command that caused it.
        assert_eq!(dial_to_restore(true, None, Some(7_030_000.0)), Some(7_030_000.0));
        // Nothing known at all is nothing to put back.
        assert_eq!(dial_to_restore(true, None, None), None);
        // And a family whose dial does not move is not written to at all — a
        // needless frame in front of the next key-down is exactly what the
        // rate limiting elsewhere in this file exists to avoid.
        assert_eq!(dial_to_restore(false, Some(14_050_000.0), Some(14_050_600.0)), None);
    }

    /// Only the family whose radios document the behaviour asks for it.
    #[test]
    fn only_elecraft_re_asserts_the_dial_after_a_mode_change() {
        let family = |f| make_protocol(&CatConfig { family: f, ..CatConfig::default() });
        assert!(family(CatFamily::Elecraft).mode_moves_dial());
        for f in [CatFamily::Icom, CatFamily::Xiegu, CatFamily::Yaesu, CatFamily::Kenwood] {
            assert!(!family(f).mode_moves_dial(), "{f:?}");
        }
    }

    /// Issue #119: a rig put in CW keys its own transmitter and ignores its
    /// sound card, so CW keyed as audio (MCW) must ride the digi sideband.
    /// Commanding CW anyway was a dead key on a Xiegu G90 — switched out of
    /// U-D, it made no power at all.
    #[test]
    fn cw_keyed_as_audio_rides_the_digi_sideband() {
        let cfg = |digi| CatConfig {
            family: CatFamily::Xiegu,
            cw_keying: CwKeying::Audio,
            mode_control: ModeControl::Cat,
            digi_mode: digi,
            ..CatConfig::default()
        };
        assert_eq!(commanded_mode(&cfg(DigiMode::Radio), Mode::Cw), None);
        assert_eq!(commanded_mode(&cfg(DigiMode::Usb), Mode::Cw), Some(Mode::Usb));
        assert_eq!(commanded_mode(&cfg(DigiMode::Data), Mode::Cw), Some(Mode::Digu));
    }

    /// The rig's own keyer can only send with the rig *in* CW, so that route
    /// still commands it — the Icom/Yaesu/Kenwood/Elecraft text keying path
    /// must not change shape.
    #[test]
    fn cw_keyed_by_the_rig_is_still_commanded_as_cw() {
        let cfg =
            |mc| CatConfig { cw_keying: CwKeying::Cat, mode_control: mc, ..CatConfig::default() };
        assert_eq!(commanded_mode(&cfg(ModeControl::Cat), Mode::Cw), Some(Mode::Cw));
        assert_eq!(commanded_mode(&cfg(ModeControl::Radio), Mode::Cw), None);
    }

    /// DIGU picked at the panel is a rig mode, not a decode layer: it obeys
    /// Mode control verbatim rather than the digi sideband mapping.
    #[test]
    fn an_on_screen_digu_still_obeys_mode_control_not_digi_mode() {
        let cfg = CatConfig { digi_mode: DigiMode::Radio, ..CatConfig::default() };
        assert_eq!(commanded_mode(&cfg, Mode::Digu), Some(Mode::Digu));
    }

    /// The digital modes' sideband choice is not disturbed by how CW is keyed.
    #[test]
    fn ft8_ignores_the_cw_keying_setting() {
        for k in CwKeying::ALL {
            let cfg = CatConfig { cw_keying: k, digi_mode: DigiMode::Data, ..CatConfig::default() };
            assert_eq!(commanded_mode(&cfg, Mode::Ft8), Some(Mode::Digu), "{k:?}");
        }
    }

    /// `PC` is watts, and the families' documented floor is 5 W — a rig cannot
    /// be asked for nothing, so the bottom of the slider is as low as it goes
    /// rather than a `PC000;` the radio would reject outright.
    #[test]
    fn the_ascii_families_send_power_as_three_digits_of_watts() {
        let sent = |frac: f32| String::from_utf8(pc_set_frame(frac)).unwrap();
        assert_eq!(sent(1.0), "PC100;");
        assert_eq!(sent(0.5), "PC050;");
        assert_eq!(sent(0.05), "PC005;");
        assert_eq!(sent(0.0), "PC005;");
        // A slider cannot ask for more than the assumed full scale.
        assert_eq!(sent(2.0), "PC100;");
    }

    /// The control traffic is what the setting is for, so all of it has to be
    /// under the setting. The meters used to run at a fixed 5 Hz underneath the
    /// dial poll, which meant an operator turning the rate down to quieten a
    /// shared USB bus removed half the frames and left the other half running.
    #[test]
    fn the_meters_follow_the_poll_rate_the_operator_set() {
        let at =
            |hz: f32| meter_period(&CatConfig { poll_hz: hz, ..CatConfig::default() }).as_millis();
        // Turning the rate down turns the meter poll down with it.
        assert_eq!(at(1.0), 1000);
        assert_eq!(at(0.5), 2000);
        // ...but never faster than a needle can be read, however high the
        // setting goes: past this the frames buy nothing and cost bus time.
        assert_eq!(at(5.0), METER_FLOOR.as_millis());
        assert_eq!(at(20.0), METER_FLOOR.as_millis());
    }

    /// A reading has to stand in until the next one can arrive, and at a low
    /// poll rate that is a long way off — a fixed window would blank the needle
    /// between every pair of honest answers.
    #[test]
    fn a_meter_reading_outlives_the_gap_to_the_next_one() {
        let at = |hz: f32| signal_max_age(&CatConfig { poll_hz: hz, ..CatConfig::default() });
        for hz in [0.5, 1.0, 2.0, 5.0, 20.0] {
            let cfg = CatConfig { poll_hz: hz, ..CatConfig::default() };
            assert!(at(hz) > meter_period(&cfg), "{hz} Hz");
        }
        // And raising the rate cannot make the needle twitchier than it was.
        assert_eq!(at(20.0), Duration::from_millis(1500));
    }

    /// CI-V transceive: a rig with that setting on broadcasts its dial and its
    /// mode the moment either moves, which is both faster than any poll and
    /// free. Reading those broadcasts is what lets the poll stand down.
    #[test]
    fn an_icom_that_reports_its_own_dial_is_noticed() {
        let mut p = icom();
        // Nothing is claimed until a broadcast has actually arrived: the
        // setting lives in the radio's menu and is off as often as it is on.
        assert!(!p.pushes_updates());
        // An answer to a poll is not a broadcast, however much it looks like
        // one — cmd 0x03 is the reply to the read this end just sent, and it
        // comes back addressed to the controller rather than to the bus.
        assert_eq!(p.parse(&mut polled_freq(14_074_000.0)), vec![CatUpdate::Freq(14_074_000.0)]);
        assert!(!p.pushes_updates());
        // The broadcast is cmd 0x00, addressed to nobody in particular, and
        // carries the same five BCD bytes.
        assert_eq!(p.parse(&mut broadcast_freq(7_055_000.0)), vec![CatUpdate::Freq(7_055_000.0)]);
        assert!(p.pushes_updates());
    }

    /// The mode has its own broadcast (cmd 0x01), and it counts for the same
    /// reason — a rig that reports one reports the other.
    #[test]
    fn a_broadcast_mode_change_counts_as_the_rig_reporting_itself() {
        let mut p = icom();
        let mut buf = vec![0xFE, 0xFE, 0x00, 0x94, 0x01, civ::mode_to_civ(Mode::Cw), 0xFD];
        assert_eq!(p.parse(&mut buf), vec![CatUpdate::Mode(Mode::Cw)]);
        assert!(p.pushes_updates());
    }

    /// Our own frames come back on the bus. A rig address of `E0` is this end
    /// talking, and treating that echo as the radio reporting itself would have
    /// every family stand its poll down on the strength of its own commands.
    #[test]
    fn our_own_echo_is_not_the_rig_reporting_itself() {
        let mut p = icom();
        // `FE FE <radio> E0 05 …` — the set this end just wrote, seen again.
        let mut buf = civ::set_freq_frame(0x94, 14_074_000.0);
        assert!(p.parse(&mut buf).is_empty());
        assert!(!p.pushes_updates());
    }

    /// The claim has to be withdrawable, or an operator who switches Transceive
    /// off mid-session keeps the three-second safety net until they restart.
    /// What withdraws it is the rig turning up somewhere nobody sent it and no
    /// broadcast announced.
    #[test]
    fn a_rig_that_moves_without_saying_so_loses_the_stand_down() {
        let mut p = icom();
        assert_eq!(p.parse(&mut broadcast_freq(14_074_000.0)), vec![CatUpdate::Freq(14_074_000.0)]);
        assert!(p.pushes_updates());
        // The operator switches Transceive off and turns the knob. Nothing is
        // broadcast; the safety-net poll is what finds out, and finding out that
        // way is the proof.
        std::thread::sleep(PUSH_CROSSED_WIRES + Duration::from_millis(50));
        assert_eq!(p.parse(&mut polled_freq(14_080_000.0)), vec![CatUpdate::Freq(14_080_000.0)]);
        assert!(!p.pushes_updates());
    }

    /// ...but a polled answer that agrees with what we already believed says
    /// nothing either way. Every one of them would otherwise be evidence, and
    /// the safety net answers one every three seconds.
    #[test]
    fn a_poll_that_confirms_what_we_knew_is_not_evidence_of_anything() {
        let mut p = icom();
        p.parse(&mut broadcast_freq(14_074_000.0));
        std::thread::sleep(PUSH_CROSSED_WIRES + Duration::from_millis(50));
        for _ in 0..3 {
            p.parse(&mut polled_freq(14_074_000.0));
            assert!(p.pushes_updates());
        }
    }

    /// Nor is our own tuning. We move the rig with `set_freq` and the answer
    /// comes back saying so — a change that arrived without a broadcast, and
    /// entirely legitimately, because this end is the one that caused it.
    #[test]
    fn our_own_tuning_is_not_the_radio_moving_behind_our_back() {
        let mut p = icom();
        p.parse(&mut broadcast_freq(14_074_000.0));
        std::thread::sleep(PUSH_CROSSED_WIRES + Duration::from_millis(50));
        let _ = p.set_freq(18_100_000.0);
        p.parse(&mut polled_freq(18_100_000.0));
        assert!(p.pushes_updates());
    }

    /// A knob being turned broadcasts every step, so a read issued in the
    /// middle of one comes back describing a dial that has already moved on.
    /// That is transceive working, not failing.
    #[test]
    fn a_poll_that_crossed_a_broadcast_on_the_wire_is_excused() {
        let mut p = icom();
        p.parse(&mut broadcast_freq(14_074_000.0));
        // No sleep: the broadcast is still warm, so the stale answer behind it
        // is put down to the two crossing rather than to a silent rig.
        p.parse(&mut polled_freq(14_073_500.0));
        assert!(p.pushes_updates());
    }

    /// And it re-arms: the operator switches Transceive back on, the rig
    /// volunteers a broadcast, and the poll stands down again without a
    /// reconnect.
    #[test]
    fn the_stand_down_comes_back_when_the_rig_starts_reporting_again() {
        let mut p = icom();
        p.parse(&mut broadcast_freq(14_074_000.0));
        std::thread::sleep(PUSH_CROSSED_WIRES + Duration::from_millis(50));
        p.parse(&mut polled_freq(14_080_000.0));
        assert!(!p.pushes_updates());
        p.parse(&mut broadcast_freq(14_090_000.0));
        assert!(p.pushes_updates());
    }

    /// The dial has to keep up with a hand on the VFO knob; the mode is a
    /// setting somebody changes a few times an evening. Every family splits the
    /// two, and the dial half is the frequency read on its own.
    #[test]
    fn the_mode_does_not_ride_along_with_every_dial_poll() {
        for f in [
            CatFamily::Icom,
            CatFamily::Xiegu,
            CatFamily::Yaesu,
            CatFamily::Kenwood,
            CatFamily::Elecraft,
            CatFamily::Elad,
            CatFamily::Rigctld,
            CatFamily::Flrig,
        ] {
            let p = make_protocol(&CatConfig { family: f, ..CatConfig::default() });
            let (full, dial) = (p.poll_requests(), p.dial_requests());
            // The dial poll is strictly smaller, and it is the front of the
            // full one — the frequency read, with the mode left off the back.
            assert!(dial.len() < full.len(), "{f:?}");
            assert_eq!(dial, full[..dial.len()], "{f:?}");
        }
    }

    /// The mode's own cadence, which is the dial's divided down and then held
    /// inside a band at both ends.
    #[test]
    fn the_mode_is_read_a_fraction_as_often_as_the_dial() {
        let at = |hz: f32| mode_poll_period(&CatConfig { poll_hz: hz, ..CatConfig::default() });
        // The default: the dial twice a second, the mode every two.
        assert_eq!(at(2.0), Duration::from_secs(2));
        // A fast dial is somebody buying a responsive readout, not a reason to
        // interrogate a setting that has not moved.
        assert_eq!(at(20.0), MODE_POLL_FLOOR);
        // And at the quiet end the two meet rather than the mode running away.
        assert_eq!(at(0.5), Duration::from_secs(5));
        for hz in [0.2, 0.5, 1.0, 2.0, 5.0, 20.0] {
            let cfg = CatConfig { poll_hz: hz, ..CatConfig::default() };
            assert!(mode_poll_period(&cfg) >= poll_period(&cfg), "{hz} Hz");
        }
    }

    /// No other family has anything like it, and none of them may claim to.
    #[test]
    fn only_civ_rigs_report_themselves() {
        let family = |f| make_protocol(&CatConfig { family: f, ..CatConfig::default() });
        for f in [
            CatFamily::Yaesu,
            CatFamily::Kenwood,
            CatFamily::Elecraft,
            CatFamily::Rigctld,
            CatFamily::Flrig,
        ] {
            assert!(!family(f).pushes_updates(), "{f:?}");
        }
    }

    #[test]
    fn a_pc_reply_reads_back_as_the_fraction_that_set_it() {
        assert_eq!(pc_parse("100"), Some(1.0));
        assert_eq!(pc_parse("050"), Some(0.5));
        assert_eq!(pc_parse("005"), Some(0.05));
        // A rig that puts out more than the assumed full scale reports more
        // watts than we would ever ask for; the slider still tops out at 1.
        assert_eq!(pc_parse("200"), Some(1.0));
        // Not a number is not a power.
        assert_eq!(pc_parse(""), None);
        assert_eq!(pc_parse("abc"), None);
    }
}
