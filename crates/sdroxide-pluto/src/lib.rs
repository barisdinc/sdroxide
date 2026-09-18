//! Native ADALM-Pluto (PlutoSDR) support — pure Rust, no libiio and no libusb.
//!
//! NATIVE ONLY. This crate must never be a dependency of any wasm-targeted
//! crate (the same invariant `sdroxide-cat`, `sdroxide-rtlsdr` and
//! `sdroxide-hpsdr` carry). It is reached only from the root binary and
//! `local_controller.rs`; the settings UI talks to it exclusively through the
//! `RadioController` trait.
//!
//! # How a Pluto is reached
//!
//! The device runs Linux with **`iiod`**, the IIO daemon, listening on TCP
//! 30431, and everything the radio can do is an IIO attribute or an IIO buffer
//! on the other end of that socket. The USB cable is a *network* interface (an
//! Ethernet gadget), not a serial port, so the same connection serves a Pluto on
//! the desk at `192.168.2.1` and one across the LAN. That is why this backend
//! needs no C library at all: [`iiod`] speaks the protocol directly.
//!
//! Three connections are opened, not one — control, RX and TX. IIOD is strictly
//! request/response per connection, so a `READBUF` blocking until the device has
//! filled a buffer would block a retune queued behind it.
//!
//! # The hardware behind the attributes
//!
//! `ad9361-phy` is the AD9361/AD9363 transceiver: local oscillators, sample
//! rate, analog filter bandwidth, gains, AGC mode and port selection.
//! `cf-ad9361-lpc` is the receive DMA buffer and `cf-ad9361-dds-core-lpc` the
//! transmit one. sdroxide reads the limits off the device rather than hardcoding
//! them, which is the only way to serve both a stock AD9363 (325 MHz–3.8 GHz)
//! and one unlocked to AD9364 (70 MHz–6 GHz) from the same code.
//!
//! The part is **zero-IF**, so its LO leakage and flicker noise sit exactly
//! where the operator's VFO would be. The engine parks the LO a quarter-span
//! away (`IqSource::lo_offset_hz`) and the source adapter DC-blocks the stream.
//!
//! # Status
//!
//! **Not verified against hardware.** The protocol is transcribed from libiio's
//! `iiod-client.c` and `iiod/ops.c`; the automated coverage is an in-process
//! fake `iiod` (`tests/iiod_loopback.rs`). Every session keeps a [`Trace`] that
//! the settings UI offers as copyable text, and the head of the first buffer
//! response is recorded in it verbatim — that is the line that settles whether
//! the chunk framing here matches a real device.

mod context;
mod discovery;
mod error;
mod iiod;
mod mdns;
mod net;
mod phy;
mod stream;
mod trace;

use std::time::Duration;

pub use context::{Context, ScanFormat};
pub use discovery::{discover, probe, test_connection};
pub use error::{Error, Result};
pub use iiod::DEFAULT_PORT;
pub use net::{PlutoHandle, PlutoLimits, PlutoRig, PlutoRx, emergency_mute_all};
pub use phy::{PHY, RX_BUFFER, TX_BUFFER};
pub use sdroxide_types::PlutoDevice;
pub use trace::{Trace, diagnostics};

/// Where an out-of-the-box Pluto lives: the device end of the USB Ethernet
/// gadget. The host takes 192.168.2.10 on the same link.
pub const DEFAULT_ADDRESS: &str = "192.168.2.1";

/// Convenience: scan for Plutos with a default 1.5 s timeout, matching the
/// HPSDR and SmartSDR discovery buttons.
pub fn discover_default() -> Vec<PlutoDevice> {
    discover(Duration::from_millis(1500))
}

/// Split `host[:port]` into its parts, defaulting to [`DEFAULT_PORT`].
///
/// Accepts what an operator will actually paste: a bare address, a
/// `host:port`, one of libiio's own `ip:192.168.2.1` URIs, or a
/// `[v6]:port`. A libiio `usb:` or `serial:` URI is rejected by name rather
/// than by a parse failure, because those are real things somebody will try.
pub fn split_addr(address: &str) -> std::result::Result<(String, u16), String> {
    let s = address.trim();
    if s.is_empty() {
        return Err("no address — enter the Pluto's address, e.g. 192.168.2.1".into());
    }
    for prefix in ["usb:", "serial:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Err(format!(
                "{s:?} is a libiio {prefix}… URI. This backend reaches a Pluto over the \
                 network — which the USB cable already provides, as an Ethernet gadget. \
                 Use the device's IP address ({DEFAULT_ADDRESS} by default) instead of \
                 {rest:?}"
            ));
        }
    }
    // libiio spells a network context `ip:host`; accept it and move on.
    let s = s.strip_prefix("ip:").unwrap_or(s);
    // A bracketed IPv6 literal, with or without a port.
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            format!("{address:?} opens a bracketed IPv6 address but never closes it")
        })?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("{p:?} is not a port number"))?,
            None => DEFAULT_PORT,
        };
        return Ok((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        // A bare IPv6 literal has several colons and no port.
        Some(_) if s.matches(':').count() > 1 => Ok((s.to_string(), DEFAULT_PORT)),
        Some((host, port)) => {
            let port = port.parse().map_err(|_| format!("{port:?} is not a port number"))?;
            if host.is_empty() {
                return Err(format!("{address:?} has a port but no host"));
            }
            Ok((host.to_string(), port))
        }
        None => Ok((s.to_string(), DEFAULT_PORT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_the_way_people_write_them() {
        assert_eq!(split_addr("192.168.2.1").unwrap(), ("192.168.2.1".into(), DEFAULT_PORT));
        assert_eq!(split_addr(" pluto.local ").unwrap(), ("pluto.local".into(), DEFAULT_PORT));
        assert_eq!(split_addr("192.168.2.1:30431").unwrap(), ("192.168.2.1".into(), 30431));
        assert_eq!(split_addr("10.0.0.5:31000").unwrap(), ("10.0.0.5".into(), 31000));
        // libiio's own network URI spelling.
        assert_eq!(split_addr("ip:192.168.2.1").unwrap(), ("192.168.2.1".into(), DEFAULT_PORT));
        // IPv6, bare and bracketed.
        assert_eq!(split_addr("fe80::1").unwrap(), ("fe80::1".into(), DEFAULT_PORT));
        assert_eq!(split_addr("[fe80::1]:30431").unwrap(), ("fe80::1".into(), 30431));
    }

    /// A `usb:` URI is what somebody who has used libiio will type first, and
    /// "invalid port" would send them looking in entirely the wrong place.
    #[test]
    fn a_libiio_usb_uri_is_refused_by_name() {
        let err = split_addr("usb:1.5.5").expect_err("usb URIs are not this backend");
        assert!(err.contains("Ethernet gadget"), "{err}");
        assert!(err.contains(DEFAULT_ADDRESS), "{err}");
    }

    #[test]
    fn nonsense_is_reported_rather_than_guessed_at() {
        assert!(split_addr("").is_err());
        assert!(split_addr("192.168.2.1:mumble").is_err());
        assert!(split_addr(":30431").is_err());
    }
}
