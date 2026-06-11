//! Integration tests for linear-light gamut conversion (invariant #8 step 4).
//! Pins the derived BT.709<->BT.2020 matrices against the BT.2087 reference
//! values (color-management.md §4.5), identity for same-gamut, and the
//! 709 -> 2020 -> 709 round-trip.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::primaries::{apply, convert_matrix, rgb_to_xyz, IDENTITY};
use multiview_core::color::ColorPrimaries;

const EPS: f64 = 1e-5;

fn assert_mat_close(actual: [[f64; 3]; 3], expected: [[f64; 3]; 3]) {
    for r in 0..3 {
        for c in 0..3 {
            assert!(
                (actual[r][c] - expected[r][c]).abs() < EPS,
                "[{r}][{c}]: {} vs {}",
                actual[r][c],
                expected[r][c]
            );
        }
    }
}

#[test]
fn bt709_to_bt2020_matches_bt2087_reference() {
    // Reference values (BT.2087, also produced by a clean NPM derivation).
    let expected = [
        [0.627_403_895_935, 0.329_283_038_378, 0.043_313_065_687],
        [0.069_097_289_358, 0.919_540_395_075, 0.011_362_315_566],
        [0.016_391_438_875, 0.088_013_307_877, 0.895_595_253_248],
    ];
    let m = convert_matrix(ColorPrimaries::Bt709, ColorPrimaries::Bt2020).unwrap();
    assert_mat_close(m, expected);
}

#[test]
fn bt2020_to_bt709_matches_reference() {
    let expected = [
        [1.660_491_002_108, -0.587_641_138_789, -0.072_849_863_320],
        [-0.124_550_474_522, 1.132_899_897_126, -0.008_349_422_604],
        [-0.018_150_763_355, -0.100_578_898_008, 1.118_729_661_363],
    ];
    let m = convert_matrix(ColorPrimaries::Bt2020, ColorPrimaries::Bt709).unwrap();
    assert_mat_close(m, expected);
}

#[test]
fn same_gamut_is_identity() {
    let m = convert_matrix(ColorPrimaries::Bt709, ColorPrimaries::Bt709).unwrap();
    assert_mat_close(m, IDENTITY);
    // And applying identity is a no-op.
    let rgb = apply(IDENTITY, [0.2, 0.5, 0.9]);
    assert!((rgb[0] - 0.2).abs() < 1e-6);
    assert!((rgb[1] - 0.5).abs() < 1e-6);
    assert!((rgb[2] - 0.9).abs() < 1e-6);
}

#[test]
fn gamut_roundtrip_709_2020_709_is_identity() {
    let to_2020 = convert_matrix(ColorPrimaries::Bt709, ColorPrimaries::Bt2020).unwrap();
    let to_709 = convert_matrix(ColorPrimaries::Bt2020, ColorPrimaries::Bt709).unwrap();
    let samples = [[0.2_f32, 0.5, 0.9], [1.0, 0.0, 0.0], [0.33, 0.33, 0.33]];
    for s in samples {
        let wide = apply(to_2020, s);
        let back = apply(to_709, wide);
        for i in 0..3 {
            assert!(
                (back[i] - s[i]).abs() < 1e-4,
                "channel {i}: {} vs {}",
                back[i],
                s[i]
            );
        }
    }
}

#[test]
fn npm_white_point_maps_to_d65_xyz() {
    // RGB (1,1,1) through the NPM must give the D65 white point XYZ (Y == 1).
    let m = rgb_to_xyz(ColorPrimaries::Bt709).unwrap();
    let y = m[1][0] + m[1][1] + m[1][2];
    assert!((y - 1.0).abs() < EPS, "white luminance = {y}");
    // X/Z of D65 white at unit luminance: x/y and (1-x-y)/y for D65 (0.3127,0.3290).
    let white_x = m[0][0] + m[0][1] + m[0][2];
    assert!(
        (white_x - 0.312_7 / 0.329_0).abs() < 1e-4,
        "white X = {white_x}"
    );
}

#[test]
fn unsupported_primaries_error() {
    assert!(convert_matrix(ColorPrimaries::Unspecified, ColorPrimaries::Bt709).is_err());
    assert!(rgb_to_xyz(ColorPrimaries::Unspecified).is_err());
}

#[test]
fn sd_to_hd_gamut_is_not_identity() {
    // NTSC (BT.601-525) into a BT.709 canvas is a real gamut conversion.
    let m = convert_matrix(ColorPrimaries::Bt601_525, ColorPrimaries::Bt709).unwrap();
    assert_mat_close_not_identity(m);
}

fn assert_mat_close_not_identity(m: [[f64; 3]; 3]) {
    let mut max_off_diag = 0.0_f64;
    for (r, row) in m.iter().enumerate() {
        for (c, value) in row.iter().enumerate() {
            if r != c {
                max_off_diag = max_off_diag.max(value.abs());
            }
        }
    }
    assert!(
        max_off_diag > 1e-3,
        "expected a non-identity gamut matrix, off-diagonal max = {max_off_diag}"
    );
}
