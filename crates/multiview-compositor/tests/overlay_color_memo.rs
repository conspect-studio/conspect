//! Exactness of the overlay bake's colour memos (`overlay::color_memo`).
//!
//! The decode table must return exactly what the
//! transcendental oracle returns — bit for bit — and the memoised bake must
//! stay byte-identical to `apply_overlays_to_nv12_reference` over random
//! frames and random draw lists, both before the background decode table is
//! ready and after.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]

use multiview_compositor::overlay::color_memo::{decode_table, prewarm_overlay_decode_table};
use multiview_compositor::overlay::subpass::{
    apply_overlays_to_nv12, apply_overlays_to_nv12_reference, OverlayColor, OverlayDrawList,
    OverlayPrimitive, OverlayRect,
};
use multiview_compositor::pipeline::{tile_yuv_to_canvas_linear, CanvasColor, Nv12Image};
use proptest::prelude::*;

fn bits(v: [f32; 3]) -> [u32; 3] {
    v.map(f32::to_bits)
}

#[test]
fn decode_table_equals_the_oracle_bit_for_bit_on_a_dense_grid() {
    let canvas = CanvasColor::default();
    assert!(prewarm_overlay_decode_table(canvas));
    let table = decode_table(canvas.output_tag(), canvas).expect("table ready after prewarm");
    // Every luma code × a stride-3 chroma lattice (incl. both extremes):
    // 256 × 86 × 86 ≈ 1.9 M points.
    let chroma: Vec<u8> = (0..=255_u8).step_by(3).chain([255]).collect();
    for &cb in &chroma {
        for &cr in &chroma {
            for y in 0..=255_u8 {
                let oracle = tile_yuv_to_canvas_linear(y, cb, cr, canvas.output_tag(), canvas)
                    .expect("oracle");
                let memo = table.get(y, cb, cr).expect("covered");
                assert_eq!(bits(memo), bits(oracle), "y={y} cb={cb} cr={cr}");
            }
        }
    }
}

#[test]
fn decode_table_does_not_serve_another_colour_pair() {
    let canvas = CanvasColor::default();
    assert!(prewarm_overlay_decode_table(canvas));
    let full = CanvasColor {
        range: multiview_core::color::ColorRange::Full,
        ..canvas
    };
    assert!(decode_table(full.output_tag(), full).is_none());
    assert!(decode_table(full.output_tag(), canvas).is_none());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Random table lookups equal the oracle exactly.
    #[test]
    fn prop_decode_table_matches_oracle(y in any::<u8>(), cb in any::<u8>(), cr in any::<u8>()) {
        let canvas = CanvasColor::default();
        prop_assert!(prewarm_overlay_decode_table(canvas));
        let table = decode_table(canvas.output_tag(), canvas).unwrap();
        let oracle = tile_yuv_to_canvas_linear(y, cb, cr, canvas.output_tag(), canvas).unwrap();
        prop_assert_eq!(bits(table.get(y, cb, cr).unwrap()), bits(oracle));
    }
}

const W: u32 = 160;
const H: u32 = 96;

fn random_frame(seed: u64) -> Nv12Image {
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 24) as u8
    };
    let y: Vec<u8> = (0..W * H).map(|_| next()).collect();
    let uv: Vec<u8> = (0..W * H / 2).map(|_| next()).collect();
    Nv12Image::new(W, H, y, uv, CanvasColor::default().output_tag()).unwrap()
}

fn primitive() -> impl Strategy<Value = OverlayPrimitive> {
    let color = (
        0.0_f32..=1.0,
        0.0_f32..=1.0,
        0.0_f32..=1.0,
        prop_oneof![Just(1.0_f32), 0.0_f32..=1.0],
    )
        .prop_map(|(r, g, b, a)| OverlayColor::new(r, g, b, a));
    prop_oneof![
        (
            -8_i32..150,
            -8_i32..90,
            1_u32..90,
            1_u32..70,
            0_u32..12,
            color.clone()
        )
            .prop_map(
                |(x, y, w, h, corner_radius, color)| OverlayPrimitive::FilledRect {
                    rect: OverlayRect::new(x, y, w, h),
                    corner_radius,
                    color,
                }
            ),
        (
            -8_i32..150,
            -8_i32..90,
            1_u32..40,
            1_u32..30,
            any::<u8>(),
            color.clone()
        )
            .prop_map(|(dest_x, dest_y, width, height, seed, color)| {
                OverlayPrimitive::Glyph {
                    dest_x,
                    dest_y,
                    width,
                    height,
                    coverage: (0..width * height)
                        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
                        .collect(),
                    color,
                }
            }),
        (
            0.0_f32..160.0,
            0.0_f32..96.0,
            0.0_f32..160.0,
            0.0_f32..96.0,
            0.5_f32..4.0,
            color
        )
            .prop_map(
                |(x0, y0, x1, y1, half_thickness, color)| OverlayPrimitive::Stroke {
                    x0,
                    y0,
                    x1,
                    y1,
                    half_thickness,
                    color,
                }
            ),
        (
            -16_i32..150,
            -8_i32..90,
            1_u32..120,
            1_u32..60,
            proptest::collection::vec(any::<u8>(), 16),
            0.0_f32..=1.0
        )
            .prop_map(|(x, y, w, h, rgba, alpha)| OverlayPrimitive::Image {
                dest: OverlayRect::new(x, y, w, h),
                src_width: 2,
                src_height: 2,
                rgba,
                alpha,
            }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The memoised production bake equals the full-canvas oracle byte for byte
    /// on random frames and random draw lists (the decode table is prewarmed,
    /// so the memo path is the one exercised).
    #[test]
    fn prop_memoised_bake_equals_reference(
        seed in any::<u64>(),
        prims in proptest::collection::vec(primitive(), 1..8),
    ) {
        let canvas = CanvasColor::default();
        prop_assert!(prewarm_overlay_decode_table(canvas));
        let image = random_frame(seed);
        let list = OverlayDrawList { primitives: prims };
        let fast = apply_overlays_to_nv12(&image, &list, canvas).unwrap();
        let oracle = apply_overlays_to_nv12_reference(&image, &list, canvas).unwrap();
        prop_assert_eq!(fast.y_plane(), oracle.y_plane());
        prop_assert_eq!(fast.uv_plane(), oracle.uv_plane());
    }
}
