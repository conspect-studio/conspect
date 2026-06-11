//! Property tests for the pure color math: round-trips that must hold for the
//! whole input domain, not just hand-picked points (invariant #8). Regressions
//! are committed under `proptest-regressions/`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::matrix::{rgb_to_yuv, yuv_to_rgb};
use multiview_compositor::primaries::{apply, convert_matrix};
use multiview_compositor::range::{compress_chroma, compress_luma, expand_chroma, expand_luma};
use multiview_compositor::transfer::{
    bt709_camera_oetf, bt709_camera_oetf_inverse, bt709_eotf, bt709_oetf_inverse, hlg_eotf,
    hlg_oetf, pq_eotf, pq_oetf, srgb_eotf, srgb_oetf,
};
use multiview_core::color::{ColorPrimaries, ColorRange, MatrixCoefficients};
use proptest::prelude::*;

proptest! {
    /// Every 8-bit luma code on the limited grid round-trips exactly.
    #[test]
    fn prop_luma_roundtrip_limited(code in 16_u8..=235) {
        let back = compress_luma(expand_luma(code, ColorRange::Limited), ColorRange::Limited);
        prop_assert_eq!(back, code);
    }

    /// Every 8-bit luma code round-trips exactly in full range.
    #[test]
    fn prop_luma_roundtrip_full(code in 0_u8..=255) {
        let back = compress_luma(expand_luma(code, ColorRange::Full), ColorRange::Full);
        prop_assert_eq!(back, code);
    }

    /// Every 8-bit chroma code on the limited grid round-trips exactly.
    #[test]
    fn prop_chroma_roundtrip_limited(code in 16_u8..=240) {
        let back = compress_chroma(expand_chroma(code, ColorRange::Limited), ColorRange::Limited);
        prop_assert_eq!(back, code);
    }

    /// The sRGB EOTF/OETF pair round-trips within tolerance across [0, 1].
    #[test]
    fn prop_srgb_roundtrip(c in 0.0_f32..=1.0) {
        let back = srgb_oetf(srgb_eotf(c));
        prop_assert!((back - c).abs() < 1e-3, "{} vs {}", back, c);
    }

    /// The BT.1886 display EOTF/inverse pair round-trips within tolerance.
    #[test]
    fn prop_bt709_display_roundtrip(c in 0.0_f32..=1.0) {
        let back = bt709_oetf_inverse(bt709_eotf(c));
        prop_assert!((back - c).abs() < 1e-3, "{} vs {}", back, c);
    }

    /// The BT.709 camera OETF/inverse pair round-trips within tolerance.
    #[test]
    fn prop_bt709_camera_roundtrip(l in 0.0_f32..=1.0) {
        let back = bt709_camera_oetf_inverse(bt709_camera_oetf(l));
        prop_assert!((back - l).abs() < 1e-3, "{} vs {}", back, l);
    }

    /// The PQ EOTF/OETF pair round-trips within tolerance across [0, 1].
    #[test]
    fn prop_pq_roundtrip(e in 0.0_f32..=1.0) {
        let back = pq_oetf(pq_eotf(e));
        prop_assert!((back - e).abs() < 2e-3, "{} vs {}", back, e);
    }

    /// The HLG OETF/inverse pair round-trips within tolerance across [0, 1].
    #[test]
    fn prop_hlg_roundtrip(l in 0.0_f32..=1.0) {
        let back = hlg_eotf(hlg_oetf(l));
        prop_assert!((back - l).abs() < 2e-3, "{} vs {}", back, l);
    }

    /// The YUV<->RGB matrix is invertible for every matrix and in-gamut sample.
    #[test]
    fn prop_matrix_roundtrip(
        y in 0.0_f32..=1.0,
        cb in -0.5_f32..=0.5,
        cr in -0.5_f32..=0.5,
        which in 0_u8..3,
    ) {
        let m = match which {
            0 => MatrixCoefficients::Bt601,
            1 => MatrixCoefficients::Bt709,
            _ => MatrixCoefficients::Bt2020Ncl,
        };
        let rgb = yuv_to_rgb(y, cb, cr, m).unwrap();
        let back = rgb_to_yuv(rgb[0], rgb[1], rgb[2], m).unwrap();
        prop_assert!((back[0] - y).abs() < 1e-3, "Y {} vs {}", back[0], y);
        prop_assert!((back[1] - cb).abs() < 1e-3, "Cb {} vs {}", back[1], cb);
        prop_assert!((back[2] - cr).abs() < 1e-3, "Cr {} vs {}", back[2], cr);
    }

    /// 709 -> 2020 -> 709 gamut round-trip is the identity within tolerance.
    #[test]
    fn prop_gamut_roundtrip(r in 0.0_f32..=1.0, g in 0.0_f32..=1.0, b in 0.0_f32..=1.0) {
        let to_2020 = convert_matrix(ColorPrimaries::Bt709, ColorPrimaries::Bt2020).unwrap();
        let to_709 = convert_matrix(ColorPrimaries::Bt2020, ColorPrimaries::Bt709).unwrap();
        let back = apply(to_709, apply(to_2020, [r, g, b]));
        prop_assert!((back[0] - r).abs() < 1e-3, "R {} vs {}", back[0], r);
        prop_assert!((back[1] - g).abs() < 1e-3, "G {} vs {}", back[1], g);
        prop_assert!((back[2] - b).abs() < 1e-3, "B {} vs {}", back[2], b);
    }
}
