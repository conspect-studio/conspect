//! Real-time per-tick **composite budget** benchmark (ADR-0022, perf gate).
//!
//! The CPU reference compositor must render one 1080p multiview canvas within a
//! single output-clock tick. The proof galleries are 1280×720@25 (≤ 40 ms/tick)
//! and stress at 1080p; this bench measures 1920×1080 at N ∈ {1, 6, 9} tiles
//! (solid + saturated-chroma red|blue split + one wide-gamut PQ tile, the same
//! transfer mix the LUT path is gated on) and, beyond the criterion timing,
//! **hard-asserts** the budget — the median of ~10 `composite()` calls at N = 9
//! must be ≤ 40 ms (mirroring `overlay_efficiency.rs`'s `bench_meta_smoke`
//! structural assertion, so a regression *fails* the bench rather than merely
//! slowing it). Criterion compiles the bench optimized (release), which is the
//! regime the budget is stated in.
//!
//! GPU-free and runs on the default build (no `required-features`), exercising
//! the parallel LUT compositor (`composite` → `composite_with_threads`).
//! `#![forbid(unsafe_code)]` is in force crate-wide.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{composite, CanvasColor, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

/// Canvas geometry: 1080p, the stated stress resolution.
const CANVAS_W: u32 = 1920;
/// Canvas height (see [`CANVAS_W`]).
const CANVAS_H: u32 = 1080;
/// The per-tick budget at 25 fps.
const TICK_BUDGET: Duration = Duration::from_millis(40);

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn pq_bt2020() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    }
}

/// A source whose chroma varies left/right (saturated red | blue) so the EOTF
/// path sees post-matrix gamma overshoot (the realistic hot case).
fn red_blue_split(width: u32, height: u32, color: ColorInfo) -> Nv12Image {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let half = w / 2;
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = if col < half { 81 } else { 41 };
        }
    }
    for crow in 0..h / 2 {
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            let left = cpair < half / 2;
            uv[idx] = if left { 90 } else { 240 };
            uv[idx + 1] = if left { 240 } else { 110 };
        }
    }
    Nv12Image::new(width, height, y, uv, color).unwrap()
}

/// Grid `(cols, rows, tile_w, tile_h)` for `n` tiles over the canvas, using
/// integer math only (no float / `as` casts): `cols = ceil(sqrt(n))`.
fn grid_dims(n: usize) -> (u32, u32, u32, u32) {
    let n32 = u32::try_from(n).unwrap_or(1).max(1);
    // ceil(sqrt(n)) via integer search.
    let mut cols = 1_u32;
    while cols * cols < n32 {
        cols += 1;
    }
    let rows = n32.div_ceil(cols);
    // Tile sizes that tile the canvas; force even (NV12 4:2:0) and >= 2.
    let tile_w = ((CANVAS_W / cols.max(1)) & !1).max(2);
    let tile_h = ((CANVAS_H / rows.max(1)) & !1).max(2);
    (cols, rows, tile_w, tile_h)
}

/// Build `n` tile source images covering the canvas in a grid; each is a
/// solid card, a red|blue split, or a PQ tile in rotation so the composite runs
/// the full transfer mix. Returns the owned images (tiles borrow them).
fn build_sources(n: usize) -> Vec<Nv12Image> {
    let (_, _, tile_w, tile_h) = grid_dims(n);
    (0..n)
        .map(|i| match i % 3 {
            0 => Nv12Image::solid(tile_w, tile_h, 120, 100, 150, bt709_limited()).unwrap(),
            1 => red_blue_split(tile_w, tile_h, bt709_limited()),
            _ => red_blue_split(tile_w, tile_h, pq_bt2020()),
        })
        .collect()
}

/// Place `images` as a grid of tiles over the canvas.
fn build_tiles(images: &[Nv12Image], n: usize) -> Vec<Tile<'_>> {
    let (cols, _, tile_w, tile_h) = grid_dims(n);
    images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let iu = u32::try_from(i).unwrap_or(0);
            let col = iu % cols;
            let row = iu / cols;
            Tile::placed(img, (col * tile_w) & !1, (row * tile_h) & !1, 1.0)
        })
        .collect()
}

/// Render the 1080p canvas with `n` tiles a few times and return the median
/// wall time of a single `composite()` call.
fn median_composite_time(n: usize, samples: usize) -> Duration {
    let images = build_sources(n);
    let tiles = build_tiles(&images, n);
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.02, 0.02);

    // One warm-up (page-in the output buffers, build the LUTs once is per-call
    // but the allocator/caches warm here).
    let warm = composite(CANVAS_W, CANVAS_H, canvas, bg, &tiles).unwrap();
    std::hint::black_box(&warm);

    let mut durations: Vec<Duration> = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let out = composite(CANVAS_W, CANVAS_H, canvas, bg, &tiles).unwrap();
        durations.push(start.elapsed());
        std::hint::black_box(&out);
    }
    durations.sort_unstable();
    durations[durations.len() / 2]
}

/// Criterion timing across N ∈ {1, 6, 9}, plus the hard per-tick budget
/// assertion at N = 9 (the design target: a 9-up 1080p multiview within one
/// 25 fps tick).
fn bench_composite_1080p(c: &mut Criterion) {
    let mut group = c.benchmark_group("composite/1080p");
    for &n in &[1_usize, 6, 9] {
        let images = build_sources(n);
        let tiles = build_tiles(&images, n);
        let canvas = CanvasColor::default();
        let bg = LinearRgba::opaque(0.02, 0.02, 0.02);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let out = composite(CANVAS_W, CANVAS_H, canvas, bg, &tiles).unwrap();
                std::hint::black_box(out);
            });
        });
    }
    group.finish();

    // HARD GATE: the 9-tile 1080p composite must fit one 25 fps tick. Measured
    // OUTSIDE the criterion loop (median of 10 calls) so the budget breach fails
    // the bench binary, mirroring overlay_efficiency.rs's structural asserts.
    let median = median_composite_time(9, 10);
    assert!(
        median <= TICK_BUDGET,
        "composite 1080p×9 median {median:?} exceeds the {TICK_BUDGET:?} per-tick budget (ADR-0022)"
    );
}

criterion_group!(benches, bench_composite_1080p);
criterion_main!(benches);
