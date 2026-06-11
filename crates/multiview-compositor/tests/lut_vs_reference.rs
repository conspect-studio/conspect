//! LUT-vs-oracle equivalence tests (ADR-0022).
//!
//! The real-time compositor swaps the per-pixel transcendental EOTF/OETF
//! (`transfer::eotf`/`oetf`, the un-LUT'd "oracle") for table lookups with
//! linear interpolation. The pipeline **order** (invariant #8) is unchanged —
//! only the *evaluation* of those two transfer functions is replaced. These
//! tests pin the LUT path to the oracle:
//!
//! - (a) **pixel-level** — for a representative 8-bit `(y, cb, cr)` grid × each
//!   supported transfer, the LUT front/back halves stay within `2e-3` (linear)
//!   of the oracle [`tile_yuv_to_canvas_linear`] / [`canvas_linear_to_output_yuv`].
//! - (b) **frame-level** — a multi-tile canvas (incl. the `red_blue_split`
//!   saturated-chroma fixture and a PQ-tagged tile) rendered through the oracle
//!   path vs the LUT path differs by at most 1 code value on Y and on UV.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite_with, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image,
    Tile,
};
use multiview_compositor::transfer_lut::LutSet;
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

/// A color tuple for a given tile transfer, otherwise BT.709 limited.
fn tile_color_with(transfer: TransferCharacteristic) -> ColorInfo {
    let (primaries, matrix) = match transfer {
        TransferCharacteristic::Pq
        | TransferCharacteristic::Hlg
        | TransferCharacteristic::Bt2020 => (ColorPrimaries::Bt2020, MatrixCoefficients::Bt2020Ncl),
        TransferCharacteristic::Bt601 => (ColorPrimaries::Bt601_525, MatrixCoefficients::Bt601),
        // Bt709, Srgb, and any other transfer use the BT.709 gamut + matrix.
        _ => (ColorPrimaries::Bt709, MatrixCoefficients::Bt709),
    };
    ColorInfo {
        primaries,
        transfer,
        matrix,
        range: ColorRange::Limited,
    }
}

/// The transfers the LUT path must support (the same set the oracle dispatch
/// implements as `Ok(..)`).
const SUPPORTED: &[TransferCharacteristic] = &[
    TransferCharacteristic::Bt709,
    TransferCharacteristic::Bt601,
    TransferCharacteristic::Bt2020,
    TransferCharacteristic::Srgb,
    TransferCharacteristic::Pq,
    TransferCharacteristic::Hlg,
];

#[test]
fn lut_front_half_matches_oracle_within_tolerance() {
    let canvas = CanvasColor::default();
    for &transfer in SUPPORTED {
        let tile = tile_color_with(transfer);
        let lut = LutSet::for_transfers([tile.transfer, canvas.transfer]);
        // A representative grid of 8-bit code values, including the extremes and
        // saturated chroma corners (those overshoot the [0,1] gamma domain).
        for &y in &[16_u8, 60, 126, 200, 235] {
            for &cb in &[16_u8, 64, 128, 200, 240] {
                for &cr in &[16_u8, 64, 128, 200, 240] {
                    let oracle = tile_yuv_to_canvas_linear(y, cb, cr, tile, canvas).unwrap();
                    let lutted = lut
                        .tile_yuv_to_canvas_linear(y, cb, cr, tile, canvas)
                        .unwrap();
                    for c in 0..3 {
                        assert!(
                            (oracle[c] - lutted[c]).abs() <= 2e-3,
                            "front half {transfer:?} ({y},{cb},{cr}) ch{c}: oracle {} vs lut {}",
                            oracle[c],
                            lutted[c]
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn lut_back_half_matches_oracle_within_tolerance() {
    let canvas = CanvasColor::default();
    let lut = LutSet::for_transfers([canvas.transfer]);
    // A grid of linear-light triples spanning the canvas working range.
    for ri in 0_u8..=8 {
        for gi in 0_u8..=8 {
            for bi in 0_u8..=8 {
                let lin = [
                    f32::from(ri) / 8.0,
                    f32::from(gi) / 8.0,
                    f32::from(bi) / 8.0,
                ];
                let oracle = canvas_linear_to_output_yuv(lin, canvas).unwrap();
                let lutted = lut.canvas_linear_to_output_yuv(lin, canvas).unwrap();
                for c in 0..3 {
                    assert!(
                        (i16::from(oracle[c]) - i16::from(lutted[c])).abs() <= 1,
                        "back half lin {lin:?} ch{c}: oracle {} vs lut {}",
                        oracle[c],
                        lutted[c]
                    );
                }
            }
        }
    }
}

/// Build an NV12 image whose chroma varies left/right (saturated red | blue),
/// mirroring `tests/pipeline.rs::red_blue_split` — exercises saturated chroma
/// (post-matrix gamma overshoots the [0,1] domain) through the LUT.
fn red_blue_split(width: u32, height: u32, color: ColorInfo) -> Nv12Image {
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
    for crow in 0..h / 2 {
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            let left = cpair < half / 2;
            uv[idx] = if left { 90 } else { 240 };
            uv[idx + 1] = if left { 240 } else { 110 };
        }
    }
    Nv12Image::new(width, height, y, uv, color).unwrap()
}

#[test]
fn frame_level_lut_matches_oracle_within_one_code() {
    // A multi-tile canvas: a BT.709 saturated red|blue split, a solid mid-gray
    // BT.601 tile, and a PQ-tagged BT.2020 tile — covering the front-half
    // transfers exercised by the LUT.
    let split = red_blue_split(32, 16, bt709_limited());
    let gray601 = Nv12Image::solid(
        16,
        16,
        150,
        128,
        128,
        tile_color_with(TransferCharacteristic::Bt601),
    )
    .unwrap();
    let pq = Nv12Image::solid(
        16,
        16,
        160,
        90,
        200,
        tile_color_with(TransferCharacteristic::Pq),
    )
    .unwrap();
    let tiles = [
        Tile::placed(&split, 0, 0, 1.0),
        Tile::placed(&gray601, 32, 0, 0.75),
        Tile::placed(&pq, 48, 0, 1.0),
    ];
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.1, 0.1, 0.1);

    let oracle = composite_with(64, 16, canvas, bg, &tiles, false).unwrap();
    let lutted = composite_with(64, 16, canvas, bg, &tiles, true).unwrap();

    assert_eq!(oracle.width(), lutted.width());
    assert_eq!(oracle.height(), lutted.height());

    let mut max_y_delta = 0_i16;
    for (o, l) in oracle.y_plane().iter().zip(lutted.y_plane()) {
        let d = (i16::from(*o) - i16::from(*l)).abs();
        max_y_delta = max_y_delta.max(d);
    }
    assert!(max_y_delta <= 1, "max Y delta {max_y_delta} > 1");

    let mut max_uv_delta = 0_i16;
    for (o, l) in oracle.uv_plane().iter().zip(lutted.uv_plane()) {
        let d = (i16::from(*o) - i16::from(*l)).abs();
        max_uv_delta = max_uv_delta.max(d);
    }
    assert!(max_uv_delta <= 1, "max UV delta {max_uv_delta} > 1");
}

#[test]
fn lut_preserves_unsupported_transfer_error() {
    // An unsupported transfer must still return the oracle's Err (never panic).
    let canvas = CanvasColor::default();
    let bad = ColorInfo {
        transfer: TransferCharacteristic::Unspecified,
        ..bt709_limited()
    };
    let lut = LutSet::for_transfers([canvas.transfer]);
    assert!(lut
        .tile_yuv_to_canvas_linear(100, 128, 128, bad, canvas)
        .is_err());
}
