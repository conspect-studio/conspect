//! Tests for the multi-region tally border render model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use multiview_overlay::geometry::PixelRect;
use multiview_overlay::tally::{TallyBorder, TallyRegion, TallyStyle};

/// A tile rectangle used throughout: 200 wide, 100 tall at the origin.
fn tile() -> PixelRect {
    PixelRect {
        x: 0.0,
        y: 0.0,
        width: 200.0,
        height: 100.0,
    }
}

/// Compare two pixel values with a small tolerance. The geometry is exact
/// integer-valued `f32` arithmetic, but a tolerance keeps the assertion robust
/// and avoids relying on bit-exact float equality.
fn approx(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn left_region_is_a_vertical_strip_on_the_left_edge() {
    let style = TallyStyle {
        border_px: 10.0,
        ..TallyStyle::default()
    };
    let lh = TallyState::program();
    let border = TallyBorder::new(style)
        .with_region(TallyRegion::Left, lh)
        .resolve(tile());

    let strip = border
        .rect_for(TallyRegion::Left)
        .expect("left region present");
    approx(strip.x, 0.0);
    approx(strip.y, 0.0);
    approx(strip.width, 10.0);
    approx(strip.height, 100.0); // spans the full tile height
}

#[test]
fn right_region_is_a_vertical_strip_on_the_right_edge() {
    let style = TallyStyle {
        border_px: 12.0,
        ..TallyStyle::default()
    };
    let border = TallyBorder::new(style)
        .with_region(TallyRegion::Right, TallyState::preview())
        .resolve(tile());

    let strip = border
        .rect_for(TallyRegion::Right)
        .expect("right region present");
    approx(strip.x, 200.0 - 12.0);
    approx(strip.width, 12.0);
    approx(strip.height, 100.0);
}

#[test]
fn text_region_is_a_bottom_strip_spanning_full_width() {
    let style = TallyStyle {
        border_px: 8.0,
        text_band_px: 20.0,
    };
    let border = TallyBorder::new(style)
        .with_region(
            TallyRegion::Text,
            TallyState {
                color: TallyColor::Amber,
                brightness: Brightness::FULL,
                source: BusSource::Aux { index: 0 },
            },
        )
        .resolve(tile());

    let band = border
        .rect_for(TallyRegion::Text)
        .expect("text region present");
    approx(band.x, 0.0);
    approx(band.width, 200.0); // text band spans the full width
    approx(band.height, 20.0);
    approx(band.y, 100.0 - 20.0); // anchored to the bottom edge
}

#[test]
fn only_lit_regions_resolve_to_rects() {
    // An Off state must not produce a drawn strip.
    let border = TallyBorder::new(TallyStyle::default())
        .with_region(
            TallyRegion::Left,
            TallyState {
                color: TallyColor::Off,
                brightness: Brightness::FULL,
                source: BusSource::Program,
            },
        )
        .resolve(tile());
    assert!(
        border.rect_for(TallyRegion::Left).is_none(),
        "an off lamp draws no strip"
    );
    assert!(border.regions().is_empty());
}

#[test]
fn region_color_maps_from_tally_color_with_brightness() {
    let style = TallyStyle::default();
    let full = TallyState::program();
    let dim = TallyState {
        color: TallyColor::Red,
        brightness: Brightness::DIM,
        source: BusSource::Program,
    };

    let bright = TallyBorder::new(style)
        .with_region(TallyRegion::Left, full)
        .resolve(tile());
    let dimmed = TallyBorder::new(style)
        .with_region(TallyRegion::Left, dim)
        .resolve(tile());

    let c_bright = bright.color_for(TallyRegion::Left).expect("lit");
    let c_dim = dimmed.color_for(TallyRegion::Left).expect("lit");

    // Same hue, but the dim variant is overall darker: strictly lower total
    // luminance, and no channel brighter than the full-brightness variant.
    let sum_bright: f32 = c_bright[0..3].iter().sum();
    let sum_dim: f32 = c_dim[0..3].iter().sum();
    assert!(
        sum_dim < sum_bright,
        "dim must be overall darker: {c_dim:?} vs {c_bright:?}"
    );
    for ch in 0..3 {
        assert!(
            c_dim[ch] <= c_bright[ch],
            "dim channel {ch} must not be brighter: {c_dim:?} vs {c_bright:?}"
        );
    }
    // The lit (red) channel is strictly dimmer at lower brightness.
    assert!(c_dim[0] < c_bright[0], "red channel must dim");
    // Red dominant for a red tally.
    assert!(c_bright[0] > c_bright[1]);
    assert!(c_bright[0] > c_bright[2]);
}

#[test]
fn green_and_amber_are_distinguishable_hues() {
    let style = TallyStyle::default();
    let green = TallyBorder::new(style)
        .with_region(TallyRegion::Left, TallyState::preview())
        .resolve(tile())
        .color_for(TallyRegion::Left)
        .expect("lit");
    let amber = TallyBorder::new(style)
        .with_region(
            TallyRegion::Left,
            TallyState {
                color: TallyColor::Amber,
                brightness: Brightness::FULL,
                source: BusSource::Program,
            },
        )
        .resolve(tile())
        .color_for(TallyRegion::Left)
        .expect("lit");
    // Green has no red; amber has strong red. They are not the same colour:
    // at least one channel differs noticeably.
    assert!(green[0] < amber[0]);
    let differs = (0..4).any(|ch| (green[ch] - amber[ch]).abs() > 1e-3);
    assert!(differs, "green {green:?} and amber {amber:?} must differ");
}

#[test]
fn accessibility_label_conveys_state_as_text_not_colour_alone() {
    // Each region exposes a text label naming colour + bus, so the tally state
    // is readable without relying on colour (a11y).
    let border = TallyBorder::new(TallyStyle::default())
        .with_region(TallyRegion::Left, TallyState::program())
        .with_region(TallyRegion::Right, TallyState::preview())
        .resolve(tile());

    let left = border.label_for(TallyRegion::Left).expect("lit");
    let right = border.label_for(TallyRegion::Right).expect("lit");
    assert!(left.to_lowercase().contains("red"));
    assert!(left.to_lowercase().contains("program"));
    assert!(right.to_lowercase().contains("green"));
    assert!(right.to_lowercase().contains("preview"));
}

#[test]
fn left_and_right_strips_do_not_overlap_text_band() {
    let style = TallyStyle {
        border_px: 10.0,
        text_band_px: 20.0,
    };
    let border = TallyBorder::new(style)
        .with_region(TallyRegion::Left, TallyState::program())
        .with_region(TallyRegion::Right, TallyState::preview())
        .with_region(
            TallyRegion::Text,
            TallyState {
                color: TallyColor::Amber,
                brightness: Brightness::FULL,
                source: BusSource::Iso { index: 1 },
            },
        )
        .resolve(tile());

    let left = border.rect_for(TallyRegion::Left).expect("lit");
    let text = border.rect_for(TallyRegion::Text).expect("lit");
    // The side strips stop above the text band so they don't paint over it.
    assert!(
        left.bottom() <= text.y,
        "left strip {left:?} must end above text band {text:?}"
    );
}

#[test]
fn regions_iterate_in_stable_order() {
    let border = TallyBorder::new(TallyStyle::default())
        .with_region(TallyRegion::Text, TallyState::program())
        .with_region(TallyRegion::Left, TallyState::preview())
        .with_region(TallyRegion::Right, TallyState::program())
        .resolve(tile());
    let order: Vec<TallyRegion> = border.regions().iter().map(|r| r.region).collect();
    assert_eq!(
        order,
        vec![TallyRegion::Left, TallyRegion::Right, TallyRegion::Text]
    );
}

#[test]
fn serde_round_trips_tally_style_tagged() {
    let style = TallyStyle::default();
    let json = serde_json::to_string(&style).expect("serialize");
    let back: TallyStyle = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(style, back);
}
