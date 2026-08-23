//! The LimeSDR family through LimeSuite, and the LimeRFE in front of it.
//!
//! # Why a vendor library rather than a driver
//!
//! Every other USB backend in this workspace speaks its radio's wire protocol
//! directly. This one does not, and the reason is the LMS7002M: driving it
//! means its register map, its CGEN/SXR/SXT synthesisers, its transceiver
//! signal-processing chain and — the part that cannot be desk-checked — its
//! receive and transmit DC-offset and IQ-imbalance calibration. That is roughly
//! ten thousand lines whose correctness only a bench can settle, and LimeSuite
//! is Apache-2.0 and already has it.
//!
//! So LimeSuite is loaded with **dlopen at runtime**, the arrangement
//! `sdroxide-sdrplay` uses for the closed `sdrplay_api`. Nothing is linked at
//! build time: this crate compiles everywhere, ships in every build variant,
//! and on a machine without the library enumerates nothing and explains what to
//! install.
//!
//! # What this adds over SoapySDR
//!
//! A LimeSDR has always been reachable here through `Backend::Soapy`, and
//! SoapyLMS7 is itself a thin wrapper over this same library — so the I/Q path
//! is not the point. The **LimeRFE** is: SoapySDR exposes none of it, so the
//! band filters, the LNA, the power amplifier and the transmit/receive relay
//! are unreachable from that side. So is the board's **second receive chain**,
//! opened here as a coherent second stream: a second aerial for diversity and
//! QRM suppression, or a transmit coupler for PureSignal predistortion. Driving the library directly also means the
//! settings panel can offer what the hardware actually has rather than what
//! SoapySDR's vocabulary can express.
//!
//! # Shape
//!
//! [`ffi`] holds the bindings and the measured layout pins. [`api`] owns the
//! one process-global library handle. [`device::DevCtl`] is the only place a
//! device pointer is dereferenced. [`handle::LimeHandle`] is an open radio.
//! [`rfe`] implements `sdroxide-limerfe`'s transport over the board's GPIO,
//! which is the one LimeRFE path that needs LimeSuite at all. `auxrx` is the
//! board's *second* receive chain and the timestamp arithmetic that pairs its
//! samples with the first's, which is what a second aerial needs to be worth
//! anything (issue #98). The DSP those streams feed is `sdroxide-dsp`'s
//! `Diversity` and `PureSignal`; this crate only gets the samples out.
//!
//! There is no stream thread and no ring buffer: LimeSuite has both already,
//! and `LMS_RecvStream` is a bounded blocking read out of its FIFO. See
//! [`handle`].
//!
//! Must never be a dependency of any wasm-targeted crate.

pub mod api;
// `auxrx`, not `aux`: AUX is a reserved DOS device name, and a file called
// `aux.rs` cannot be checked out on Windows at all — git refuses the path and
// the whole clone fails, long before anything is compiled.
pub(crate) mod auxrx;
pub mod device;
pub mod error;
pub mod ffi;
pub mod handle;
pub mod rfe;
pub mod trace;

pub use api::{Enumeration, list, try_list};
pub use device::DevInfo;
pub use error::{Error, Result};
pub use handle::LimeHandle;
pub use rfe::BoardTransport;
pub use trace::diagnostics;

use sdroxide_limerfe::LimeRfeHandle;
use sdroxide_types::{LimeRfeConfig, RfeLink};

/// Open the LimeRFE this configuration describes, whichever way it is attached.
///
/// The serial link is `sdroxide-limerfe`'s own and needs nothing from here; the
/// board link bit-bangs I²C on `radio`'s GPIO pins and so needs the open
/// device. Returns `Ok(None)` when no board is configured, which is the
/// ordinary case and not a failure.
pub fn open_rfe(
    cfg: &LimeRfeConfig,
    radio: &LimeHandle,
) -> sdroxide_limerfe::Result<Option<LimeRfeHandle>> {
    match cfg.link {
        RfeLink::Off => Ok(None),
        RfeLink::Serial => sdroxide_limerfe::open_serial(cfg),
        RfeLink::Board => {
            let transport = BoardTransport::open(radio.shared_device(), radio.label())?;
            Ok(Some(sdroxide_limerfe::spawn(Box::new(transport), cfg.clone())))
        }
    }
}
