//! Scale-at-composite (RT-6 / ADR-0034 FIX #2): a tile whose source NV12 image
//! is a **different size** from its destination cell rectangle must be resampled
//! into that rectangle at composite time — never clipped, smeared, or drawn at
//! its native size into a mismatched cell.
//!
//! This is the cross-geometry router case: re-pointing a layout cell to a source
//! decoded at a canonical size that differs from the cell. Before the fix the
//! reference compositor placed every tile 1:1 (dst extent == image extent), so a
//! source larger than its cell overdrew its neighbours and a source smaller than
//! its cell left the rest of the cell on the background — both visually wrong.
//!
//! Invariant #5 (NV12-throughout) is preserved: the resample is an NV12-plane
//! scale into the cell rect; no per-tile RGBA is materialised.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{composite, CanvasColor, Nv12Image, Tile};
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

/// A solid achromatic (cb=cr=128) NV12 image of the given size and luma.
fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, bt709_limited()).unwrap()
}

#[test]
fn placed_constructor_is_one_to_one() {
    // The 1:1 constructor sets the destination extent to the image's own size —
    // the prior behaviour exactly. A tile placed this way has dst extent ==
    // image extent.
    let img = solid(8, 8, 200);
    let t = Tile::placed(&img, 0, 0, 1.0);
    assert_eq!(t.dst_w, 8);
    assert_eq!(t.dst_h, 8);
    assert_eq!(t.dst_x, 0);
    assert_eq!(t.dst_y, 0);
}

#[test]
fn downscale_source_fills_only_the_destination_cell() {
    // A LARGE source (64x64, bright) re-pointed into a SMALL 8x8 cell at the
    // canvas origin must fill exactly that 8x8 rect with the source colour — not
    // overdraw the 64x64 region (the as-built 1:1 bug smears the whole canvas).
    let (cw, ch) = (32u32, 32u32);
    let src = solid(64, 64, 200);
    let bg = LinearRgba::opaque(0.0, 0.0, 0.0); // black background

    let tiles = [Tile::scaled(&src, 0, 0, 8, 8, 1.0)];
    let out = composite(cw, ch, CanvasColor::default(), bg, &tiles).unwrap();

    // Inside the 8x8 destination: the bright source.
    let (y_in, _, _) = out.sample(2, 2).unwrap();
    assert!(
        y_in > 150,
        "a pixel inside the 8x8 destination must carry the bright source (got {y_in})"
    );
    // Just OUTSIDE the 8x8 destination (the as-built 1:1 path would have drawn
    // the source out to 64x64 and smeared here): must be the dark background.
    let (y_out, _, _) = out.sample(20, 20).unwrap();
    assert!(
        y_out < 80,
        "a pixel outside the 8x8 destination must stay background, not smeared \
         source (got {y_out}) — proves the source was scaled INTO the cell, not \
         placed 1:1 at native 64x64"
    );
}

#[test]
fn upscale_source_covers_the_whole_destination_cell() {
    // A SMALL source (4x4, bright) re-pointed into a LARGER 16x16 cell must be
    // upscaled to fill the entire 16x16 rect — the as-built 1:1 path would only
    // paint the top-left 4x4 and leave the rest of the cell on the background.
    let (cw, ch) = (32u32, 32u32);
    let src = solid(4, 4, 200);
    let bg = LinearRgba::opaque(0.0, 0.0, 0.0);

    let tiles = [Tile::scaled(&src, 0, 0, 16, 16, 1.0)];
    let out = composite(cw, ch, CanvasColor::default(), bg, &tiles).unwrap();

    // The far corner of the 16x16 destination (13,13) is well outside the source
    // native 4x4 — under 1:1 it would be background; with upscaling it is source.
    let (y_far, _, _) = out.sample(13, 13).unwrap();
    assert!(
        y_far > 150,
        "the far corner of the 16x16 destination must be filled by the upscaled \
         source (got {y_far}); 1:1 placement would leave it on the background"
    );
}

#[test]
fn scaled_source_is_clipped_to_the_canvas() {
    // A destination rect that runs off the canvas edge must clip to the canvas —
    // no out-of-bounds write, no panic — exactly like a 1:1 tile.
    let (cw, ch) = (16u32, 16u32);
    let src = solid(8, 8, 200);
    let bg = LinearRgba::opaque(0.0, 0.0, 0.0);
    // Destination 12x12 placed at (8,8): only the 8x8 lower-right quadrant is on
    // canvas.
    let tiles = [Tile::scaled(&src, 8, 8, 12, 12, 1.0)];
    let out = composite(cw, ch, CanvasColor::default(), bg, &tiles).unwrap();
    assert_eq!(out.width(), cw);
    assert_eq!(out.height(), ch);
    // On-canvas part of the destination is the source.
    let (y_in, _, _) = out.sample(12, 12).unwrap();
    assert!(y_in > 150, "clipped destination still paints the source");
    // Top-left, outside the destination, is background.
    let (y_bg, _, _) = out.sample(1, 1).unwrap();
    assert!(y_bg < 80, "outside the destination stays background");
}
