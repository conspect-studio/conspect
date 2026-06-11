//! Integration tests for the broadcast layout additions (`layout` module).
//!
//! These pin the multi-head / video-wall / per-tile crop+rotation foundation
//! (broadcast-multiviewer brief §1): per-tile region-of-interest crop, per-tile
//! quarter-turn rotation, output orientation, and the `Head` / `VideoWall`
//! multi-head wall model with bezel compensation. The additions are **additive
//! and defaulted** — an existing `Cell` constructed without them must still
//! validate and round-trip unchanged.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::layout::{
    BezelCompensation, Canvas, Cell, CropRect, FitMode, Head, Layout, Orientation, QuarterTurn,
    VideoWall,
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
fn default_cell_has_no_crop_no_rotation() {
    let c = Cell::default();
    assert!(c.crop.is_none());
    assert_eq!(c.rotation, QuarterTurn::None);
}

#[test]
fn plain_cell_without_broadcast_fields_still_validates() {
    // Backwards compatibility: a cell built without crop/rotation must validate.
    let l = layout_with(vec![plain_cell()]);
    assert!(l.validate().is_ok());
}

#[test]
fn cell_round_trips_without_broadcast_fields() {
    // A JSON document predating the new fields must still deserialize (the new
    // fields are `#[serde(default)]`).
    let json = r#"{"x":0.0,"y":0.0,"w":0.5,"h":0.5,"z":0,"fit":"Contain","source":null}"#;
    let c: Cell = serde_json::from_str(json).unwrap();
    assert!(c.crop.is_none());
    assert_eq!(c.rotation, QuarterTurn::None);
    let l = layout_with(vec![c]);
    assert!(l.validate().is_ok());
}

#[test]
fn valid_crop_rect_passes() {
    let mut c = plain_cell();
    c.crop = Some(CropRect {
        x: 0.1,
        y: 0.1,
        w: 0.5,
        h: 0.5,
    });
    let l = layout_with(vec![c]);
    assert!(l.validate().is_ok());
}

#[test]
fn full_frame_crop_passes() {
    let mut c = plain_cell();
    c.crop = Some(CropRect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    });
    let l = layout_with(vec![c]);
    assert!(l.validate().is_ok());
}

#[test]
fn crop_with_negative_origin_fails() {
    let mut c = plain_cell();
    c.crop = Some(CropRect {
        x: -0.1,
        y: 0.0,
        w: 0.5,
        h: 0.5,
    });
    let l = layout_with(vec![c]);
    let msg = l.validate().unwrap_err().to_string();
    assert!(msg.contains("crop"), "message was: {msg}");
}

#[test]
fn crop_exceeding_unit_square_fails() {
    let mut c = plain_cell();
    c.crop = Some(CropRect {
        x: 0.8,
        y: 0.0,
        w: 0.5, // x + w = 1.3 > 1.0
        h: 0.5,
    });
    let l = layout_with(vec![c]);
    assert!(l.validate().is_err());
}

#[test]
fn crop_with_zero_extent_fails() {
    let mut c = plain_cell();
    c.crop = Some(CropRect {
        x: 0.1,
        y: 0.1,
        w: 0.0,
        h: 0.5,
    });
    let l = layout_with(vec![c]);
    assert!(l.validate().is_err());
}

#[test]
fn crop_with_nan_fails() {
    let mut c = plain_cell();
    c.crop = Some(CropRect {
        x: f32::NAN,
        y: 0.0,
        w: 0.5,
        h: 0.5,
    });
    let l = layout_with(vec![c]);
    assert!(l.validate().is_err());
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
fn rotated_cell_validates() {
    let mut c = plain_cell();
    c.rotation = QuarterTurn::Cw90;
    let l = layout_with(vec![c]);
    assert!(l.validate().is_ok());
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
