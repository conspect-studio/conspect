//! Integration tests for the broadcast layout additions (`layout` module).
//!
//! These pin the multi-head / video-wall foundation (broadcast-multiviewer
//! brief §1): the per-output `Orientation` and the shared `QuarterTurn` rotation
//! vocabulary, the per-tile `opacity`, and the `Head` / `VideoWall` multi-head
//! wall model with bezel compensation. The additions are **additive and
//! defaulted** — an existing `Cell` constructed without them must still validate
//! and round-trip unchanged.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::layout::{
    BezelCompensation, Canvas, Cell, FitMode, Head, Layout, Orientation, QuarterTurn, VideoWall,
};

fn canvas() -> Canvas {
    Canvas {
        width: 1920,
        height: 1080,
        fps_num: 60_000,
        fps_den: 1001,
    }
}

/// A plain cell with no broadcast extras (the pre-existing shape).
fn plain_cell() -> Cell {
    Cell {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
        z: 0,
        fit: FitMode::Contain,
        source: None,
        ..Cell::default()
    }
}

fn layout_with(cells: Vec<Cell>) -> Layout {
    Layout {
        name: "test".to_owned(),
        canvas: canvas(),
        cells,
    }
}

#[test]
fn plain_cell_without_broadcast_fields_still_validates() {
    // Backwards compatibility: a cell built without the broadcast extras must validate.
    let l = layout_with(vec![plain_cell()]);
    assert!(l.validate().is_ok());
}

#[test]
fn cell_round_trips_without_broadcast_fields() {
    // A JSON document predating the per-tile `opacity` field must still
    // deserialize (it is `#[serde(default)]`) and validate unchanged.
    let json = r#"{"x":0.0,"y":0.0,"w":0.5,"h":0.5,"z":0,"fit":"Contain","source":null}"#;
    let c: Cell = serde_json::from_str(json).unwrap();
    let l = layout_with(vec![c]);
    assert!(l.validate().is_ok());
}

#[test]
fn rotation_quarter_turn_degrees_map() {
    assert_eq!(QuarterTurn::None.degrees(), 0);
    assert_eq!(QuarterTurn::Cw90.degrees(), 90);
    assert_eq!(QuarterTurn::Cw180.degrees(), 180);
    assert_eq!(QuarterTurn::Cw270.degrees(), 270);
}

#[test]
fn rotation_swaps_aspect_for_odd_quarter_turns() {
    assert!(!QuarterTurn::None.swaps_axes());
    assert!(QuarterTurn::Cw90.swaps_axes());
    assert!(!QuarterTurn::Cw180.swaps_axes());
    assert!(QuarterTurn::Cw270.swaps_axes());
}

#[test]
fn orientation_default_is_landscape() {
    assert_eq!(Orientation::default(), Orientation::Landscape);
    assert!(!Orientation::Landscape.swaps_axes());
    assert!(Orientation::Portrait.swaps_axes());
}

#[test]
fn head_binds_canvas_and_layout_and_validates() {
    let head = Head {
        id: "head-0".to_owned(),
        canvas: canvas(),
        orientation: Orientation::Landscape,
        layout: "wall-main".to_owned(),
    };
    assert!(head.validate().is_ok());
}

#[test]
fn head_with_empty_id_fails() {
    let head = Head {
        id: String::new(),
        canvas: canvas(),
        orientation: Orientation::Landscape,
        layout: "wall-main".to_owned(),
    };
    assert!(head.validate().is_err());
}

#[test]
fn head_with_bad_canvas_fails() {
    let mut bad = canvas();
    bad.width = 0;
    let head = Head {
        id: "head-0".to_owned(),
        canvas: bad,
        orientation: Orientation::Landscape,
        layout: "wall-main".to_owned(),
    };
    assert!(head.validate().is_err());
}

#[test]
fn video_wall_validates_grid_and_heads() {
    let wall = VideoWall {
        name: "wall".to_owned(),
        cols: 2,
        rows: 2,
        bezel: BezelCompensation::default(),
        heads: vec![
            Head {
                id: "h0".to_owned(),
                canvas: canvas(),
                orientation: Orientation::Landscape,
                layout: "l".to_owned(),
            },
            Head {
                id: "h1".to_owned(),
                canvas: canvas(),
                orientation: Orientation::Landscape,
                layout: "l".to_owned(),
            },
            Head {
                id: "h2".to_owned(),
                canvas: canvas(),
                orientation: Orientation::Landscape,
                layout: "l".to_owned(),
            },
            Head {
                id: "h3".to_owned(),
                canvas: canvas(),
                orientation: Orientation::Landscape,
                layout: "l".to_owned(),
            },
        ],
    };
    assert!(wall.validate().is_ok());
}

#[test]
fn video_wall_rejects_zero_grid() {
    let wall = VideoWall {
        name: "wall".to_owned(),
        cols: 0,
        rows: 2,
        bezel: BezelCompensation::default(),
        heads: vec![],
    };
    assert!(wall.validate().is_err());
}

#[test]
fn video_wall_rejects_head_count_mismatch() {
    // 2x2 = 4 heads expected; supplying one fails.
    let wall = VideoWall {
        name: "wall".to_owned(),
        cols: 2,
        rows: 2,
        bezel: BezelCompensation::default(),
        heads: vec![Head {
            id: "h0".to_owned(),
            canvas: canvas(),
            orientation: Orientation::Landscape,
            layout: "l".to_owned(),
        }],
    };
    assert!(wall.validate().is_err());
}

#[test]
fn video_wall_rejects_duplicate_head_ids() {
    let head = |id: &str| Head {
        id: id.to_owned(),
        canvas: canvas(),
        orientation: Orientation::Landscape,
        layout: "l".to_owned(),
    };
    let wall = VideoWall {
        name: "wall".to_owned(),
        cols: 1,
        rows: 2,
        bezel: BezelCompensation::default(),
        heads: vec![head("dup"), head("dup")],
    };
    assert!(wall.validate().is_err());
}

#[test]
fn bezel_default_is_zero() {
    let b = BezelCompensation::default();
    assert_eq!(b.horizontal_px, 0);
    assert_eq!(b.vertical_px, 0);
}

#[test]
fn bezel_negative_compensation_fails() {
    let wall = VideoWall {
        name: "wall".to_owned(),
        cols: 1,
        rows: 1,
        bezel: BezelCompensation {
            horizontal_px: -5,
            vertical_px: 0,
        },
        heads: vec![Head {
            id: "h0".to_owned(),
            canvas: canvas(),
            orientation: Orientation::Landscape,
            layout: "l".to_owned(),
        }],
    };
    assert!(wall.validate().is_err());
}

#[test]
fn video_wall_round_trips_via_json() {
    let wall = VideoWall {
        name: "wall".to_owned(),
        cols: 1,
        rows: 1,
        bezel: BezelCompensation {
            horizontal_px: 4,
            vertical_px: 6,
        },
        heads: vec![Head {
            id: "h0".to_owned(),
            canvas: canvas(),
            orientation: Orientation::Portrait,
            layout: "l".to_owned(),
        }],
    };
    let json = serde_json::to_string(&wall).unwrap();
    let back: VideoWall = serde_json::from_str(&json).unwrap();
    assert_eq!(wall, back);
}
