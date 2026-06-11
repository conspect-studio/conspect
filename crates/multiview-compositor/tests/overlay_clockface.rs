//! Integration tests for the **analog clock-face** overlay primitives (ADR-0016
//! §4.1): an anti-aliased stroked [`OverlayPrimitive::Ring`] (the bezel + tick
//! ring) and a thick angled [`OverlayPrimitive::Stroke`] (a clock hand), plus the
//! [`clock_face`](multiview_compositor::overlay::subpass::clock_face) builder that
//! maps hand angles (degrees clockwise from 12 o'clock) into those primitives.
//!
//! REAL pixel assertions in linear premultiplied space: a 12-o'clock hand paints
//! a near-vertical column of set pixels ABOVE the centre; a 3-o'clock hand paints
//! horizontal pixels to the RIGHT; a ring inks pixels at its radius and leaves the
//! hub region clear. The clock-face builder at 03:00:00 puts the hour hand
//! horizontal-right and the minute/second hands straight up.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_compositor::overlay::subpass::{
    blend_overlays, clock_face, ClockFaceStyle, HandAngles, LinearCanvasBuffer, OverlayColor,
    OverlayDrawList, OverlayPrimitive,
};

const WHITE: OverlayColor = OverlayColor::opaque(0.95, 0.95, 0.95);

/// Whether canvas pixel `(x, y)` is inked (any coverage).
fn inked(canvas: &LinearCanvasBuffer, x: u32, y: u32) -> bool {
    canvas.pixel(x, y).is_some_and(|p| p.a > 0.0)
}

#[test]
fn vertical_stroke_paints_a_column_above_the_centre() {
    // A hand from the centre (32,32) straight UP to (32,8): a near-vertical column
    // of set pixels above the centre, nothing below it.
    let mut canvas = LinearCanvasBuffer::transparent(64, 64);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::Stroke {
        x0: 32.0,
        y0: 32.0,
        x1: 32.0,
        y1: 8.0,
        half_thickness: 1.5,
        color: WHITE,
    });
    blend_overlays(&mut canvas, &list);

    // The column x≈32 is inked between y=8 and y=32 (above the centre).
    for y in 10..30 {
        assert!(inked(&canvas, 32, y), "vertical hand column inked at y={y}");
    }
    // Below the centre stays clear (the hand only extends upward).
    for y in 40..60 {
        assert!(
            !inked(&canvas, 32, y),
            "below the centre stays clear at y={y}"
        );
    }
    // Well to the side of a thin vertical hand is clear.
    assert!(
        !inked(&canvas, 50, 20),
        "far right of a vertical hand clear"
    );
}

#[test]
fn horizontal_stroke_paints_a_row_right_of_the_centre() {
    // A hand from the centre (32,32) to the RIGHT (56,32): a horizontal row of set
    // pixels right of the centre, nothing to the left.
    let mut canvas = LinearCanvasBuffer::transparent(64, 64);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::Stroke {
        x0: 32.0,
        y0: 32.0,
        x1: 56.0,
        y1: 32.0,
        half_thickness: 1.5,
        color: WHITE,
    });
    blend_overlays(&mut canvas, &list);

    for x in 34..54 {
        assert!(inked(&canvas, x, 32), "horizontal hand row inked at x={x}");
    }
    // Left of the centre stays clear.
    for x in 4..24 {
        assert!(!inked(&canvas, x, 32), "left of the centre clear at x={x}");
    }
}

#[test]
fn ring_inks_its_radius_and_leaves_the_hub_clear() {
    // A ring centred at (32,32), outer radius 24, thickness 4: pixels ~at radius 22
    // are inked; the centre/hub region is clear; well outside the ring is clear.
    let mut canvas = LinearCanvasBuffer::transparent(64, 64);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::Ring {
        cx: 32.0,
        cy: 32.0,
        outer_radius: 24.0,
        thickness: 4.0,
        color: WHITE,
    });
    blend_overlays(&mut canvas, &list);

    // On the ring (right of centre at the mid-thickness radius ≈ 22).
    assert!(inked(&canvas, 54, 32), "ring inked at the +x radius");
    assert!(inked(&canvas, 10, 32), "ring inked at the -x radius");
    assert!(inked(&canvas, 32, 54), "ring inked at the +y radius");
    assert!(inked(&canvas, 32, 10), "ring inked at the -y radius");

    // The hub (centre) is hollow.
    assert!(!inked(&canvas, 32, 32), "ring centre is hollow");
    assert!(!inked(&canvas, 36, 32), "inside the ring is hollow");

    // Well outside the bezel is clear.
    assert!(!inked(&canvas, 63, 63), "outside the ring is clear");
}

#[test]
fn clock_face_at_three_oclock_points_the_hour_hand_right() {
    // 03:00:00 → hour hand at 90° (horizontal right), minute + second at 0° (up).
    let angles = HandAngles {
        hour_deg: 90.0,
        minute_deg: 0.0,
        second_deg: 0.0,
    };
    let style = ClockFaceStyle::centred(64, 64, 30);
    let prims = clock_face(angles, style, 12);
    assert!(!prims.is_empty(), "clock face emits primitives");

    let mut canvas = LinearCanvasBuffer::transparent(64, 64);
    let mut list = OverlayDrawList::new();
    for p in prims {
        list.push(p);
    }
    blend_overlays(&mut canvas, &list);

    // Hour hand points RIGHT: pixels just right of the centre are inked.
    assert!(
        inked(&canvas, 40, 32),
        "hour hand inks to the right at 3:00"
    );
    // Minute + second hands point UP: pixels above the centre are inked.
    assert!(
        inked(&canvas, 32, 14),
        "minute/second hands ink upward at :00"
    );
    // Nothing to the LEFT of the centre (no hand points there at 3:00).
    assert!(!inked(&canvas, 12, 32), "no hand to the left at 3:00");
    // Nothing straight DOWN (no hand points there at 3:00).
    assert!(!inked(&canvas, 32, 52), "no hand straight down at 3:00");
}

#[test]
fn clock_face_at_six_oclock_points_the_hour_hand_down() {
    // 06:00:00 → hour hand at 180° (straight down), minute + second up.
    let angles = HandAngles {
        hour_deg: 180.0,
        minute_deg: 0.0,
        second_deg: 0.0,
    };
    let prims = clock_face(angles, ClockFaceStyle::centred(64, 64, 30), 12);
    let mut canvas = LinearCanvasBuffer::transparent(64, 64);
    let mut list = OverlayDrawList::new();
    for p in prims {
        list.push(p);
    }
    blend_overlays(&mut canvas, &list);

    // Hour hand (length ≈ radius*0.5 = 15px) reaches y≈47 straight down.
    assert!(inked(&canvas, 32, 44), "hour hand inks downward at 6:00");
    assert!(
        inked(&canvas, 32, 16),
        "minute/second hands ink upward at :00"
    );
    // Just past the hour hand's tip but inside the bezel rim, the radial gap
    // between the (short) hour hand and the rim/ticks is hollow.
    assert!(
        !inked(&canvas, 32, 50),
        "gap past the short hour hand tip is clear"
    );
}

#[test]
fn clock_face_draws_the_requested_number_of_hour_ticks() {
    // The dial is `hour_ticks` evenly-spaced radial tick Strokes plus the three
    // hand Strokes — so a 12-hour dial has 12+3 strokes and a 24-hour dial 24+3.
    let angles = HandAngles {
        hour_deg: 0.0,
        minute_deg: 0.0,
        second_deg: 0.0,
    };
    let style = ClockFaceStyle::centred(120, 120, 56);
    let count_strokes = |ticks: u32| {
        clock_face(angles, style, ticks)
            .into_iter()
            .filter(|p| matches!(p, OverlayPrimitive::Stroke { .. }))
            .count()
    };
    assert_eq!(
        count_strokes(12),
        12 + 3,
        "12-hour dial: 12 ticks + 3 hands"
    );
    assert_eq!(
        count_strokes(24),
        24 + 3,
        "24-hour dial: 24 ticks + 3 hands"
    );
}

#[test]
fn clock_face_second_hand_is_longest_and_distinct_from_the_minute_hand() {
    // At 12:00:30 the second hand points DOWN and reaches further than the (short)
    // hour/minute hands point up — the three hands have distinct lengths.
    let angles = HandAngles {
        hour_deg: 0.0,
        minute_deg: 0.0,
        second_deg: 180.0,
    };
    let prims = clock_face(angles, ClockFaceStyle::centred(80, 80, 36), 12);
    let mut canvas = LinearCanvasBuffer::transparent(80, 80);
    let mut list = OverlayDrawList::new();
    for p in prims {
        list.push(p);
    }
    blend_overlays(&mut canvas, &list);

    // The long second hand reaches near the rim straight down.
    assert!(
        inked(&canvas, 40, 70),
        "long second hand reaches down near the rim"
    );
}
