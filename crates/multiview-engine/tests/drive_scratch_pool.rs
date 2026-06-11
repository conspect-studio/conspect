//! Per-tick **scratch-pool** allocation gate for the compositor drive loop
//! (EFF-11; CLAUDE.md safety rule §5 "frame buffers come from per-device pools
//! allocated at start, never per-frame").
//!
//! The drive loop's [`CompositorDrive::compose`] resolves each cell to a held
//! frame + placement every tick. The per-tick *scratch* it uses to do that — the
//! `held` Arc vector, the `placements` vector, and the z-order index vector — must
//! come from a **pool reused across ticks**, not be freshly allocated and dropped
//! every tick on the protected output clock (invariant #1). These tests pin:
//!
//! 1. **Bounded scratch reservations.** Composing N ticks reserves the scratch
//!    backing stores a *bounded* number of times (one-time warm-up), **NOT**
//!    proportional to N. This is the allocation/copy-count gate: a regression that
//!    reintroduced a per-tick `Vec::with_capacity` would make the reservation
//!    count grow with N and fail here.
//! 2. **Byte-identity.** Pooling the scratch does not change the composed canvas:
//!    the output over a fixed scene is byte-for-byte identical across repeated
//!    composes (the pool is cleared-and-reused, never carrying stale pixels).
//! 3. **Pool-under-pressure never stalls.** A layout swap that *grows* the cell
//!    count forces the pool to grow once, then re-stabilises — the clock keeps
//!    producing exactly one valid frame per tick across the growth, never
//!    erroring or stalling (invariant #1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::Tick;
use multiview_engine::{CompositorDrive, OutputClock};
use multiview_framestore::TileStore;

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, resolved_color()).unwrap()
}

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap()
}

/// A grid layout with `n` cells, each bound to its own primed source, tiled
/// left-to-right (the cells need not be pixel-perfect; this drives the scratch).
fn grid_layout(w: u32, h: u32, n: usize) -> (Layout, HashMap<String, Arc<TileStore<Nv12Image>>>) {
    let mut cells = Vec::with_capacity(n);
    let mut stores = HashMap::new();
    // `as`-free fraction maths (guardrail: `as_conversions` is denied in tests):
    // widen the small counts losslessly through `u16` -> `f32`.
    let n_f = f32::from(u16::try_from(n.max(1)).unwrap());
    for i in 0..n {
        let id = format!("cam-{i}");
        let frac = 1.0_f32 / n_f;
        cells.push(Cell {
            x: frac * f32::from(u16::try_from(i).unwrap()),
            y: 0.0,
            w: frac,
            h: 1.0,
            z: 0,
            fit: FitMode::Contain,
            source: Some(id.clone()),
            ..Cell::default()
        });
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults(id.as_str()));
        store.publish(
            solid(w, h, 40 + u8::try_from(i % 200).unwrap()),
            MediaTime::ZERO,
        );
        stores.insert(id, store);
    }
    let layout = Layout {
        name: "grid".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells,
    };
    (layout, stores)
}

fn make_drive(
    layout: Layout,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
) -> CompositorDrive<Nv12Image> {
    let (w, h) = (layout.canvas.width, layout.canvas.height);
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

fn tick_at(index: u64) -> Tick {
    let clock = OutputClock::new(Rational::FPS_60).unwrap();
    Tick {
        index,
        pts: clock.pts_at(index),
    }
}

#[test]
fn scratch_reservations_are_bounded_not_proportional_to_ticks() {
    // The headline allocation-count gate: over many ticks the per-tick scratch
    // (held / placements / order) must be reserved a BOUNDED number of times
    // (a one-time warm-up), never once-per-tick. With a stable layout the pool
    // stabilises after the first compose, so the reservation count after the
    // warm-up tick equals the count after thousands more ticks.
    const N: u64 = 5_000;
    let (w, h) = (64, 48);
    let (layout, stores) = grid_layout(w, h, 9);
    let drive = make_drive(layout, stores);

    // Warm up the pool with the first compose, then snapshot the reservation
    // count. The pool now holds backing stores sized for this stable layout.
    let _ = drive.compose(tick_at(0)).unwrap();
    let after_warmup = drive.scratch_reservations();

    // Many more ticks over the SAME layout must not reserve again.
    for i in 1..=N {
        let _ = drive.compose(tick_at(i)).unwrap();
    }
    let after_n = drive.scratch_reservations();

    assert_eq!(
        after_n,
        after_warmup,
        "scratch must be POOLED: {N} extra ticks reserved \
         {extra} more times (must be 0 — allocations are one-time, not per-tick)",
        extra = after_n - after_warmup
    );
}

#[test]
fn pooling_is_byte_identical_across_repeated_composes() {
    // Pooling (clear-and-reuse) must not change a single output byte: the same
    // scene composed twice — separated by composes of OTHER ticks that reuse the
    // very same pooled buffers — is byte-for-byte identical. A pool that leaked
    // stale pixels (failed to clear/overwrite) would diverge here.
    let (w, h) = (64, 48);
    let (layout, stores) = grid_layout(w, h, 4);
    let drive = make_drive(layout, stores);

    let first = drive.compose(tick_at(0)).unwrap();
    let first_y = first.canvas.y_plane().to_vec();
    let first_uv = first.canvas.uv_plane().to_vec();

    // Churn the pool with intervening composes, then recompose the same tick.
    for i in 1..50 {
        let _ = drive.compose(tick_at(i)).unwrap();
    }
    let again = drive.compose(tick_at(0)).unwrap();

    assert_eq!(
        again.canvas.y_plane(),
        first_y.as_slice(),
        "Y plane must be byte-identical after the pool was reused"
    );
    assert_eq!(
        again.canvas.uv_plane(),
        first_uv.as_slice(),
        "UV plane must be byte-identical after the pool was reused"
    );
}

#[test]
fn pool_growth_under_a_larger_layout_never_stalls_the_clock() {
    // Pool-under-pressure: start with a small layout, then hot-swap to one with
    // many more cells. The pool must grow ONCE to fit (a bounded, one-time
    // reservation bump) and keep producing exactly one valid frame per tick
    // across the growth — never erroring, never stalling (invariant #1). After
    // the growth the larger layout is again pool-stable.
    let (w, h) = (64, 48);
    let (small, small_stores) = grid_layout(w, h, 2);
    let (large, large_stores) = grid_layout(w, h, 16);

    // Build the drive over the UNION of stores so the swapped-in cells resolve.
    let mut stores = small_stores;
    for (k, v) in large_stores {
        stores.entry(k).or_insert(v);
    }
    let mut drive = make_drive(small, stores);

    // Steady on the small layout: every frame is valid.
    for i in 0..10 {
        let f = drive.compose(tick_at(i)).unwrap();
        assert_eq!(f.canvas.width(), w);
        assert_eq!(f.canvas.height(), h);
    }

    // Hot-swap to the larger layout at a frame boundary.
    drive.set_layout(Arc::new(large)).unwrap();

    // The pool grows to fit the 16 cells, but the clock never falters: each tick
    // still yields exactly one valid composited frame.
    for i in 10..40 {
        let f = drive.compose(tick_at(i)).unwrap();
        assert_eq!(
            f.canvas.width(),
            w,
            "frame must stay valid across pool growth"
        );
        assert_eq!(f.canvas.height(), h);
        assert_eq!(f.tick.index, i);
    }

    // After the growth the larger layout is pool-stable: a long run reserves no
    // further (the growth was a one-time bump, not per-tick).
    let stable = drive.scratch_reservations();
    for i in 40..2_000 {
        let _ = drive.compose(tick_at(i)).unwrap();
    }
    assert_eq!(
        drive.scratch_reservations(),
        stable,
        "the larger layout must be pool-stable after one growth (no per-tick reserve)"
    );
}
