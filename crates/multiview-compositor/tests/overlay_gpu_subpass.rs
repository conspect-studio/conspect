//! GPU-free static validation + packing checks for the overlay sub-pass
//! (Stage 2, feature `overlay` + `wgpu`).
//!
//! There is no GPU adapter in CI, so `naga` validates the overlay WGSL without
//! one (the gate that keeps the batched-blend shader honest), and the CPU-side
//! primitive packing is asserted to mirror the shader's `OverlayPrim` layout.
#![cfg(all(feature = "overlay", feature = "wgpu"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_compositor::gpu::shader;
use multiview_compositor::overlay::gpu_subpass::{
    OverlayPrimGpu, OverlaySubpass, PrimitiveKind, MAX_OVERLAY_PRIMS,
};
use multiview_compositor::overlay::subpass::{
    meter_bar, OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect,
};

#[test]
fn overlay_shader_parses_and_validates_with_naga() {
    shader::validate_overlay_shader().expect("overlay.wgsl must validate with naga");
}

#[test]
fn overlay_shader_is_included_in_the_full_validate_set() {
    // The standalone module validator must also accept it (no GPU).
    shader::validate_module("overlay.wgsl", &shader::overlay_wgsl())
        .expect("overlay.wgsl validates individually");
}

#[test]
fn packing_a_filled_rect_carries_geometry_and_rect_kind() {
    let prim = OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(7, 9, 12, 4),
        corner_radius: 3,
        color: OverlayColor::opaque(0.0, 1.0, 0.0),
    };
    let packed = OverlayPrimGpu::pack(&prim, 0, 0);
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Rect.as_u32());
    assert_eq!(packed.kind_meta[1], 3, "corner radius preserved");
    assert_eq!(packed.rect, [7, 9, 12, 4], "dest + size preserved");
    assert_eq!(packed.color, [0.0, 1.0, 0.0, 1.0]);
}

#[test]
fn packing_a_glyph_carries_its_atlas_slot() {
    let prim = OverlayPrimitive::Glyph {
        dest_x: 3,
        dest_y: 4,
        width: 8,
        height: 10,
        coverage: vec![0; 8 * 10],
        color: OverlayColor::opaque(1.0, 1.0, 1.0),
    };
    let packed = OverlayPrimGpu::pack(&prim, 64, 128);
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Glyph.as_u32());
    assert_eq!(
        (packed.kind_meta[2], packed.kind_meta[3]),
        (64, 128),
        "atlas xy"
    );
    assert_eq!(packed.rect, [3, 4, 8, 10]);
}

#[test]
fn meter_bar_packs_to_an_analytic_rect_not_a_bitmap() {
    let prim = meter_bar(
        OverlayRect::new(0, 0, 20, 4),
        0.5,
        false,
        OverlayColor::opaque(1.0, 0.0, 0.0),
    );
    let packed = OverlayPrimGpu::pack(&prim, 0, 0);
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Rect.as_u32());
    assert_eq!(packed.rect[2], 10, "50% of a 20px track fills 10 columns");
}

#[test]
fn packing_a_stroke_carries_its_endpoints_and_thickness() {
    let prim = OverlayPrimitive::Stroke {
        x0: 10.0,
        y0: 20.0,
        x1: 40.0,
        y1: 20.0,
        half_thickness: 2.5,
        color: OverlayColor::opaque(1.0, 1.0, 1.0),
    };
    let packed = OverlayPrimGpu::pack(&prim, 0, 0);
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Stroke.as_u32());
    // The half-thickness rides the y meta slot as f32 bits (the shader bitcasts).
    assert_eq!(
        f32::from_bits(packed.kind_meta[1]),
        2.5,
        "thickness round-trips"
    );
    assert_eq!(packed.geom, [10.0, 20.0, 40.0, 20.0], "endpoints preserved");
    // The bounding box covers the segment padded by the radius (+1 AA px).
    assert!(packed.rect[0] <= 7, "bbox left padded past x0");
    assert!(
        packed.rect[0] + packed.rect[2] >= 43,
        "bbox right padded past x1"
    );
}

#[test]
fn packing_a_ring_carries_its_centre_and_band() {
    let prim = OverlayPrimitive::Ring {
        cx: 32.0,
        cy: 32.0,
        outer_radius: 24.0,
        thickness: 4.0,
        color: OverlayColor::opaque(1.0, 1.0, 1.0),
    };
    let packed = OverlayPrimGpu::pack(&prim, 0, 0);
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Ring.as_u32());
    // geom = (cx, cy, mid_radius = outer - band_half, band_half = thickness/2).
    assert_eq!(packed.geom[0], 32.0, "centre x");
    assert_eq!(packed.geom[1], 32.0, "centre y");
    assert_eq!(packed.geom[2], 22.0, "mid radius = 24 - 2");
    assert_eq!(packed.geom[3], 2.0, "band half = thickness/2");
    // The bounding box spans the whole bezel (centre ± outer radius + AA).
    assert!(packed.rect[0] <= 7, "bbox left ≈ cx - outer");
}

#[test]
fn pack_list_skips_glyphs_with_no_resident_atlas_slot() {
    let mut list = OverlayDrawList::new();
    // index 0: a rect (always packs); index 1: a glyph whose slot resolver says
    // "not resident" → skipped (hold last-good, never crash).
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(0, 0, 4, 4),
        corner_radius: 0,
        color: OverlayColor::opaque(1.0, 1.0, 1.0),
    });
    list.push(OverlayPrimitive::Glyph {
        dest_x: 0,
        dest_y: 0,
        width: 4,
        height: 4,
        coverage: vec![0; 16],
        color: OverlayColor::opaque(1.0, 1.0, 1.0),
    });
    let packed = OverlaySubpass::pack_list(&list, |_| None);
    assert_eq!(packed.len(), 1, "only the rect packs; the glyph is skipped");
    assert_eq!(packed[0].kind_meta[0], PrimitiveKind::Rect.as_u32());
}

#[test]
fn pack_list_truncates_at_the_bounded_prim_cap() {
    // Queue more primitives than the cap; packing must stop at MAX_OVERLAY_PRIMS
    // (bounded data-plane memory, never per-frame growth — T4 / ADR-E005).
    let over = usize::try_from(MAX_OVERLAY_PRIMS).unwrap() + 32;
    let mut list = OverlayDrawList::new();
    for _ in 0..over {
        list.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(0, 0, 2, 2),
            corner_radius: 0,
            color: OverlayColor::opaque(1.0, 1.0, 1.0),
        });
    }
    let packed = OverlaySubpass::pack_list(&list, |_| Some((0, 0)));
    assert_eq!(
        u32::try_from(packed.len()).unwrap(),
        MAX_OVERLAY_PRIMS,
        "packing is capped at the bounded primitive count"
    );
}
