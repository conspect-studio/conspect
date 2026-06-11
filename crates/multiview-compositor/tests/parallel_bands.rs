//! Band-partition equivalence tests for the parallel CPU compositor (ADR-0022,
//! STEP 2). Parallelizing `composite` across cores splits the canvas into
//! even-row-aligned bands, each thread owning whole UV rows of a disjoint
//! `&mut` slice. The split must be *purely* a scheduling change: the output is
//! byte-identical regardless of the thread count, because every band runs the
//! identical deterministic per-pixel pipeline and the global row used for tile
//! addressing is rebased per band.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{composite_with_threads, CanvasColor, Nv12Image, Tile};
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

/// A vertically-varying source so a band split mid-image is visible: top rows
/// red-ish, bottom rows blue-ish, with luma ramping by row. Any band-boundary
/// chroma/luma addressing bug would show as a seam.
fn vertical_ramp(width: u32, height: u32) -> Nv12Image {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for row in 0..h {
        let luma = 16 + u8::try_from((row * 200) / h.max(1)).unwrap_or(200);
        for col in 0..w {
            y[row * w + col] = luma;
        }
    }
    for crow in 0..h / 2 {
        let top = crow < h / 4;
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            uv[idx] = if top { 90 } else { 240 }; // Cb
            uv[idx + 1] = if top { 240 } else { 110 }; // Cr
        }
    }
    Nv12Image::new(width, height, y, uv, bt709_limited()).unwrap()
}

#[test]
fn parallel_bands_are_byte_identical_to_single_thread() {
    // A height of 60 chroma-row-pairs forces a mid-image split at an even row for
    // any thread count in 2..=many. The source covers the full canvas, plus an
    // offset sub-tile so tile-local rebasing is exercised across a band edge.
    let canvas_w: u32 = 96;
    let canvas_h: u32 = 120; // 60 chroma-row pairs
    let bg = vertical_ramp(canvas_w, canvas_h);
    let pip = vertical_ramp(40, 40);
    let tiles = [
        Tile::placed(&bg, 0, 0, 1.0),
        // straddles a band boundary at common thread counts
        Tile::placed(&pip, 8, 50, 0.6),
    ];
    let canvas = CanvasColor::default();
    let background = LinearRgba::opaque(0.05, 0.05, 0.05);

    let single =
        composite_with_threads(canvas_w, canvas_h, canvas, background, &tiles, true, 1).unwrap();

    // Compare against a range of thread counts, including ones that split the
    // 60 chroma-row-pairs unevenly (so a band lands mid-image).
    for n in [2_usize, 3, 4, 7, 16, 64] {
        let multi = composite_with_threads(canvas_w, canvas_h, canvas, background, &tiles, true, n)
            .unwrap();
        assert_eq!(
            single.y_plane(),
            multi.y_plane(),
            "Y plane differs between 1 and {n} threads"
        );
        assert_eq!(
            single.uv_plane(),
            multi.uv_plane(),
            "UV plane differs between 1 and {n} threads"
        );
    }
}

#[test]
fn parallel_oracle_path_also_byte_identical() {
    // The same equivalence must hold on the oracle (non-LUT) path.
    let canvas_w: u32 = 64;
    let canvas_h: u32 = 48;
    let src = vertical_ramp(canvas_w, canvas_h);
    let tiles = [Tile::placed(&src, 0, 0, 1.0)];
    let canvas = CanvasColor::default();
    let bg = LinearRgba::TRANSPARENT;

    let single = composite_with_threads(canvas_w, canvas_h, canvas, bg, &tiles, false, 1).unwrap();
    let multi = composite_with_threads(canvas_w, canvas_h, canvas, bg, &tiles, false, 8).unwrap();
    assert_eq!(single.y_plane(), multi.y_plane());
    assert_eq!(single.uv_plane(), multi.uv_plane());
}
