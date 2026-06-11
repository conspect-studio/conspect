//! Anchor + padding box-layout math: resolve normalized rects and anchored
//! boxes to exact pixel coordinates on a canvas.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_overlay::geometry::{Anchor, BoxSize, NormRect, Padding, PixelRect, Region};

/// Assert two pixel values are equal within a sub-pixel tolerance. The layout
/// math is exact integer-valued float arithmetic, but comparing with a tolerance
/// keeps the assertion robust and avoids relying on bit-exact float equality.
fn assert_px(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "expected {expected}, got {actual}"
    );
}

/// A 1920x1080 canvas region (the full canvas).
fn full_canvas() -> Region {
    Region::from_pixels(0.0, 0.0, 1920.0, 1080.0)
}

#[test]
fn norm_rect_resolves_to_full_canvas_pixels() {
    let region = NormRect::FULL.to_region(1920, 1080);
    assert_px(region.x, 0.0);
    assert_px(region.y, 0.0);
    assert_px(region.width, 1920.0);
    assert_px(region.height, 1080.0);
}

#[test]
fn norm_rect_resolves_subrect_to_pixels() {
    // A quarter-canvas rect at the centre-ish.
    let rect = NormRect::new(0.25, 0.5, 0.5, 0.25);
    let region = rect.to_region(1920, 1080);
    assert_px(region.x, 480.0); // 0.25 * 1920
    assert_px(region.y, 540.0); // 0.5 * 1080
    assert_px(region.width, 960.0); // 0.5 * 1920
    assert_px(region.height, 270.0); // 0.25 * 1080
}

#[test]
fn anchor_top_left_with_padding_places_box_at_inset() {
    let region = full_canvas();
    let size = BoxSize::new(200.0, 60.0);
    let padding = Padding::uniform(16.0);
    let placed: PixelRect = Anchor::TopLeft.place(region, size, padding);
    // Top-left: box origin = region origin + (left padding, top padding).
    assert_px(placed.x, 16.0);
    assert_px(placed.y, 16.0);
    assert_px(placed.width, 200.0);
    assert_px(placed.height, 60.0);
}

#[test]
fn anchor_bottom_right_with_padding_places_box_against_far_edge() {
    let region = full_canvas();
    let size = BoxSize::new(200.0, 60.0);
    let padding = Padding::uniform(16.0);
    let placed = Anchor::BottomRight.place(region, size, padding);
    // Right edge: region.x + region.width - right_pad - box_w = 1920 - 16 - 200.
    assert_px(placed.x, 1920.0 - 16.0 - 200.0);
    // Bottom edge: 1080 - 16 - 60.
    assert_px(placed.y, 1080.0 - 16.0 - 60.0);
    assert_px(placed.width, 200.0);
    assert_px(placed.height, 60.0);
}

#[test]
fn anchor_center_ignores_padding_and_centres_box() {
    let region = full_canvas();
    let size = BoxSize::new(400.0, 200.0);
    // Center anchor: padding does not shift a centred box on either axis.
    let placed = Anchor::Center.place(region, size, Padding::uniform(50.0));
    assert_px(placed.x, (1920.0 - 400.0) / 2.0); // 760.0
    assert_px(placed.y, (1080.0 - 200.0) / 2.0); // 440.0
}

#[test]
fn anchor_top_center_uses_top_padding_and_centres_horizontally() {
    let region = full_canvas();
    let size = BoxSize::new(300.0, 80.0);
    let padding = Padding {
        top: 24.0,
        right: 10.0,
        bottom: 10.0,
        left: 10.0,
    };
    let placed = Anchor::TopCenter.place(region, size, padding);
    // Horizontal centre is unaffected by left/right padding.
    assert_px(placed.x, (1920.0 - 300.0) / 2.0);
    // Vertical uses the top padding.
    assert_px(placed.y, 24.0);
}

#[test]
fn anchor_center_right_uses_right_padding_and_centres_vertically() {
    let region = full_canvas();
    let size = BoxSize::new(120.0, 400.0);
    let padding = Padding {
        top: 5.0,
        right: 30.0,
        bottom: 5.0,
        left: 5.0,
    };
    let placed = Anchor::CenterRight.place(region, size, padding);
    assert_px(placed.x, 1920.0 - 30.0 - 120.0);
    assert_px(placed.y, (1080.0 - 400.0) / 2.0);
}

#[test]
fn placement_within_a_tile_region_is_relative_to_the_tile() {
    // A tile occupying the bottom-right quadrant.
    let tile = NormRect::new(0.5, 0.5, 0.5, 0.5).to_region(1920, 1080);
    // tile origin (960, 540), size (960, 540)
    let size = BoxSize::new(100.0, 40.0);
    let placed = Anchor::TopLeft.place(tile, size, Padding::uniform(8.0));
    assert_px(placed.x, 960.0 + 8.0);
    assert_px(placed.y, 540.0 + 8.0);
}

#[test]
fn box_larger_than_region_clamps_origin_to_region_start() {
    // A box wider/taller than the region must not produce a negative origin
    // that escapes the region on the near edge; it pins to the region origin.
    let region = Region::from_pixels(100.0, 100.0, 50.0, 50.0);
    let size = BoxSize::new(200.0, 200.0);
    let placed = Anchor::BottomRight.place(region, size, Padding::uniform(4.0));
    // Far-edge math would give a negative offset; clamp to region origin.
    assert_px(placed.x, 100.0);
    assert_px(placed.y, 100.0);
}

#[test]
fn padding_uniform_sets_all_sides() {
    let p = Padding::uniform(12.0);
    assert_px(p.top, 12.0);
    assert_px(p.right, 12.0);
    assert_px(p.bottom, 12.0);
    assert_px(p.left, 12.0);
}

#[test]
fn pixel_rect_right_and_bottom_edges() {
    let r = PixelRect {
        x: 10.0,
        y: 20.0,
        width: 100.0,
        height: 50.0,
    };
    assert_px(r.right(), 110.0);
    assert_px(r.bottom(), 70.0);
}

#[test]
fn norm_rect_rejects_out_of_range_and_accepts_valid() {
    assert!(NormRect::FULL.validate().is_ok());
    assert!(NormRect::new(0.0, 0.0, 1.0, 1.0).validate().is_ok());
    // Out of [0,1].
    assert!(NormRect::new(-0.1, 0.0, 0.5, 0.5).validate().is_err());
    // Exceeds the right edge.
    assert!(NormRect::new(0.6, 0.0, 0.5, 0.5).validate().is_err());
    // Non-positive extent.
    assert!(NormRect::new(0.0, 0.0, 0.0, 0.5).validate().is_err());
    // Non-finite.
    assert!(NormRect::new(f32::NAN, 0.0, 0.5, 0.5).validate().is_err());
}
