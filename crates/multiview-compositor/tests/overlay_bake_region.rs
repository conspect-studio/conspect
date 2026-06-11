//! Region-limited overlay bake (ADR-0023).
//!
//! `apply_overlays_to_nv12` must round-trip colour through only the overlay
//! **dirty region** and pass every other pixel through the NV12 planes
//! **byte-identically** — invariant #5 (no full-frame RGBA round-trip). The old
//! bake re-converted the whole canvas YUV->linear->YUV every frame, so a pixel
//! no overlay touched was needlessly (and lossily) round-tripped. These tests
//! pin the new contract:
//!
//! - an empty draw list is a byte-identical no-op;
//! - a small overlay changes only the pixels inside its even-aligned dirty
//!   region and leaves the rest of the Y/UV planes byte-for-byte intact.
//!
//! These are **output-preservation guards**: the full-canvas round-trip the old
//! bake performed is *effectively lossless* for in-range SDR (so it already
//! produced these outputs — the defect was performance, not pixels, ADR-0023),
//! and the region-limited bake must keep producing them exactly. The matching
//! perf win is asserted in `benches/overlay_efficiency.rs`; the LUT fidelity
//! inside the dirty region is pinned by `bake_matches_the_full_canvas_reference`
//! below.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap
)]

use multiview_compositor::overlay::subpass::{
    apply_overlays_to_nv12, apply_overlays_to_nv12_reference, OverlayColor, OverlayDrawList,
    OverlayPrimitive, OverlayRect,
};
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
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

/// A vivid, varied NV12 image with saturated chroma, so the lossy full-canvas
/// YUV->linear->YUV round-trip the OLD bake performed shifts code values on
/// untouched pixels — i.e. these tests are genuinely red against that bake.
fn vivid_input(w: u32, h: u32) -> Nv12Image {
    let wu = w as usize;
    let hu = h as usize;
    let mut y = vec![0u8; wu * hu];
    for yy in 0..hu {
        for xx in 0..wu {
            y[yy * wu + xx] = (16 + ((xx * 9 + yy * 31) % 219)) as u8;
        }
    }
    let mut uv = vec![128u8; wu * hu / 2];
    for cy in 0..hu / 2 {
        for cx in 0..wu / 2 {
            let i = cy * wu + cx * 2;
            uv[i] = (16 + (cx * 17) % 219) as u8; // cb: saturated sweep
            uv[i + 1] = (235 - (cy * 23) % 219) as u8; // cr: saturated sweep
        }
    }
    Nv12Image::new(w, h, y, uv, bt709_limited()).unwrap()
}

#[test]
fn bake_with_no_overlays_is_a_byte_identical_no_op() {
    let input = vivid_input(32, 16);
    let list = OverlayDrawList::new();
    let out = apply_overlays_to_nv12(&input, &list, CanvasColor::default()).unwrap();
    assert_eq!(
        out.y_plane(),
        input.y_plane(),
        "an empty overlay list must leave the Y plane byte-identical (inv #5)"
    );
    assert_eq!(
        out.uv_plane(),
        input.uv_plane(),
        "an empty overlay list must leave the UV plane byte-identical (inv #5)"
    );
}

#[test]
fn bake_touches_only_the_overlay_dirty_region() {
    let input = vivid_input(32, 16);
    let mut list = OverlayDrawList::new();
    // Opaque green rect, even-aligned on every edge, well inside the canvas:
    // spans x[8,16) y[4,10); chroma blocks cx[4,8) cy[2,5).
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(8, 4, 8, 6),
        corner_radius: 0,
        color: OverlayColor::opaque(0.0, 1.0, 0.0),
    });
    let out = apply_overlays_to_nv12(&input, &list, CanvasColor::default()).unwrap();

    let w = 32usize;
    let mut changed_inside = 0u32;
    for yy in 0..16usize {
        for xx in 0..32usize {
            let inside = (8..16).contains(&xx) && (4..10).contains(&yy);
            let before = input.y_plane()[yy * w + xx];
            let after = out.y_plane()[yy * w + xx];
            if inside {
                if after != before {
                    changed_inside += 1;
                }
            } else {
                assert_eq!(
                    after, before,
                    "Y pixel ({xx},{yy}) is outside the overlay and must be byte-identical"
                );
            }
        }
    }
    assert!(
        changed_inside > 0,
        "the overlay must actually change pixels inside its region"
    );

    for cy in 0..8usize {
        for cx in 0..16usize {
            let inside = (4..8).contains(&cx) && (2..5).contains(&cy);
            if !inside {
                let i = cy * w + cx * 2;
                assert_eq!(
                    out.uv_plane()[i],
                    input.uv_plane()[i],
                    "cb block ({cx},{cy}) is outside the overlay and must be byte-identical"
                );
                assert_eq!(
                    out.uv_plane()[i + 1],
                    input.uv_plane()[i + 1],
                    "cr block ({cx},{cy}) is outside the overlay and must be byte-identical"
                );
            }
        }
    }
}

/// The region-limited bake must reproduce the full-canvas oracle reference
/// exactly inside the overlay rects (same oracle, same raster-order chroma) and
/// pass the input through outside them — exercising every primitive kind so the
/// dirty-rect cover is validated end-to-end (a too-tight rect would clip a
/// primitive's edge: there opt==input but the reference shows the overlay, so the
/// assertion below fails).
#[test]
fn bake_matches_the_full_canvas_reference_inside_rects_and_passes_through_outside() {
    let input = vivid_input(64, 48);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(10, 8, 20, 12),
        corner_radius: 3,
        color: OverlayColor::opaque(0.0, 1.0, 0.0),
    });
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(0, 0, 64, 2),
        color: OverlayColor::opaque(1.0, 0.0, 0.0),
    });
    list.push(OverlayPrimitive::Stroke {
        x0: 5.0,
        y0: 6.0,
        x1: 40.0,
        y1: 30.0,
        half_thickness: 2.0,
        color: OverlayColor::new(1.0, 1.0, 1.0, 0.8),
    });
    list.push(OverlayPrimitive::Ring {
        cx: 48.0,
        cy: 22.0,
        outer_radius: 10.0,
        thickness: 3.0,
        color: OverlayColor::opaque(0.2, 0.4, 1.0),
    });
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(28, 30, 8, 8),
        src_width: 2,
        src_height: 2,
        rgba: vec![255u8; 2 * 2 * 4],
        alpha: 1.0,
    });
    list.push(OverlayPrimitive::Glyph {
        dest_x: 52,
        dest_y: 38,
        width: 4,
        height: 4,
        coverage: vec![200u8; 4 * 4],
        color: OverlayColor::opaque(1.0, 1.0, 0.0),
    });

    let canvas = CanvasColor::default();
    let opt = apply_overlays_to_nv12(&input, &list, canvas).unwrap();
    let reference = apply_overlays_to_nv12_reference(&input, &list, canvas).unwrap();

    // Each pixel is either (a) byte-identical to the oracle reference — inside an
    // overlay rect, where we run the identical oracle round-trip/blend — or (b)
    // exact passthrough of the input — outside every rect, where the OLD
    // full-canvas reference merely round-trip-drifts a code value or two while our
    // passthrough is bit-exact. A clipped overlay pixel satisfies neither: opt==
    // input but the reference shows the overlay, so this catches a too-tight rect.
    let check = |o: u8, r: u8, inp: u8, plane: &str, i: usize| {
        assert!(
            o == r || o == inp,
            "{plane}[{i}] opt {o} ref {r} input {inp}: neither reference-exact nor passthrough"
        );
    };
    assert_eq!(opt.y_plane().len(), reference.y_plane().len());
    for i in 0..opt.y_plane().len() {
        check(
            opt.y_plane()[i],
            reference.y_plane()[i],
            input.y_plane()[i],
            "Y",
            i,
        );
    }
    for i in 0..opt.uv_plane().len() {
        check(
            opt.uv_plane()[i],
            reference.uv_plane()[i],
            input.uv_plane()[i],
            "UV",
            i,
        );
    }
}
