//! Overlay rasterizer **efficiency benchmarks** (feature `overlay`, ADR-0016
//! T1-T5, perf-regression gate ADR-E009).
//!
//! These criterion benches measure the load-bearing efficiency targets the
//! overlay design is built around — and, crucially, they *assert* the structural
//! invariants that make the targets hold (criterion measures wall time; the
//! `assert!`s below prove the "zero"/"bounded" claims directly so a regression
//! fails the bench, not just slows it):
//!
//! - **T2 — zero re-rasterization on an unchanged frame.** A second `prepare_run`
//!   of an identical label inserts **zero** new atlas entries; a glyph-overlapping
//!   clock tick (`...:56 -> ...:57`) inserts only the genuinely new glyph. The
//!   bench brackets the steady-state re-prepare and asserts `atlas_entry_count`
//!   does not change.
//! - **T3 — zero per-frame heap allocation on the overlay draw path.** The
//!   sub-pass blends **in place** (`blend_overlays(&mut canvas, &list) -> ()`,
//!   write-only via `get_mut`; no `Vec`/`Box` is built per frame), so the only
//!   heap object it could touch is the canvas buffer. The bench asserts the
//!   canvas's backing capacity ([`LinearCanvasBuffer::buffer_capacity`]) is
//!   **unchanged** across a steady-state blend — a reallocation would change it.
//! - **T4 — bounded atlas (LRU + byte cap).** After churning far more distinct
//!   glyphs than fit, the bench asserts `atlas_used_bytes <= byte_cap`.
//! - **T5 — overlay prep cost ~constant in label count (batched).** Prep is timed
//!   at N = 1, 16, 64 already-resident labels gathered into one draw list and
//!   blended in one pass; the per-label work is bounded (no full-canvas re-upload,
//!   no per-tile pass).
//!
//! Run: `cargo bench -p multiview-compositor --features overlay`. GPU-free: the
//! bench exercises the same CPU draw path the GPU sub-pass feeds (one rasterizer,
//! two consumers — overlay-rendering.md §4.4). `#![forbid(unsafe_code)]` is in
//! force, so the zero-alloc proof uses the safe capacity fingerprint, not a
//! custom global allocator.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss
)]

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};

use multiview_compositor::overlay::subpass::{
    blend_overlays, LinearCanvasBuffer, OverlayColor, OverlayDrawList, OverlayPrimitive,
    OverlayRect,
};
use multiview_compositor::overlay::text::{FontFamily, RasterizedRun, TextEngine};

/// White, fully opaque straight RGBA (the text fill used throughout).
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Opaque linear green overlay tint.
const GREEN: OverlayColor = OverlayColor::opaque(0.0, 1.0, 0.0);

/// Build a draw list from `labels`: each label is rasterized into a text run and
/// pushed, plus a meter bar and a tally border — a representative steady-state
/// multiviewer overlay frame (text + analytic primitives, ADR-0016 §4.2).
fn build_draw_list(engine: &mut TextEngine, labels: &[&str]) -> OverlayDrawList {
    let mut list = OverlayDrawList::new();
    let runs: Vec<RasterizedRun> = labels
        .iter()
        .map(|label| {
            engine
                .rasterize_run(label, FontFamily::Mono, 24.0, WHITE)
                .expect("rasterize label")
        })
        .collect();
    for run in &runs {
        list.push_run(run, GREEN);
    }
    // Analytic primitives (no bitmap, no upload) per the design.
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(8, 8, 120, 8),
        corner_radius: 0,
        color: GREEN,
    });
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(0, 0, 320, 2),
        color: GREEN,
    });
    list
}

/// T2: an unchanged frame re-prepares with ZERO new atlas entries; a clock tick
/// inserts only the genuinely new glyph.
fn bench_t2_unchanged_frame_zero_reraster(c: &mut Criterion) {
    let mut group = c.benchmark_group("overlay/T2_unchanged_frame");
    group.bench_function("reprepare_identical_clock", |b| {
        let mut engine = TextEngine::new().expect("fonts load");
        // Warm the atlas with the clock face.
        engine
            .prepare_run("12:34:56", FontFamily::Mono, 32.0, WHITE)
            .expect("warm");
        let resident = engine.atlas_entry_count();
        assert!(resident > 0, "atlas warmed");
        b.iter(|| {
            let before = engine.atlas_entry_count();
            let run = engine
                .prepare_run("12:34:56", FontFamily::Mono, 32.0, WHITE)
                .expect("reprepare");
            let after = engine.atlas_entry_count();
            // T2: an unchanged frame inserts NOTHING into the atlas.
            assert_eq!(after, before, "unchanged frame re-rasterized a glyph");
            std::hint::black_box(run);
        });
        // A glyph-overlapping tick adds exactly the one new digit ('7').
        let before_tick = engine.atlas_entry_count();
        engine
            .prepare_run("12:34:57", FontFamily::Mono, 32.0, WHITE)
            .expect("tick");
        assert_eq!(
            engine.atlas_entry_count(),
            before_tick + 1,
            "clock tick rasterized more than the one new glyph"
        );
    });
    group.finish();
}

/// T3: the steady-state overlay sub-pass blend performs ZERO per-frame heap
/// allocation — it blends in place and never reallocates the canvas buffer.
fn bench_t3_zero_alloc_draw_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("overlay/T3_zero_alloc_blend");
    let mut engine = TextEngine::new().expect("fonts load");
    let list = build_draw_list(&mut engine, &["CAM 1", "12:34:56", "REC"]);
    // Pre-allocated canvas: the sub-pass only reads-then-writes existing pixels.
    let mut canvas = LinearCanvasBuffer::transparent(512, 256);
    // Warm once, then snapshot the backing capacity.
    blend_overlays(&mut canvas, &list);
    let cap_before = canvas.buffer_capacity();

    // Hard-assert the zero-allocation property OUTSIDE the timing loop so a
    // regression fails loudly rather than merely slowing the bench: a blend that
    // allocated would have to grow/reallocate the canvas buffer, changing its
    // capacity.
    for _ in 0..1024 {
        blend_overlays(&mut canvas, &list);
    }
    assert_eq!(
        canvas.buffer_capacity(),
        cap_before,
        "overlay sub-pass reallocated the canvas buffer on a steady-state frame (T3)"
    );

    group.bench_function("blend_overlays_512x256", |b| {
        b.iter(|| {
            blend_overlays(&mut canvas, &list);
            std::hint::black_box(&canvas);
        });
        // The blend never grows the canvas across the whole timed run either.
        assert_eq!(
            canvas.buffer_capacity(),
            cap_before,
            "canvas buffer capacity drifted during the timed blend (T3)"
        );
    });
    group.finish();
}

/// T4: churning far more distinct glyphs than fit keeps the atlas within its hard
/// byte cap via LRU eviction (bounded memory).
fn bench_t4_bounded_atlas(c: &mut Criterion) {
    let mut group = c.benchmark_group("overlay/T4_bounded_atlas");
    let cap = 8 * 1024_usize;
    // Distinct glyph soup, far larger than an 8 KiB atlas can hold at 40px.
    let labels: Vec<String> = (0..64).map(|i| format!("L{i:03}-ABCDEFGHIJ")).collect();
    group.bench_function("churn_distinct_glyphs_under_cap", |b| {
        b.iter_batched(
            || TextEngine::with_atlas_byte_cap(cap).expect("fonts load under cap"),
            |mut engine| {
                for label in &labels {
                    // Distinct labels at a large size force eviction.
                    let _ = engine.prepare_run(label, FontFamily::Sans, 40.0, WHITE);
                }
                // T4: bounded memory — never exceeds the hard byte cap.
                assert!(
                    engine.atlas_used_bytes() <= cap,
                    "atlas {} B exceeded the {cap} B cap (T4)",
                    engine.atlas_used_bytes()
                );
                std::hint::black_box(engine.atlas_entry_count());
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

/// T5: overlay prep cost is ~constant in label count — one gather, one blend.
/// Prepares N already-resident labels into one draw list and times the gather +
/// blend; per-label work is bounded (no full-canvas re-upload, no per-tile pass).
fn bench_t5_prep_constant_in_label_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("overlay/T5_prep_scaling");
    for &n in &[1_usize, 16, 64] {
        // A pool of distinct labels, all pre-resident so prepare hits the cache.
        let labels: Vec<String> = (0..n).map(|i| format!("CAM {i:02}")).collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut engine = TextEngine::new().expect("fonts load");
        // Warm every glyph so the timed gather re-rasterizes nothing (T2 again).
        let _ = build_draw_list(&mut engine, &label_refs);
        let mut canvas = LinearCanvasBuffer::transparent(1280, 720);

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                // One gather (the batched draw list) + one blend pass (T5).
                let list = build_draw_list(&mut engine, &label_refs);
                blend_overlays(&mut canvas, &list);
                std::hint::black_box(&canvas);
            });
        });
    }
    group.finish();
}

/// A tiny manual-timing sanity bench using `iter_custom` so the harness records a
/// real number even when the others are filtered out (keeps the bench binary
/// self-describing for the CI gate).
/// ADR-0023: the region-limited overlay **bake** must cost only the overlay
/// footprint, not the whole canvas. A small overlay (a few labels + a meter + a
/// border) burned into a 1280×720 program frame must run **much faster** than the
/// full-canvas reference — the structural proof that the per-frame bake tracks
/// the footprint, not the canvas. (The `assert!` makes a regression *fail* the
/// bench, not just slow it, like `composite_realtime.rs`.)
fn bench_region_limited_bake(c: &mut Criterion) {
    use multiview_compositor::overlay::subpass::{
        apply_overlays_to_nv12, apply_overlays_to_nv12_reference,
    };
    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};

    fn time_avg<F: Fn()>(iters: u32, f: F) -> Duration {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        start.elapsed() / iters.max(1)
    }

    let canvas = CanvasColor::default();
    let image = Nv12Image::solid(1280, 720, 128, 128, 128, canvas.output_tag()).expect("nv12");
    let mut engine = TextEngine::new().expect("fonts load");
    let list = build_draw_list(&mut engine, &["CAM 1", "CAM 2", "LIVE"]);

    let optimized = time_avg(20, || {
        std::hint::black_box(apply_overlays_to_nv12(&image, &list, canvas).expect("bake"));
    });
    let reference = time_avg(20, || {
        std::hint::black_box(apply_overlays_to_nv12_reference(&image, &list, canvas).expect("ref"));
    });
    assert!(
        optimized * 3 < reference,
        "region-limited bake ({optimized:?}) must be >3x faster than the full-canvas \
         reference ({reference:?}) for a small overlay on a 1280x720 canvas (ADR-0023)"
    );

    c.bench_function("overlay/bake_region_limited", |b| {
        b.iter(|| {
            std::hint::black_box(apply_overlays_to_nv12(&image, &list, canvas).expect("bake"))
        });
    });
}

fn bench_meta_smoke(c: &mut Criterion) {
    c.bench_function("overlay/smoke", |b| {
        b.iter_custom(|iters| {
            let mut engine = TextEngine::new().expect("fonts load");
            let start = Instant::now();
            for _ in 0..iters {
                let run = engine
                    .prepare_run("0", FontFamily::Mono, 16.0, WHITE)
                    .expect("prepare");
                std::hint::black_box(run);
            }
            start.elapsed().max(Duration::from_nanos(1))
        });
    });
}

criterion_group!(
    benches,
    bench_t2_unchanged_frame_zero_reraster,
    bench_t3_zero_alloc_draw_path,
    bench_t4_bounded_atlas,
    bench_t5_prep_constant_in_label_count,
    bench_region_limited_bake,
    bench_meta_smoke,
);
criterion_main!(benches);
