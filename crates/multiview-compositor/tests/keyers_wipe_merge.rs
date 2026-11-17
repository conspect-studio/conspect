//! Keyers K3 (#388, ADR-0113 §3.2): the exact WIPE scene merge
//! `lerp(out, in, m(x, y))` on the CPU oracle, and the complemented wipe mask
//! a tied keyer going off draws (`1 − m(x, y)`). Every expectation composes
//! each scene separately (one composite per scene, no groups) and merges the
//! decoded pixels with the mask coverage written out here, so the grouped
//! composite is checked against an independent reference.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)] // Test fixtures: scene layers are named A, B, K as in ADR-0113 §2.

use multiview_compositor::blend::{over, LinearRgba, PremulRgba};
use multiview_compositor::mask::{MaskShape, WipeMask};
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite_reference, composite_with, composite_with_threads,
    tile_yuv_to_canvas_linear, CanvasColor, Nv12Image, Tile,
};
use multiview_compositor::{SceneMember, SceneMerge, TileGroup, TileKey};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

const BLACK: LinearRgba = LinearRgba::opaque(0.0, 0.0, 0.0);
const W: u32 = 16;
const H: u32 = 8;

fn bt709() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn solid(w: u32, h: u32, y: u8, cb: u8, cr: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, cb, cr, bt709()).unwrap()
}

fn canvas() -> CanvasColor {
    CanvasColor::default()
}

fn comp(tiles: &[Tile<'_>]) -> Nv12Image {
    composite_with(W, H, canvas(), BLACK, tiles, false).unwrap()
}

/// Premultiplied linear value of a composed canvas pixel (decoded through the
/// public front half; the canvas is opaque).
fn decoded(img: &Nv12Image, x: u32, y: u32) -> PremulRgba {
    let (y8, cb, cr) = img.sample(x, y).unwrap();
    let rgb = tile_yuv_to_canvas_linear(y8, cb, cr, img.color(), canvas()).unwrap();
    LinearRgba {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
        a: 1.0,
    }
    .premultiplied()
}

fn lin(img: &Nv12Image) -> PremulRgba {
    decoded(img, 0, 0)
}

fn encode(p: PremulRgba) -> [u8; 3] {
    let s = p.unpremultiplied();
    canvas_linear_to_output_yuv([s.r, s.g, s.b], canvas()).unwrap()
}

fn yuv(img: &Nv12Image, x: u32, y: u32) -> [u8; 3] {
    let (a, b, c) = img.sample(x, y).unwrap();
    [a, b, c]
}

fn lerp_px(out: PremulRgba, inc: PremulRgba, t: f32) -> PremulRgba {
    PremulRgba {
        r: out.r * (1.0 - t) + inc.r * t,
        g: out.g * (1.0 - t) + inc.g * t,
        b: out.b * (1.0 - t) + inc.b * t,
        a: out.a * (1.0 - t) + inc.a * t,
    }
}

/// The horizontal hard-edge wipe written out independently of `mask.rs`: a
/// pixel is revealed once the edge, travelling `aspect` canvas-height units
/// over the flight, has passed its centre `u·aspect`.
fn horizontal_reveal(progress: f32, px: u32) -> f32 {
    let aspect = W as f32 / H as f32;
    let u = (px as f32 + 0.5) / W as f32;
    if progress <= 0.0 {
        0.0
    } else if progress >= 1.0 || progress * aspect - u * aspect >= 0.0 {
        1.0
    } else {
        0.0
    }
}

/// Chroma is 2x2-subsampled, so an exact per-pixel check uses the luma of
/// pixels whose 2x2 block is uniform in the reference, and the full YUV
/// within one code value.
fn assert_close(got: [u8; 3], want: [u8; 3], what: &str) {
    for (g, w) in got.iter().zip(want) {
        assert!(g.abs_diff(w) <= 1, "{what}: {got:?} vs {want:?}");
    }
}

fn wipe(progress: f32, shape: MaskShape, softness: f32) -> WipeMask {
    WipeMask::new(shape, progress, W, H, [0.5, 0.5], false, softness, 0.0)
}

fn group(member: SceneMember, merge: SceneMerge) -> TileGroup {
    TileGroup::Scene { member, merge }
}

/// Frames 0, mid and end of a hard horizontal WIPE from A to B with an opaque
/// keyed K going on: the grouped composite equals `lerp(scene_out, scene_in,
/// m(x, y))`, with each scene composed on its own.
#[test]
fn a_wipe_with_a_key_going_on_merges_the_two_scenes_per_pixel() {
    let a = solid(W, H, 80, 128, 128);
    let b = solid(W, H, 160, 100, 150);
    let k = solid(8, 4, 200, 170, 90);
    // The scenes on their own: out = A, in = B + K at (4, 2).
    let scene_out = comp(&[Tile::placed(&a, 0, 0, 1.0)]);
    let scene_in = comp(&[
        Tile::placed(&b, 0, 0, 1.0),
        Tile::placed(&k, 4, 2, 1.0).with_key(TileKey::linear()),
    ]);
    for progress in [0.0_f32, 0.5, 1.0] {
        let merge = SceneMerge::Wipe {
            mask: wipe(progress, MaskShape::Horizontal, 0.0),
        };
        let out = comp(&[
            Tile::placed(&a, 0, 0, 1.0).in_group(group(SceneMember::Out, merge)),
            Tile::placed(&b, 0, 0, 1.0).in_group(group(SceneMember::In, merge)),
            Tile::placed(&k, 4, 2, 1.0)
                .with_key(TileKey::linear())
                .in_group(group(SceneMember::In, merge)),
        ]);
        for y in [0_u32, 3, 5] {
            for x in 0..W {
                let m = horizontal_reveal(progress, x);
                let want = lerp_px(decoded(&scene_out, x, y), decoded(&scene_in, x, y), m);
                // Luma is per pixel; compare it exactly (within one code).
                let got = yuv(&out, x, y)[0];
                assert!(
                    got.abs_diff(encode(want)[0]) <= 1,
                    "p={progress} ({x},{y}): {got} vs {:?}",
                    encode(want)
                );
            }
        }
    }
    // End: the incoming scene exactly; frame 0: the outgoing scene exactly.
    let end = SceneMerge::Wipe {
        mask: wipe(1.0, MaskShape::Horizontal, 0.0),
    };
    let start = SceneMerge::Wipe {
        mask: wipe(0.0, MaskShape::Horizontal, 0.0),
    };
    for (merge, want) in [(end, &scene_in), (start, &scene_out)] {
        let out = comp(&[
            Tile::placed(&a, 0, 0, 1.0).in_group(group(SceneMember::Out, merge)),
            Tile::placed(&b, 0, 0, 1.0).in_group(group(SceneMember::In, merge)),
            Tile::placed(&k, 4, 2, 1.0)
                .with_key(TileKey::linear())
                .in_group(group(SceneMember::In, merge)),
        ]);
        assert_eq!(out.y_plane(), want.y_plane(), "endpoint is the scene");
    }
}

/// A key-only WIPE (background not armed): A is in both scenes, K goes off.
/// Where the mask has revealed the incoming scene the key is gone; elsewhere
/// it is still drawn.
#[test]
fn a_key_only_wipe_takes_a_key_off_behind_the_edge() {
    let a = solid(W, H, 100, 128, 128);
    let k = solid(W, H, 220, 160, 100);
    let merge = SceneMerge::Wipe {
        mask: wipe(0.5, MaskShape::Horizontal, 0.0),
    };
    let out = comp(&[
        Tile::placed(&a, 0, 0, 1.0).in_group(group(SceneMember::Both, merge)),
        Tile::placed(&k, 0, 0, 1.0)
            .with_key(TileKey::linear())
            .in_group(group(SceneMember::Out, merge)),
    ]);
    let (pa, pk) = (lin(&a), lin(&k));
    for x in 0..W {
        let m = horizontal_reveal(0.5, x);
        assert_close(
            yuv(&out, x, 4),
            encode(lerp_px(over(pk, pa), pa, m)),
            &format!("column {x}"),
        );
    }
    // The edge sweeps left to right: the revealed (incoming) side is left.
    assert_close(yuv(&out, 0, 4), encode(pa), "left half: K gone");
    assert_close(yuv(&out, W - 1, 4), encode(pk), "right half: K still on");
}

/// A soft circle wipe, checked against the mask's own coverage per pixel,
/// on the tile-driven kernel at 1-4 threads and the pixel-driven oracle.
#[test]
fn a_soft_wipe_merge_is_equal_on_every_kernel_and_thread_count() {
    let a = solid(W, H, 60, 140, 110);
    let b = solid(W, H, 190, 90, 160);
    let k = solid(W, H, 120, 200, 60);
    let mask = wipe(0.4, MaskShape::Circle, 0.25);
    let merge = SceneMerge::Wipe { mask };
    let tiles = [
        Tile::placed(&a, 0, 0, 1.0).in_group(group(SceneMember::Out, merge)),
        Tile::placed(&b, 0, 0, 1.0).in_group(group(SceneMember::In, merge)),
        Tile::placed(&k, 0, 0, 0.7)
            .with_key(TileKey::linear())
            .in_group(group(SceneMember::In, merge)),
    ];
    let oracle = composite_reference(W, H, canvas(), BLACK, &tiles, true).unwrap();
    for threads in 1..=4 {
        let fast = composite_with_threads(W, H, canvas(), BLACK, &tiles, true, threads).unwrap();
        assert_eq!(fast.y_plane(), oracle.y_plane(), "threads {threads}");
        assert_eq!(fast.uv_plane(), oracle.uv_plane(), "threads {threads}");
    }
    let out = comp(&tiles);
    let pin = over(
        PremulRgba {
            r: lin(&k).r * 0.7,
            g: lin(&k).g * 0.7,
            b: lin(&k).b * 0.7,
            a: 0.7,
        },
        lin(&b),
    );
    for x in 0..W {
        let m = mask.coverage(x, 3).clamp(0.0, 1.0);
        let got = yuv(&out, x, 3)[0];
        let want = encode(lerp_px(lin(&a), pin, m))[0];
        assert!(got.abs_diff(want) <= 1, "column {x}: {got} vs {want}");
    }
}

/// The complemented mask is exactly `1 − m(x, y)` (the tied keyer going off,
/// ADR-0113 §3.2), endpoints included, and a mask valid for another canvas
/// merges as the outgoing scene (never a poisoned pixel).
#[test]
fn the_complement_is_one_minus_the_mask_and_an_invalid_mask_keeps_the_outgoing_scene() {
    for progress in [0.0_f32, 0.3, 0.5, 1.0] {
        let m = wipe(progress, MaskShape::Diagonal, 0.2);
        let c = m.complemented();
        assert!(c.complement && !c.complemented().complement);
        for (x, y) in [(0, 0), (5, 3), (W - 1, H - 1), (8, 4)] {
            assert_eq!(c.coverage(x, y), 1.0 - m.coverage(x, y), "p={progress}");
        }
    }
    let a = solid(W, H, 80, 128, 128);
    let b = solid(W, H, 160, 100, 150);
    let foreign = WipeMask::new(
        MaskShape::Horizontal,
        1.0,
        W * 2,
        H,
        [0.5, 0.5],
        false,
        0.0,
        0.0,
    );
    let merge = SceneMerge::Wipe { mask: foreign };
    let out = comp(&[
        Tile::placed(&a, 0, 0, 1.0).in_group(group(SceneMember::Out, merge)),
        Tile::placed(&b, 0, 0, 1.0).in_group(group(SceneMember::In, merge)),
    ]);
    assert_eq!(
        out.y_plane(),
        comp(&[Tile::placed(&a, 0, 0, 1.0)]).y_plane()
    );
}

/// A keyer tile drawn through a complemented mask (a tied DSK going off
/// through a WIPE) shows `1 − m` coverage: it stays where the wipe has not
/// reached and is gone behind the edge.
#[test]
fn a_tile_through_a_complemented_mask_is_drawn_ahead_of_the_edge() {
    let a = solid(W, H, 80, 128, 128);
    let d = solid(W, H, 230, 128, 128);
    let mask = wipe(0.5, MaskShape::Horizontal, 0.0).complemented();
    let out = comp(&[
        Tile::placed(&a, 0, 0, 1.0),
        Tile::placed(&d, 0, 0, 1.0)
            .with_key(TileKey::linear())
            .with_mask(mask),
    ]);
    for x in 0..W {
        let cov = 1.0 - horizontal_reveal(0.5, x);
        let want = lerp_px(lin(&a), lin(&d), cov);
        assert_close(yuv(&out, x, 2), encode(want), &format!("column {x}"));
    }
    assert!(multiview_compositor::key::needs_cpu_keyer_path(&[
        Tile::placed(&a, 0, 0, 1.0).with_mask(mask)
    ]));
}
