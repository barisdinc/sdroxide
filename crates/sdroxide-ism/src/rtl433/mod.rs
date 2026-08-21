//! The embedded rtl_433 lane.
//!
//! rtl_433 covers several hundred ISM device protocols, across OOK as well as
//! FSK, which is most of what actually transmits on these bands. The native
//! decoders beside this module stay for what rtl_433 does not reach — Z-Wave and
//! Homematic have no equivalent there — but where the two overlap, rtl_433 knows
//! far more variants and wins.
//!
//! # Shape
//!
//! ```text
//! window IQ ──► Ddc to the band rate ──► cs16 ──► rtl_433 ──► key/value event
//!                                                                  │
//!                                            IsmReport ◄── map ◄────┘
//! ```
//!
//! [`sys`] owns the FFI, [`flex`] parses and vets user decoder specs, [`bands`]
//! says where the lane can listen, and [`map`] turns an event into the same
//! [`crate::proto::Decoded`] the native decoders produce, so both lanes share
//! one device table.

pub mod bands;
pub mod flex;
pub mod map;
pub mod sys;

use sdroxide_types::IsmProtocol;

/// Native protocols the embedded rtl_433 covers better than this crate does.
///
/// Not a guess: checked against upstream's decoder list. rtl_433 ships fifteen
/// LaCrosse decoders to this crate's one, nineteen Fine Offset to seven, and
/// eight Bresser to four — so where both lanes can hear a device, rtl_433 is the
/// one more likely to name it correctly.
///
/// Z-Wave and Homematic are deliberately absent: rtl_433 has no decoder for
/// either, so the native modules are the only thing that reads them and must
/// never be gated off.
///
/// Re-check this list whenever the submodule moves.
pub const COVERED_NATIVES: &[IsmProtocol] =
    &[IsmProtocol::Bresser, IsmProtocol::FineOffset, IsmProtocol::LaCrosseIt];

/// Whether the native decoder for `p` should stand down.
///
/// Only while the rtl_433 lane is actually listening on the frequency in
/// question: switching the lane off, or tuning to a band it is not watching,
/// has to give the native decoders back rather than silently leaving a gap.
pub fn suppresses(p: IsmProtocol, rtl433_live_here: bool) -> bool {
    rtl433_live_here && COVERED_NATIVES.contains(&p)
}
