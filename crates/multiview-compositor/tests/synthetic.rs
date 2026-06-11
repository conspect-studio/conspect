//! Synthetic NV12 source constructors (ADR-0027): colour bars + solid-from-RGB.
//!
//! These are content-aware assertions — the **descending luma staircase** that
//! is the hallmark of correct 75 % colour bars, and brightness ordering for
//! solids — never a byte hash of a lossy buffer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::pipeline::{CanvasColor, Nv12Image};

#[test]
fn color_bars_have_the_descending_luma_staircase() {
    let canvas = CanvasColor::default();
    // 560 / 7 = 80 px per bar (both dimensions even for 4:2:0).
    let (w, h) = (560_u32, 240_u32);
    let img = Nv12Image::color_bars(w, h, canvas).expect("build colour bars");
    assert_eq!(img.width(), w);
    assert_eq!(img.height(), h);

    // Luma at the centre of each of the 7 bars. For 75 % bars the luma descends
    // strictly white > yellow > cyan > green > magenta > red > blue — that
    // ordering IS the correctness check (not the exact code values).
    let mid_y = h / 2;
    let lumas: Vec<u8> = (0..7_u32)
        .map(|k| {
            let cx = (k * w / 7) + (w / 14); // centre of bar k
            img.sample(cx, mid_y).expect("sample in-bounds").0
        })
        .collect();
    for win in lumas.windows(2) {
        assert!(
            win[0] > win[1],
            "75% bars luma must strictly descend across bars, got {lumas:?}"
        );
    }
    assert!(
        i32::from(lumas[0]) - i32::from(lumas[6]) > 60,
        "white..blue luma span too small: {lumas:?}"
    );
}

#[test]
fn color_bars_are_vertical_bands_constant_down_a_column() {
    let canvas = CanvasColor::default();
    let (w, h) = (560_u32, 240_u32);
    let img = Nv12Image::color_bars(w, h, canvas).expect("bars");
    // A bar is a vertical band: the luma at a column is the same top and bottom.
    let cx = w / 14; // inside bar 0
    assert_eq!(
        img.sample(cx, 0).unwrap().0,
        img.sample(cx, h - 1).unwrap().0,
        "a colour bar must be constant down its column"
    );
}

#[test]
fn solid_rgb_is_uniform_and_orders_by_brightness() {
    let canvas = CanvasColor::default();
    let white = Nv12Image::solid_rgb(64, 64, 255, 255, 255, canvas).expect("white");
    let black = Nv12Image::solid_rgb(64, 64, 0, 0, 0, canvas).expect("black");
    let red = Nv12Image::solid_rgb(64, 64, 255, 0, 0, canvas).expect("red");

    // Uniform: every sampled pixel equals the top-left one.
    let tl = white.sample(0, 0).unwrap();
    for (x, y) in [(10_u32, 10_u32), (63, 63), (31, 0), (0, 63)] {
        assert_eq!(
            white.sample(x, y).unwrap(),
            tl,
            "a solid source must be spatially uniform"
        );
    }
    // Luma orders white > red > black.
    assert!(white.sample(0, 0).unwrap().0 > red.sample(0, 0).unwrap().0);
    assert!(red.sample(0, 0).unwrap().0 > black.sample(0, 0).unwrap().0);
}

#[test]
fn synthetic_constructors_reject_odd_dimensions() {
    let canvas = CanvasColor::default();
    assert!(Nv12Image::color_bars(7, 8, canvas).is_err(), "odd width");
    assert!(
        Nv12Image::solid_rgb(8, 7, 1, 2, 3, canvas).is_err(),
        "odd height"
    );
}
