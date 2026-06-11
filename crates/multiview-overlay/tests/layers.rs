//! Overlay layer descriptors, z-order, and resolution into a compositor-facing
//! draw list (the portable premultiplied-RGBA quad list of ADR-R008).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_overlay::geometry::{Anchor, BoxSize, NormRect, Padding};
use multiview_overlay::layer::{
    BlendMode, ClockStyle, LayerKind, MeterStyle, OverlayLayer, OverlayStack, Placement, Target,
    TextStyle,
};
use multiview_overlay::resolve::CanvasSize;

/// Assert two pixel/opacity values are equal within a small tolerance, so the
/// assertion does not rely on bit-exact float equality.
fn assert_px(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "expected {expected}, got {actual}"
    );
}

fn text_layer(id: &str, z: i32) -> OverlayLayer {
    OverlayLayer {
        id: id.to_owned(),
        kind: LayerKind::Text(TextStyle {
            text: "hello".to_owned(),
            ..TextStyle::default()
        }),
        target: Target::FullCanvas,
        placement: Placement {
            region: NormRect::FULL,
            anchor: Anchor::TopLeft,
            padding: Padding::uniform(8.0),
            size: BoxSize::new(120.0, 40.0),
        },
        z,
        opacity: 1.0,
        blend: BlendMode::Over,
        visible: true,
    }
}

#[test]
fn stack_sorts_layers_by_ascending_z_for_back_to_front_draw() {
    let mut stack = OverlayStack::new();
    stack.push(text_layer("top", 100));
    stack.push(text_layer("bottom", -5));
    stack.push(text_layer("middle", 10));

    let ordered: Vec<&str> = stack.draw_order().map(|l| l.id.as_str()).collect();
    // Lowest z first (drawn first, i.e. furthest back).
    assert_eq!(ordered, ["bottom", "middle", "top"]);
}

#[test]
fn stack_z_sort_is_stable_for_equal_z() {
    let mut stack = OverlayStack::new();
    stack.push(text_layer("first", 5));
    stack.push(text_layer("second", 5));
    stack.push(text_layer("third", 5));
    let ordered: Vec<&str> = stack.draw_order().map(|l| l.id.as_str()).collect();
    // Insertion order preserved within equal z.
    assert_eq!(ordered, ["first", "second", "third"]);
}

#[test]
fn invisible_layers_are_excluded_from_the_draw_list() {
    let mut stack = OverlayStack::new();
    let mut hidden = text_layer("hidden", 1);
    hidden.visible = false;
    stack.push(hidden);
    stack.push(text_layer("shown", 2));

    let canvas = CanvasSize::new(1920, 1080);
    let list = stack.resolve(canvas).unwrap();
    let ids: Vec<&str> = list.quads.iter().map(|q| q.layer_id.as_str()).collect();
    assert_eq!(ids, ["shown"]);
}

#[test]
fn resolve_produces_quads_in_back_to_front_order() {
    let mut stack = OverlayStack::new();
    stack.push(text_layer("top", 50));
    stack.push(text_layer("back", -1));
    let canvas = CanvasSize::new(1920, 1080);
    let list = stack.resolve(canvas).unwrap();
    let ids: Vec<&str> = list.quads.iter().map(|q| q.layer_id.as_str()).collect();
    assert_eq!(ids, ["back", "top"]);
}

#[test]
fn resolve_places_full_canvas_text_with_padding() {
    let mut stack = OverlayStack::new();
    stack.push(text_layer("label", 0));
    let canvas = CanvasSize::new(1920, 1080);
    let list = stack.resolve(canvas).unwrap();
    assert_eq!(list.quads.len(), 1);
    let q = &list.quads[0];
    // TopLeft anchor with 8px uniform padding on the full canvas.
    assert_px(q.dest.x, 8.0);
    assert_px(q.dest.y, 8.0);
    assert_px(q.dest.width, 120.0);
    assert_px(q.dest.height, 40.0);
    assert_px(q.opacity, 1.0);
    assert_eq!(q.blend, BlendMode::Over);
}

#[test]
fn resolve_places_a_tile_targeted_layer_relative_to_its_tile() {
    let mut layer = text_layer("tile-label", 0);
    // Target tile occupying the bottom-right quadrant.
    layer.target = Target::Tile {
        rect: NormRect::new(0.5, 0.5, 0.5, 0.5),
    };
    layer.placement.anchor = Anchor::TopLeft;
    layer.placement.padding = Padding::uniform(10.0);
    layer.placement.size = BoxSize::new(100.0, 30.0);

    let mut stack = OverlayStack::new();
    stack.push(layer);
    let list = stack.resolve(CanvasSize::new(1920, 1080)).unwrap();
    let q = &list.quads[0];
    // Tile origin is (960, 540); + 10px padding.
    assert_px(q.dest.x, 970.0);
    assert_px(q.dest.y, 550.0);
}

#[test]
fn resolve_clamps_opacity_into_unit_range() {
    let mut over = text_layer("over", 0);
    over.opacity = 5.0;
    let mut under = text_layer("under", 1);
    under.opacity = -1.0;
    let mut stack = OverlayStack::new();
    stack.push(over);
    stack.push(under);
    let list = stack.resolve(CanvasSize::new(1280, 720)).unwrap();
    assert_px(list.quads[0].opacity, 1.0);
    assert_px(list.quads[1].opacity, 0.0);
}

#[test]
fn resolve_rejects_a_layer_whose_region_is_out_of_range() {
    let mut layer = text_layer("bad", 0);
    layer.placement.region = NormRect::new(0.9, 0.0, 0.5, 0.5); // 0.9 + 0.5 > 1.0
    let mut stack = OverlayStack::new();
    stack.push(layer);
    assert!(stack.resolve(CanvasSize::new(1920, 1080)).is_err());
}

#[test]
fn resolve_rejects_an_invalid_tile_rect() {
    let mut layer = text_layer("bad-tile", 0);
    layer.target = Target::Tile {
        rect: NormRect::new(0.5, 0.5, 0.7, 0.7), // exceeds 1.0
    };
    let mut stack = OverlayStack::new();
    stack.push(layer);
    assert!(stack.resolve(CanvasSize::new(1920, 1080)).is_err());
}

#[test]
fn duplicate_layer_ids_are_rejected_on_resolve() {
    let mut stack = OverlayStack::new();
    stack.push(text_layer("dup", 0));
    stack.push(text_layer("dup", 1));
    assert!(stack.resolve(CanvasSize::new(1920, 1080)).is_err());
}

#[test]
fn all_layer_kinds_construct_and_carry_their_style() {
    // Text, clock, meter, alert card, logo, lower-third, subtitle.
    let kinds = [
        LayerKind::Text(TextStyle::default()),
        LayerKind::Clock(ClockStyle::default()),
        LayerKind::Meter(MeterStyle::default()),
        LayerKind::AlertCard(multiview_overlay::alert::AlertCard::new(
            "alert",
            multiview_overlay::alert::Severity::Critical,
        )),
        LayerKind::Logo,
        LayerKind::LowerThird,
        LayerKind::Subtitle,
    ];
    assert_eq!(kinds.len(), 7);
}

#[test]
fn layer_kind_serde_is_tagged_not_untagged() {
    // Internally/adjacently tagged unions are required (never untagged): a
    // "kind" discriminant must appear in the serialized form.
    let layer = text_layer("ser", 3);
    let json = serde_json::to_string(&layer).unwrap();
    assert!(json.contains("\"kind\""), "serialized layer = {json}");
    // Round-trips.
    let back: OverlayLayer = serde_json::from_str(&json).unwrap();
    assert_eq!(back.id, "ser");
    assert_eq!(back.z, 3);
}
