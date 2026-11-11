//! Band-parallel overlay subpass exactness (perf(overlay): band-parallel exact
//! overlay subpass).
//!
//! `apply_overlays_to_nv12` now fans large dirty rects out across
//! `std::thread::scope` row-band workers (see `bake_rect_bands` in
//! `src/overlay/subpass.rs`). This must be a **pure scheduling change**: the
//! output is byte-identical to the full-canvas CPU oracle
//! (`apply_overlays_to_nv12_reference`) regardless of how many threads the host
//! offers, because every band runs the identical deterministic per-pixel
//! pipeline over its own disjoint rows.
//!
//! This test builds a realistic 1920x1080 overlay list — several large filled
//! rects (each well above the `64*64`-pixel parallel-admission threshold, so
//! this exercises the parallel path on any host with `>1` available core),
//! overlapping rects (exercising raster/last-writer order across band
//! boundaries), rounded corners (AA coverage, sensitive to any accidental
//! per-band coordinate rebasing), a glyph quad, a stroke, a ring and a clipped
//! image — and asserts the production bake equals the oracle byte-for-byte
//! across the WHOLE canvas (inside every rect and in the untouched gaps
//! between them).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use multiview_compositor::overlay::subpass::{
    apply_overlays_to_nv12, apply_overlays_to_nv12_reference, OverlayColor, OverlayDrawList,
    OverlayPrimitive, OverlayRect,
};
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// A non-uniform NV12 source (not a flat fill) so the decode half of the
/// pipeline is exercised with real per-pixel variation, matching a live frame.
fn source_frame() -> Nv12Image {
    let canvas = CanvasColor::default();
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let y: Vec<u8> = (0..w * h)
        .map(|i| (16 + (i / 7 + (i / w) * 11) % 220) as u8)
        .collect();
    let uv: Vec<u8> = (0..w * h / 2)
        .map(|i| (16 + (i * 17 + i / w) % 220) as u8)
        .collect();
    Nv12Image::new(WIDTH, HEIGHT, y, uv, canvas.output_tag()).expect("1080p source")
}

fn green(a: f32) -> OverlayColor {
    OverlayColor::new(0.1, 0.85, 0.2, a)
}

fn magenta() -> OverlayColor {
    OverlayColor::opaque(0.9, 0.1, 0.6)
}

/// A production-realistic draw list: several LARGE filled rects (each
/// `>= 64*64` so every one takes the parallel band path on hosts with spare
/// cores), two of them deliberately overlapping (raster/last-writer order
/// across a band boundary), one rounded (AA coverage), plus a glyph, a stroke,
/// a ring and an edge-clipped caption image.
fn realistic_overlay_list() -> OverlayDrawList {
    let mut list = OverlayDrawList::new();

    // Three large non-overlapping lower-third style cards, each 600x300 — well
    // above the 64x64 parallel threshold and tall enough (300 rows = 150
    // chroma pairs) to split into multiple bands on an 8-worker host.
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(40, 700, 600, 300),
        corner_radius: 0,
        color: green(0.55),
    });
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(700, 700, 600, 300),
        corner_radius: 24,
        color: magenta(),
    });
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(1360, 700, 520, 300),
        corner_radius: 0,
        color: green(0.3),
    });

    // Two overlapping large rects at the top: the second must win on the
    // intersection (source-over, raster order), and both are tall enough to
    // span multiple bands under the parallel split.
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(200, 40, 500, 400),
        corner_radius: 0,
        color: OverlayColor::new(1.0, 0.2, 0.2, 0.6),
    });
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(400, 160, 500, 400),
        corner_radius: 12,
        color: OverlayColor::new(0.2, 0.3, 1.0, 0.6),
    });

    // A glyph quad (text run) inside one of the cards.
    list.push(OverlayPrimitive::Glyph {
        dest_x: 60,
        dest_y: 720,
        width: 64,
        height: 24,
        coverage: (0..64 * 24).map(|i| (i * 53 % 256) as u8).collect(),
        color: OverlayColor::opaque(1.0, 1.0, 1.0),
    });

    // A long diagonal stroke crossing several band boundaries.
    list.push(OverlayPrimitive::Stroke {
        x0: 50.0,
        y0: 500.0,
        x1: 1800.0,
        y1: 640.0,
        half_thickness: 6.0,
        color: OverlayColor::opaque(1.0, 0.8, 0.0),
    });

    // A ring (annulus) large enough to span multiple bands.
    list.push(OverlayPrimitive::Ring {
        cx: 1700.0,
        cy: 200.0,
        outer_radius: 90.0,
        thickness: 14.0,
        color: OverlayColor::opaque(0.1, 0.6, 1.0),
    });

    // A caption bitmap clipped at the left edge (x < 0), like a DVB-sub cue.
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(-20, 1000, 400, 70),
        src_width: 2,
        src_height: 2,
        rgba: vec![
            80, 60, 30, 180, 30, 40, 50, 210, 0, 10, 30, 160, 90, 20, 10, 220,
        ],
        alpha: 0.8,
    });

    list
}

#[test]
fn band_parallel_bake_matches_the_full_canvas_oracle_byte_for_byte() {
    let canvas = CanvasColor::default();
    let image = source_frame();
    let list = realistic_overlay_list();

    let actual = apply_overlays_to_nv12(&image, &list, canvas).expect("production bake");
    let oracle = apply_overlays_to_nv12_reference(&image, &list, canvas).expect("oracle bake");

    assert_eq!(
        actual.y_plane(),
        oracle.y_plane(),
        "Y plane must be byte-identical to the full-canvas oracle"
    );
    assert_eq!(
        actual.uv_plane(),
        oracle.uv_plane(),
        "UV plane must be byte-identical to the full-canvas oracle"
    );
}

/// Re-running the same bake must be deterministic across repeated calls (no
/// thread-scheduling-dependent nondeterminism from the band split).
#[test]
fn band_parallel_bake_is_deterministic_across_repeated_runs() {
    let canvas = CanvasColor::default();
    let image = source_frame();
    let list = realistic_overlay_list();

    let first = apply_overlays_to_nv12(&image, &list, canvas).expect("first bake");
    for _ in 0..4 {
        let again = apply_overlays_to_nv12(&image, &list, canvas).expect("repeat bake");
        assert_eq!(again.y_plane(), first.y_plane());
        assert_eq!(again.uv_plane(), first.uv_plane());
    }
}
