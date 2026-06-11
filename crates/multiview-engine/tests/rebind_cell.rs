//! Instant VIDEO->cell re-point (RT-6 / ADR-0034 FIX #1): `CompositorDrive::
//! rebind_cell` re-points which source a layout cell samples, LIVE, as an O(1)
//! map/pointer mutation — no `solve_layout`/`validate` re-solve (geometry is
//! unchanged on a pure source re-point), and the next compose tick draws the new
//! source.
//!
//! The cross-geometry correctness (FIX #2, scale-at-composite) is exercised end
//! to end here: a source whose native frame size differs from the destination
//! cell must composite correctly into that cell after a re-point — no clip/smear.
//!
//! Invariants: the re-point never blocks the clock (it is a pointer/map
//! mutation), an unknown target source is a clean error/hold (never a panic), and
//! a no-op run (no re-point) composites byte-identically to today.
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
use multiview_engine::clock::{OutputClock, Tick};
use multiview_engine::CompositorDrive;
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

/// A single full-canvas cell bound to `source`.
fn one_cell_layout(w: u32, h: u32, source: &str) -> Layout {
    Layout {
        name: "test".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            z: 0,
            fit: FitMode::Contain,
            source: Some(source.to_owned()),
            ..Cell::default()
        }],
    }
}

/// Build a drive whose single cell carries the id `cell_id` (the drive's own
/// cell-id -> index binding map, populated via `with_cell_ids`).
fn make_drive_with_id(
    layout: Layout,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    w: u32,
    h: u32,
    cell_id: &str,
) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some(cell_id.to_owned())])
}

fn make_drive(
    layout: Layout,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    w: u32,
    h: u32,
) -> CompositorDrive<Nv12Image> {
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
fn rebind_cell_repoints_which_source_a_cell_samples() {
    let (w, h) = (64, 64);
    // Cell "c0" bound to cam-a (dark); cam-b is a declared-but-unbound spare
    // (bright), already decoding into its store.
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    store_a.publish(solid(w, h, 40), MediaTime::ZERO);
    store_b.publish(solid(w, h, 200), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);
    let layout = one_cell_layout(w, h, "cam-a");
    let mut drive = make_drive_with_id(layout, stores, w, h, "c0");

    // Before: the cell draws cam-a (dark).
    let f0 = drive
        .compose(Tick {
            index: 1,
            pts: MediaTime::from_nanos(1_000_000),
        })
        .unwrap();
    let (y0, _, _) = f0.canvas.sample(w / 2, h / 2).unwrap();
    assert!(
        y0 < 100,
        "before re-point the cell shows dark cam-a (got {y0})"
    );

    // Re-point the cell "c0" to cam-b.
    drive
        .rebind_cell("c0", "cam-b")
        .expect("rebind to a known source");

    // After: the NEXT compose tick draws cam-b (bright) — no re-solve required.
    let f1 = drive
        .compose(Tick {
            index: 2,
            pts: MediaTime::from_nanos(2_000_000),
        })
        .unwrap();
    let (y1, _, _) = f1.canvas.sample(w / 2, h / 2).unwrap();
    assert!(
        y1 > 150,
        "after re-point the cell must show bright cam-b (got {y1})"
    );
}

#[test]
fn rebind_to_unknown_source_is_a_clean_error_and_holds() {
    let (w, h) = (32, 32);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w, h, 40), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    let layout = one_cell_layout(w, h, "cam-a");
    let mut drive = make_drive_with_id(layout, stores, w, h, "c0");

    // Re-pointing to a source with no store is an honest error — never a panic.
    let res = drive.rebind_cell("c0", "ghost");
    assert!(
        res.is_err(),
        "unknown target source must be an error, not a panic"
    );

    // The binding is held: the cell still samples cam-a (the dark frame), and the
    // clock keeps producing valid frames.
    let f = drive
        .compose(Tick {
            index: 1,
            pts: MediaTime::from_nanos(1_000_000),
        })
        .unwrap();
    let (y, _, _) = f.canvas.sample(w / 2, h / 2).unwrap();
    assert!(
        y < 100,
        "a rejected re-point must hold the prior cam-a binding (got {y})"
    );
}

#[test]
fn rebind_unknown_cell_is_a_clean_error() {
    let (w, h) = (32, 32);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);
    let layout = one_cell_layout(w, h, "cam-a");
    let mut drive = make_drive_with_id(layout, stores, w, h, "c0");

    // No cell with this id: an honest error, never a panic.
    assert!(drive.rebind_cell("no_such_cell", "cam-b").is_err());
}

#[test]
fn cross_geometry_repoint_renders_correct_no_clip_or_smear() {
    // THE headline RT-6 / verdict test: re-point a cell to a source whose native
    // size differs from the cell, and assert the composited tile is correctly
    // scaled — no clip, no smear, no wrong-size paint. The as-built 1:1 path
    // breaks this (a 16x16 source painted into a 64x64 cell would only fill the
    // top-left 16x16 and leave the rest on the slate/background).
    let (w, h) = (64, 64);

    // cam-a: a full-canvas-sized dark source (already cell-shaped).
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w, h, 40), MediaTime::ZERO);
    // cam-b: a SMALL 16x16 bright source — a different geometry than the 64x64
    // cell it will be re-pointed into.
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    store_b.publish(solid(16, 16, 210), MediaTime::ZERO);

    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);

    let layout = one_cell_layout(w, h, "cam-a");
    let mut drive = make_drive_with_id(layout, stores, w, h, "c0");

    drive.rebind_cell("c0", "cam-b").expect("rebind to cam-b");

    let now = MediaTime::from_nanos(1_000_000);
    let f = drive.compose(Tick { index: 1, pts: now }).unwrap();

    // EVERY sampled point across the full 64x64 cell must carry the bright cam-b
    // source — including the far corner (60,60), which is OUTSIDE cam-b's native
    // 16x16 frame. Under the broken 1:1 path the far corner would still show the
    // background/slate, not the source.
    for &(px, py) in &[(2u32, 2u32), (32, 32), (60, 60), (60, 2), (2, 60)] {
        let (y, _, _) = f.canvas.sample(px, py).unwrap();
        assert!(
            y > 150,
            "cross-geometry re-point: ({px},{py}) must carry the upscaled bright \
             cam-b (got {y}); a low value means the 16x16 source was NOT scaled \
             into the 64x64 cell (clip/smear bug)"
        );
    }
}

#[test]
fn no_repoint_compose_is_unchanged() {
    // Protect the output clock: a drive that is never re-pointed composes exactly
    // as a freshly-constructed identical drive — the rebind machinery is inert
    // until used.
    let (w, h) = (64, 64);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w, h, 123), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), Arc::clone(&store_a));
    let drive = make_drive(one_cell_layout(w, h, "cam-a"), stores, w, h);

    // A second, independent drive built identically.
    let mut stores2 = HashMap::new();
    stores2.insert("cam-a".to_owned(), store_a);
    let drive2 = make_drive(one_cell_layout(w, h, "cam-a"), stores2, w, h);

    let f = drive.compose(tick_at(7)).unwrap();
    let f2 = drive2.compose(tick_at(7)).unwrap();
    assert_eq!(
        f.canvas.y_plane(),
        f2.canvas.y_plane(),
        "an un-re-pointed drive must compose byte-identically"
    );
    assert_eq!(f.canvas.uv_plane(), f2.canvas.uv_plane());
}
