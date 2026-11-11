//! Emitted-line count for an HLS reconnect flood through the libav log
//! suppressor: 6 sources, each reconnecting every 10 s for 10 minutes; every
//! reconnect opens fresh `tls`/`https`/`hls` contexts (new `@ 0x…` pointers)
//! and logs the same handful of lines. Also times `observe_scoped`.
//!
//! Run: `cargo run --release -p multiview-ffmpeg --example log_flood`
#![allow(clippy::print_stdout, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use multiview_ffmpeg::log_bridge::{BridgeLevel, SuppressOutcome, Suppressor};

fn main() {
    let lines = [
        "[tls @ 0x{:x}] Error in the pull function.",
        "[tls @ 0x{:x}] IO error: End of file",
        "[https @ 0x{:x}] Will reconnect at 1234 in 0 second(s), error=End of file.",
        "[hls @ 0x{:x}] Failed to reload playlist 0",
    ];
    let mut s = Suppressor::new(256, Duration::from_secs(5));
    let (mut seen, mut emitted) = (0_u64, 0_u64);
    let started = Instant::now();
    let mut addr = 0x5607_a5fe_d2c0_u64;
    for second in 0..600_u64 {
        for src in ["a", "b", "c", "d", "e", "f"] {
            if second % 10 != 0 {
                continue;
            }
            for _burst in 0..2 {
                addr = addr.wrapping_add(0x1a40);
                for l in lines {
                    let text = l.replace("{:x}", &format!("{addr:x}"));
                    seen += 1;
                    let out = s.observe_scoped(
                        BridgeLevel::Error,
                        Some(src),
                        &text,
                        Duration::from_secs(second),
                    );
                    if !matches!(out, SuppressOutcome::Suppress) {
                        emitted += 1;
                    }
                }
            }
        }
    }
    let per = started.elapsed().as_secs_f64() * 1e9 / f64::from(u32::try_from(seen).unwrap());
    println!(
        "{seen} libav lines over 10 min -> {emitted} emitted ({:.0}/min); observe {per:.0} ns/line",
        f64::from(u32::try_from(emitted).unwrap()) / 10.0
    );
}
