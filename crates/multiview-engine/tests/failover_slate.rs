//! The configurable per-CELL failover slate in the compositor drive (ADR-0027 /
//! ADR-0030): a down cell composites the slate its `on_loss` policy selects —
//! `Bars` → SMPTE colour bars, `NoSignal` → the signal-lost card, `Black` → a
//! black fill — while a LIVE cell is unaffected (byte-identical), and each
//! distinct slate is built ONCE (not per tick — invariant #1).
//!
//! This is the LAYOUT-tile half of the single shared policy; the passthrough
//! program honours the SAME `FailoverSlate` choice via `multiview-output`.
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
use multiview_config::FailoverSlate;
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

/// The default nosignal card the drive is built with (the back-compat path,
/// shown only when NO per-cell policy is supplied). A recognisable mid-luma so
/// the default-card path is distinguishable from the explicit policy slates.
const DEFAULT_CARD_LUMA: u8 = 80;

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    solid(w, h, DEFAULT_CARD_LUMA)
}

/// One full-canvas cell bound to `source`, with the given failover policy.
fn one_cell_layout(w: u32, h: u32, source: &str) -> Layout {
    Layout {
        name: "failover-test".to_owned(),
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
    canvas_w: u32,
    canvas_h: u32,
    slates: Vec<FailoverSlate>,
) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal_card(canvas_w, canvas_h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_slates(slates)
}

fn tick_at(index: u64, secs: u64) -> Tick {
    let clock = OutputClock::new(Rational::FPS_60).unwrap();
    let _ = clock.pts_at(index);
    Tick {
        index,
        pts: MediaTime::from_nanos(i64::try_from(secs).unwrap() * 1_000_000_000),
    }
}

/// A down cell (no store, `NoSignal`) with `on_loss = Bars` composites the bars
/// pattern — a descending-luma staircase across the canvas, never the flat card.
#[test]
fn down_cell_with_bars_policy_composites_bars() {
    let (w, h) = (256, 64);
    let stores = HashMap::new(); // no store -> the cell is down (NoSignal)
    let drive = make_drive(
        one_cell_layout(w, h, "cam-a"),
        stores,
        w,
        h,
        vec![FailoverSlate::Bars],
    );

    let frame = drive.compose(tick_at(0, 0)).unwrap();
    // Bars: the left band (white) is brighter than the right band (blue), and the
    // luma is NOT uniform (a flat card would be uniform).
    let (y_left, _, _) = frame.canvas.sample(4, 32).unwrap();
    let (y_right, _, _) = frame.canvas.sample(w - 4, 32).unwrap();
    assert!(
        y_left > y_right,
        "bars descend in luma left->right (white..blue): left {y_left} must exceed right {y_right}"
    );
    // Not a flat fill: at least two distinct luma values exist across the row.
    let distinct: std::collections::BTreeSet<u8> = (0..w)
        .filter_map(|x| frame.canvas.sample(x, 32).map(|s| s.0))
        .collect();
    assert!(
        distinct.len() >= 3,
        "the bars slate is a multi-band staircase, not a flat card (got {} distinct lumas)",
        distinct.len()
    );
}

/// A down cell with `on_loss = NoSignal` composites the signal-lost card — a
/// flat, recognisable card distinct from both bars (a multi-band staircase) and
/// black (luma 16).
#[test]
fn down_cell_with_nosignal_policy_composites_the_card() {
    let (w, h) = (128, 64);
    let drive = make_drive(
        one_cell_layout(w, h, "cam-a"),
        HashMap::new(),
        w,
        h,
        vec![FailoverSlate::NoSignal],
    );
    let frame = drive.compose(tick_at(0, 0)).unwrap();
    let (y, cb, cr) = frame.canvas.sample(w / 2, h / 2).unwrap();
    // The signal-lost card is a flat field across the row (a card, not bars).
    let uniform = (0..w).all(|x| frame.canvas.sample(x, h / 2).map(|s| s.0) == Some(y));
    assert!(
        uniform,
        "the NoSignal card is a flat fill, not the bars staircase"
    );
    // It is a distinct, recognisable card — not pure black, and chroma-tinted
    // (the "no-signal blue"), so it is unmistakably a no-signal card.
    assert_ne!(
        y, 16,
        "the NoSignal card is distinct from pure black (luma 16)"
    );
    assert_ne!(
        (cb, cr),
        (128, 128),
        "the NoSignal card is chroma-tinted (the no-signal blue), not neutral"
    );
}

/// A down cell with `on_loss = Black` composites a black fill (luma 16),
/// distinct from the `NoSignal` card and from bars.
#[test]
fn down_cell_with_black_policy_composites_black() {
    let (w, h) = (128, 64);
    let drive = make_drive(
        one_cell_layout(w, h, "cam-a"),
        HashMap::new(),
        w,
        h,
        vec![FailoverSlate::Black],
    );
    let frame = drive.compose(tick_at(0, 0)).unwrap();
    let (y, cb, cr) = frame.canvas.sample(w / 2, h / 2).unwrap();
    assert_eq!(
        y, 16,
        "the Black policy must draw black (limited-range luma 16)"
    );
    assert_eq!((cb, cr), (128, 128), "black is neutral chroma");
    assert_ne!(
        y, DEFAULT_CARD_LUMA,
        "Black is distinct from the default card"
    );
}

/// A LIVE cell is byte-identical regardless of its `on_loss` policy — the slate
/// only ever shows when the cell is down (the policy changes WHAT shows on loss,
/// never the live picture).
#[test]
fn live_cell_is_byte_identical_across_policies() {
    let (w, h) = (128, 64);
    let live_luma = 200_u8;

    let compose_live = |slate: FailoverSlate| -> Nv12Image {
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
        store.publish(solid(w, h, live_luma), MediaTime::ZERO);
        let mut stores = HashMap::new();
        stores.insert("cam-a".to_owned(), store);
        let drive = make_drive(one_cell_layout(w, h, "cam-a"), stores, w, h, vec![slate]);
        // Read just after publish (< hold) -> LIVE.
        drive
            .compose(Tick {
                index: 1,
                pts: MediaTime::from_nanos(10_000_000),
            })
            .unwrap()
            .canvas
    };

    let bars = compose_live(FailoverSlate::Bars);
    let nosignal = compose_live(FailoverSlate::NoSignal);
    let black = compose_live(FailoverSlate::Black);

    assert_eq!(
        bars.y_plane(),
        nosignal.y_plane(),
        "a LIVE cell's Y is identical regardless of on_loss policy"
    );
    assert_eq!(bars.y_plane(), black.y_plane());
    assert_eq!(bars.uv_plane(), nosignal.uv_plane());
    assert_eq!(bars.uv_plane(), black.uv_plane());
    // And it is the live picture, not any slate.
    assert_eq!(bars.sample(w / 2, h / 2).unwrap().0, live_luma);
}

/// Each distinct slate is built ONCE, not per tick: composing the same down cell
/// for many ticks does not rebuild the slate (invariant #1 — no per-tick slate
/// allocation on the protected output clock).
#[test]
fn slates_are_built_once_not_per_tick() {
    let (w, h) = (64, 32);
    let drive = make_drive(
        one_cell_layout(w, h, "cam-a"),
        HashMap::new(),
        w,
        h,
        vec![FailoverSlate::Bars],
    );
    // Nothing is built until the slate is first needed.
    assert_eq!(
        drive.slate_builds(),
        0,
        "no slate built before the first compose"
    );

    // The FIRST compose of the down cell builds the bars slate exactly once.
    let _ = drive.compose(tick_at(0, 0)).unwrap();
    let built_after_first = drive.slate_builds();
    assert_eq!(
        built_after_first, 1,
        "the bars slate is built once on first use"
    );

    // Every subsequent tick REUSES it — the build count never advances with the
    // tick count (invariant #1: no per-tick slate allocation on the output clock).
    for index in 1..200_u64 {
        let _ = drive.compose(tick_at(index, 0)).unwrap();
    }
    assert_eq!(
        drive.slate_builds(),
        built_after_first,
        "the bars slate is reused tick-over-tick, never rebuilt per compose"
    );
    // Bounded by the number of distinct policies (<= 3), never the tick count.
    assert!(
        drive.slate_builds() <= 3,
        "at most one build per distinct slate kind, got {}",
        drive.slate_builds()
    );
}
