//! Compositor drive-loop tests — invariants #1 and #2.
//!
//! Prove the loop produces a valid composited frame on time at every tick, even
//! when a tile's store is empty or its producer has died (holds last-good or
//! shows the `NoSignal` slate), and that it never stalls awaiting an input.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::blend::{over, LinearRgba};
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image,
};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::{MediaTime, Rational};
use multiview_core::traits::SourceState;
use multiview_engine::clock::Tick;
use multiview_engine::{CompositorDrive, OutputClock};
use multiview_framestore::{TileStore, TileThresholds};

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, resolved_color()).unwrap()
}

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    // A distinct slate luma so tests can tell it apart from real frames.
    Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap()
}

fn two_cell_layout(w: u32, h: u32) -> Layout {
    Layout {
        name: "test".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![
            Cell {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            },
            Cell {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-b".to_owned()),
                ..Cell::default()
            },
        ],
    }
}

fn make_drive(
    layout: Layout,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    canvas_w: u32,
    canvas_h: u32,
) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal_card(canvas_w, canvas_h),
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
fn invalid_layout_is_rejected_at_construction() {
    let mut layout = two_cell_layout(640, 480);
    layout.canvas.width = 0; // invalid
    let err = CompositorDrive::<Nv12Image>::new(
        Arc::new(layout),
        HashMap::new(),
        nosignal_card(640, 480),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    );
    assert!(
        err.is_err(),
        "a structurally invalid layout must be rejected"
    );
}

#[test]
fn produces_a_valid_frame_when_all_stores_are_empty() {
    // No tile has ever produced a frame -> every tile is NoSignal, yet the loop
    // MUST still produce one valid canvas on time (invariant #1/#2).
    let (w, h) = (640, 480);
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    stores.insert(
        "cam-b".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b")),
    );
    let drive = make_drive(two_cell_layout(w, h), stores, w, h);

    let frame = drive.compose(tick_at(0)).unwrap();
    assert_eq!(frame.canvas.width(), w);
    assert_eq!(frame.canvas.height(), h);
    assert_eq!(frame.tick.index, 0);
    // Both sources reported NoSignal.
    assert_eq!(
        frame.source_states.get("cam-a"),
        Some(&SourceState::NoSignal)
    );
    assert_eq!(
        frame.source_states.get("cam-b"),
        Some(&SourceState::NoSignal)
    );
    // The canvas is the slate luma everywhere (both tiles drew the slate card).
    assert!(frame.canvas.y_plane().iter().all(|&p| p > 0));
}

#[test]
fn holds_last_good_frame_when_a_producer_dies() {
    // A tile publishes once, then its producer "dies" (never publishes again).
    // While within the stale/reconnecting window the loop holds the LAST-GOOD
    // frame; the loop never stalls.
    let (w, h) = (640, 480);
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    let store_a = Arc::new(TileStore::<Nv12Image>::new(
        "cam-a",
        thresholds,
        multiview_framestore::NoSignalPolicy::Slate,
    ));
    // cam-a publishes a bright (y=200) frame at t=0, then dies.
    store_a.publish(solid(w / 2, h, 200), MediaTime::ZERO);

    let store_b = Arc::new(TileStore::<Nv12Image>::new(
        "cam-b",
        thresholds,
        multiview_framestore::NoSignalPolicy::Slate,
    ));

    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), Arc::clone(&store_a));
    stores.insert("cam-b".to_owned(), Arc::clone(&store_b));
    let drive = make_drive(two_cell_layout(w, h), stores, w, h);

    // t = 1s: past `hold` (500ms) -> STALE, but within `stale`/`nosignal`. The
    // last-good frame is still composited (left half is the bright frame).
    let stale_tick = Tick {
        index: 60,
        pts: MediaTime::from_nanos(1_000_000_000),
    };
    let frame = drive.compose(stale_tick).unwrap();
    assert_eq!(frame.canvas.width(), w);
    assert_eq!(
        frame.source_states.get("cam-a"),
        Some(&SourceState::Stale),
        "cam-a should be STALE (holding last-good)"
    );
    // Sample a left-half pixel: it must still carry the held bright frame.
    let (y_left, _, _) = frame.canvas.sample(10, 10).unwrap();
    assert!(
        y_left > 150,
        "left half should hold the last-good bright frame"
    );

    // t = 20s: past `nosignal` (10s) under Slate policy -> NoSignal slate.
    let dead_tick = Tick {
        index: 1200,
        pts: MediaTime::from_nanos(20_000_000_000),
    };
    let frame2 = drive.compose(dead_tick).unwrap();
    assert_eq!(
        frame2.source_states.get("cam-a"),
        Some(&SourceState::NoSignal),
        "cam-a should be NO_SIGNAL after the slate threshold"
    );
    let (y_left2, _, _) = frame2.canvas.sample(10, 10).unwrap();
    assert!(
        y_left2 < 100,
        "left half should now show the dark slate card"
    );
}

#[test]
fn loop_never_stalls_across_many_ticks_with_a_missing_store() {
    // A cell references a source with NO store at all (its producer never even
    // started). The loop must still produce a valid frame for every tick. A
    // tiny canvas keeps the per-pixel CPU reference fast; the robustness being
    // proven (never stalls / never errors on a missing store) is independent of
    // canvas size, and a few hundred ticks exercises the steady state.
    let (w, h) = (32, 24);
    let mut stores = HashMap::new();
    // Only cam-a has a store; cam-b's store is entirely absent.
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    let drive = make_drive(two_cell_layout(w, h), stores, w, h);

    for index in 0..500_u64 {
        let frame = drive.compose(tick_at(index)).unwrap();
        assert_eq!(frame.canvas.width(), w);
        assert_eq!(frame.canvas.height(), h);
        assert_eq!(frame.tick.index, index);
        // cam-b (missing store) is reported NoSignal, never an error/stall.
        assert_eq!(
            frame.source_states.get("cam-b"),
            Some(&SourceState::NoSignal)
        );
    }
}

#[test]
fn fresh_frame_is_composited_live() {
    let (w, h) = (640, 480);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    store_a.publish(solid(w / 2, h, 200), MediaTime::ZERO);
    store_b.publish(solid(w / 2, h, 50), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);
    let drive = make_drive(two_cell_layout(w, h), stores, w, h);

    // Read at t just after publish (< hold) -> both LIVE.
    let tick = Tick {
        index: 1,
        pts: MediaTime::from_nanos(10_000_000),
    };
    let frame = drive.compose(tick).unwrap();
    assert_eq!(frame.source_states.get("cam-a"), Some(&SourceState::Live));
    assert_eq!(frame.source_states.get("cam-b"), Some(&SourceState::Live));
    // Left half (cam-a, y=200) brighter than right half (cam-b, y=50).
    let (y_left, _, _) = frame.canvas.sample(10, 10).unwrap();
    let (y_right, _, _) = frame.canvas.sample(w - 10, 10).unwrap();
    assert!(y_left > y_right);
}

#[test]
fn sample_states_uses_the_latched_frame_age_not_producer_liveness() {
    // FRESHNESS DIVERGENCE regression (multiview-engine side): `sample_states` must
    // classify each tile on the LATCHED (shown) frame's age — exactly what the
    // compositor draws via `read_at` — not on the newest published frame.
    //
    // Scenario: cam-a is an ahead-decoding source. It has published frame@0 and
    // then raced ahead to a future-stamped frame (100s). At output time 20s the
    // tile is latched on frame@0 (20s stale -> NO_SIGNAL), yet the producer's
    // newest frame is future-stamped (so a producer-liveness `state()` would
    // wrongly report LIVE). The sampled state MUST be NO_SIGNAL and MUST equal
    // what `compose` reports for the same tick.
    let (w, h) = (640, 480);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    store_a.publish(solid(w / 2, h, 200), MediaTime::ZERO);
    store_a.publish(solid(w / 2, h, 200), MediaTime::from_nanos(100_000_000_000));
    // cam-b is a healthy live source tracking output time.
    store_b.publish(solid(w / 2, h, 50), MediaTime::from_nanos(20_000_000_000));
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);
    let drive = make_drive(two_cell_layout(w, h), stores, w, h);

    let now = MediaTime::from_nanos(20_000_000_000);
    let states = drive.sample_states(now);
    assert_eq!(
        states.get("cam-a"),
        Some(&SourceState::NoSignal),
        "cam-a's latched picture is 20s old -> NO_SIGNAL (not producer-liveness LIVE)"
    );
    assert_eq!(
        states.get("cam-b"),
        Some(&SourceState::Live),
        "cam-b tracks output time -> LIVE"
    );

    // sample_states must agree with what compose() actually draws this tick.
    let tick = Tick {
        index: 1200,
        pts: now,
    };
    let composed = drive.compose(tick).unwrap();
    assert_eq!(
        composed.source_states.get("cam-a"),
        states.get("cam-a"),
        "sample_states and compose must report the same latched-frame state"
    );
    assert_eq!(composed.source_states.get("cam-b"), states.get("cam-b"),);
}

#[test]
fn hot_swapping_layout_takes_effect_between_ticks() {
    let (w, h) = (640, 480);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w, h, 200), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);

    // Start with a single full-canvas cam-a cell.
    let layout1 = Layout {
        name: "full".to_owned(),
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
            source: Some("cam-a".to_owned()),
            ..Cell::default()
        }],
    };
    let mut drive = make_drive(layout1, stores, w, h);
    let f1 = drive.compose(tick_at(0)).unwrap();
    assert_eq!(f1.source_states.len(), 1);

    // Swap to a layout whose only cell is unbound -> slate everywhere.
    let layout2 = Layout {
        name: "blank".to_owned(),
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
            source: None,
            ..Cell::default()
        }],
    };
    drive.set_layout(Arc::new(layout2)).unwrap();
    let f2 = drive.compose(tick_at(1)).unwrap();
    // No bound sources now.
    assert!(f2.source_states.is_empty());
    // Rejecting an invalid swap keeps the prior layout.
    let mut bad = two_cell_layout(w, h);
    bad.canvas.height = 0;
    assert!(drive.set_layout(Arc::new(bad)).is_err());
    let f3 = drive.compose(tick_at(2)).unwrap();
    assert!(f3.source_states.is_empty(), "bad swap must not apply");
}

#[test]
fn per_cell_opacity_cross_fades_overlapping_tiles_in_linear_light() {
    // PER-CELL OPACITY regression: an overlapping TOP cell at opacity 0.5 over an
    // opaque bottom cell must produce the premultiplied linear-light `over` blend
    // — NOT a hard-cover of the top colour. This fails while `drive.rs` hardwires
    // every `Tile.opacity = 1.0` (the compositor already honours `Tile.opacity`,
    // but the drive never sets it from the cell).
    let (w, h) = (64, 64);

    // Two achromatic (cb=cr=128) sources at distinct lumas, each full-frame.
    let bottom_luma: u8 = 60;
    let top_luma: u8 = 210;
    let store_bottom = Arc::new(TileStore::<Nv12Image>::with_defaults("bottom"));
    let store_top = Arc::new(TileStore::<Nv12Image>::with_defaults("top"));
    store_bottom.publish(solid(w, h, bottom_luma), MediaTime::ZERO);
    store_top.publish(solid(w, h, top_luma), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("bottom".to_owned(), store_bottom);
    stores.insert("top".to_owned(), store_top);

    // Two fully-overlapping full-canvas cells: bottom opaque (z=0), top at
    // opacity 0.5 (z=1, drawn on top).
    let layout = Layout {
        name: "pip-ghost".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![
            Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("bottom".to_owned()),
                opacity: 1.0,
            },
            Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 1,
                fit: FitMode::Contain,
                source: Some("top".to_owned()),
                opacity: 0.5,
            },
        ],
    };

    // The drive's background is `LinearRgba::TRANSPARENT` (see `make_drive`); the
    // opaque bottom fully covers it, so it does not affect the centre pixel.
    let drive = make_drive(layout, stores, w, h);

    // Independently compute the EXPECTED centre pixel via the compositor's own
    // public pipeline functions, in the documented back-to-front order:
    //   acc = over(bottom@1.0, transparent) = bottom_opaque
    //   acc = over(top@0.5, bottom_opaque)
    // then encode through the back half. This is NOT tautological: it derives the
    // blend from the colour primitives, never from the drive output.
    let canvas_color = CanvasColor::default();
    let color = resolved_color();
    let lin_bottom = tile_yuv_to_canvas_linear(bottom_luma, 128, 128, color, canvas_color).unwrap();
    let lin_top = tile_yuv_to_canvas_linear(top_luma, 128, 128, color, canvas_color).unwrap();
    let bottom_premul = LinearRgba {
        r: lin_bottom[0],
        g: lin_bottom[1],
        b: lin_bottom[2],
        a: 1.0,
    }
    .premultiplied();
    let top_premul = LinearRgba {
        r: lin_top[0],
        g: lin_top[1],
        b: lin_top[2],
        a: 0.5,
    }
    .premultiplied();
    let folded = over(
        top_premul,
        over(bottom_premul, LinearRgba::TRANSPARENT.premultiplied()),
    );
    let straight = folded.unpremultiplied();
    let expected =
        canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas_color).unwrap();

    let frame = drive.compose(tick_at(0)).unwrap();
    let (cx, cy) = (w / 2, h / 2);
    let (y, cb, cr) = frame.canvas.sample(cx, cy).unwrap();

    // The composited centre pixel is the linear 50/50 blend (within ±1 code
    // value for chroma rounding), NOT the opaque top colour.
    assert!(
        i32::from(y).abs_diff(i32::from(expected[0])) <= 1,
        "centre luma {y} must equal the linear blend {} (top opacity 0.5), not the \
         hard-covered top luma {top_luma}",
        expected[0]
    );
    assert!(i32::from(cb).abs_diff(i32::from(expected[1])) <= 1);
    assert!(i32::from(cr).abs_diff(i32::from(expected[2])) <= 1);

    // Guard the regression directly: the blended luma must sit strictly between
    // the two source lumas — a hard cover would render the top luma (~210), the
    // bug today.
    let y_i = i32::from(y);
    assert!(
        y_i > i32::from(bottom_luma) && y_i < i32::from(top_luma),
        "blended luma {y} must lie strictly between bottom {bottom_luma} and top {top_luma}; \
         equal-to-top means opacity was ignored (hard cover)"
    );
}
