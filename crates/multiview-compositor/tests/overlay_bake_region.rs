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
fn large_overlay_lists_and_overlap_never_reject_valid_frames() {
    let input = vivid_input(1920, 1080);
    let mut list = OverlayDrawList::new();
    for n in 0..1025 {
        list.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(0, 0, 96, 96),
            corner_radius: 0,
            color: OverlayColor::new(0.2, 0.6, 0.4, 0.001),
        });
        if n == 1024 {
            let out = apply_overlays_to_nv12(&input, &list, CanvasColor::default()).unwrap();
            let reference =
                apply_overlays_to_nv12_reference(&input, &list, CanvasColor::default()).unwrap();
            assert_eq!(out.y_plane(), reference.y_plane());
            assert_eq!(out.uv_plane(), reference.uv_plane());
        }
    }
}

#[test]
fn overlapping_1080_regions_exceeding_legacy_budget_keep_every_overlay() {
    let input = vivid_input(1920, 1080);
    let mut list = OverlayDrawList::new();
    for i in 0..20 {
        list.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(i * 3, 10, 480, 300),
            corner_radius: 0,
            color: OverlayColor::new(0.1, 0.8, 0.2, 0.2),
        });
    }
    let out = apply_overlays_to_nv12(&input, &list, CanvasColor::default()).unwrap();
    let reference =
        apply_overlays_to_nv12_reference(&input, &list, CanvasColor::default()).unwrap();
    assert_eq!(out.y_plane(), reference.y_plane());
    assert_eq!(out.uv_plane(), reference.uv_plane());
}

#[test]
fn four_k_overlay_and_color_tag_mismatch_do_not_hit_fixed_1080_budget() {
    let input = vivid_input(3840, 2160);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(10, 10, 32, 24),
        corner_radius: 0,
        color: OverlayColor::opaque(0.2, 0.6, 0.4),
    });
    assert!(apply_overlays_to_nv12(&input, &list, CanvasColor::default()).is_ok());
    let mismatch = Nv12Image::new(
        3840,
        2160,
        input.y_plane().to_vec(),
        input.uv_plane().to_vec(),
        ColorInfo {
            range: ColorRange::Full,
            ..bt709_limited()
        },
    )
    .unwrap();
    assert!(apply_overlays_to_nv12(&mismatch, &list, CanvasColor::default()).is_ok());
}

#[test]
fn sparse_bake_covers_news_style_badges_meters_glyphs_and_clipped_captions() {
    let input = vivid_input(1920, 1080);
    let mut list = OverlayDrawList::new();
    for (x, y) in [(7, 11), (501, 15), (1003, 541), (1677, 925)] {
        list.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(x, y, 110, 33),
            corner_radius: 4,
            color: OverlayColor::new(0.1, 0.3, 0.8, 0.7),
        });
        list.push(OverlayPrimitive::Glyph {
            dest_x: x + 4,
            dest_y: y + 3,
            width: 17,
            height: 13,
            coverage: (0..221).map(|i| (i * 37 % 256) as u8).collect(),
            color: OverlayColor::opaque(1.0, 1.0, 1.0),
        });
        list.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(x + 3, y + 23, 92, 5),
            corner_radius: 0,
            color: OverlayColor::new(0.0, 1.0, 0.0, 0.8),
        });
    }
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(-10, 1050, 220, 48),
        src_width: 2,
        src_height: 2,
        rgba: vec![
            120, 50, 25, 180, 0, 0, 0, 0, 40, 80, 100, 200, 90, 20, 10, 120,
        ],
        alpha: 0.7,
    });
    let canvas = CanvasColor::default();
    let out = apply_overlays_to_nv12(&input, &list, canvas).unwrap();
    let reference = apply_overlays_to_nv12_reference(&input, &list, canvas).unwrap();
    // An untouched sample passes through. A sample inside any even-aligned
    // footprint matches the full-frame oracle, even where primitives overlap.
    // Check Y and both chroma bytes in every NV12 block.
    let rects = [
        (7, 11, 110, 33),
        (501, 15, 110, 33),
        (1003, 541, 110, 33),
        (1677, 925, 110, 33),
        (11, 14, 17, 13),
        (505, 18, 17, 13),
        (1007, 544, 17, 13),
        (1681, 928, 17, 13),
        (10, 34, 92, 5),
        (504, 38, 92, 5),
        (1006, 564, 92, 5),
        (1680, 948, 92, 5),
        (-10, 1050, 220, 48),
    ];
    for y in 0..1080usize {
        for x in 0..1920usize {
            let inside = rects.iter().any(|&(rx, ry, rw, rh)| {
                let x = x as i32;
                let y = y as i32;
                x >= (rx & !1)
                    && x < ((rx + rw + 1) & !1)
                    && y >= (ry & !1)
                    && y < ((ry + rh + 1) & !1)
            });
            let index = y * 1920 + x;
            assert_eq!(
                out.y_plane()[index],
                if inside {
                    reference.y_plane()[index]
                } else {
                    input.y_plane()[index]
                },
                "Y ({x},{y})"
            );
            if x % 2 == 0 && y % 2 == 0 {
                let uv_index = y / 2 * 1920 + x;
                for i in 0..2 {
                    assert_eq!(
                        out.uv_plane()[uv_index + i],
                        if inside {
                            reference.uv_plane()[uv_index + i]
                        } else {
                            input.uv_plane()[uv_index + i]
                        },
                        "UV ({x},{y}) channel {i}"
                    );
                }
            }
        }
    }
}

#[test]
fn merged_footprints_keep_both_corner_holes_exact_for_all_sdr_tags() {
    for (primaries, transfer, matrix, range) in [
        (
            ColorPrimaries::Bt709,
            TransferCharacteristic::Bt709,
            MatrixCoefficients::Bt709,
            ColorRange::Limited,
        ),
        (
            ColorPrimaries::Bt709,
            TransferCharacteristic::Bt709,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
        ),
        (
            ColorPrimaries::Bt601_525,
            TransferCharacteristic::Bt709,
            MatrixCoefficients::Bt601,
            ColorRange::Limited,
        ),
    ] {
        let tag = ColorInfo {
            primaries,
            transfer,
            matrix,
            range,
        };
        let w = 48usize;
        let h = 48usize;
        let y: Vec<u8> = (0..w * h).map(|i| [0, 255, 16, 235][i % 4]).collect();
        let uv: Vec<u8> = (0..w * h / 2).map(|i| [0, 255, 16, 240][i % 4]).collect();
        let input = Nv12Image::new(w as u32, h as u32, y, uv, tag).unwrap();
        let mut list = OverlayDrawList::new();
        for (x, y) in [(4, 4), (14, 14)] {
            list.push(OverlayPrimitive::FilledRect {
                rect: OverlayRect::new(x, y, 20, 20),
                corner_radius: 0,
                color: OverlayColor::new(0.9, 0.2, 0.1, 0.75),
            });
        }
        let canvas = CanvasColor {
            primaries,
            transfer,
            matrix,
            range,
        };
        let output = apply_overlays_to_nv12(&input, &list, canvas).unwrap();
        let baseline = apply_overlays_to_nv12_reference(&input, &list, canvas).unwrap();
        let mut inked = false;
        for by in (0..h).step_by(2) {
            for bx in (0..w).step_by(2) {
                let covered = (4..24).contains(&bx) && (4..24).contains(&by)
                    || (14..34).contains(&bx) && (14..34).contains(&by);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let i = (by + dy) * w + bx + dx;
                        assert_eq!(
                            output.y_plane()[i],
                            if covered {
                                baseline.y_plane()[i]
                            } else {
                                input.y_plane()[i]
                            },
                            "Y tag {tag:?} ({bx},{by})"
                        );
                        inked |= covered && output.y_plane()[i] != input.y_plane()[i];
                    }
                }
                let i = by / 2 * w + bx;
                for k in 0..2 {
                    assert_eq!(
                        output.uv_plane()[i + k],
                        if covered {
                            baseline.uv_plane()[i + k]
                        } else {
                            input.uv_plane()[i + k]
                        },
                        "UV tag {tag:?} ({bx},{by})"
                    );
                }
            }
        }
        assert!(inked, "visible overlay must survive for {tag:?}");
    }
}

#[test]
fn dense_chain_and_disjoint_lists_fall_back_without_losing_ink() {
    let input = vivid_input(128, 64);
    for disjoint in [false, true] {
        let mut list = OverlayDrawList::new();
        for i in 0..4096 {
            let x = if disjoint { (i % 64) * 2 } else { (i % 32) * 2 };
            let y = if disjoint { (i / 64) % 32 * 2 } else { 8 };
            list.push(OverlayPrimitive::FilledRect {
                rect: OverlayRect::new(x, y, if disjoint { 2 } else { 8 }, 2),
                corner_radius: 0,
                color: OverlayColor::opaque(0.1, 0.8, 0.2),
            });
        }
        let out = apply_overlays_to_nv12(&input, &list, CanvasColor::default()).unwrap();
        let reference =
            apply_overlays_to_nv12_reference(&input, &list, CanvasColor::default()).unwrap();
        assert_eq!(out.y_plane(), reference.y_plane());
        assert_eq!(out.uv_plane(), reference.uv_plane());
        assert_ne!(
            out.y_plane(),
            input.y_plane(),
            "fallback must not discard overlays"
        );
    }
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(0, 0, 128, 64),
        corner_radius: 0,
        color: OverlayColor::opaque(0.1, 0.8, 0.2),
    });
    let out = apply_overlays_to_nv12(&input, &list, CanvasColor::default()).unwrap();
    assert_ne!(
        out.y_plane(),
        input.y_plane(),
        "the admitted style must remain visible"
    );
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
