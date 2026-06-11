//! Integration tests for quantization-range expand/compress (invariant #8
//! step 1). Pins the exact 8-bit limited/full numerics from
//! color-management.md §4.2 and the expand<->compress round-trip.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: this test asserts two code paths produce the *bit-identical* f32 (the
// Unspecified arm and the Limited arm share one expression), so exact float
// equality is the correct assertion here, not an epsilon compare.
#![allow(clippy::float_cmp)]

use multiview_compositor::range::{
    compress_chroma, compress_luma, expand_chroma, expand_luma, require_resolved,
};
use multiview_core::color::ColorRange;

const EPS: f32 = 1e-5;

#[test]
fn limited_luma_edges_map_to_unit_interval() {
    // Black 16 -> 0.0, white 235 -> 1.0 (color-management.md §4.2).
    assert!((expand_luma(16, ColorRange::Limited) - 0.0).abs() < EPS);
    assert!((expand_luma(235, ColorRange::Limited) - 1.0).abs() < EPS);
    // Mid-scale 125.5 is not representable; 126 is just over half.
    let mid = expand_luma(126, ColorRange::Limited);
    assert!(mid > 0.5 && mid < 0.51, "mid = {mid}");
}

#[test]
fn full_luma_edges_map_to_unit_interval() {
    assert!((expand_luma(0, ColorRange::Full) - 0.0).abs() < EPS);
    assert!((expand_luma(255, ColorRange::Full) - 1.0).abs() < EPS);
}

#[test]
fn limited_chroma_center_and_edges() {
    // 128 is the neutral center -> 0.0 for both ranges.
    assert!((expand_chroma(128, ColorRange::Limited)).abs() < EPS);
    assert!((expand_chroma(128, ColorRange::Full)).abs() < EPS);
    // Limited chroma spans 16..240 -> [-0.5, 0.5].
    assert!((expand_chroma(16, ColorRange::Limited) - (-0.5)).abs() < EPS);
    assert!((expand_chroma(240, ColorRange::Limited) - 0.5).abs() < EPS);
}

#[test]
fn luma_expand_compress_roundtrip_all_codes_limited() {
    // Every 8-bit code that lies on the limited grid must survive the
    // expand -> compress round-trip exactly.
    for code in 16_u8..=235 {
        let normalized = expand_luma(code, ColorRange::Limited);
        let back = compress_luma(normalized, ColorRange::Limited);
        assert_eq!(back, code, "luma code {code} did not round-trip");
    }
}

#[test]
fn luma_expand_compress_roundtrip_all_codes_full() {
    for code in 0_u8..=255 {
        let normalized = expand_luma(code, ColorRange::Full);
        let back = compress_luma(normalized, ColorRange::Full);
        assert_eq!(back, code, "full luma code {code} did not round-trip");
    }
}

#[test]
fn chroma_expand_compress_roundtrip_limited() {
    for code in 16_u8..=240 {
        let normalized = expand_chroma(code, ColorRange::Limited);
        let back = compress_chroma(normalized, ColorRange::Limited);
        assert_eq!(back, code, "chroma code {code} did not round-trip");
    }
}

#[test]
fn compress_clamps_out_of_range_values() {
    // Super-white / super-black normalized values clamp to the byte range, not
    // wrap.
    assert_eq!(compress_luma(2.0, ColorRange::Limited), 255);
    assert_eq!(compress_luma(-1.0, ColorRange::Limited), 0);
    assert_eq!(compress_chroma(5.0, ColorRange::Full), 255);
    assert_eq!(compress_chroma(-5.0, ColorRange::Full), 0);
}

#[test]
fn unspecified_range_is_treated_as_limited() {
    // The kernel-facing functions are total: Unspecified follows the limited
    // (broadcast-video) convention.
    assert_eq!(
        expand_luma(16, ColorRange::Unspecified),
        expand_luma(16, ColorRange::Limited)
    );
}

#[test]
fn require_resolved_rejects_unspecified() {
    assert!(require_resolved(ColorRange::Unspecified).is_err());
    assert_eq!(
        require_resolved(ColorRange::Full).unwrap(),
        ColorRange::Full
    );
    assert_eq!(
        require_resolved(ColorRange::Limited).unwrap(),
        ColorRange::Limited
    );
}
