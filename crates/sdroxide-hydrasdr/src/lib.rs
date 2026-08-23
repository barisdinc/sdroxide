//! Native HydraSDR RFOne driver.
//!
//! Pure Rust over `nusb`: no libhydrasdr, no libusb, no SoapySDR module, so
//! this backend is in every build variant on every platform.
//!
//! # A fork, not a relative
//!
//! The RFOne descends from the Airspy R2 by *source*: `hydrasdr-host`'s
//! `hydrasdr.c` still carries Youssef Touil's 2014 libairspy copyright header,
//! vendor requests 0–26 line up number for number and meaning for meaning, and
//! the two curated gain curves are byte-for-byte identical — the tuner moved
//! from an R820T2 to an R828D and the stage ranges did not.
//!
//! So this crate is deliberately shaped like `sdroxide-airspy` next door, and
//! it shares that crate's [`convert`](sdroxide_airspy::convert)`::HostDsp` and
//! 12-bit unpacking outright: both are fixed by hardware the two receivers have
//! in common — a real ADC whose wanted signal sits at fs/4, and a firmware
//! packing format inherited unchanged — so they cannot diverge, and a second
//! copy could only ever drift from the one with a measured half-band behind it.
//!
//! Everything else is this radio's own, because **the two drivers cannot drive
//! each other's hardware**:
//!
//! * `SET_FREQ` carries a `uint64_t` here and a `uint32_t` on an Airspy. The
//!   firmware schedules a receive of eight bytes; four would land in the low
//!   half of a static whose high half is *usually* already zero, which is the
//!   worst kind of wrong.
//! * Two USB ids, one of them Airspy's own — see below.
//! * Three RF input sockets, only one of which carries the bias tee.
//! * Seven sample rates against the R2's two, and only three of the seven are
//!   ones the receiver will admit to having.
//!
//! # The USB id an RFOne shares with an Airspy R2
//!
//! Production boards enumerate as `38af:0001`. Prototypes — flashed before the
//! vendor id existed — come up on `1d50:60a1`, which is the Airspy R2 and
//! Mini's pair. `hydrasdr-host`'s device registry carries both.
//!
//! This driver therefore separates them twice. [`list`] claims the legacy pair
//! only when the USB descriptors say HydraSDR, which costs nothing and opens
//! nothing; and the open sequence reads the firmware version string and refuses
//! anything not beginning `HydraSDR RF`, which is the check libhydrasdr itself
//! makes and the only fully dependable one. An operator who picks this
//! interface for an Airspy R2 is told which interface to pick instead, rather
//! than being left with a receiver that tunes four bytes' worth of somewhere
//! else.
//!
//! # Rates the receiver will not admit to
//!
//! `GET_SAMPLERATES` reports the firmware's *primary* configurations: 10, 5 and
//! 2.5 Msps complex. The RFOne firmware also carries an alternate table — 12,
//! 8, 6 and 4.096 Msps — which no enumeration mentions and which can only be
//! reached by sending the ADC rate in kilohertz. They are real, they are in
//! `hydrasdr_rfone_conf.c`, and a driver that only offered what the receiver
//! listed would leave the top of the radio's range unreachable. See
//! [`protocol::ALT_RATES`], and note that an alternate the firmware refuses
//! falls back to a listed rate rather than leaving the span silently wrong.
//!
//! # What the host has to do
//!
//! This receiver's ADC is **real**, and the wanted signal sits at a quarter of
//! the sample rate. So the host does the downconversion: DC removal, a
//! multiply-free fs/4 rotation, and a half-band decimator. That is also why the
//! rate programmed into the receiver is twice the complex rate the operator
//! picks; see [`protocol::program_rate_hz`].
//!
//! # Provenance
//!
//! The protocol, the rate arithmetic, the gain curves and the capability
//! fallback are transcribed from HydraSDR's
//! [hydrasdr-host](https://github.com/hydrasdr/hydrasdr-host) —
//! `libhydrasdr/src/hydrasdr.c`, `hydrasdr_shared.c`, `hydrasdr_rfone.c` and
//! `hydrasdr_commands.h` (MIT / BSD-3-Clause, compatible with this workspace's
//! GPL-3.0-or-later). Where the host library is not the authority — the
//! `SET_FREQ` width, the alternate rate table, the string descriptors — the
//! figures come from the RFOne firmware itself
//! ([rfone_fw](https://github.com/hydrasdr/rfone_fw), `m0/usb_req.c`,
//! `m0/usb_descriptor.c` and `common/hydrasdr_rfone_conf.c`).
//!
//! # Not verified against hardware
//!
//! This driver was written from the reference implementation and the firmware
//! source, not on a bench. Two things a receiver has to settle:
//!
//! * **Which way the spectrum runs.** The fs/4 rotation matches libairspy's
//!   sign, which is the convention every other program's picture of this
//!   hardware is drawn in, but nothing here has watched a known carrier land on
//!   the correct side of the dial.
//! * **Whether the alternate rates work as transcribed.** The kilohertz figure
//!   is of the *ADC* rate, which the firmware matches against
//!   `r82x_if_freq * 4`; a build that disagrees answers with a stall, which
//!   this driver handles — but "handled" and "correct" are different claims.
//!
//! The `probe` example prints the lines that answer both.

pub mod device;
pub mod error;
pub mod handle;
pub mod protocol;
pub mod stream;
pub mod trace;
pub mod usb;

pub use device::Device;
pub use error::{Error, Result};
pub use handle::HydraSdrHandle;
pub use trace::{FIELD_REPORT_HINT, diagnostics};
pub use usb::list;
