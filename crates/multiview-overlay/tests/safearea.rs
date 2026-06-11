//! Safe-area / title-safe / action-safe / center-cross graticule geometry per
//! SMPTE ST 2046-1: action-safe is 93 % and title-safe 90 % of the Production
//! Aperture, centred. The rectangles are computed at known canvas sizes and
//! checked to the exact pixel.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_overlay::geometry::PixelRect;
use multiview_overlay::resolve::CanvasSize;
use multiview_overlay::safearea::{SafeAreaKind, SafeAreaMarkers, SafeAreaStyle};

/// Assert two pixel values are equal within a sub-pixel tolerance.
fn assert_px(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "expected {expected}, got {actual}"
    );
}

fn assert_rect(actual: PixelRect, x: f32, y: f32, w: f32, h: f32) {
    assert_px(actual.x, x);
    assert_px(actual.y, y);
    assert_px(actual.width, w);
    assert_px(actual.height, h);
}

#[test]
fn action_safe_is_93_percent_centred_on_1920x1080() {
    // ST 2046-1: action-safe = 93 % of the Production Aperture, centred.
    let rect = SafeAreaKind::ActionSafe.rect(CanvasSize::new(1920, 1080));
    // 93 % of 1920 = 1785.6; inset each side by (1920 - 1785.6)/2 = 67.2.
    assert_rect(rect, 67.2, 37.8, 1785.6, 1004.4);
}

#[test]
fn title_safe_is_90_percent_centred_on_1920x1080() {
    // ST 2046-1: title-safe = 90 % of the Production Aperture, centred.
    let rect = SafeAreaKind::TitleSafe.rect(CanvasSize::new(1920, 1080));
    // 90 % of 1920 = 1728; inset each side by 96. 90 % of 1080 = 972; inset 54.
    assert_rect(rect, 96.0, 54.0, 1728.0, 972.0);
}

#[test]
fn fractions_are_exactly_0_93_and_0_90() {
    assert_px(SafeAreaKind::ActionSafe.fraction(), 0.93);
    assert_px(SafeAreaKind::TitleSafe.fraction(), 0.90);
}

#[test]
fn rects_are_concentric_action_safe_contains_title_safe() {
    let canvas = CanvasSize::new(1280, 720);
    let action = SafeAreaKind::ActionSafe.rect(canvas);
    let title = SafeAreaKind::TitleSafe.rect(canvas);
    // Both centred on the same point.
    let action_cx = action.x + action.width / 2.0;
    let title_cx = title.x + title.width / 2.0;
    assert_px(action_cx, title_cx);
    assert_px(action_cx, 640.0);
    // Title-safe nests strictly inside action-safe.
    assert!(title.x > action.x);
    assert!(title.right() < action.right());
    assert!(title.y > action.y);
    assert!(title.bottom() < action.bottom());
}

#[test]
fn center_cross_is_at_the_geometric_centre() {
    let canvas = CanvasSize::new(1920, 1080);
    let markers = SafeAreaMarkers::default().with_center_cross(true);
    let model = markers.resolve(canvas);
    let cross = model.center_cross.expect("center cross enabled");
    assert_px(cross.x, 960.0);
    assert_px(cross.y, 540.0);
}

#[test]
fn disabled_kinds_are_absent_from_the_model() {
    let canvas = CanvasSize::new(1920, 1080);
    // Default markers draw nothing until enabled.
    let model = SafeAreaMarkers::default().resolve(canvas);
    assert!(model.rects.is_empty());
    assert!(model.center_cross.is_none());
}

#[test]
fn enabled_kinds_appear_in_the_model() {
    let canvas = CanvasSize::new(1920, 1080);
    let markers = SafeAreaMarkers::default()
        .with_kind(SafeAreaKind::ActionSafe, true)
        .with_kind(SafeAreaKind::TitleSafe, true);
    let model = markers.resolve(canvas);
    assert_eq!(model.rects.len(), 2);
    // Each resolved rect carries its kind so the renderer can label it (a11y:
    // meaning conveyed by text/glyph, not colour alone).
    let kinds: Vec<SafeAreaKind> = model.rects.iter().map(|r| r.kind).collect();
    assert!(kinds.contains(&SafeAreaKind::ActionSafe));
    assert!(kinds.contains(&SafeAreaKind::TitleSafe));
}

#[test]
fn style_round_trips_through_json_tagged() {
    let style = SafeAreaStyle::default();
    let json = serde_json::to_string(&style).unwrap();
    let back: SafeAreaStyle = serde_json::from_str(&json).unwrap();
    assert_eq!(style, back);
}

#[test]
fn label_is_descriptive_text_not_colour() {
    // a11y: the marker must convey meaning beyond colour.
    assert_eq!(SafeAreaKind::ActionSafe.label(), "action-safe");
    assert_eq!(SafeAreaKind::TitleSafe.label(), "title-safe");
}
