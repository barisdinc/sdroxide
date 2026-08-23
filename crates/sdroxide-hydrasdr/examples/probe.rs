//! Open a HydraSDR RFOne, receive for a few seconds, and print everything a bug
//! report needs.
//!
//! ```text
//! cargo run -p sdroxide-hydrasdr --example probe -- [MHz] [Msps]
//! ```
//!
//! Three lines are worth the trouble, and none of them can be produced from the
//! host side alone.
//!
//! `firmware` settles *which radio this is*. A prototype RFOne enumerates on
//! the Airspy R2's own USB id, and only the version string separates them: it
//! has to begin `HydraSDR RF`.
//!
//! `first raw 12-bit` settles whether the unpacking is right: the values should
//! sit near mid-scale (2048) and vary smoothly. Values jumping between 0 and
//! 4095 mean the 12-bit groups are being read at the wrong offset.
//!
//! `decoded as (re,im)` settles which way the spectrum runs. Point the receiver
//! at a carrier deliberately *above* the tuned frequency: the energy has to
//! land above DC. If it lands below, the fs/4 rotation is inverted and every
//! spectrum this backend produces is mirrored.
//!
//! Worth running at one of the alternate rates as well — 12, 8, 6 or 4.096 —
//! because those are the ones no enumeration reports and the ones a firmware
//! may not carry. The trace says which table each rate came from.

use std::time::{Duration, Instant};

use sdroxide_hydrasdr::HydraSdrHandle;
use sdroxide_types::HydraSdrConfig;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sdroxide_hydrasdr=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let mhz: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(93.2);
    let msps: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5.0);

    println!("=== devices on the bus ===");
    let devices = sdroxide_hydrasdr::list();
    if devices.is_empty() {
        println!("  none — check the udev rule and the cable");
        println!("  (a prototype board on 1d50:60a1 is only listed here when its USB");
        println!("   descriptors say HydraSDR; otherwise it belongs to the Airspy driver)");
        return;
    }
    for (i, d) in devices.iter().enumerate() {
        println!("  {i}: {}", d.label());
    }

    let cfg = HydraSdrConfig { sample_rate_hz: msps * 1e6, ..Default::default() };

    println!("\n=== opening at {mhz} MHz, {msps} Msps complex ===");
    let mut h = match HydraSdrHandle::open(&cfg, mhz * 1e6) {
        Ok(h) => h,
        Err(e) => {
            println!("open failed: {e}");
            if let Some(t) = sdroxide_hydrasdr::diagnostics() {
                println!("\n{t}");
            }
            return;
        }
    };

    println!("  receiver    : {}", h.label);
    println!("  firmware    : {:?}", h.firmware);
    println!("  board       : {}", h.board);
    println!("  serial      : {}", h.serial.as_deref().unwrap_or("none"));
    println!(
        "  usb id      : {}",
        if h.legacy_usb_id { "1d50:60a1 (legacy)" } else { "38af:0001" }
    );
    println!(
        "  rate        : {:.3} Msps complex ({:.3} Msps at the ADC)",
        h.sample_rate_hz / 1e6,
        h.sample_rate_hz * 2.0 / 1e6
    );
    let rates: Vec<String> = h.rates_hz.iter().map(|r| format!("{:.3}", r / 1e6)).collect();
    println!("  offers      : {} Msps complex", rates.join(", "));
    println!("  packing     : {}", if h.packing_active { "12-bit" } else { "off (old firmware)" });
    println!("  rf port     : {}", if h.rf_port_active { "commandable" } else { "fixed on ANT" });
    if let Some(from) = h.snapped_from {
        println!("  rate snapped: asked {:.3} Msps", from / 1e6);
    }

    println!("\n=== receiving for 3 s ===");
    let mut buf = vec![0f32; 1 << 16];
    let mut total = 0usize;
    let mut peak: f32 = 0.0;
    let mut sum_sq = 0.0f64;

    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        let n = h.rx_read(&mut buf);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        for v in &buf[..n] {
            peak = peak.max(v.abs());
            sum_sq += (*v * *v) as f64;
        }
        total += n / 2;
    }

    let rms = (sum_sq / (total * 2).max(1) as f64).sqrt();
    println!("  samples     : {total} ({:.3} Msps measured)", total as f64 / 3.0 / 1e6);
    println!("  peak        : {peak:.4} of full scale");
    println!("  rms         : {rms:.4}");
    if total == 0 {
        println!("  NOTE: nothing arrived at all. The trace below is the whole story.");
    } else if peak >= 0.99 {
        println!("  NOTE: the front end is saturating — lower the gain step.");
    } else if peak < 0.01 {
        println!("  NOTE: nearly silent — check the antenna, the gain, the RF port and the");
        println!("        frequency. This radio has three sockets and only ANT is the default.");
    }

    h.release();
    println!("\n=== session trace ===");
    match sdroxide_hydrasdr::diagnostics() {
        Some(t) => println!("{t}"),
        None => println!("(none)"),
    }
}
