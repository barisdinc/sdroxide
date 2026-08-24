//! The one blocking thread that owns the receiver, and the bulk pump inside it.
//!
//! # The rate change is the interesting part
//!
//! Everything else here is the shape `sdroxide-airspy` and `sdroxide-rtlsdr`
//! already use. What differs is that a rate change on this receiver is not a
//! setting — it is a stop, a reprogram, and a restart, because the tuner's IF
//! and the clock dividers both move with it. So the endpoint is torn down and
//! rebuilt around it, the host DSP is reset (its delay line holds the old
//! band), and the ring is left to drain naturally rather than being cleared:
//! what is in it is real signal from a moment ago, at a rate the consumer was
//! told about.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use nusb::MaybeFuture;
use nusb::transfer::{Bulk, In, TransferError};
use rtrb::Producer;
use sdroxide_airspy::convert::HostDsp;
use sdroxide_airspy::protocol::{PACKED_GROUP_BYTES, unpack12_bytes};
use sdroxide_dsp::Complex32;
use sdroxide_types::HydraSdrConfig;

use crate::device::Device;
use crate::error::{Error, Result};
use crate::handle::{
    Ctrl, DeviceInfo, HydraSdrHandle, Pending, RxStats, Shared, push_iq, ring_for,
};
use crate::protocol::{ALL_RATES, BULK_EP};
use crate::trace::{self, Trace};
use crate::usb::UsbDev;

/// How long a first completion may block before the loop goes back to serve
/// control. Bounds how long a dial drag waits when the stream has gone quiet.
const COMPLETE_TIMEOUT: Duration = Duration::from_millis(20);

/// How long to wait for cancelled transfers to come back before dropping an
/// endpoint.
const CANCEL_TIMEOUT: Duration = Duration::from_millis(500);

/// Transfer geometry, bounded against a hand-edited config.
///
/// This is a USB 2.0 device carrying up to 36 MB/s, so the per-transfer
/// overhead matters more than the control latency — nothing here transmits, so
/// a retune landing mid-transfer costs a few milliseconds of stale spectrum and
/// nothing else.
const MIN_TRANSFERS: usize = 4;
const MAX_TRANSFERS: usize = 64;
const MIN_TRANSFER_BYTES: usize = 4 * 1024;
const MAX_TRANSFER_BYTES: usize = 512 * 1024;

/// Transfer lengths must be a whole number of `u32` words, because the packed
/// format is defined on words. Rounding to 4 KiB satisfies that and the USB
/// packet size together.
const GRANULE: usize = 4096;

fn geometry(cfg: &HydraSdrConfig) -> (usize, usize) {
    let n = (cfg.transfers as usize).clamp(MIN_TRANSFERS, MAX_TRANSFERS);
    let bytes = ((cfg.transfer_kib as usize) * 1024).clamp(MIN_TRANSFER_BYTES, MAX_TRANSFER_BYTES)
        / GRANULE
        * GRANULE;
    (n, bytes.max(GRANULE))
}

/// Arm the endpoint *after* the receiver is told to run, never before.
///
/// `set_receiver_mode(RECEIVER_MODE_RX)` re-initialises the bulk IN endpoint in
/// the firmware, which flushes it and resets its data toggle. Reads posted
/// before that are posted against an endpoint that is about to be pulled out
/// from under them: the receiver is stopped, so nothing answers the IN tokens,
/// and then the endpoint is re-initialised underneath the queue. On macOS the
/// first read comes back with a transport error and the host controller stops
/// servicing the pipe, so the stream never starts at all — a receiver that
/// configures perfectly and then sends nothing.
///
/// libhydrasdr's start sequence is explicit about the order: mode off, clear
/// the halt, mode RX, *then* submit. This is that order, and it is the reason
/// [`open_endpoint`] and [`prime`] are two functions rather than one.
fn prime(ep: &mut nusb::Endpoint<Bulk, In>, in_flight: usize, transfer_bytes: usize) {
    while ep.pending() < in_flight {
        ep.submit(ep.allocate(transfer_bytes));
    }
}

/// Open the receiver and start the stream thread.
///
/// The device is opened *on the thread* so that all control transfers happen on
/// one thread; this function blocks on a handshake until that has succeeded or
/// failed, so the caller still gets a synchronous `Result`.
pub(crate) fn spawn(cfg: &HydraSdrConfig, center_hz: f64) -> Result<HydraSdrHandle> {
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded::<Ctrl>();
    // Rendezvous for the open result, so the caller learns about a permission
    // problem, a missing receiver or the wrong radio as a normal error rather
    // than as a stream that silently never starts.
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<DeviceInfo>>(1);

    let shared = Arc::new(Shared::new());
    let t = Trace::new();
    trace::remember(&t);

    // Sized for the largest rate this receiver can be moved to at runtime, not
    // the one it opens on: a rate change does not reallocate the ring, and one
    // sized for 2.5 Msps would hold a fiftieth of a second at 12.
    let (rx_prod, rx_cons) = ring_for(ALL_RATES[0]);

    let cfg = cfg.clone();
    let thread_shared = Arc::clone(&shared);
    let thread_trace = t.clone();
    let join = std::thread::Builder::new()
        .name("sdroxide-hydrasdr".into())
        .spawn(move || {
            run(
                cfg,
                center_hz,
                ctrl_rx,
                rx_prod,
                Arc::clone(&thread_shared),
                ready_tx,
                thread_trace,
            );
            thread_shared.alive.store(false, Ordering::Relaxed);
        })
        .map_err(|e| Error::Access(format!("could not start the HydraSDR thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(HydraSdrHandle::from_parts(rx_cons, ctrl_tx, shared, join, t, info)),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        // The thread died before reporting; joining surfaces a panic message.
        Err(_) => {
            let _ = join.join();
            Err(Error::Access("the HydraSDR thread stopped before it opened the receiver".into()))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: HydraSdrConfig,
    center_hz: f64,
    ctrl: crossbeam_channel::Receiver<Ctrl>,
    mut rx: Producer<f32>,
    shared: Arc<Shared>,
    ready: crossbeam_channel::Sender<Result<DeviceInfo>>,
    trace: Trace,
) {
    let usb = match UsbDev::open(&cfg.serial, &trace) {
        Ok(d) => d,
        Err(e) => {
            trace.note(format!("open failed: {e}"));
            let _ = ready.send(Err(e));
            return;
        }
    };
    let serial = usb.serial().map(str::to_string);
    let legacy_usb_id = usb.is_legacy_usb_id();
    if !usb.is_high_speed_or_better() {
        trace.note(
            "this receiver is not on a high-speed link — no rate it offers will fit, \
             and the stream will drop almost everything",
        );
    }

    let mut dev = match Device::open(usb, &cfg, center_hz) {
        Ok(d) => d,
        Err(e) => {
            trace.note(format!("configure failed: {e}"));
            let _ = ready.send(Err(e));
            return;
        }
    };

    shared.rate_milli_hz.store((dev.sample_rate_hz() * 1000.0) as u64, Ordering::Relaxed);
    let _ = ready.send(Ok(DeviceInfo {
        label: dev.describe(),
        firmware: dev.firmware.clone(),
        board: dev.board_id.map(|b| b.name()).unwrap_or_else(|| "unread".to_string()),
        serial,
        legacy_usb_id,
        sample_rate_hz: dev.sample_rate_hz(),
        rates_hz: dev.available_rates(),
        snapped_from: dev.snapped_from,
        packing_active: dev.packing_active(),
        rf_port_active: dev.rf_port_active(),
    }));

    if let Err(e) = pump(&mut dev, &cfg, &ctrl, &mut rx, &shared, &trace) {
        tracing::warn!("HydraSDR stream stopped: {e}");
        trace.note(format!("stream stopped: {e}"));
    }
    dev.shutdown();
}

fn pump(
    dev: &mut Device,
    cfg: &HydraSdrConfig,
    ctrl: &crossbeam_channel::Receiver<Ctrl>,
    rx: &mut Producer<f32>,
    shared: &Arc<Shared>,
    trace: &Trace,
) -> Result<()> {
    let (in_flight, transfer_bytes) = geometry(cfg);
    tracing::info!(
        "HydraSDR streaming: {in_flight} transfers x {} KiB, {} at {:.3} Msps complex on {}",
        transfer_bytes / 1024,
        if dev.packing_active() { "12-bit packed" } else { "16-bit unpacked" },
        dev.sample_rate_hz() / 1e6,
        dev.rf_port().name(),
    );

    let mut dsp = HostDsp::new();
    dsp.set_dc_enabled(cfg.dc_block);
    let mut stats = RxStats::new(dev.sample_rate_hz());
    let started = Instant::now();
    let mut raw: Vec<u16> = Vec::with_capacity(transfer_bytes);
    let mut samples: Vec<Complex32> = Vec::with_capacity(transfer_bytes / 2);
    let mut interleaved: Vec<f32> = Vec::with_capacity(transfer_bytes);
    let mut logged_first = false;
    // Bytes of a sample group that a completion ended in the middle of. See
    // `decode_raw`.
    let mut carry: Vec<u8> = Vec::with_capacity(PACKED_GROUP_BYTES);

    let mut ep = open_endpoint(dev)?;
    dev.start()?;
    prime(&mut ep, in_flight, transfer_bytes);

    loop {
        // 1. Collapse the whole control channel, then apply each field once.
        let mut pending = Pending::default();
        while let Ok(c) = ctrl.try_recv() {
            pending.absorb(c);
        }
        if pending.shutdown {
            break;
        }
        if !pending.is_empty() {
            // A rate change is not a setting on this receiver — it stops the
            // sample flow, so the endpoint has to come down with it.
            if let Some(hz) = pending.rate {
                // Stop the receiver *before* the endpoint comes down, as
                // libhydrasdr's stop sequence does. Cancelling reads under a
                // running stream leaves the receiver pushing into a queue that
                // is going away, which is the same wedged-pipe hazard `prime`
                // exists to avoid at the other end.
                let _ = dev.stop();
                close(ep);
                let outcome = rebuild(dev, hz, &mut dsp, &mut stats, shared, trace);
                // Whatever was mid-group belonged to the old rate.
                carry.clear();
                ep = open_endpoint(dev)?;
                dev.start()?;
                prime(&mut ep, in_flight, transfer_bytes);
                if let Err(e) = outcome {
                    tracing::warn!("HydraSDR: rate change failed: {e}");
                    trace.note(format!("rate change failed: {e}"));
                }
            } else {
                // Keep the hardware fed while we talk on endpoint 0.
                prime(&mut ep, in_flight, transfer_bytes);
            }
            if let Err(e) = apply(dev, &pending, &mut dsp) {
                // A setting that cannot be applied is worth saying out loud,
                // but it is not a reason to tear the stream down — the operator
                // can pick another value.
                tracing::warn!("HydraSDR: {e}");
                trace.note(format!("control change failed: {e}"));
            }
        }

        // 2. Refill before draining, so the queue is never empty while the
        //    receiver is producing.
        prime(&mut ep, in_flight, transfer_bytes);

        // 3. Drain every completion that is ready, not just one. Blocking only
        //    on the first; the rest are polled. Taking one per iteration
        //    starves the stream under a sustained dial drag.
        let mut drained = 0usize;
        loop {
            let timeout = if drained == 0 { COMPLETE_TIMEOUT } else { Duration::ZERO };
            let Some(completion) = ep.wait_next_complete(timeout) else {
                break;
            };
            match completion.status {
                Ok(()) => {
                    let bytes = &completion.buffer[..];
                    raw.clear();
                    decode_raw(bytes, dev.packing_active(), &mut carry, &mut raw);

                    samples.clear();
                    dsp.process(&raw, &mut samples);

                    if !logged_first && !samples.is_empty() {
                        logged_first = true;
                        // The two lines a developer without this hardware
                        // cannot produce: whether the 12-bit unpacking is right,
                        // and which way the spectrum runs.
                        trace.first_samples(&raw, &samples);
                    }

                    if !samples.is_empty() {
                        interleaved.clear();
                        interleaved.reserve(samples.len() * 2);
                        for s in &samples {
                            interleaved.push(s.re);
                            interleaved.push(s.im);
                        }
                        stats.on_iq(samples.len());
                        push_iq(
                            rx,
                            &interleaved,
                            &mut stats,
                            shared.rx_paused.load(Ordering::Relaxed),
                        );
                        shared
                            .last_rx_ms
                            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    }
                }
                // A cancellation is this driver's own doing — a teardown or a
                // rate change — and the buffer simply comes back unused.
                Err(TransferError::Cancelled) => {}
                Err(TransferError::Disconnected) => {
                    return Err(Error::NotFound("the HydraSDR was unplugged".into()));
                }
                // Everything else is treated as a halted pipe, not only the
                // error that says so.
                //
                // A stall is the *named* case, but it is not the only one that
                // wedges an endpoint: macOS stops servicing a pipe after a
                // transport error of any kind and will not resume until the
                // halt is cleared, and which `TransferError` a given failure
                // arrives as is the OS backend's choice, not the receiver's.
                // Clearing after an error that did not need it costs one
                // control transfer; *not* clearing after one that did costs the
                // whole stream, silently, until the program is restarted.
                Err(e) => {
                    let stalled = matches!(e, TransferError::Stall);
                    tracing::warn!("HydraSDR transfer error ({e}); clearing the endpoint");
                    let _ = ep.clear_halt().wait();
                    // The stream restarts from a packet boundary, so the filter
                    // history and the packing phase both belong to nothing.
                    dsp.reset();
                    carry.clear();
                    if stalled {
                        stats.on_stall();
                    }
                    // Named once, then counted: at sixty transfers a second an
                    // error per line would push the whole opening sequence out
                    // of a bounded trace.
                    stats.on_error(&format!("{e}"), trace);
                }
            }

            // Reuse the buffer rather than allocating a new one every few
            // milliseconds.
            let mut buf = completion.buffer;
            buf.clear();
            ep.submit(buf);

            drained += 1;
            // Go back and serve control rather than spinning here forever if
            // the receiver is outrunning us.
            if drained >= in_flight {
                break;
            }
        }

        stats.tick(trace);
    }

    trace.note(format!("stream ended: {}", stats.summary()));
    // Stopped first, endpoint down second — the same order as the rate change,
    // and for the same reason.
    let _ = dev.stop();
    close(ep);
    Ok(())
}

/// Turn a completion into raw 12-bit sample values.
///
/// Unpacked, the receiver sends little-endian `u16` per sample with the top
/// four bits clear. Packed, it sends three `u32` words per eight samples — a
/// third less USB bandwidth, and on a USB 2.0 device carrying 36 MB/s that is
/// the difference between keeping up and not.
///
/// # Why the carry is not optional
///
/// The receiver's stream is **continuous across transfers**. A completion ends
/// wherever the host's buffer filled — a boundary the receiver knows nothing
/// about — and the packed format's unit is twelve bytes, which 4 KiB is not a
/// multiple of. So a completion routinely ends part-way through a group, and
/// the bytes that finish it arrive at the head of the next one.
///
/// Dropping that remainder would not cost eight samples; it would cost every
/// sample afterwards. The next completion would be decoded from an offset the
/// format does not start on, and each group would be read across two real ones
/// — full-scale nonsense, indistinguishable from a receiver that is simply
/// producing noise. Hence `carry`, which the caller keeps for the life of the
/// stream and clears wherever sync is lost.
fn decode_raw(bytes: &[u8], packed: bool, carry: &mut Vec<u8>, out: &mut Vec<u16>) {
    // The smallest run of bytes that decodes on its own: one packed group, or
    // one little-endian sample.
    let unit = if packed { PACKED_GROUP_BYTES } else { 2 };
    let mut src = bytes;

    if !carry.is_empty() {
        let take = (unit - carry.len()).min(src.len());
        carry.extend_from_slice(&src[..take]);
        src = &src[take..];
        if carry.len() < unit {
            // A completion shorter than the rest of one group. Vanishingly
            // rare, but "vanishingly rare" and "impossible" are different
            // states to leave a decoder in.
            return;
        }
        decode_whole(carry, packed, out);
        carry.clear();
    }

    let whole = src.len() / unit * unit;
    decode_whole(&src[..whole], packed, out);
    carry.extend_from_slice(&src[whole..]);
}

/// The decode proper, over a length that is a whole number of units.
fn decode_whole(bytes: &[u8], packed: bool, out: &mut Vec<u16>) {
    if packed {
        unpack12_bytes(bytes, out);
    } else {
        out.extend(bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) & 0x0fff));
    }
}

/// Apply the collapsible settings, in dependency order.
///
/// The rate is deliberately absent: it is handled by the caller, which has to
/// take the endpoint down around it. The port comes before the bias tee for a
/// reason — only the antenna socket has one, and `Device::set_rf_port` restates
/// it, so a change that moves both has to move the port first.
fn apply(dev: &mut Device, p: &Pending, dsp: &mut HostDsp) -> Result<()> {
    if let Some(v) = p.center {
        dev.set_center_hz(v)?;
    }
    if let Some(v) = p.curve {
        dev.set_curve(v)?;
    }
    if let Some(v) = p.gain_step {
        dev.set_gain_step(v)?;
    }
    if let Some(v) = p.lna_agc {
        dev.set_lna_agc(v)?;
    }
    if let Some(v) = p.mixer_agc {
        dev.set_mixer_agc(v)?;
    }
    if let Some(v) = p.rf_port {
        dev.set_rf_port(v)?;
    }
    if let Some(v) = p.bias_tee {
        dev.set_bias_tee(v)?;
    }
    // Purely host-side, so it switches between one block and the next with
    // nothing to reconfigure on the receiver.
    if let Some(v) = p.dc_block {
        dsp.set_dc_enabled(v);
    }
    Ok(())
}

/// Reprogram the rate, with the endpoint already closed.
///
/// The DSP is reset because its delay line holds a few dozen samples of the old
/// band at the old rate, and the clock measurement is restarted because the gap
/// would otherwise read as an enormous negative error.
fn rebuild(
    dev: &mut Device,
    hz: f64,
    dsp: &mut HostDsp,
    stats: &mut RxStats,
    shared: &Arc<Shared>,
    trace: &Trace,
) -> Result<()> {
    let r = dev.set_rate(hz);
    dsp.reset();
    let now = dev.sample_rate_hz();
    stats.restart_clock(now);
    shared.rate_milli_hz.store((now * 1000.0) as u64, Ordering::Relaxed);
    trace.note(format!("rate now {:.3} Msps complex", now / 1e6));
    r
}

/// Open the bulk endpoint and clear its halt, leaving it **unarmed**.
///
/// Nothing is submitted here: see [`prime`], which must not run until the
/// receiver has been told to transmit.
fn open_endpoint(dev: &Device) -> Result<nusb::Endpoint<Bulk, In>> {
    let mut ep = dev
        .usb()
        .interface()
        .endpoint::<Bulk, In>(BULK_EP)
        .map_err(|e| Error::Access(format!("cannot open the HydraSDR bulk endpoint: {e}")))?;
    // libhydrasdr clears this halt between stopping and starting the receiver,
    // and it is worth keeping: a session that died mid-stream can leave the
    // endpoint halted, and nothing else in an open would clear it. It also
    // resets the host's data toggle, which is what the receiver assumes when
    // `set_receiver_mode(RX)` re-initialises its own end.
    let _ = ep.clear_halt().wait();
    Ok(ep)
}

/// Cancel and reap everything outstanding, then drop the endpoint.
fn close(mut ep: nusb::Endpoint<Bulk, In>) {
    ep.cancel_all();
    let deadline = Instant::now() + CANCEL_TIMEOUT;
    while ep.pending() > 0 && Instant::now() < deadline {
        if ep.wait_next_complete(Duration::from_millis(50)).is_none() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transfer lengths must be a whole number of 32-bit words, because that is
    /// what the packed format is defined on. The clamps must also hold against
    /// a hand-edited config file.
    #[test]
    fn geometry_rounds_to_whole_words_and_clamps() {
        let cfg = HydraSdrConfig::default();
        let (n, bytes) = geometry(&cfg);
        assert_eq!(n, 16);
        assert_eq!(bytes, 128 * 1024);
        assert_eq!(bytes % 4, 0, "the packed format is defined on u32 words");
        assert_eq!(bytes % GRANULE, 0);

        let zero = HydraSdrConfig { transfers: 0, transfer_kib: 0, ..Default::default() };
        let (n, bytes) = geometry(&zero);
        assert_eq!(n, MIN_TRANSFERS);
        assert!(bytes >= GRANULE && bytes % GRANULE == 0);

        let huge = HydraSdrConfig { transfers: 255, transfer_kib: 4096, ..Default::default() };
        let (n, bytes) = geometry(&huge);
        assert_eq!(n, MAX_TRANSFERS);
        assert_eq!(bytes, MAX_TRANSFER_BYTES);

        for kib in [4u16, 7, 64, 128, 511, 4096] {
            let (_, b) = geometry(&HydraSdrConfig { transfer_kib: kib, ..Default::default() });
            assert_eq!(b % 4, 0, "{kib} KiB is not a whole number of words");
        }
    }

    /// Both wire formats decode to the same 12-bit range, and neither leaks the
    /// top four bits of the unpacked form into a sample.
    #[test]
    fn both_wire_formats_decode_to_twelve_bit_values() {
        // Unpacked: little-endian u16, top nibble ignored. A firmware that
        // leaves rubbish up there must not turn a mid-scale sample into a
        // full-scale one.
        let bytes: Vec<u8> =
            [0x00u16, 0x0800, 0xF7FF, 0x0FFF].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut carry = Vec::new();
        let mut out = Vec::new();
        decode_raw(&bytes, false, &mut carry, &mut out);
        assert_eq!(out, vec![0x000, 0x800, 0x7FF, 0xFFF]);
        assert!(carry.is_empty());

        // Packed: three words in, eight samples out, all within 12 bits.
        let words = [0x1234_5678u32, 0x9ABC_DEF0, 0x0F1E_2D3C];
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut carry = Vec::new();
        let mut out = Vec::new();
        decode_raw(&bytes, true, &mut carry, &mut out);
        assert_eq!(out.len(), 8);
        assert!(out.iter().all(|s| *s <= 0xfff));

        // A partial group is held, not dropped: the bytes that finish it are at
        // the head of the next completion.
        let mut carry = Vec::new();
        let mut out = Vec::new();
        decode_raw(&bytes[..8], true, &mut carry, &mut out);
        assert!(out.is_empty(), "two words are not a group");
        assert_eq!(carry.len(), 8, "the partial group has to survive the boundary");
    }

    /// The stream does not restart at a transfer boundary, so the decode must
    /// not either. This is the test that would have caught a decoder that reads
    /// every completion from byte zero: at 4 KiB granularity a packed transfer
    /// never ends on a group, so everything after the first one would be noise.
    #[test]
    fn a_group_split_across_completions_decodes_the_same_as_a_whole_one() {
        // Six groups' worth of recognisable bytes.
        let stream: Vec<u8> = (0..(PACKED_GROUP_BYTES * 6) as u32)
            .map(|i| (i.wrapping_mul(37) ^ 0x5a) as u8)
            .collect();

        for packed in [true, false] {
            let mut whole = Vec::new();
            decode_raw(&stream, packed, &mut Vec::new(), &mut whole);

            // Deliberately awkward splits: one shorter than a group, one that
            // lands mid-group, one that lands on a boundary.
            let mut carry = Vec::new();
            let mut split = Vec::new();
            let mut pos = 0;
            for len in [5usize, 7, 1, 13, 24, 3] {
                let end = (pos + len).min(stream.len());
                decode_raw(&stream[pos..end], packed, &mut carry, &mut split);
                pos = end;
            }
            decode_raw(&stream[pos..], packed, &mut carry, &mut split);

            assert_eq!(whole, split, "packed = {packed}: the split decode drifted");
            assert!(carry.is_empty(), "the whole stream is a whole number of groups");
        }
    }
}
