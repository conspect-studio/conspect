//! Integration tests for the YUV'<->R'G'B' matrices (invariant #8 step 2).
//! Pins the luma weights and the documented BT.601/709/2020-NCL coefficients
//! (color-management.md §4.1, §4.3) and matrix invertibility (round-trip).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::matrix::{luma_weights, rgb_to_yuv, yuv_to_rgb};
use multiview_core::color::MatrixCoefficients;

const EPS: f32 = 2e-4;

#[test]
fn luma_weights_match_itu() {
    assert_eq!(
        luma_weights(MatrixCoefficients::Bt601),
        Some((0.299, 0.587, 0.114))
    );
    assert_eq!(
        luma_weights(MatrixCoefficients::Bt709),
        Some((0.2126, 0.7152, 0.0722))
    );
    assert_eq!(
        luma_weights(MatrixCoefficients::Bt2020Ncl),
        Some((0.2627, 0.6780, 0.0593))
    );
    // Kr + Kg + Kb == 1 for each system.
    for m in [
        MatrixCoefficients::Bt601,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Bt2020Ncl,
    ] {
        let (kr, kg, kb) = luma_weights(m).unwrap();
        assert!((kr + kg + kb - 1.0).abs() < 1e-12, "weights for {m:?}");
    }
}

#[test]
fn unspecified_and_rgb_have_no_luma_weights() {
    assert_eq!(luma_weights(MatrixCoefficients::Unspecified), None);
    assert_eq!(luma_weights(MatrixCoefficients::Rgb), None);
    assert!(yuv_to_rgb(0.5, 0.0, 0.0, MatrixCoefficients::Unspecified).is_err());
    assert!(rgb_to_yuv(0.5, 0.5, 0.5, MatrixCoefficients::Rgb).is_err());
}

#[test]
fn neutral_gray_has_zero_chroma_and_equal_rgb() {
    // Y=0.5, neutral chroma -> R=G=B=0.5 for every matrix.
    for m in [
        MatrixCoefficients::Bt601,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Bt2020Ncl,
    ] {
        let rgb = yuv_to_rgb(0.5, 0.0, 0.0, m).unwrap();
        assert!((rgb[0] - 0.5).abs() < EPS, "{m:?} R");
        assert!((rgb[1] - 0.5).abs() < EPS, "{m:?} G");
        assert!((rgb[2] - 0.5).abs() < EPS, "{m:?} B");
    }
}

#[test]
fn bt709_known_coefficients() {
    // R' = Y + 1.5748 Cr ; B' = Y + 1.8556 Cb (color-management.md §4.3).
    // Pick Y=0, Cr=0.5, Cb=0 -> R' = 0.7874.
    let rgb = yuv_to_rgb(0.0, 0.0, 0.5, MatrixCoefficients::Bt709).unwrap();
    assert!((rgb[0] - 0.7874).abs() < EPS, "R' = {}", rgb[0]);
    // Y=0, Cb=0.5, Cr=0 -> B' = 0.9278.
    let rgb = yuv_to_rgb(0.0, 0.5, 0.0, MatrixCoefficients::Bt709).unwrap();
    assert!((rgb[2] - 0.9278).abs() < EPS, "B' = {}", rgb[2]);
}

#[test]
fn bt2020ncl_green_coefficients_full_precision() {
    // G' = Y - 0.16455312684366 Cb - 0.57135312684366 Cr.
    // Y=0, Cb=1.0, Cr=0 isolates the Cb coefficient (use 1.0 to read it off).
    let rgb = yuv_to_rgb(0.0, 1.0, 0.0, MatrixCoefficients::Bt2020Ncl).unwrap();
    assert!(
        (rgb[1] - (-0.164_553_13)).abs() < EPS,
        "Cb coeff = {}",
        rgb[1]
    );
    let rgb = yuv_to_rgb(0.0, 0.0, 1.0, MatrixCoefficients::Bt2020Ncl).unwrap();
    assert!(
        (rgb[1] - (-0.571_353_13)).abs() < EPS,
        "Cr coeff = {}",
        rgb[1]
    );
}

#[test]
fn matrix_is_invertible_roundtrip() {
    // YUV -> RGB -> YUV must return the original within tolerance for every
    // matrix and a spread of in-gamut samples.
    let samples = [
        (0.5_f32, 0.0_f32, 0.0_f32),
        (0.2, 0.1, -0.2),
        (0.8, -0.3, 0.25),
        (0.1, 0.4, 0.4),
        (0.95, -0.45, -0.45),
    ];
    for m in [
        MatrixCoefficients::Bt601,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Bt2020Ncl,
    ] {
        for (y, cb, cr) in samples {
            let rgb = yuv_to_rgb(y, cb, cr, m).unwrap();
            let back = rgb_to_yuv(rgb[0], rgb[1], rgb[2], m).unwrap();
            assert!((back[0] - y).abs() < EPS, "{m:?} Y {y} -> {}", back[0]);
            assert!((back[1] - cb).abs() < EPS, "{m:?} Cb {cb} -> {}", back[1]);
            assert!((back[2] - cr).abs() < EPS, "{m:?} Cr {cr} -> {}", back[2]);
        }
    }
}

#[test]
fn pure_red_in_bt709_has_expected_luma() {
    // Gamma R'=1, G'=B'=0 -> Y = Kr = 0.2126 in BT.709.
    let yuv = rgb_to_yuv(1.0, 0.0, 0.0, MatrixCoefficients::Bt709).unwrap();
    assert!((yuv[0] - 0.2126).abs() < EPS, "Y = {}", yuv[0]);
}
