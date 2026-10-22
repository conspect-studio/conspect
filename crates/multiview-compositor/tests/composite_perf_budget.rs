//! Per-tick composite **budget** + **coverage-scaling** perf gates for the
//! tile-driven CPU kernel (ADR-0022), isolated in their OWN integration test
//! binary.
//!
//! WHY A DEDICATED BINARY: libtest runs every `#[test]` in one test binary
//! concurrently. Kept in `composite_tile_driven.rs` next to the 48-case
//! equivalence proptest and the correctness composites, these two 1080p perf
//! tests were scheduled at the same time as a dozen-plus other multi-threaded
//! composites, so their 4-thread composites were CPU-starved and the measured
//! wall-clock median inflated ~2–3× — a measurement artifact, not a kernel
//! regression (#75). In their own test executable libtest schedules only these
//! two, and `PERF_GATE` serializes the two against each other, so each 1080p
//! composite is timed without a sibling composite oversubscribing the cores.
//!
//! This does NOT claim isolation from other OS processes: on a shared host the
//! median still rides transient load, which the 7-sample central estimator
//! absorbs; the real 40 ms/tick guarantee is the release gate on a dedicated
//! runner. The statistic is the MEDIAN (not a best-case min, which would be a
//! rule-19 weakening — a min can pass when one fast sample dips under the
//! ceiling while most executions regress above it).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{composite_with_threads, CanvasColor, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

/// Serializes the two heavy 1080p perf tests against each other. libtest runs
/// this binary's `#[test]`s concurrently, so without the gate both tests would
/// fire their multi-threaded 1080p composites at once and oversubscribe the
/// CPU, inflating the measured wall time. Poison-tolerant: a panicking holder
/// must not wedge the other perf test. See #75.
static PERF_GATE: Mutex<()> = Mutex::new(());

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

/// Build a small NV12 image whose Y and chroma vary deterministically with a
/// per-image `seed`, so tiles differ from one another. All dimensions are
/// forced even (4:2:0).
fn varied_image(width: u32, height: u32, seed: u32, color: ColorInfo) -> Nv12Image {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let seed_u = usize::try_from(seed).unwrap();
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for row in 0..h {
        for col in 0..w {
            let val = (row.wrapping_mul(7) ^ col.wrapping_mul(5) ^ seed_u) % 220;
            y[row * w + col] = 16 + u8::try_from(val).unwrap_or(0);
        }
    }
    for crow in 0..h / 2 {
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            let cb = (crow.wrapping_mul(11) ^ cpair.wrapping_mul(3) ^ seed_u) % 240;
            let cr = (crow.wrapping_mul(3) ^ cpair.wrapping_mul(13) ^ seed_u) % 240;
            uv[idx] = u8::try_from(cb).unwrap_or(128);
            uv[idx + 1] = u8::try_from(cr).unwrap_or(128);
        }
    }
    Nv12Image::new(width, height, y, uv, color).unwrap()
}

/// Build a solid / split / PQ rotation of 9 grid tile *images* covering 1080p,
/// mirror of the bench's `build_sources`, for the budget smokes. Tiles borrow
/// these, so the caller holds the vec.
fn nine_up_1080p() -> Vec<Nv12Image> {
    let cols = 3_u32;
    let rows = 3_u32;
    let tile_w = ((1920 / cols) & !1).max(2);
    let tile_h = ((1080 / rows) & !1).max(2);
    (0..9)
        .map(|i| match i % 3 {
            0 => Nv12Image::solid(tile_w, tile_h, 120, 100, 150, bt709_limited()).unwrap(),
            1 => varied_image(tile_w, tile_h, i, bt709_limited()),
            _ => varied_image(tile_w, tile_h, i, pq_bt2020()),
        })
        .collect()
}

/// Place the 9-up `images` as a 3×3 grid over the 1080p canvas.
fn nine_up_tiles(images: &[Nv12Image]) -> Vec<Tile<'_>> {
    let cols = 3_u32;
    let rows = 3_u32;
    let tile_w = ((1920 / cols) & !1).max(2);
    let tile_h = ((1080 / rows) & !1).max(2);
    images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let iu = u32::try_from(i).unwrap_or(0);
            Tile::placed(
                img,
                ((iu % cols) * tile_w) & !1,
                ((iu / cols) * tile_h) & !1,
                1.0,
            )
        })
        .collect()
}

/// Median wall time of `samples` `composite_with_threads` calls (4 threads, LUT
/// path), after one warm-up. The median (not min/mean) is the reproducible
/// central estimator: it rejects the occasional stolen sample on a shared host
/// without hiding a recurring regression the way a best-case min would.
fn median_composite_time(
    canvas_w: u32,
    canvas_h: u32,
    tiles: &[Tile<'_>],
    samples: usize,
) -> Duration {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.02, 0.02);
    let warm = composite_with_threads(canvas_w, canvas_h, canvas, bg, tiles, true, 4).unwrap();
    std::hint::black_box(&warm);
    let mut durations: Vec<Duration> = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let out = composite_with_threads(canvas_w, canvas_h, canvas, bg, tiles, true, 4).unwrap();
        durations.push(start.elapsed());
        std::hint::black_box(&out);
    }
    durations.sort_unstable();
    durations[durations.len() / 2]
}

#[test]
fn tile_driven_meets_budget_9up_1080p() {
    // CI per-tick budget gate as a #[test] (not only a bench, ADR-0022). In
    // release this is the real 40 ms/tick (25 fps) gate; debug runs the color
    // math unoptimized (~10×+ slower), so it uses a generous ceiling — the test
    // still proves the composite completes per-tick-shaped work without the
    // O(pixels × tiles) blow-up (separately pinned by
    // `tile_driven_scales_with_coverage_not_tile_count`).
    const CANVAS_W: u32 = 1920;
    const CANVAS_H: u32 = 1080;
    // 7 samples: an odd count for an unambiguous median, with enough headroom to
    // ride out the occasional stolen sample on a shared host.
    const SAMPLES: usize = 7;
    let ceiling = if cfg!(debug_assertions) {
        Duration::from_millis(1500)
    } else {
        Duration::from_millis(40)
    };

    // Serialize against the sibling coverage-scaling perf test so neither test's
    // 1080p composites race the other inside this binary (#75).
    let _perf = PERF_GATE.lock().unwrap_or_else(PoisonError::into_inner);

    let images = nine_up_1080p();
    let tiles = nine_up_tiles(&images);

    let median = median_composite_time(CANVAS_W, CANVAS_H, &tiles, SAMPLES);
    assert!(
        median <= ceiling,
        "tile-driven composite 1080p×9 median {median:?} exceeds the {ceiling:?} \
         per-tick budget (ADR-0022)"
    );
}

#[test]
fn tile_driven_scales_with_coverage_not_tile_count() {
    // Regression guard for the O(pixels × tiles) blow-up that the rewrite kills:
    // with MANY small, mostly-disjoint tiles, the old kernel coverage-tested
    // EVERY tile at EVERY canvas pixel (here 256 tiles × ~2M px ≈ 530M tests),
    // while the tile-driven kernel touches only Σ tile-area (~2M). So a 256-tile
    // canvas must NOT be dramatically slower than the same canvas with 9 tiles
    // covering the same total area — if it is, the per-pixel-all-tiles loop has
    // returned. This holds regardless of build profile (it is a *ratio*, not an
    // absolute time), so it gates in both debug and release.
    const CANVAS_W: u32 = 1920;
    const CANVAS_H: u32 = 1080;
    let canvas = CanvasColor::default();

    // Serialize against the sibling budget perf test so neither test's 1080p
    // composites race the other inside this binary (#75).
    let _perf = PERF_GATE.lock().unwrap_or_else(PoisonError::into_inner);

    // 16×16 = 256 disjoint tiles tiling the canvas (each pixel covered once).
    let many_cols = 16_u32;
    let many_rows = 16_u32;
    let mw = ((CANVAS_W / many_cols) & !1).max(2);
    let mh = ((CANVAS_H / many_rows) & !1).max(2);
    let many_imgs: Vec<Nv12Image> = (0..(many_cols * many_rows))
        .map(|i| varied_image(mw, mh, i, bt709_limited()))
        .collect();
    let many_tiles: Vec<Tile<'_>> = many_imgs
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let iu = u32::try_from(i).unwrap_or(0);
            Tile::placed(
                img,
                ((iu % many_cols) * mw) & !1,
                ((iu / many_cols) * mh) & !1,
                1.0,
            )
        })
        .collect();
    let _ = canvas;

    let few_imgs = nine_up_1080p();
    let few_tiles = nine_up_tiles(&few_imgs);

    let many = median_composite_time(CANVAS_W, CANVAS_H, &many_tiles, 5);
    let few = median_composite_time(CANVAS_W, CANVAS_H, &few_tiles, 5);

    // Both composite ~the same covered area; with the old kernel `many` would be
    // ~28× the coverage-test work of `few`. Allow a generous 4× headroom for the
    // extra per-tile rect setup + scheduler noise; a true O(pixels × tiles)
    // regression blows far past this.
    let bound = few.saturating_mul(4) + Duration::from_millis(20);
    assert!(
        many <= bound,
        "256-tile composite ({many:?}) is far slower than 9-tile ({few:?}); \
         the O(pixels × tiles) coverage loop appears to have returned"
    );
}
