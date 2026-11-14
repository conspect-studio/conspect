//! ADR-0112 §3: the closed-form wipe masks on the CPU reference path.
//!
//! For every pattern, at progress 0, mid, 1/2 ("switch point" of a wipe: the
//! edge half way) and 1, the composite of an outgoing black tile under an
//! incoming white tile drawn through the mask equals, pixel for pixel, the
//! expected mask computed independently here from the pattern's geometry.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names
)]
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::mask::{MaskShape, WipeMask};
use multiview_compositor::pipeline::{composite, CanvasColor, Nv12Image, Tile};
use multiview_core::color::ColorInfo;

const W: u32 = 64;
const H: u32 = 36;
const SHAPES: [MaskShape; 5] = [
    MaskShape::Horizontal,
    MaskShape::Vertical,
    MaskShape::Diagonal,
    MaskShape::Box,
    MaskShape::Circle,
];

fn solid(y: u8) -> Nv12Image {
    Nv12Image::solid(
        W,
        H,
        y,
        128,
        128,
        ColorInfo::default().resolve_defaults(W, H),
    )
    .unwrap()
}

fn render(mask: WipeMask) -> Nv12Image {
    let (black, white) = (solid(16), solid(235));
    composite(
        W,
        H,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &[
            Tile::scaled(&black, 0, 0, W, H, 1.0),
            Tile::scaled(&white, 0, 0, W, H, 1.0).with_mask(mask),
        ],
    )
    .unwrap()
}

/// Independent geometry (f64): is pixel (x, y) revealed at `progress` for a
/// hard edge with no border, centre origin, no reverse? `None` for a pixel
/// centre within 1e-4 of the edge, which f32 may round to either side.
fn expected(shape: MaskShape, progress: f64, x: u32, y: u32) -> Option<bool> {
    let a = f64::from(W) / f64::from(H);
    let u = (f64::from(x) + 0.5) / f64::from(W);
    let v = (f64::from(y) + 0.5) / f64::from(H);
    let (f, extent) = match shape {
        MaskShape::Horizontal => (u * a, a),
        MaskShape::Vertical => (v, 1.0),
        MaskShape::Diagonal => ((u * a + v) / 2_f64.sqrt(), (a + 1.0) / 2_f64.sqrt()),
        MaskShape::Box => ((u - 0.5).abs().max((v - 0.5).abs()), 0.5),
        MaskShape::Circle => {
            let (dx, dy) = ((u - 0.5) * a, v - 0.5);
            ((dx * dx + dy * dy).sqrt(), (0.25 * a * a + 0.25).sqrt())
        }
    };
    let edge = progress * extent;
    ((f - edge).abs() > 1e-4).then_some(f <= edge)
}

fn revealed(image: &Nv12Image, x: u32, y: u32) -> bool {
    image.sample(x, y).unwrap().0 > 128
}

#[test]
fn every_pattern_matches_its_geometry_at_start_mid_half_and_end() {
    for shape in SHAPES {
        for progress in [0.0_f32, 0.25, 0.5, 1.0] {
            let mask = WipeMask::new(shape, progress, W, H, [0.5, 0.5], false, 0.0, 0.0);
            let image = render(mask);
            let mut wrong = Vec::new();
            for y in 0..H {
                for x in 0..W {
                    if let Some(want) = expected(shape, f64::from(progress), x, y) {
                        if revealed(&image, x, y) != want {
                            wrong.push((x, y));
                        }
                    }
                }
            }
            assert!(
                wrong.is_empty(),
                "{shape:?} at {progress}: {} pixels differ, e.g. {:?}",
                wrong.len(),
                &wrong[..wrong.len().min(4)]
            );
            let count = (0..H)
                .flat_map(|y| (0..W).map(move |x| (x, y)))
                .filter(|&(x, y)| revealed(&image, x, y))
                .count();
            if progress == 0.0 {
                assert_eq!(count, 0, "{shape:?}: nothing at the start");
            }
            if progress == 1.0 {
                assert_eq!(count, (W * H) as usize, "{shape:?}: everything at the end");
            }
        }
    }
}

#[test]
fn coverage_grows_monotonically_and_reverse_mirrors_it() {
    for shape in SHAPES {
        let mut previous = 0;
        for step in 0..=20 {
            let p = step as f32 / 20.0;
            let image = render(WipeMask::new(shape, p, W, H, [0.5, 0.5], false, 0.1, 0.0));
            let sum: u32 = image.y_plane().iter().map(|&y| u32::from(y)).sum();
            assert!(sum >= previous, "{shape:?} step {step}");
            previous = sum;
        }
    }
    // A reversed horizontal wipe starts at the right edge.
    let image = render(WipeMask::new(
        MaskShape::Horizontal,
        0.25,
        W,
        H,
        [0.5, 0.5],
        true,
        0.0,
        0.0,
    ));
    assert!(revealed(&image, W - 1, 10) && !revealed(&image, 0, 10));
    // A reversed iris closes in from the corners.
    let image = render(WipeMask::new(
        MaskShape::Circle,
        0.25,
        W,
        H,
        [0.5, 0.5],
        true,
        0.0,
        0.0,
    ));
    assert!(revealed(&image, 0, 0) && !revealed(&image, W / 2, H / 2));
}

#[test]
fn a_soft_edge_ramps_and_a_hard_edge_does_not() {
    let hard = render(WipeMask::new(
        MaskShape::Horizontal,
        0.5,
        W,
        H,
        [0.5, 0.5],
        false,
        0.0,
        0.0,
    ));
    let soft = render(WipeMask::new(
        MaskShape::Horizontal,
        0.5,
        W,
        H,
        [0.5, 0.5],
        false,
        0.2,
        0.0,
    ));
    let row = |img: &Nv12Image| {
        (0..W)
            .map(|x| img.sample(x, 10).unwrap().0)
            .collect::<Vec<_>>()
    };
    let (hard, soft) = (row(&hard), row(&soft));
    assert!(hard.iter().all(|&y| y == 16 || y == 235), "{hard:?}");
    let ramp = soft.iter().filter(|&&y| y > 20 && y < 230).count();
    assert!(
        ramp >= 6,
        "a 0.2-height soft edge spans several pixels: {soft:?}"
    );
    assert!(
        soft.windows(2).all(|w| w[1] <= w[0]),
        "monotone ramp: {soft:?}"
    );
}

#[test]
fn degenerate_parameters_never_panic_and_stay_in_range() {
    for shape in SHAPES {
        for &(p, soft, border) in &[
            (f32::NAN, f32::NAN, f32::NAN),
            (f32::INFINITY, -1.0, 9.0),
            (-3.0, f32::INFINITY, -2.0),
        ] {
            let mask = WipeMask::new(shape, p, W, H, [f32::NAN, 7.0], false, soft, border);
            for (x, y) in [(0, 0), (W - 1, H - 1), (W / 2, H / 2)] {
                let c = mask.coverage(x, y);
                assert!((0.0..=1.0).contains(&c), "{shape:?} {c}");
            }
        }
    }
}
