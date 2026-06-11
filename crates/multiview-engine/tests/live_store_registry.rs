//! Live source registration/unregistration on the running drive (ADR-W018):
//! `CompositorDrive::insert_store` (live add) makes a brand-new source
//! addressable for the O(1) `rebind_cell` crosspoint, and
//! `CompositorDrive::remove_store` (live remove) makes every cell bound to the
//! id composite its failover slate from the **next** tick — never a panic,
//! never a stall (invariants #1/#2). A rebind to a removed source is an honest
//! held error.
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
use multiview_core::traits::SourceState;
use multiview_engine::clock::{OutputClock, Tick};
use multiview_engine::CompositorDrive;
use multiview_framestore::TileStore;

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(64, 64)
}

fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, resolved_color()).unwrap()
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

fn make_drive(
    layout: Layout,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    w: u32,
    h: u32,
    cell_id: &str,
) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        solid(w, h, 16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some(cell_id.to_owned())])
}

fn tick_at(index: u64) -> Tick {
    let clock = OutputClock::new(Rational::FPS_60).unwrap();
    Tick {
        index,
        pts: clock.pts_at(index),
    }
}

#[test]
fn insert_store_makes_a_live_added_source_rebindable() {
    // A drive built with ONE source; a second source arrives at runtime
    // (live add): after `insert_store` the new id is addressable by
    // `rebind_cell` and the next compose draws its frame.
    let (w, h) = (64, 64);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w, h, 40), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    let mut drive = make_drive(one_cell_layout(w, h, "cam-a"), stores, w, h, "c0");

    // Before the live add, a rebind to the unknown id is held (honest error).
    assert!(drive.rebind_cell("c0", "live-1").is_err());

    let live = Arc::new(TileStore::<Nv12Image>::with_defaults("live-1"));
    live.publish(solid(w, h, 200), MediaTime::ZERO);
    drive.insert_store("live-1", Arc::clone(&live));
    assert!(
        drive.store("live-1").is_some(),
        "an inserted store is addressable via the store accessor"
    );
    drive
        .rebind_cell("c0", "live-1")
        .expect("rebind to the live-added source");
    let frame = drive.compose(tick_at(1)).expect("compose");
    let (y, _, _) = frame.canvas.sample(32, 32).expect("sample");
    assert!(
        y > 150,
        "the cell must draw the live-added bright source, got luma {y}"
    );
}

#[test]
fn remove_store_slates_bound_cells_next_tick_and_holds_rebinds() {
    let (w, h) = (64, 64);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w, h, 200), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    let mut drive = make_drive(one_cell_layout(w, h, "cam-a"), stores, w, h, "c0");

    // Tick 1: the bound source draws bright.
    let before = drive.compose(tick_at(1)).expect("compose before");
    assert_eq!(before.source_states.get("cam-a"), Some(&SourceState::Live));

    // Live remove: the store unregisters; the bound cell composites the slate
    // (dark card) from the very next tick, with the honest NoSignal state.
    assert!(
        drive.remove_store("cam-a"),
        "removal of a known id reports true"
    );
    assert!(drive.store("cam-a").is_none());
    let after = drive.compose(tick_at(2)).expect("compose after remove");
    assert_eq!(
        after.source_states.get("cam-a"),
        Some(&SourceState::NoSignal),
        "a removed source's bound cell rides its on_loss slate"
    );
    let (y, _, _) = after.canvas.sample(32, 32).expect("sample");
    assert!(y < 60, "the cell must draw the slate, got luma {y}");

    // A rebind to the removed id is an honest held error; removing it again
    // reports false (idempotent-shaped, never a panic).
    assert!(drive.rebind_cell("c0", "cam-a").is_err());
    assert!(!drive.remove_store("cam-a"));
}
