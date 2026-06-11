//! Integration tests for the overlay TEXT ENGINE (Stage 1, ADR-0016 §3.2):
//! cosmic-text + swash shaping/rasterization into an etagere shelf-packed,
//! byte-capped, LRU-evicting glyph atlas, plus the CPU-reference text-run
//! rasterizer.
//!
//! These are REAL pixel assertions: rendering `7` yields non-zero glyph
//! coverage in the expected box; an unchanged second render adds ZERO new atlas
//! entries (per-glyph cache, T2); and the atlas honors its byte cap by evicting
//! (T4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::overlay::text::{FontFamily, TextEngine};

/// White, fully opaque, in straight (non-premultiplied) form.
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

#[test]
fn rendering_seven_yields_nonzero_coverage_in_glyph_box() {
    let mut engine = TextEngine::new().expect("bundled fonts load");

    let run = engine
        .prepare_run("7", FontFamily::Mono, 32.0, WHITE)
        .expect("shape + rasterize '7'");

    // Exactly one glyph for a single digit.
    assert_eq!(run.glyphs().len(), 1, "one glyph for one digit");
    let g = &run.glyphs()[0];

    // The glyph has a real, positive-area box.
    assert!(g.width > 0, "glyph box has positive width");
    assert!(g.height > 0, "glyph box has positive height");

    // A 32px digit must not be absurdly small or larger than its em.
    assert!(
        g.width <= 64 && g.height <= 64,
        "glyph box {}x{} is within the em-ish bound",
        g.width,
        g.height
    );

    // The rasterized coverage for '7' must contain ink (non-zero coverage).
    let raster = engine
        .rasterize_run("7", FontFamily::Mono, 32.0, WHITE)
        .expect("CPU-reference rasterize '7'");
    assert_eq!(raster.glyphs().len(), 1);
    let rg = &raster.glyphs()[0];
    assert_eq!(rg.width, g.width, "CPU + atlas geometry agree (width)");
    assert_eq!(rg.height, g.height, "CPU + atlas geometry agree (height)");

    // Premultiplied RGBA, 4 bytes/px, sized to the box.
    assert_eq!(
        rg.premultiplied_rgba.len(),
        usize::try_from(rg.width).unwrap() * usize::try_from(rg.height).unwrap() * 4
    );

    // There is at least one pixel with non-zero alpha coverage (real ink).
    let inked = rg
        .premultiplied_rgba
        .chunks_exact(4)
        .filter(|px| px[3] != 0)
        .count();
    assert!(inked > 0, "rasterized '7' has non-zero coverage");

    // White text, premultiplied: at every inked pixel rgb == alpha (coverage).
    for px in rg.premultiplied_rgba.chunks_exact(4) {
        if px[3] != 0 {
            assert_eq!(px[0], px[3], "premultiplied white: R == A");
            assert_eq!(px[1], px[3], "premultiplied white: G == A");
            assert_eq!(px[2], px[3], "premultiplied white: B == A");
        }
    }
}

#[test]
fn unchanged_second_render_adds_zero_new_atlas_entries() {
    let mut engine = TextEngine::new().expect("bundled fonts load");

    engine
        .prepare_run("12:34:56", FontFamily::Mono, 32.0, WHITE)
        .expect("first render");
    let after_first = engine.atlas_entry_count();
    assert!(after_first > 0, "first render populates the atlas");

    // Re-render the IDENTICAL string: every glyph is already resident, so no
    // new atlas entries are inserted (mpv #7615 lesson / T2).
    engine
        .prepare_run("12:34:56", FontFamily::Mono, 32.0, WHITE)
        .expect("second identical render");
    assert_eq!(
        engine.atlas_entry_count(),
        after_first,
        "identical re-render uploads nothing new"
    );

    // A glyph-overlapping clock tick (`...56` -> `...57`) reuses every resident
    // glyph and inserts only the genuinely new glyph(s): `7`. The digits
    // 1,2,3,4,5,6 and `:` are all already resident.
    engine
        .prepare_run("12:34:57", FontFamily::Mono, 32.0, WHITE)
        .expect("overlapping render");
    assert_eq!(
        engine.atlas_entry_count(),
        after_first + 1,
        "only the one new glyph '7' is inserted"
    );
}

#[test]
fn atlas_respects_byte_cap_via_eviction() {
    // A tiny byte cap that can hold only a handful of glyphs forces eviction.
    let cap_bytes = 8 * 1024;
    let mut engine =
        TextEngine::with_atlas_byte_cap(cap_bytes).expect("bundled fonts load under a byte cap");

    // Render a long run of DISTINCT glyphs at a large size so the atlas is
    // pushed well past its cap and must evict.
    let many = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    engine
        .prepare_run(many, FontFamily::Sans, 48.0, WHITE)
        .expect("render many distinct glyphs");

    // The atlas never exceeds its hard byte cap (T4: bounded memory).
    assert!(
        engine.atlas_used_bytes() <= cap_bytes,
        "atlas used {} bytes must stay within the {}-byte cap",
        engine.atlas_used_bytes(),
        cap_bytes
    );

    // Eviction actually happened: not every distinct glyph can be resident at
    // once under this cap.
    let distinct = many.chars().count();
    assert!(
        engine.atlas_entry_count() < distinct,
        "byte cap forced eviction: {} resident < {} distinct glyphs",
        engine.atlas_entry_count(),
        distinct
    );
}
