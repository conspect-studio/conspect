//! Integration tests for the fixed-order pipeline and CPU reference compositor
//! (invariant #8). The CPU path is bit-exact, so these are golden-frame tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image, Tile,
};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

#[test]
fn front_then_back_half_roundtrips_when_tile_matches_canvas() {
    // When the tile color == canvas color, the front half and back half are an
    // exact inverse pair: a YUV sample must survive within +/-1 code value
    // (only 8-bit quantization loss).
    let canvas = CanvasColor::default();
    let tile = bt709_limited();
    for &(y, cb, cr) in &[
        (16_u8, 128_u8, 128_u8),
        (126, 100, 150),
        (235, 16, 240),
        (90, 200, 60),
    ] {
        let lin = tile_yuv_to_canvas_linear(y, cb, cr, tile, canvas).unwrap();
        let out = canvas_linear_to_output_yuv(lin, canvas).unwrap();
        assert!(
            (i16::from(out[0]) - i16::from(y)).abs() <= 1,
            "Y {y} -> {}",
            out[0]
        );
        assert!(
            (i16::from(out[1]) - i16::from(cb)).abs() <= 1,
            "Cb {cb} -> {}",
            out[1]
        );
        assert!(
            (i16::from(out[2]) - i16::from(cr)).abs() <= 1,
            "Cr {cr} -> {}",
            out[2]
        );
    }
}

#[test]
fn solid_opaque_tile_reproduces_itself_on_matching_canvas() {
    // A solid limited BT.709 mid-gray tile, opaque, placed 1:1 on a matching
    // canvas must reproduce its own code values across the whole frame
    // (golden: bit-stable to +/-1 from quantization).
    let tile_img = Nv12Image::solid(4, 4, 126, 128, 128, bt709_limited()).unwrap();
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    let out = composite(
        4,
        4,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    )
    .unwrap();
    for &y in out.y_plane() {
        assert!((i16::from(y) - 126).abs() <= 1, "luma {y}");
    }
    for pair in out.uv_plane().chunks_exact(2) {
        assert!((i16::from(pair[0]) - 128).abs() <= 1, "cb {}", pair[0]);
        assert!((i16::from(pair[1]) - 128).abs() <= 1, "cr {}", pair[1]);
    }
}

#[test]
fn output_is_tagged_with_canvas_color() {
    let tile_img = Nv12Image::solid(2, 2, 100, 128, 128, bt709_limited()).unwrap();
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    let out = composite(
        2,
        2,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    )
    .unwrap();
    // Tagging != converting: the output carries the canvas tag exactly.
    assert_eq!(out.color(), CanvasColor::default().output_tag());
    assert_eq!(out.color().primaries, ColorPrimaries::Bt709);
    assert_eq!(out.color().range, ColorRange::Limited);
}

#[test]
fn uncovered_pixels_take_background() {
    // A 2x2 tile in the top-left of a 4x4 canvas; the rest is opaque-white
    // background. Bottom-right pixel must be white-ish (high luma), top-left
    // must be the dark tile.
    let tile_img = Nv12Image::solid(2, 2, 16, 128, 128, bt709_limited()).unwrap(); // black
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    let white_bg = LinearRgba::opaque(1.0, 1.0, 1.0);
    let out = composite(4, 4, CanvasColor::default(), white_bg, &tiles).unwrap();
    let w = 4_usize;
    // top-left covered by black tile -> luma ~16.
    assert!((i16::from(out.y_plane()[0]) - 16).abs() <= 2, "tile luma");
    // bottom-right uncovered -> white -> luma ~235 (limited white).
    let br = out.y_plane()[3 * w + 3];
    assert!(i16::from(br) >= 233, "background luma {br}");
}

#[test]
fn half_opacity_tile_blends_with_background_in_linear() {
    // 50%-opaque white tile over opaque black background -> linear 0.5 ->
    // BT.709 limited luma. Linear 0.5 -> gamma 0.5^(1/2.4) ~ 0.7492 ->
    // limited Y = round(0.7492*219+16) ~ 180. (Gamma-space blending would give
    // ~0.5 gamma -> Y ~ 126, which is the WRONG answer this guards against.)
    let tile_img = Nv12Image::solid(2, 2, 235, 128, 128, bt709_limited()).unwrap(); // white
    let tiles = [Tile::placed(&tile_img, 0, 0, 0.5)];
    let black_bg = LinearRgba::opaque(0.0, 0.0, 0.0);
    let out = composite(2, 2, CanvasColor::default(), black_bg, &tiles).unwrap();
    let luma = out.y_plane()[0];
    assert!(
        (170..=190).contains(&luma),
        "linear-light 50% blend luma = {luma} (gamma-space would be ~126)"
    );
}

#[test]
fn nv12_geometry_is_validated() {
    // Odd dimensions rejected.
    assert!(Nv12Image::solid(3, 2, 0, 0, 0, bt709_limited()).is_err());
    // Wrong plane lengths rejected.
    assert!(Nv12Image::new(2, 2, vec![0; 3], vec![0; 2], bt709_limited()).is_err());
    assert!(Nv12Image::new(2, 2, vec![0; 4], vec![0; 1], bt709_limited()).is_err());
    // Correct lengths accepted.
    assert!(Nv12Image::new(2, 2, vec![0; 4], vec![0; 2], bt709_limited()).is_ok());
}

#[test]
fn composite_rejects_odd_canvas() {
    let tile_img = Nv12Image::solid(2, 2, 100, 128, 128, bt709_limited()).unwrap();
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    assert!(composite(
        3,
        2,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles
    )
    .is_err());
}

#[test]
fn unresolved_tile_color_is_rejected() {
    // An Unspecified axis must never reach the kernel.
    let bad = ColorInfo {
        range: ColorRange::Unspecified,
        ..bt709_limited()
    };
    assert!(tile_yuv_to_canvas_linear(100, 128, 128, bad, CanvasColor::default()).is_err());
    let bad_matrix = ColorInfo {
        matrix: MatrixCoefficients::Unspecified,
        ..bt709_limited()
    };
    assert!(tile_yuv_to_canvas_linear(100, 128, 128, bad_matrix, CanvasColor::default()).is_err());
}

#[test]
fn two_tile_golden_composite_places_each_tile() {
    // A 4x2 canvas: left 2x2 = dark tile (Y=40), right 2x2 = bright tile
    // (Y=200). Tags BT.709 limited. Verifies placement + per-tile color path.
    let dark = Nv12Image::solid(2, 2, 40, 128, 128, bt709_limited()).unwrap();
    let bright = Nv12Image::solid(2, 2, 200, 128, 128, bt709_limited()).unwrap();
    let tiles = [
        Tile::placed(&dark, 0, 0, 1.0),
        Tile::placed(&bright, 2, 0, 1.0),
    ];
    let out = composite(
        4,
        2,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    )
    .unwrap();
    let w = 4_usize;
    // Left half ~40, right half ~200, every row.
    for row in 0..2_usize {
        assert!((i16::from(out.y_plane()[row * w]) - 40).abs() <= 2, "left");
        assert!(
            (i16::from(out.y_plane()[row * w + 1]) - 40).abs() <= 2,
            "left2"
        );
        assert!(
            (i16::from(out.y_plane()[row * w + 2]) - 200).abs() <= 2,
            "right"
        );
        assert!(
            (i16::from(out.y_plane()[row * w + 3]) - 200).abs() <= 2,
            "right2"
        );
    }
}

#[test]
fn neutral_gray_survives_cross_matrix_pipeline() {
    // A BT.601 (SD/NTSC) neutral-gray tile onto the BT.709 canvas: neutral gray
    // has no chroma, so it must stay neutral (cb=cr=128) and keep its luma
    // (within quantization + the SD->HD gamut conversion, which is identity for
    // the achromatic axis).
    let sd_color = ColorInfo {
        primaries: ColorPrimaries::Bt601_525,
        transfer: TransferCharacteristic::Bt601,
        matrix: MatrixCoefficients::Bt601,
        range: ColorRange::Limited,
    };
    let tile_img = Nv12Image::solid(2, 2, 150, 128, 128, sd_color).unwrap();
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    let out = composite(
        2,
        2,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    )
    .unwrap();
    // Chroma stays neutral.
    for pair in out.uv_plane().chunks_exact(2) {
        assert!((i16::from(pair[0]) - 128).abs() <= 1, "cb {}", pair[0]);
        assert!((i16::from(pair[1]) - 128).abs() <= 1, "cr {}", pair[1]);
    }
    // Luma preserved (same transfer family, neutral axis is gamut-invariant).
    assert!((i16::from(out.y_plane()[0]) - 150).abs() <= 2, "luma");
}

/// Build an NV12 image whose chroma varies left/right so a wrong chroma
/// stride/offset (or half-resolution indexing) would corrupt it: the left half
/// is saturated red `(Y=81, Cb=90, Cr=240)`, the right half saturated blue
/// `(Y=41, Cb=240, Cr=110)`. Both round-trip 1:1 through the BT.709-matched
/// pipeline (verified by [`full_canvas_tile_preserves_saturated_chroma`]).
fn red_blue_split(width: u32, height: u32) -> Nv12Image {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let half = w / 2;
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = if col < half { 81 } else { 41 };
        }
    }
    // One interleaved Cb/Cr pair per 2x2 chroma block; the chroma row stride is
    // `w` bytes (`w/2` pairs). Left half red, right half blue.
    for crow in 0..h / 2 {
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            let left = cpair < half / 2;
            uv[idx] = if left { 90 } else { 240 }; // Cb
            uv[idx + 1] = if left { 240 } else { 110 }; // Cr
        }
    }
    Nv12Image::new(width, height, y, uv, bt709_limited()).unwrap()
}

#[test]
fn full_canvas_tile_preserves_saturated_chroma() {
    // REGRESSION (BUG 1: full-frame/PiP green chroma cast). A source whose
    // destination covers the FULL canvas must have its chroma sampled + written
    // at the correct 4:2:0 positions — exactly as a sub-region (grid) tile does.
    // A mis-addressed chroma plane (wrong stride/offset, half-resolution
    // indexing, or a zeroed second plane) collapses chroma toward neutral 128 or
    // 0,0, which renders as a green wash with luma still visible. Here we
    // composite a saturated red|blue split tile onto a MATCHING full-canvas dest
    // (1:1) and assert the chroma comes back saturated + correct per side — never
    // green.
    let canvas_w: u32 = 64;
    let canvas_h: u32 = 36;
    let src = red_blue_split(canvas_w, canvas_h);
    let tiles = [Tile::placed(&src, 0, 0, 1.0)];
    let out = composite(
        canvas_w,
        canvas_h,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    )
    .unwrap();

    let w = usize::try_from(canvas_w).unwrap();
    let h = usize::try_from(canvas_h).unwrap();
    let half = w / 2;
    // Inspect chroma at every 2x2 block (UV plane is `h/2` rows of `w` bytes).
    for crow in 0..h / 2 {
        for cpair in 0..half {
            let idx = crow * w + cpair * 2;
            let cb = i16::from(out.uv_plane()[idx]);
            let cr = i16::from(out.uv_plane()[idx + 1]);
            if cpair < half / 2 {
                // Left = saturated red: Cb≈90, Cr≈240. NOT neutral, NOT zeroed.
                assert!((cb - 90).abs() <= 2, "left Cb {cb} (expected ~90, red)");
                assert!((cr - 240).abs() <= 2, "left Cr {cr} (expected ~240, red)");
            } else {
                // Right = saturated blue: Cb≈240, Cr≈110.
                assert!((cb - 240).abs() <= 2, "right Cb {cb} (expected ~240, blue)");
                assert!((cr - 110).abs() <= 2, "right Cr {cr} (expected ~110, blue)");
            }
        }
    }

    // Explicit anti-green guard: the full-frame chroma must be genuinely
    // saturated (mean |Cb-128| + |Cr-128| large), proving it was neither
    // flattened to neutral 128 nor zeroed to 0 (both of which read as green).
    let mut chroma_energy: u64 = 0;
    for pair in out.uv_plane().chunks_exact(2) {
        chroma_energy += u64::from((i16::from(pair[0]) - 128).unsigned_abs());
        chroma_energy += u64::from((i16::from(pair[1]) - 128).unsigned_abs());
    }
    let pairs = u64::try_from(out.uv_plane().len() / 2).unwrap();
    let mean_chroma_dev = chroma_energy / (2 * pairs);
    // Saturated red|blue averages ~70 here; a chroma collapsed to neutral 128
    // averages ~0. A floor of 40 cleanly distinguishes "still saturated" from
    // "flattened toward neutral" (the green-cast failure mode).
    assert!(
        mean_chroma_dev > 40,
        "full-canvas chroma collapsed toward neutral (mean |chroma-128| = {mean_chroma_dev}); \
         this is the green-cast defect — chroma must stay saturated"
    );
}

#[test]
fn full_canvas_chroma_matches_sub_region_tile() {
    // The full-canvas placement and a sub-region placement of the SAME source
    // must produce identical chroma in the covered area — i.e. the chroma path
    // is size-independent (the green-cast bug would make the full-frame case
    // differ from a grid sub-tile).
    let src = red_blue_split(32, 16);

    // (a) Full-canvas: the tile IS the canvas.
    let full = composite(
        32,
        16,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &[Tile::placed(&src, 0, 0, 1.0)],
    )
    .unwrap();

    // (b) Sub-region: the same 32x16 tile placed at an even offset inside a
    //     larger canvas; the covered window must match the full-canvas chroma.
    let big = composite(
        64,
        32,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &[Tile::placed(&src, 16, 8, 1.0)],
    )
    .unwrap();

    let fw = 32_usize;
    let bw = 64_usize;
    for crow in 0..8_usize {
        for cpair in 0..16_usize {
            let full_idx = crow * fw + cpair * 2;
            // Sub-region tile origin (16,8): chroma block offset is (8, 4).
            let big_idx = (crow + 4) * bw + (cpair + 8) * 2;
            assert_eq!(
                (full.uv_plane()[full_idx], full.uv_plane()[full_idx + 1]),
                (big.uv_plane()[big_idx], big.uv_plane()[big_idx + 1]),
                "chroma differs between full-canvas and sub-region at block ({cpair},{crow})"
            );
        }
    }
}
