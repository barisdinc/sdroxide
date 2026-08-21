//! The ISM burst decoder: reads the unattended digital traffic that fills the
//! licence-free bands — weather and soil sensors, utility meters, home
//! automation, remotes and tyre-pressure sensors — and reports each device it
//! hears with its readings in real units.
//!
//! Native-only, like the skimmers: it runs in the engine and reaches the UI as
//! [`sdroxide_types::IsmReport`]s over the ordinary event path.
//!
//! # Shape of the thing
//!
//! ```text
//! wide IQ window  ──►  one Ddc per channel in the plan  ──►  Gate
//!                                                              │ burst
//!                            Frame ◄── slice ◄── discriminate ◄─┘
//!                              │
//!                              └──► protocol registry ──► CRC ──► IsmReport
//! ```
//!
//! [`plan`] says which channels exist and which of them the receiver can reach;
//! [`gate`] cuts the stream into transmissions; [`demod`] and [`slice`] recover
//! bits; [`proto`] turns validated frames into readings. [`controller`] wraps the
//! lot in a worker thread.
//!
//! # The second lane
//!
//! [`rtl433`] is the embedded `rtl_433`, which brings several hundred more
//! device protocols and — unlike the pipeline above, whose slicer is FSK only —
//! reads OOK as well. That matters most at 433.92 MHz, where nearly everything
//! is OOK and this crate's own decoders hear nothing at all.
//!
//! The two lanes share one window, one device table and one panel. Where both
//! can read a device, rtl_433 wins and the native decoder stands down: it knows
//! fifteen LaCrosse variants to this crate's one, and nineteen Fine Offset to
//! seven. Z-Wave and Homematic are the other way round — rtl_433 has no decoder
//! for either — so those two are never handed over. See
//! [`rtl433::COVERED_NATIVES`].
//!
//! # Provenance
//!
//! Every protocol module under `proto` is written from that protocol's
//! published specification or public reverse-engineering write-up, cited in its
//! own header, and not ported from an existing decoder. That was true before
//! `rtl_433` was vendored and remains true: field layouts are facts about the
//! devices, and cross-checking against prior art is fair, but no code came from
//! it.
//!
//! What has changed is the crate's dependencies. Under the `rtl433` feature —
//! on by default — this crate links the vendored `rtl_433` (GPL-2.0-or-later,
//! used unmodified), so it is no longer free of GPL dependencies and the binary
//! carries those obligations. Built without that feature it is pure Rust with no
//! C at all. See `PROVENANCE.md` beside this crate for the details and for what
//! to re-check when the submodule moves.

pub mod class;
mod controller;
mod crc;
mod demod;
mod gate;
mod plan;
pub mod probe;
mod proto;
#[cfg(feature = "rtl433")]
pub mod rtl433;
mod slice;

pub use class::classify;
pub use controller::{IsmAction, IsmController};
pub use gate::Burst;
pub use plan::{
    CHANNELS, Channel, USABLE_FRACTION, WindowPlan, ideal_center_hz, span_hz, window_center_for,
    window_center_hz, window_plan,
};
pub use probe::{BurstReport, Probe, Survey};
