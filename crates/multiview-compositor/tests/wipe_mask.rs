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

/// Review MED-01: progress 0 covers nothing and progress 1 covers everything
/// even when a box or circle origin falls exactly on a pixel centre, with a
/// border band (the expanded draw) and reversed.
#[test]
fn endpoints_are_exact_for_a_pixel_centre_origin_with_border_and_reverse() {
    let (w, h) = (128_u32, 64_u32);
    let origin = [64.5 / 128.0, 32.5 / 64.0];
    for shape in SHAPES {
        for reverse in [false, true] {
            for (soft, border) in [(0.0, 0.0), (0.0, 0.125), (0.1, 0.05)] {
                let start = WipeMask::new(shape, 0.0, w, h, origin, reverse, soft, border);
                let end = WipeMask::new(shape, 1.0, w, h, origin, reverse, soft, border);
                for y in 0..h {
                    for x in 0..w {
                        for (mask, want) in
                            [(start, 0.0), (start.expanded(border), 0.0), (end, 1.0)]
                        {
                            assert_eq!(
                                mask.coverage(x, y),
                                want,
                                "{shape:?} reverse={reverse} soft={soft} border={border} at ({x},{y})"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Review MED-02: soft edge and border widths are fractions of the canvas
/// height on every side of a box, on a non-square canvas, computed here from
/// pixel counts (not from the mask formula).
#[test]
fn box_border_and_soft_edge_have_the_same_pixel_width_on_every_side() {
    let (w, h) = (128_u32, 64_u32);
    let mask = WipeMask::new(MaskShape::Box, 0.4, w, h, [0.5, 0.5], false, 0.0, 0.125);
    let band = mask.expanded(0.125);
    let in_band = |x: u32, y: u32| band.coverage(x, y) == 1.0 && mask.coverage(x, y) == 0.0;
    let horizontal = (w / 2..w).filter(|&x| in_band(x, h / 2)).count();
    let vertical = (h / 2..h).filter(|&y| in_band(w / 2, y)).count();
    // 0.125 of a 64-pixel height is 8 pixels on each side.
    assert!((7..=9).contains(&horizontal), "side band {horizontal} px");
    assert!((7..=9).contains(&vertical), "top/bottom band {vertical} px");
    let soft = WipeMask::new(MaskShape::Box, 0.4, w, h, [0.5, 0.5], false, 0.25, 0.0);
    let ramp = |c: f32| c > 0.0 && c < 1.0;
    let horizontal = (w / 2..w)
        .filter(|&x| ramp(soft.coverage(x, h / 2)))
        .count();
    let vertical = (h / 2..h)
        .filter(|&y| ramp(soft.coverage(w / 2, y)))
        .count();
    // 0.25 of 64 is a 16-pixel ramp on each side.
    assert!((15..=17).contains(&horizontal), "side ramp {horizontal} px");
    assert!(
        (15..=17).contains(&vertical),
        "top/bottom ramp {vertical} px"
    );
    // A portrait canvas too.
    let (w, h) = (36_u32, 64_u32);
    let mask = WipeMask::new(MaskShape::Box, 0.5, w, h, [0.5, 0.5], false, 0.0, 0.125);
    let band = mask.expanded(0.125);
    let in_band = |x: u32, y: u32| band.coverage(x, y) == 1.0 && mask.coverage(x, y) == 0.0;
    let horizontal = (w / 2..w).filter(|&x| in_band(x, h / 2)).count();
    let vertical = (h / 2..h).filter(|&y| in_band(w / 2, y)).count();
    assert!(
        (7..=9).contains(&horizontal),
        "portrait side band {horizontal} px"
    );
    assert!(
        (7..=9).contains(&vertical),
        "portrait top/bottom band {vertical} px"
    );
    // And it starts empty with the band included.
    let start = WipeMask::new(MaskShape::Box, 0.0, w, h, [0.5, 0.5], false, 0.0, 0.125);
    assert!((0..h).all(|y| (0..w).all(|x| start.expanded(0.125).coverage(x, y) == 0.0)));
}

/// Review MED-03: the border band never covers the revealed interior, so a
/// missing, stale or transparent incoming picture leaves the outgoing one
/// visible there; with an opaque incoming picture on top the visible band is
/// exactly the strip the grown edge adds.
#[test]
fn border_band_never_covers_the_revealed_interior() {
    let (w, h) = (96_u32, 64_u32);
    for shape in SHAPES {
        for reverse in [false, true] {
            for soft in [0.0, 0.1] {
                for progress in [0.1_f32, 0.3, 0.5, 0.8, 0.95] {
                    let mask = WipeMask::new(shape, progress, w, h, [0.4, 0.6], reverse, soft, 0.1);
                    let band = mask.expanded(0.1);
                    let mut interior = 0;
                    let mut in_band = 0;
                    for y in 0..h {
                        for x in 0..w {
                            let c = mask.coverage(x, y);
                            let b = band.coverage(x, y);
                            assert!((0.0..=1.0).contains(&b));
                            // Under the soft ramp the band fades out as the
                            // incoming picture fades in (no hard inner edge).
                            assert!(
                                b <= 1.0 - c + 1e-6,
                                "{shape:?} rev={reverse} soft={soft} p={progress} ({x},{y}): band {b} incoming {c}"
                            );
                            if c >= 1.0 {
                                interior += 1;
                                assert_eq!(
                                    b, 0.0,
                                    "{shape:?} rev={reverse} soft={soft} p={progress} ({x},{y})"
                                );
                            } else if c <= 0.0 && b >= 0.5 {
                                in_band += 1;
                            }
                        }
                    }
                    if (0.3..=0.8).contains(&progress) {
                        // Non-vacuous mid-flight (the band leaves the canvas near the end): a revealed interior and a
                        // band (at least half strength outside the edge) both exist.
                        assert!(interior > 0, "{shape:?} p={progress}: nothing revealed");
                        assert!(
                            in_band > 0,
                            "{shape:?} rev={reverse} soft={soft} p={progress}: no band"
                        );
                    }
                }
            }
        }
    }
}

/// Review MED-03: a soft border narrower than the soft edge has no hard inner
/// cut-off. Along a horizontal wipe's travel axis the band's coverage changes
/// by at most a bounded step per pixel (the ramp slope), and its peak is
/// strictly inside (0, 1) where it overlaps the incoming ramp.
#[test]
fn a_soft_narrow_band_is_continuous_across_the_ramp() {
    let (w, h) = (256_u32, 64_u32);
    let (soft, border) = (0.2_f32, 0.02_f32);
    let mask = WipeMask::new(
        MaskShape::Horizontal,
        0.5,
        w,
        h,
        [0.5, 0.5],
        false,
        soft,
        border,
    );
    let band = mask.expanded(border);
    let row: Vec<f32> = (0..w).map(|x| band.coverage(x, h / 2)).collect();
    // One pixel is 1/64 of the height; the band is a product of two ramps of
    // slope 1/softness, so a step is at most about 2/(softness*h).
    let max_step = 2.0 / (soft * h as f32) + 1e-3;
    for pair in row.windows(2) {
        assert!(
            (pair[1] - pair[0]).abs() <= max_step,
            "jump {pair:?} > {max_step}"
        );
    }
    let peak = row.iter().copied().fold(0.0_f32, f32::max);
    assert!(peak > 0.0 && peak < 1.0, "peak {peak}");
}
