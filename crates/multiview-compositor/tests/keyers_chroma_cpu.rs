//! Keyers K7 (#392, ADR-C007 §3): the CPU oracle for the chroma key. The
//! matte is `clamp((d − clip) × gain, 0, 1)` on the Euclidean distance, in the
//! fill's **own-gamut linear RGB** (after its EOTF, before the primaries
//! convert), between the fill pixel and the reference colour linearized once
//! with the fill's EOTF. Every expectation is computed here from the public
//! colour functions, independently of `key::chroma_matte`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_compositor::blend::{over, LinearRgba, PremulRgba};
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite_reference, composite_with, composite_with_threads,
    tile_yuv_to_canvas_linear, tile_yuv_to_tile_linear, CanvasColor, Nv12Image, Tile,
};
use multiview_compositor::{KeyMask, SceneMember, SceneMerge, TileGroup, TileKey};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use proptest::prelude::*;

const BLACK: LinearRgba = LinearRgba::opaque(0.0, 0.0, 0.0);
const GREY: LinearRgba = LinearRgba::opaque(0.2, 0.2, 0.2);

fn bt709(range: ColorRange) -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range,
    }
}

fn pq2020() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    }
}

/// An image of 2x2 blocks (NV12-exact: one chroma sample per block), block
/// `i` (raster order, `w / 2` per row) taking `blocks[i % len]`.
fn blocks(w: u32, h: u32, blocks: &[(u8, u8, u8)], color: ColorInfo) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let bw = wu / 2;
    let mut y = vec![0_u8; wu * hu];
    let mut uv = vec![0_u8; wu * hu / 2];
    for by in 0..hu / 2 {
        for bx in 0..bw {
            let (yy, cb, cr) = blocks[(by * bw + bx) % blocks.len()];
            for dy in 0..2 {
                for dx in 0..2 {
                    y[(by * 2 + dy) * wu + bx * 2 + dx] = yy;
                }
            }
            uv[by * wu + bx * 2] = cb;
            uv[by * wu + bx * 2 + 1] = cr;
        }
    }
    Nv12Image::new(w, h, y, uv, color).unwrap()
}

/// A green screen with a subject: studio green, a slightly lighter and a
/// darker green (soft edge), skin, magenta, grey, white and black.
const SCREEN: [(u8, u8, u8); 8] = [
    (129, 96, 55),  // studio green (#00b140 in limited BT.709)
    (136, 99, 62),  // a lighter green (soft edge)
    (118, 100, 64), // a darker, less saturated green (soft edge)
    (150, 110, 160),
    (90, 200, 200),
    (126, 128, 128),
    (235, 128, 128),
    (16, 128, 128),
];

fn canvas() -> CanvasColor {
    CanvasColor::default()
}

/// The reference colour in the fill's linear space, written out here:
/// `code / 255` through the fill's EOTF.
fn reference_linear(reference: [u8; 3], transfer: TransferCharacteristic) -> [f32; 3] {
    reference.map(|c| multiview_compositor::transfer::eotf(f32::from(c) / 255.0, transfer).unwrap())
}

/// The independent per-pixel chroma matte (before invert).
fn matte(px: [f32; 3], reference: [f32; 3], clip: f32, gain: f32) -> f32 {
    let d = ((px[0] - reference[0]).powi(2)
        + (px[1] - reference[1]).powi(2)
        + (px[2] - reference[2]).powi(2))
    .sqrt();
    ((d - clip) * gain).clamp(0.0, 1.0)
}

/// The expected composite of one chroma-keyed full-canvas tile over a flat
/// background, pixel by pixel; 4:2:0 chroma is the bottom-right pixel of each
/// 2x2 block (the reference kernel's last writer).
fn expected(
    fill: &Nv12Image,
    bg: LinearRgba,
    reference: [u8; 3],
    clip: f32,
    gain: f32,
    invert: bool,
) -> (Vec<u8>, Vec<u8>) {
    let (w, h) = (fill.width(), fill.height());
    let r = reference_linear(reference, fill.color().transfer);
    let mut y_out = vec![0_u8; (w * h) as usize];
    let mut uv_out = vec![0_u8; (w * h / 2) as usize];
    for py in 0..h {
        for px in 0..w {
            let (y8, cb, cr) = fill.sample(px, py).unwrap();
            let tile_lin = tile_yuv_to_tile_linear(y8, cb, cr, fill.color()).unwrap();
            let m = matte(tile_lin, r, clip, gain);
            let a = if invert { 1.0 - m } else { m };
            let lin = tile_yuv_to_canvas_linear(y8, cb, cr, fill.color(), canvas()).unwrap();
            let src = LinearRgba {
                r: lin[0],
                g: lin[1],
                b: lin[2],
                a,
            }
            .premultiplied();
            let s = over(src, bg.premultiplied()).unpremultiplied();
            let out = canvas_linear_to_output_yuv([s.r, s.g, s.b], canvas()).unwrap();
            y_out[(py * w + px) as usize] = out[0];
            if py % 2 == 1 && px % 2 == 1 {
                let i = ((py / 2) * w + (px / 2) * 2) as usize;
                uv_out[i] = out[1];
                uv_out[i + 1] = out[2];
            }
        }
    }
    (y_out, uv_out)
}

fn assert_golden(got: &Nv12Image, want: &(Vec<u8>, Vec<u8>), what: &str) {
    assert_eq!(got.y_plane(), want.0.as_slice(), "{what}: Y");
    assert_eq!(got.uv_plane(), want.1.as_slice(), "{what}: UV");
}

#[test]
fn a_green_screen_keys_by_linear_rgb_distance_golden() {
    let fill = blocks(16, 4, &SCREEN, bt709(ColorRange::Limited));
    for (reference, clip, gain, invert) in [
        ([0x00, 0xb1, 0x40], 0.05_f32, 8.0_f32, false),
        ([0x00, 0xb1, 0x40], 0.05, 8.0, true),
        ([0x00, 0xb1, 0x40], 0.0, 1.0, false),
        ([0x10, 0xc0, 0x50], 0.15, 2.5, false),
        ([0xff, 0x00, 0xff], 0.3, 3.0, false),
    ] {
        let key = TileKey::chroma(reference, clip, gain).inverted(invert);
        let tiles = [Tile::placed(&fill, 0, 0, 1.0).with_key(key)];
        let got = composite_with(16, 4, canvas(), GREY, &tiles, false).unwrap();
        let want = expected(&fill, GREY, reference, clip, gain, invert);
        assert_golden(
            &got,
            &want,
            &format!("{reference:?} {clip} {gain} {invert}"),
        );
        // The reference kernel agrees byte for byte.
        let oracle = composite_reference(16, 4, canvas(), GREY, &tiles, false).unwrap();
        assert_eq!(got, oracle);
    }
}

#[test]
fn the_studio_green_is_keyed_out_and_the_subject_kept() {
    // Pinned golden row: with #00b140, clip 0.05, gain 8 the studio green
    // block shows the background, magenta/skin/white show themselves.
    let fill = blocks(16, 2, &SCREEN, bt709(ColorRange::Limited));
    let tiles =
        [Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma([0x00, 0xb1, 0x40], 0.05, 8.0))];
    let got = composite_with(16, 2, canvas(), GREY, &tiles, false).unwrap();
    let bg = composite_with(16, 2, canvas(), GREY, &[], false).unwrap();
    let plain = composite_with(
        16,
        2,
        canvas(),
        GREY,
        &[Tile::placed(&fill, 0, 0, 1.0)],
        false,
    )
    .unwrap();
    let y = |img: &Nv12Image, block: u32| img.sample(block * 2, 0).unwrap();
    assert_eq!(y(&got, 0), y(&bg, 0), "studio green keyed out");
    for block in 3..8 {
        assert_eq!(
            y(&got, block),
            y(&plain, block),
            "subject block {block} kept"
        );
    }
    // A near green sits on the soft edge: neither keyed out nor kept.
    let on_edge = (1..3).any(|block| {
        let (gy, by, py) = (y(&got, block).0, y(&bg, block).0, y(&plain, block).0);
        gy != by && gy != py
    });
    assert!(on_edge);
}

#[test]
fn the_distance_is_measured_in_the_tile_gamut_not_the_canvas_gamut() {
    // A PQ / BT.2020 fill on the BT.709 canvas: the matte follows the
    // tile-gamut linear RGB (ADR-C007 §3); the canvas-gamut distance differs.
    let fill = blocks(16, 4, &SCREEN, pq2020());
    let (reference, clip, gain) = ([0x40, 0x90, 0x50], 0.02_f32, 20.0_f32);
    let tiles = [Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma(reference, clip, gain))];
    let got = composite_with(16, 4, canvas(), BLACK, &tiles, false).unwrap();
    assert_golden(
        &got,
        &expected(&fill, BLACK, reference, clip, gain, false),
        "pq2020 fill",
    );
    // At least one block's matte differs between the two domains, so the
    // golden pins the domain choice.
    let r = reference_linear(reference, TransferCharacteristic::Pq);
    let differs = SCREEN.iter().any(|&(y8, cb, cr)| {
        let t = tile_yuv_to_tile_linear(y8, cb, cr, pq2020()).unwrap();
        let c = tile_yuv_to_canvas_linear(y8, cb, cr, pq2020(), canvas()).unwrap();
        (matte(t, r, clip, gain) - matte(c, r, clip, gain)).abs() > 1e-3
    });
    assert!(differs);
}

#[test]
fn a_chroma_key_is_premultiplied_once_with_opacity_alpha_and_mask() {
    let fill = blocks(8, 4, &SCREEN, bt709(ColorRange::Full));
    let key = TileKey::chroma([0xff, 0x00, 0xff], 0.1, 4.0).masked(KeyMask {
        x: 2,
        y: 0,
        w: 4,
        h: 2,
    });
    let opacity = 0.6_f32;
    let got = composite_with(
        8,
        4,
        canvas(),
        GREY,
        &[Tile::placed(&fill, 0, 0, opacity).with_key(key)],
        false,
    )
    .unwrap();
    let r = reference_linear([0xff, 0x00, 0xff], TransferCharacteristic::Bt709);
    for (px, py) in [(0_u32, 0_u32), (2, 0), (3, 1), (5, 1), (6, 0), (4, 2)] {
        let (y8, cb, cr) = fill.sample(px, py).unwrap();
        let inside = (2..6).contains(&px) && py < 2;
        let m = if inside {
            matte(
                tile_yuv_to_tile_linear(y8, cb, cr, fill.color()).unwrap(),
                r,
                0.1,
                4.0,
            )
        } else {
            0.0
        };
        let lin = tile_yuv_to_canvas_linear(y8, cb, cr, fill.color(), canvas()).unwrap();
        let src: PremulRgba = LinearRgba {
            r: lin[0],
            g: lin[1],
            b: lin[2],
            a: opacity * m,
        }
        .premultiplied();
        let s = over(src, GREY.premultiplied()).unpremultiplied();
        let want = canvas_linear_to_output_yuv([s.r, s.g, s.b], canvas()).unwrap();
        assert_eq!(got.sample(px, py).unwrap().0, want[0], "({px}, {py})");
    }
}

#[test]
fn a_key_source_on_a_chroma_key_is_ignored() {
    // Config refuses a chroma key with a key source; the compositor still
    // keys the fill's own colour if one is set.
    let fill = blocks(8, 4, &SCREEN, bt709(ColorRange::Limited));
    let black = Nv12Image::solid(8, 4, 16, 128, 128, bt709(ColorRange::Limited)).unwrap();
    let k = TileKey::chroma([0x00, 0xb1, 0x40], 0.05, 8.0);
    let a = composite_with(
        8,
        4,
        canvas(),
        GREY,
        &[Tile::placed(&fill, 0, 0, 1.0).with_key(k)],
        false,
    )
    .unwrap();
    let b = composite_with(
        8,
        4,
        canvas(),
        GREY,
        &[Tile::placed(&fill, 0, 0, 1.0).with_key(k.with_source(&black))],
        false,
    )
    .unwrap();
    assert_eq!(a, b);
}

#[test]
fn degenerate_chroma_parameters_never_poison_the_layers_beneath() {
    let fill = blocks(8, 4, &SCREEN, bt709(ColorRange::Limited));
    let bg_tile = Nv12Image::solid(8, 4, 80, 140, 120, bt709(ColorRange::Limited)).unwrap();
    let nan = f32::NAN;
    let inf = f32::INFINITY;
    let under = composite_with(
        8,
        4,
        canvas(),
        GREY,
        &[Tile::placed(&bg_tile, 0, 0, 1.0)],
        false,
    )
    .unwrap();
    for (clip, gain, invert) in [
        (nan, 1.0, false),
        (0.1, nan, false),
        (inf, 1.0, false),
        (0.1, 0.0, false),
        (0.1, -2.0, false),
        (-1e-40, inf, true),
    ] {
        let tiles = [
            Tile::placed(&bg_tile, 0, 0, 1.0),
            Tile::placed(&fill, 0, 0, 1.0)
                .with_key(TileKey::chroma([0, 255, 0], clip, gain).inverted(invert)),
        ];
        let got = composite_with(8, 4, canvas(), GREY, &tiles, false).unwrap();
        // Every case is coverage 0: a NaN clip or gain, a +inf clip and a
        // zero or negative gain are matte 0; the subnormal clip flushes to 0,
        // so an infinite gain is matte 1 for every d > 0 (no fill pixel is
        // exactly the reference), and inverted 0.
        assert_eq!(got, under, "clip={clip} gain={gain} invert={invert}");
    }
    // -inf clip with a positive gain keeps everything (matte 1).
    let tiles = [
        Tile::placed(&bg_tile, 0, 0, 1.0),
        Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma([0, 255, 0], -inf, 1.0)),
    ];
    let got = composite_with(8, 4, canvas(), GREY, &tiles, false).unwrap();
    let plain = composite_with(
        8,
        4,
        canvas(),
        GREY,
        &[
            Tile::placed(&bg_tile, 0, 0, 1.0),
            Tile::placed(&fill, 0, 0, 1.0),
        ],
        false,
    )
    .unwrap();
    assert_eq!(got, plain);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn the_tile_driven_kernel_equals_the_oracle_with_chroma_keys(
        seed in 0_usize..8,
        fx in 0_u32..10, fy in 0_u32..4,
        r in any::<u8>(), g in any::<u8>(), b in any::<u8>(),
        clip in 0.0_f32..0.5, gain in 0.0_f32..30.0, invert in any::<bool>(),
        p in 0.0_f32..1.0, grouped in any::<bool>(), lut in any::<bool>(),
        threads in 1_usize..5,
    ) {
        let mut screen = SCREEN;
        screen.rotate_left(seed);
        let bg_img = blocks(32, 16, &screen, bt709(ColorRange::Limited));
        let fill = blocks(8, 6, &screen, pq2020());
        let group = if grouped {
            TileGroup::Scene { member: SceneMember::In, merge: SceneMerge::Mix { p } }
        } else {
            TileGroup::Top
        };
        let tiles = [
            Tile::placed(&bg_img, 0, 0, 1.0),
            Tile::scaled(&fill, fx * 2, fy * 2, 12, 8, 0.9)
                .with_key(TileKey::chroma([r, g, b], clip, gain).inverted(invert)
                    .masked(KeyMask { x: 1, y: 0, w: 6, h: 5 }))
                .in_group(group),
        ];
        let oracle = composite_reference(32, 16, canvas(), BLACK, &tiles, lut).unwrap();
        let fast = composite_with_threads(32, 16, canvas(), BLACK, &tiles, lut, threads).unwrap();
        prop_assert_eq!(oracle, fast);
    }
}
