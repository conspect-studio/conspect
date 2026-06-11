//! Multi-head wall composition tests (ADR-MV001): heads are placed row-major with
//! bezel gaps inserted before interior heads, mixed-resolution heads are sized
//! independently, bindings can be updated by head id, and an invalid wall is
//! rejected — pure value computation, never blocking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::layout::{BezelCompensation, Canvas, Head, VideoWall};
use multiview_engine::heads::{HeadBinding, WallComposition};

fn canvas(w: u32, h: u32) -> Canvas {
    Canvas {
        width: w,
        height: h,
        fps_num: 50,
        fps_den: 1,
    }
}

fn head(id: &str, w: u32, h: u32) -> Head {
    Head {
        id: id.to_owned(),
        canvas: canvas(w, h),
        orientation: multiview_core::layout::Orientation::Landscape,
        layout: "grid".to_owned(),
    }
}

fn wall_2x2(bezel: BezelCompensation) -> VideoWall {
    VideoWall {
        name: "wall".to_owned(),
        cols: 2,
        rows: 2,
        bezel,
        heads: vec![
            head("h00", 1920, 1080),
            head("h01", 1920, 1080),
            head("h10", 1920, 1080),
            head("h11", 1920, 1080),
        ],
    }
}

#[test]
fn places_heads_row_major_no_bezel() {
    let comp = WallComposition::resolve(&wall_2x2(BezelCompensation::default())).unwrap();
    let p = comp.placements();
    assert_eq!(p.len(), 4);
    // Row-major: (0,0), (1920,0), (0,1080), (1920,1080).
    assert_eq!((p[0].x, p[0].y), (0, 0));
    assert_eq!((p[1].x, p[1].y), (1920, 0));
    assert_eq!((p[2].x, p[2].y), (0, 1080));
    assert_eq!((p[3].x, p[3].y), (1920, 1080));
    // No bezel: extent is exactly 2x the head size.
    assert_eq!(comp.extent(), (3840, 2160));
    assert_eq!(comp.grid(), (2, 2));
}

#[test]
fn inserts_bezel_gaps_between_interior_heads() {
    let bezel = BezelCompensation {
        horizontal_px: 10,
        vertical_px: 6,
    };
    let comp = WallComposition::resolve(&wall_2x2(bezel)).unwrap();
    let p = comp.placements();
    // Second column shifted right by head width + horizontal bezel.
    assert_eq!(p[1].x, 1930);
    // Second row shifted down by head height + vertical bezel.
    assert_eq!(p[2].y, 1086);
    assert_eq!((p[3].x, p[3].y), (1930, 1086));
    // Extent includes only interior bezels (the trailing edge has no gap).
    assert_eq!(comp.extent(), (3850, 2166));
}

#[test]
fn mixed_resolution_heads_are_sized_independently() {
    // Column 0 width 1280, column 1 width 1920; row 0 height 720, row 1 height 1080.
    let wall = VideoWall {
        name: "mixed".to_owned(),
        cols: 2,
        rows: 2,
        bezel: BezelCompensation::default(),
        heads: vec![
            head("a", 1280, 720),
            head("b", 1920, 720),
            head("c", 1280, 1080),
            head("d", 1920, 1080),
        ],
    };
    let comp = WallComposition::resolve(&wall).unwrap();
    let p = comp.placements();
    // Column origins from the first row's heads (1280 then 1920).
    assert_eq!(p[0].x, 0);
    assert_eq!(p[1].x, 1280);
    // Row origins from the first column's heads (720 then 1080).
    assert_eq!(p[2].y, 720);
    assert_eq!(p[3].y, 720);
    assert_eq!(comp.extent(), (1280 + 1920, 720 + 1080));
}

#[test]
fn bindings_start_empty_and_can_be_updated_by_id() {
    let mut comp = WallComposition::resolve(&wall_2x2(BezelCompensation::default())).unwrap();
    assert_eq!(comp.bindings().len(), 4);
    assert!(comp.bindings()[0].sources.is_empty());

    let binding = HeadBinding::new("h01")
        .with_sources(vec![Some("cam-1".to_owned()), None])
        .with_overlays(vec!["clock".to_owned()]);
    assert!(comp.bind_head(binding.clone()));
    assert_eq!(comp.bindings()[1], binding);

    // Unknown head id: no update.
    assert!(!comp.bind_head(HeadBinding::new("nope")));
}

#[test]
fn rejects_invalid_wall() {
    // Head count mismatch.
    let bad = VideoWall {
        name: "bad".to_owned(),
        cols: 2,
        rows: 2,
        bezel: BezelCompensation::default(),
        heads: vec![head("a", 100, 100)],
    };
    assert!(WallComposition::resolve(&bad).is_err());

    // Single-head wall (1x1) is valid.
    let single = VideoWall {
        name: "single".to_owned(),
        cols: 1,
        rows: 1,
        bezel: BezelCompensation::default(),
        heads: vec![head("only", 1920, 1080)],
    };
    let comp = WallComposition::resolve(&single).unwrap();
    assert_eq!(comp.extent(), (1920, 1080));
    assert_eq!(comp.placements().len(), 1);
}
