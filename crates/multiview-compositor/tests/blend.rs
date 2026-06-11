//! Integration tests for premultiplied-alpha source-over in linear light
//! (invariant #8 step 5 / ADR-C003).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::blend::{over, LinearRgba, PremulRgba};

const EPS: f32 = 1e-6;

#[test]
fn opaque_source_fully_replaces_destination() {
    let src = LinearRgba::opaque(0.2, 0.4, 0.6).premultiplied();
    let dst = LinearRgba::opaque(0.9, 0.9, 0.9).premultiplied();
    let out = over(src, dst).unpremultiplied();
    assert!((out.r - 0.2).abs() < EPS);
    assert!((out.g - 0.4).abs() < EPS);
    assert!((out.b - 0.6).abs() < EPS);
    assert!((out.a - 1.0).abs() < EPS);
}

#[test]
fn transparent_source_leaves_destination_untouched() {
    let src = LinearRgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 0.0,
    }
    .premultiplied();
    let dst = LinearRgba::opaque(0.3, 0.5, 0.7).premultiplied();
    let out = over(src, dst).unpremultiplied();
    assert!((out.r - 0.3).abs() < EPS);
    assert!((out.g - 0.5).abs() < EPS);
    assert!((out.b - 0.7).abs() < EPS);
    assert!((out.a - 1.0).abs() < EPS);
}

#[test]
fn over_transparent_destination_is_identity() {
    let src = LinearRgba {
        r: 0.4,
        g: 0.5,
        b: 0.6,
        a: 0.5,
    }
    .premultiplied();
    let out = over(src, PremulRgba::TRANSPARENT);
    // Result equals the source (premultiplied) over nothing.
    assert!((out.r - 0.4 * 0.5).abs() < EPS);
    assert!((out.a - 0.5).abs() < EPS);
}

#[test]
fn half_opaque_white_over_opaque_black_is_linear_midpoint() {
    // 50%-opaque white over opaque black -> straight RGB 0.5, alpha 1.0,
    // because the blend is in LINEAR light (this is the whole point of
    // ADR-C003: the average happens on linear values, not gamma).
    let src = LinearRgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 0.5,
    }
    .premultiplied();
    let dst = LinearRgba::opaque(0.0, 0.0, 0.0).premultiplied();
    let out = over(src, dst).unpremultiplied();
    assert!((out.r - 0.5).abs() < EPS, "r = {}", out.r);
    assert!((out.g - 0.5).abs() < EPS);
    assert!((out.b - 0.5).abs() < EPS);
    assert!((out.a - 1.0).abs() < EPS);
}

#[test]
fn premultiply_unpremultiply_roundtrip() {
    let c = LinearRgba {
        r: 0.3,
        g: 0.6,
        b: 0.9,
        a: 0.4,
    };
    let back = c.premultiplied().unpremultiplied();
    assert!((back.r - c.r).abs() < EPS);
    assert!((back.g - c.g).abs() < EPS);
    assert!((back.b - c.b).abs() < EPS);
    assert!((back.a - c.a).abs() < EPS);
}

#[test]
fn alpha_composes_associatively_for_coverage() {
    // Stacking two 50% layers yields the standard 1 - (1-a)^2 = 0.75 coverage.
    let layer = LinearRgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 0.5,
    }
    .premultiplied();
    let stacked = over(layer, over(layer, PremulRgba::TRANSPARENT));
    assert!((stacked.a - 0.75).abs() < EPS, "alpha = {}", stacked.a);
}
