//! A self-contained session trace, so a user with an RFOne can produce a report
//! that a developer without one can read.
//!
//! This backend was written from the reference implementation and the firmware
//! source rather than on a bench, which makes the trace load-bearing rather
//! than a nicety. Everything here also goes to `tracing`, but `tracing` is the
//! wrong shape for a bug report: it needs `RUST_LOG` set before the run, and
//! asking somebody to reproduce an open failure with the right filter set is
//! asking them to reproduce it twice. A [`Trace`] is always on, bounded, and
//! dumps as one text block.
//!
//! # The three things it exists to settle
//!
//! **Which radio answered.** A prototype RFOne and an Airspy R2 share a USB id,
//! so the first thing any report needs is the firmware version string and the
//! board id — [`Trace::set_identity`] puts both outside the line ring, where a
//! long session cannot age them out.
//!
//! **The rate doubling, and which table a rate came from.**
//! [`Trace::rate_plan`] records the complex rate asked for, the programmed rate
//! that follows from it, how that was encoded, and what the receiver's own list
//! said. Three of this receiver's seven rates are not on that list at all and
//! have to go out by value; a span that is wrong by an octave, or a rate that
//! quietly snapped to a listed one, both look like a working receiver.
//!
//! **Which way the spectrum runs.** [`Trace::first_samples`] shows the raw
//! 12-bit values and the complex samples they became. Point the receiver at a
//! carrier deliberately above the dial and the energy has to land above DC; if
//! it lands below, the fs/4 rotation is inverted and every spectrum this
//! backend produces is mirrored. That cannot be checked from this side of the
//! wire, and on a symmetric signal it cannot be seen on screen either.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sdroxide_dsp::Complex32;

/// What the settings panel tells an operator, and what the manual repeats.
pub const FIELD_REPORT_HINT: &str = "This backend has not been verified against \
    real hardware. If it misbehaves, please attach the session trace \
    (Settings → Radio → Copy diagnostic report) to a bug report — it contains \
    every command exchanged with the receiver, the sample-rate arithmetic, and \
    the first samples decoded as I/Q.";

#[derive(Debug, Default)]
struct Inner {
    lines: VecDeque<String>,
    dropped: u64,
    identity: Option<String>,
}

/// A shared, bounded record of one HydraSDR RFOne session.
///
/// Cloning shares the same buffer — the stream thread and the `IqSource` both
/// hold one.
#[derive(Debug, Clone)]
pub struct Trace {
    inner: Arc<Mutex<Inner>>,
    started: Instant,
}

impl Default for Trace {
    fn default() -> Self {
        Trace::new()
    }
}

impl Trace {
    /// How many lines are kept before the oldest are discarded. At roughly 80
    /// bytes a line this is a few hundred kilobytes — small enough to hold
    /// permanently, long enough to cover an open plus several minutes of use.
    pub const CAPACITY: usize = 4000;

    pub fn new() -> Trace {
        Trace { inner: Arc::new(Mutex::new(Inner::default())), started: Instant::now() }
    }

    /// Record a free-form event.
    pub fn note(&self, what: impl AsRef<str>) {
        self.push(format!("{:>9.3}  {}", self.started.elapsed().as_secs_f64(), what.as_ref()));
    }

    /// Record a control transfer and what came of it.
    ///
    /// `got` is the reply length for an IN request and `None` for an OUT one.
    /// It is a separate column rather than part of the outcome text because
    /// "asked for 32, got 8" is the shape of every framing surprise, and it
    /// must be greppable.
    // Eight columns, and every one of them is a field of the USB setup packet
    // or its outcome. A struct here would be a struct with eight fields.
    #[allow(clippy::too_many_arguments)]
    pub fn ctrl(
        &self,
        request: u8,
        name: &str,
        value: u16,
        index: u16,
        want: usize,
        got: Option<usize>,
        outcome: &str,
    ) {
        let len = match got {
            Some(n) if n == want => format!("len {n}"),
            Some(n) => format!("len {n}/{want} SHORT"),
            None => format!("len {want}"),
        };
        self.note(format!(
            "req {request:>2} {name:<26} val 0x{value:04x} idx 0x{index:04x} {len:<16} {outcome}"
        ));
        tracing::debug!(target: "hydrasdr::ctrl", request, name, value, index, want, ?got, outcome);
    }

    /// The whole sample-rate decision, in one place.
    ///
    /// The doubling is invisible in any single control transfer: what goes on
    /// the wire is a plausible number either way. Writing the arithmetic out
    /// means a report can be read against it — if the operator says the span is
    /// half what it should be, this line says whether the driver or the
    /// receiver is responsible — and it names the alternate configurations,
    /// which no enumeration will ever mention.
    pub fn rate_plan(&self, complex_hz: f64, programmed_hz: f64, encoded: &str, offered: &[f64]) {
        let list: Vec<String> = offered.iter().map(|r| format!("{:.3}M", r / 1e6)).collect();
        self.note(format!(
            "rate: {:.3} Msps complex -> {:.3} Msps programmed (x2, real ADC), sent as {encoded}; \
             receiver lists [{}] programmed",
            complex_hz / 1e6,
            programmed_hz / 1e6,
            list.join(", ")
        ));
    }

    /// Store the one-off identity summary. Kept out of the ring so a long
    /// session cannot age it out — it is the first thing anybody reading a
    /// report needs, and on the legacy USB id it is the only thing that says
    /// which radio this even is.
    pub fn set_identity(&self, summary: impl Into<String>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).identity = Some(summary.into());
    }

    /// Record raw bytes as hex.
    pub fn wire_head(&self, what: &str, bytes: &[u8], n: usize) {
        self.note(format!("▷ {what}: {}", hex_head(bytes, n)));
    }

    /// The head of the first completion: raw 12-bit values and the complex
    /// samples they became.
    ///
    /// Both halves are needed. The raw values settle whether the 12-bit
    /// unpacking is right — they should sit near mid-scale (2048) and vary
    /// smoothly, not jump between extremes. The decoded pairs settle which way
    /// the spectrum runs.
    pub fn first_samples(&self, raw: &[u16], decoded: &[Complex32]) {
        let vals: Vec<String> = raw.iter().take(16).map(|v| format!("{v}")).collect();
        self.note(format!("▷ first raw 12-bit: {}", vals.join(" ")));
        let pairs: Vec<String> =
            decoded.iter().take(8).map(|s| format!("({:+.4},{:+.4})", s.re, s.im)).collect();
        self.note(format!("▷ decoded as (re,im): {}", pairs.join(" ")));
    }

    /// Render the whole trace as one text block, ready to paste into an issue.
    pub fn dump(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::with_capacity(64 * 1024);
        out.push_str("=== sdroxide HydraSDR RFOne session trace ===\n");
        out.push_str(&format!("elapsed: {:.1} s\n", self.started.elapsed().as_secs_f64()));
        if g.dropped > 0 {
            out.push_str(&format!(
                "note: {} earlier lines were discarded (ring holds {})\n",
                g.dropped,
                Trace::CAPACITY
            ));
        }
        out.push_str("\n--- identity ---\n");
        match &g.identity {
            Some(c) => {
                out.push_str(c);
                out.push('\n');
            }
            None => out.push_str("(the receiver was never identified)\n"),
        }
        out.push_str("\n--- session ---\n");
        for line in &g.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    fn push(&self, line: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.lines.len() >= Trace::CAPACITY {
            g.lines.pop_front();
            g.dropped += 1;
        }
        g.lines.push_back(line);
    }
}

/// Format the first `n` bytes as spaced uppercase hex.
pub fn hex_head(bytes: &[u8], n: usize) -> String {
    bytes.iter().take(n).map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

/// Where the always-on traces of the most recent sessions live, so the settings
/// UI can offer them as copyable text after the receiver has already failed and
/// the handle has been dropped.
///
/// Open sessions and probes get separate slots. With one slot, a Rescan or a
/// `--probe` after a streaming fault would replace the streaming trace — the
/// entire point of the report — with an enumeration.
static LAST_OPEN: Mutex<Option<Trace>> = Mutex::new(None);
static LAST_PROBE: Mutex<Option<Trace>> = Mutex::new(None);

/// Remember an open (streaming) session's trace. The stored clone shares the
/// live buffer, so the slot keeps filling for as long as the session runs.
pub fn remember(trace: &Trace) {
    *LAST_OPEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(trace.clone());
}

/// Remember a probe's trace, in its own slot so it cannot displace the evidence
/// of an open session.
pub fn remember_probe(trace: &Trace) {
    *LAST_PROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some(trace.clone());
}

/// The most recent session traces, for a bug report: the open session first —
/// it is the one with streaming evidence — then the most recent probe. `None`
/// before the first attempt of either kind.
pub fn diagnostics() -> Option<String> {
    let open = LAST_OPEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let probe = LAST_PROBE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if open.is_none() && probe.is_none() {
        return None;
    }
    let mut out = String::new();
    if let Some(t) = open {
        out.push_str("### radio session (open / stream)\n");
        out.push_str(&t.dump());
    }
    if let Some(t) = probe {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("### probe\n");
        out.push_str(&t.dump());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_reports_requests_and_the_identity() {
        let t = Trace::new();
        t.set_identity("HydraSDR RFOne, firmware HydraSDR RFOne v1.1.0");
        t.ctrl(25, "GetSamplerates", 0, 0, 12, Some(12), "10.000M 5.000M 2.500M");
        let d = t.dump();
        assert!(d.contains("req 25 GetSamplerates"), "{d}");
        assert!(d.contains("v1.1.0"), "{d}");
    }

    /// The doubling is invisible in any single transfer, so the trace writes
    /// the whole decision out. A span that is half what it should be looks like
    /// a working receiver.
    #[test]
    fn the_rate_plan_shows_both_rates_and_the_encoding() {
        let t = Trace::new();
        // An alternate configuration: 6 Msps complex is not on the receiver's
        // own list and can only go out by value.
        t.rate_plan(6.0e6, 12.0e6, "Khz(12000)", &[20.0e6, 10.0e6, 5.0e6]);
        let d = t.dump();
        assert!(d.contains("6.000 Msps complex"), "{d}");
        assert!(d.contains("12.000 Msps programmed"), "{d}");
        assert!(d.contains("Khz(12000)"), "{d}");
        assert!(d.contains("20.000M, 10.000M, 5.000M"), "{d}");
    }

    /// A short reply is worth flagging where it is a surprise. Here that is
    /// every fixed-length read; the version string is the one that is expected
    /// to be short, and it says so beside itself.
    #[test]
    fn a_short_reply_is_called_out_in_the_length_column() {
        let t = Trace::new();
        t.ctrl(11, "BoardPartidSerialnoRead", 0, 0, 24, Some(8), "truncated");
        assert!(t.dump().contains("len 8/24 SHORT"), "{}", t.dump());
        let t2 = Trace::new();
        t2.ctrl(11, "BoardPartidSerialnoRead", 0, 0, 24, Some(24), "ok");
        assert!(!t2.dump().contains("SHORT"));
    }

    /// The identity must survive a session long enough to overflow the ring —
    /// it is the first thing a reader needs, and on the legacy USB id it is the
    /// only thing that says which radio answered.
    #[test]
    fn the_identity_outlives_the_line_ring() {
        let t = Trace::new();
        t.set_identity("HydraSDR RFOne");
        for i in 0..(Trace::CAPACITY + 50) {
            t.note(format!("line {i}"));
        }
        let d = t.dump();
        assert!(d.contains("50 earlier lines were discarded"));
        assert!(d.contains("HydraSDR RFOne"));
        assert!(d.contains(&format!("line {}", Trace::CAPACITY + 49)));
    }

    /// Raw values settle the 12-bit unpacking; the decoded pairs settle which
    /// way the spectrum runs. Neither answers the other's question.
    #[test]
    fn the_first_samples_are_shown_raw_and_decoded() {
        let t = Trace::new();
        t.first_samples(&[2048, 2100, 1990, 2048], &[Complex32::new(0.5, -0.25)]);
        let d = t.dump();
        assert!(d.contains("2048 2100 1990"), "{d}");
        assert!(d.contains("(+0.5000,-0.2500)"), "{d}");
    }

    #[test]
    fn an_empty_trace_still_dumps() {
        assert!(Trace::new().dump().contains("never identified"));
    }

    /// The mistake this guards against: an operator whose stream is misbehaving
    /// presses Rescan to check the receiver is still there, and that click
    /// replaces the streaming trace — the entire point of the report — with an
    /// enumeration.
    #[test]
    fn a_probe_does_not_displace_an_open_sessions_trace() {
        let open = Trace::new();
        open.note("streaming evidence");
        remember(&open);
        let probe = Trace::new();
        probe.note("just an enumeration");
        remember_probe(&probe);
        let d = diagnostics().expect("both slots filled");
        let open_at = d.find("radio session").expect("open section present");
        let probe_at = d.find("### probe").expect("probe section present");
        assert!(open_at < probe_at, "the open session must come first:\n{d}");
        assert!(d.contains("streaming evidence"), "{d}");
    }
}
