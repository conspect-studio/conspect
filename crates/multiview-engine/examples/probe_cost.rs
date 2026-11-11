//! Release-mode per-tile cost of the content-fault probes the per-tick
//! projection runs for every visible source (black = `mean_luma`, freeze =
//! `changed_fraction` against the previous frame), at 640x360 and 1920x1080.
//!
//! Run: `cargo run --release -p multiview-engine --example probe_cost`
#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::items_after_statements
)]

use std::time::Instant;

use multiview_engine::probe::{DetectionZone, LumaView};

fn main() {
    const N: u32 = 2000;
    for (w, h) in [(640_u32, 360_u32), (1920, 1080)] {
        let n = usize::try_from(w * h).unwrap();
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let a: Vec<u8> = (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect();
        let b: Vec<u8> = a.iter().map(|v| v.wrapping_add(1)).collect();
        let cur = LumaView::packed(&a, w, h).unwrap();
        let prev = LumaView::packed(&b, w, h).unwrap();
        let t0 = Instant::now();
        let mut acc = 0.0;
        for _ in 0..N {
            acc += std::hint::black_box(&cur).mean_luma(DetectionZone::FULL);
        }
        let mean = t0.elapsed().as_secs_f64() * 1e6 / f64::from(N);
        let t0 = Instant::now();
        for _ in 0..N {
            acc += std::hint::black_box(&cur).changed_fraction(&prev, DetectionZone::FULL, 2);
        }
        let changed = t0.elapsed().as_secs_f64() * 1e6 / f64::from(N);
        println!(
            "{w}x{h}: mean_luma {mean:8.1} us  changed_fraction {changed:8.1} us  \
             (per tile per tick; x6 tiles x 29.97 = {:5.2}% of a core) [{acc:.0}]",
            (mean + changed) * 6.0 * 29.97 / 1e4
        );
    }
}
