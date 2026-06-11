//! Row-sized band accumulator: the serial / low-core composite path must size
//! its premultiplied-linear accumulator to the **covered row range** of the band
//! (`O(band_rows × width)`), not the full frame — inv #5 (NV12-throughout) says
//! never materialise a full-frame RGBA scratch.
//!
//! Two things are pinned here:
//!
//! 1. [`covered_row_span`] — the even-row-aligned band-local `[start, end)` of
//!    the rows any tile touches (the accumulator's extent). Unit + property
//!    tested directly: it must cover every touched row, stay inside the band,
//!    and stay even-row-aligned (NV12 chroma is 2×2 subsampled, so a 2×2 block
//!    must never straddle the span boundary).
//!
//! 2. Byte-identity: with the accumulator row-sized, the production composite
//!    (serial AND parallel, LUT and oracle transfer paths) must remain EXACTLY
//!    equal — bit-for-bit — to the [`composite_reference`] oracle, for random
//!    layouts including odd dims, zero tiles, full-cover tiles, and tiny 1-row
//!    bands. This is a memory optimisation, not a visual change.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{
    composite_reference, composite_with_threads, covered_row_span, CanvasColor, Nv12Image, Tile,
};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use proptest::prelude::*;

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn pq_bt2020() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    }
}

/// Build a small NV12 image whose Y and chroma vary deterministically with a
/// per-image `seed`. All dimensions are forced even (4:2:0).
fn varied_image(width: u32, height: u32, seed: u32, color: ColorInfo) -> Nv12Image {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let seed_u = usize::try_from(seed).unwrap();
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for row in 0..h {
        for col in 0..w {
            let val = (row.wrapping_mul(7) ^ col.wrapping_mul(5) ^ seed_u) % 220;
            y[row * w + col] = 16 + u8::try_from(val).unwrap_or(0);
        }
    }
    for crow in 0..h / 2 {
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            let cb = (crow.wrapping_mul(11) ^ cpair.wrapping_mul(3) ^ seed_u) % 240;
            let cr = (crow.wrapping_mul(3) ^ cpair.wrapping_mul(13) ^ seed_u) % 240;
            uv[idx] = u8::try_from(cb).unwrap_or(128);
            uv[idx + 1] = u8::try_from(cr).unwrap_or(128);
        }
    }
    Nv12Image::new(width, height, y, uv, color).unwrap()
}

// ----------------------------------------------------------------------------
// 1. covered_row_span — the accumulator's row extent.
// ----------------------------------------------------------------------------

#[test]
fn empty_band_has_no_covered_rows() {
    // No tiles at all → no accumulator rows needed.
    assert_eq!(covered_row_span(20, 0, &[]), None);
}

#[test]
fn tile_disjoint_from_band_has_no_covered_rows() {
    // A tile entirely below the band contributes no covered rows.
    let img = varied_image(8, 8, 1, bt709_limited());
    let tile = Tile::placed(&img, 0, 100, 1.0);
    assert_eq!(covered_row_span(20, 0, &[tile]), None);
}

#[test]
fn single_tile_span_is_even_aligned_and_covers_tile() {
    // Tile at global rows [3, 11) in a band starting at global row 0. The span
    // must cover those band-local rows and be even-aligned: start floored to 2,
    // end ceiled to 12.
    let img = varied_image(8, 8, 1, bt709_limited());
    let tile = Tile::placed(&img, 0, 3, 1.0);
    let (start, end) = covered_row_span(20, 0, &[tile]).expect("tile overlaps band");
    assert_eq!(start, 2, "start floored to even ≤ 3");
    assert_eq!(end, 12, "end ceiled to even ≥ 11");
    assert_eq!(start % 2, 0, "start even-aligned");
    assert_eq!(end % 2, 0, "end even-aligned");
}

#[test]
fn span_clipped_to_top_edge() {
    // Tile starting above the band (negative band-local rows) clips to 0.
    let img = varied_image(8, 8, 1, bt709_limited());
    // Band covers global rows [10, 30); tile covers [5, 13) → band-local [0, 3).
    let tile = Tile::placed(&img, 0, 5, 1.0);
    let (start, end) = covered_row_span(20, 10, &[tile]).expect("tile overlaps band top");
    assert_eq!(start, 0, "clipped to band top");
    assert_eq!(end, 4, "ceiled to even ≥ band-local 3");
}

#[test]
fn span_clipped_to_bottom_edge() {
    // Tile overhanging the band bottom clips end to band_rows (even canvas).
    let img = varied_image(8, 20, 1, bt709_limited());
    // Band has 12 rows; tile covers global [4, 24) → band-local [4, 12).
    let tile = Tile::placed(&img, 0, 4, 1.0);
    let (start, end) = covered_row_span(12, 0, &[tile]).expect("tile overlaps band bottom");
    assert_eq!(start, 4);
    assert_eq!(end, 12, "clipped to band_rows");
}

#[test]
fn single_row_tile_yields_a_two_row_even_span() {
    // A 2-row tile placed at an odd start still produces an even-aligned span
    // that contains both its rows (no 2×2 chroma block may straddle the span).
    let img = varied_image(4, 2, 1, bt709_limited());
    let tile = Tile::placed(&img, 0, 5, 1.0); // global rows [5, 7)
    let (start, end) = covered_row_span(40, 0, &[tile]).expect("overlaps");
    assert!(start % 2 == 0 && end % 2 == 0, "even-aligned");
    assert!(start <= 5 && end >= 7, "covers the tile's rows [5,7)");
}

#[test]
fn multiple_tiles_span_is_the_union() {
    let a = varied_image(8, 8, 1, bt709_limited());
    let b = varied_image(8, 8, 2, bt709_limited());
    let tiles = [
        Tile::placed(&a, 0, 3, 1.0),  // [3, 11)
        Tile::placed(&b, 0, 20, 1.0), // [20, 28)
    ];
    let (start, end) = covered_row_span(40, 0, &tiles).expect("overlaps");
    assert_eq!(start, 2, "min start floored even");
    assert_eq!(end, 28, "max end (already even)");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// `covered_row_span` is always even-aligned, inside the band, and covers
    /// every band row that any tile touches (and nothing more than the
    /// even-rounded union). `band_rows` is even, matching the function's
    /// precondition (every production caller passes an even-height band).
    #[test]
    fn prop_covered_row_span_is_sound(
        band_pairs in 1_usize..60,
        py_start in 0_u32..200,
        placements in proptest::collection::vec(
            (0_u32..240, 2_u32..40),
            0..=6,
        ),
    ) {
        let band_rows = band_pairs * 2; // even, per the precondition
        let images: Vec<Nv12Image> = placements
            .iter()
            .enumerate()
            .map(|(i, &(_dy, h))| {
                let eh = (h & !1).max(2);
                varied_image(4, eh, u32::try_from(i).unwrap_or(0), bt709_limited())
            })
            .collect();
        let tiles: Vec<Tile<'_>> = images
            .iter()
            .zip(placements.iter())
            .map(|(img, &(dy, _h))| Tile::placed(img, 0, dy, 1.0))
            .collect();

        let band_top = i64::from(py_start);
        let band_bottom = band_top + i64::try_from(band_rows).unwrap();

        // Reference: the set of band-local rows actually touched by some tile.
        let mut touched: Vec<bool> = vec![false; band_rows];
        for tile in &tiles {
            let ty0 = i64::from(tile.dst_y);
            let ty1 = ty0 + i64::from(tile.image.height());
            let lo = ty0.max(band_top);
            let hi = ty1.min(band_bottom);
            let mut g = lo;
            while g < hi {
                let local = usize::try_from(g - band_top).unwrap();
                touched[local] = true;
                g += 1;
            }
        }

        let span = covered_row_span(band_rows, py_start, &tiles);
        let any_touched = touched.iter().any(|&t| t);

        match span {
            None => {
                prop_assert!(!any_touched, "span=None but some row was touched");
            }
            Some((start, end)) => {
                prop_assert!(start < end, "non-empty span");
                prop_assert_eq!(start % 2, 0, "start even");
                prop_assert_eq!(end % 2, 0, "end even");
                prop_assert!(end <= band_rows, "span inside band");
                // Every touched row is inside the span.
                for (r, &t) in touched.iter().enumerate() {
                    if t {
                        prop_assert!(start <= r && r < end, "touched row {} outside span", r);
                    }
                }
                prop_assert!(any_touched, "non-None span but nothing touched");
            }
        }
    }
}

// ----------------------------------------------------------------------------
// 2. Byte-identity to the reference oracle with the row-sized accumulator.
// ----------------------------------------------------------------------------

/// Assert the production kernel (serial + parallel, LUT + oracle paths) is
/// byte-identical to the reference for the given canvas + tile stack.
fn assert_equivalent(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
) {
    for &use_lut in &[true, false] {
        let reference =
            composite_reference(canvas_w, canvas_h, canvas, background, tiles, use_lut).unwrap();
        for &n in &[1_usize, 2, 8] {
            let prod =
                composite_with_threads(canvas_w, canvas_h, canvas, background, tiles, use_lut, n)
                    .unwrap();
            assert_eq!(
                prod.y_plane(),
                reference.y_plane(),
                "Y plane diverged (use_lut={use_lut}, threads={n})"
            );
            assert_eq!(
                prod.uv_plane(),
                reference.uv_plane(),
                "UV plane diverged (use_lut={use_lut}, threads={n})"
            );
        }
    }
}

#[test]
fn tile_at_top_edge_matches_reference() {
    let img = varied_image(20, 12, 1, bt709_limited());
    let tiles = [Tile::placed(&img, 4, 0, 1.0)];
    assert_equivalent(
        48,
        32,
        CanvasColor::default(),
        LinearRgba::opaque(0.1, 0.2, 0.3),
        &tiles,
    );
}

#[test]
fn tile_at_bottom_edge_matches_reference() {
    let img = varied_image(20, 12, 2, bt709_limited());
    // dst_y 20 + height 12 = 32 = canvas_h: hugs the bottom edge exactly.
    let tiles = [Tile::placed(&img, 4, 20, 0.6)];
    assert_equivalent(
        48,
        32,
        CanvasColor::default(),
        LinearRgba::opaque(0.1, 0.2, 0.3),
        &tiles,
    );
}

#[test]
fn full_cover_tile_matches_reference() {
    // A tile covering the whole canvas: accumulator span == full band.
    let img = varied_image(48, 32, 3, bt709_limited());
    let tiles = [Tile::placed(&img, 0, 0, 1.0)];
    assert_equivalent(
        48,
        32,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    );
}

#[test]
fn zero_tiles_matches_reference() {
    assert_equivalent(
        48,
        32,
        CanvasColor::default(),
        LinearRgba::opaque(0.4, 0.1, 0.7),
        &[],
    );
}

#[test]
fn single_row_pair_band_matches_reference() {
    // A 2-tall tile (one chroma row-pair) at an even row: the span is a single
    // 2-row band — the tiniest accumulator the row-sizing can produce.
    let img = varied_image(8, 2, 5, bt709_limited());
    let tiles = [Tile::placed(&img, 6, 14, 0.9)];
    assert_equivalent(
        32,
        32,
        CanvasColor::default(),
        LinearRgba::opaque(0.2, 0.2, 0.2),
        &tiles,
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// With the accumulator row-sized, production stays BIT-IDENTICAL to the
    /// reference over random layouts: varied canvas dims (incl. small + uneven
    /// aspect), tile sizes/positions (incl. off-canvas), opacities, colors, and
    /// zero tiles.
    #[test]
    fn prop_row_sized_accumulator_bit_identical_to_reference(
        canvas_w_half in 2_u32..40,
        canvas_h_half in 2_u32..40,
        seeds in proptest::collection::vec(
            (0_u32..1000, 0_u32..90, 0_u32..90, 2_u32..40, 2_u32..40, 0.0_f32..=1.0, any::<bool>()),
            0..=6,
        ),
        bg in (0.0_f32..=1.0, 0.0_f32..=1.0, 0.0_f32..=1.0, 0.0_f32..=1.0),
    ) {
        let canvas_w = canvas_w_half * 2; // always even & positive
        let canvas_h = canvas_h_half * 2;
        let canvas = CanvasColor::default();
        let background = LinearRgba { r: bg.0, g: bg.1, b: bg.2, a: bg.3 };

        let images: Vec<Nv12Image> = seeds
            .iter()
            .map(|&(seed, _x, _y, w, h, _op, is_pq)| {
                let ew = (w & !1).max(2);
                let eh = (h & !1).max(2);
                let color = if is_pq { pq_bt2020() } else { bt709_limited() };
                varied_image(ew, eh, seed, color)
            })
            .collect();
        let tiles: Vec<Tile<'_>> = images
            .iter()
            .zip(seeds.iter())
            .map(|(img, &(_seed, x, y, _w, _h, op, _pq))| Tile::placed(img, x, y, op))
            .collect();

        for &use_lut in &[true, false] {
            let reference = composite_reference(
                canvas_w, canvas_h, canvas, background, &tiles, use_lut,
            ).unwrap();
            // n=1 forces the serial path (the row-sizing target); larger n
            // exercises the parallel bands too.
            for &n in &[1_usize, 2, 8] {
                let prod = composite_with_threads(
                    canvas_w, canvas_h, canvas, background, &tiles, use_lut, n,
                ).unwrap();
                prop_assert_eq!(
                    prod.y_plane(),
                    reference.y_plane(),
                    "Y diverged (w={}, h={}, use_lut={}, threads={})",
                    canvas_w, canvas_h, use_lut, n
                );
                prop_assert_eq!(
                    prod.uv_plane(),
                    reference.uv_plane(),
                    "UV diverged (w={}, h={}, use_lut={}, threads={})",
                    canvas_w, canvas_h, use_lut, n
                );
            }
        }
    }
}
