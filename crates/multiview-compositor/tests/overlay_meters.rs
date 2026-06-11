//! Integration tests for the audio meters/scopes draw-data (Stage 3, ADR-0016
//! §4.2: "meters are geometry, not pictures"). REAL geometry + pixel assertions:
//! a −6 dBFS reading on a default `−60…0 dBFS` scale fills exactly 90 % of the
//! track height; the meter draws a live fill + a peak-hold tick; a goniometer
//! dot lands at the box centre for a mono signal; a histogram's tallest bin
//! fills the box.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_compositor::overlay::meters::{
    goniometer, histogram, GonioDot, MeterBar, MeterScale,
};
use multiview_compositor::overlay::subpass::{
    blend_overlays, LinearCanvasBuffer, OverlayColor, OverlayDrawList, OverlayPrimitive,
    OverlayRect,
};

const GREEN: OverlayColor = OverlayColor::opaque(0.0, 1.0, 0.0);
const WHITE: OverlayColor = OverlayColor::opaque(1.0, 1.0, 1.0);

#[test]
fn minus_six_dbfs_fills_ninety_percent_of_a_vertical_track() {
    // Default scale is −60..0 dBFS over the full track. −6 dBFS is
    // (−6 − −60) / (0 − −60) = 54/60 = 0.9 of the track.
    let scale = MeterScale::default();
    assert!((scale.deflection(-6.0) - 0.9).abs() < 1e-6, "−6 dBFS → 0.9");
    assert_eq!(scale.deflection(0.0), 1.0, "0 dBFS fills the track");
    assert_eq!(scale.deflection(-60.0), 0.0, "−60 dBFS reads empty");
    assert_eq!(scale.deflection(-120.0), 0.0, "below floor clamps to empty");

    // A 100px-tall vertical track at −6 dBFS fills the TOP 90px (vertical meters
    // fill bottom→up, so the filled rect's top is at 10px from the track top).
    let mut bar = MeterBar::new(scale);
    bar.observe_db(-6.0);
    assert!((bar.level() - 0.9).abs() < 1e-6);

    let track = OverlayRect::new(4, 0, 8, 100);
    let prims = bar.primitives(track, true, GREEN, WHITE);
    // background + fill + peak tick.
    assert_eq!(prims.len(), 3, "background, fill, peak tick");

    // The live fill is the 2nd primitive: a bottom-anchored 90px-tall rect.
    let fill = match &prims[1] {
        OverlayPrimitive::FilledRect { rect, .. } => *rect,
        other => panic!("expected a filled rect, got {other:?}"),
    };
    assert_eq!(fill.height, 90, "−6 dBFS fills 90 of 100 px");
    assert_eq!(fill.y, 10, "fill is anchored at the bottom of the track");
    assert_eq!(fill.x, track.x);
    assert_eq!(fill.width, track.width);
}

#[test]
fn meter_fill_inks_the_canvas_to_the_expected_height() {
    // Blend the meter into a real linear canvas and count the inked column rows
    // of the live fill — proving the geometry reaches actual pixels.
    let scale = MeterScale::default();
    let mut bar = MeterBar::new(scale);
    bar.observe_db(-6.0);

    let track = OverlayRect::new(2, 0, 4, 100);
    let mut list = OverlayDrawList::new();
    bar.push_into(&mut list, track, true, GREEN, WHITE);

    let mut canvas = LinearCanvasBuffer::transparent(8, 100);
    blend_overlays(&mut canvas, &list);

    // Column x=3 is inside the bar. The opaque region (live fill plus the 1px
    // white peak-hold tick that sits on the fill's top row) is the bottom 90
    // rows (y in 10..100); above it only the dim background.
    let opaque_rows = (0..100)
        .filter(|&y| canvas.pixel(3, y).unwrap().a > 0.5)
        .count();
    assert_eq!(opaque_rows, 90, "the bar reaches exactly 90 of 100 rows");

    // Of those, the green live-fill rows are the bottom 89; the topmost opaque
    // row (y=10) is the white peak-hold tick (peak == live level here).
    let green_rows = (0..100)
        .filter(|&y| {
            let p = canvas.pixel(3, y).unwrap();
            p.a > 0.5 && (p.g - p.a).abs() < 1e-6 && p.r.abs() < 1e-6
        })
        .count();
    assert_eq!(green_rows, 89, "live green fill under the peak tick");
    let tick = canvas.pixel(3, 10).unwrap();
    assert!(
        (tick.r - tick.a).abs() < 1e-6 && (tick.g - tick.a).abs() < 1e-6,
        "the peak-hold tick at the top of the fill is white"
    );

    // The topmost row (y=0) is only the dim background, not the live fill.
    let top = canvas.pixel(3, 0).unwrap();
    assert!(top.a > 0.0 && top.a < 0.5, "headroom shows dim background");
}

#[test]
fn horizontal_meter_fills_left_to_right() {
    let scale = MeterScale::new(-60.0, 0.0);
    let mut bar = MeterBar::new(scale);
    bar.observe_db(-30.0); // 30/60 = 0.5
    assert!((bar.level() - 0.5).abs() < 1e-6);

    let track = OverlayRect::new(0, 0, 100, 6);
    let prims = bar.primitives(track, false, GREEN, WHITE);
    let fill = match &prims[1] {
        OverlayPrimitive::FilledRect { rect, .. } => *rect,
        other => panic!("expected a filled rect, got {other:?}"),
    };
    assert_eq!(fill.x, 0, "horizontal fill starts at the left");
    assert_eq!(fill.width, 50, "−30 dBFS fills half the 100px track");
}

#[test]
fn peak_hold_rises_then_decays_to_the_live_level() {
    let mut bar = MeterBar::new(MeterScale::default());
    bar.observe_db(-6.0); // level 0.9
    let peak_high = bar.peak();
    assert!((peak_high - 0.9).abs() < 1e-6);

    // A quieter reading drops the live level but the peak holds.
    bar.observe_db(-36.0); // 24/60 = 0.4
    assert!((bar.level() - 0.4).abs() < 1e-6);
    assert!(
        (bar.peak() - 0.9).abs() < 1e-6,
        "peak holds above the live level"
    );

    // Decaying the peak slowly brings it down, but never below the live level.
    for _ in 0..100 {
        bar.decay_peak(0.01);
    }
    assert!(
        (bar.peak() - bar.level()).abs() < 1e-6,
        "peak decays down to the live level, never below"
    );
}

#[test]
fn goniometer_centres_a_mono_dot() {
    // A mono (in-phase) signal has zero side component (x=0) and a positive mid
    // (y>0). In the box it should land on the vertical centre line, above centre.
    let box_rect = OverlayRect::new(0, 0, 100, 100);
    let dots = [GonioDot { x: 0.0, y: 0.6 }];
    let prims = goniometer(box_rect, &dots, 2, WHITE);
    assert_eq!(prims.len(), 1);
    let rect = match &prims[0] {
        OverlayPrimitive::FilledRect { rect, .. } => *rect,
        other => panic!("expected a dot rect, got {other:?}"),
    };
    // Centre x = 50; the 2px dot is centred there (x = 49).
    assert_eq!(rect.x, 49, "mono dot sits on the vertical centre line");
    // y = centre(50) − 0.6*half(50) = 50 − 30 = 20, minus half the 2px dot.
    assert_eq!(rect.y, 19, "positive mid lands above centre");
}

#[test]
fn histogram_tallest_bin_fills_the_box() {
    // Bins [1, 4, 2, 0]: max is 4 → that column fills the full 40px box height;
    // a bin of 1 fills a quarter; a zero bin draws nothing.
    let box_rect = OverlayRect::new(0, 0, 40, 40);
    let bins = [1_u64, 4, 2, 0];
    let prims = histogram(box_rect, &bins, GREEN);
    // Three non-zero bins.
    assert_eq!(prims.len(), 3, "zero bins draw nothing");

    let heights: Vec<u32> = prims
        .iter()
        .map(|p| match p {
            OverlayPrimitive::FilledRect { rect, .. } => rect.height,
            other => panic!("expected rect, got {other:?}"),
        })
        .collect();
    assert!(heights.contains(&40), "the max bin fills the full height");
    assert!(heights.contains(&10), "a 1/4-count bin fills a quarter");
}
