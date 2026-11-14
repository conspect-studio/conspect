//! Keyers K1 (#386, ADR-0113 §2-§4): the CPU oracle for per-tile key coverage
//! and the exact two-scene merge. Every expectation is computed independently
//! from the public colour functions (front half → `over` → merge → back half)
//! on the transcendental oracle path, so the composite must match it exactly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)] // Test fixtures: scene layers are named A, B, K, U, D as in ADR-0113 §2.

use multiview_compositor::blend::{over, LinearRgba, PremulRgba};
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite_reference, composite_with, composite_with_threads,
    tile_yuv_to_canvas_linear, AlphaPlanePool, CanvasColor, Nv12Image, Tile,
};
use multiview_compositor::{KeyMask, RunBackend, SceneMember, SceneMerge, TileGroup, TileKey};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use proptest::prelude::*;
use std::sync::Arc;

const BLACK: LinearRgba = LinearRgba::opaque(0.0, 0.0, 0.0);

fn bt709(range: ColorRange) -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range,
    }
}

fn pq2020() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    }
}

fn solid(w: u32, h: u32, y: u8, cb: u8, cr: u8, color: ColorInfo) -> Nv12Image {
    Nv12Image::solid(w, h, y, cb, cr, color).unwrap()
}

/// An image whose luma varies by column (`ys[col % ys.len()]`), neutral chroma.
fn columns(w: u32, h: u32, ys: &[u8], color: ColorInfo) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let mut y = vec![0_u8; wu * hu];
    for row in 0..hu {
        for col in 0..wu {
            y[row * wu + col] = ys[col % ys.len()];
        }
    }
    Nv12Image::new(w, h, y, vec![128_u8; wu * hu / 2], color).unwrap()
}

fn canvas() -> CanvasColor {
    CanvasColor::default()
}

/// Oracle composite (no LUTs).
fn comp(w: u32, h: u32, tiles: &[Tile<'_>]) -> Nv12Image {
    composite_with(w, h, canvas(), BLACK, tiles, false).unwrap()
}

fn lin(img: &Nv12Image, x: u32, y: u32) -> [f32; 3] {
    let (y8, cb, cr) = img.sample(x, y).unwrap();
    tile_yuv_to_canvas_linear(y8, cb, cr, img.color(), canvas()).unwrap()
}

fn premul(rgb: [f32; 3], a: f32) -> PremulRgba {
    LinearRgba {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
        a,
    }
    .premultiplied()
}

fn encode(p: PremulRgba) -> [u8; 3] {
    let s = p.unpremultiplied();
    canvas_linear_to_output_yuv([s.r, s.g, s.b], canvas()).unwrap()
}

fn luma(img: &Nv12Image, x: u32, y: u32) -> u8 {
    img.sample(x, y).unwrap().0
}

fn yuv(img: &Nv12Image, x: u32, y: u32) -> [u8; 3] {
    let (a, b, c) = img.sample(x, y).unwrap();
    [a, b, c]
}

// --- test-local scalar oracles (never the production helpers) ----------------

/// `clamp((y − clip) × gain, 0, 1)` written out here, independent of
/// `key::luma_matte`.
fn matte(y: f32, clip: f32, gain: f32) -> f32 {
    ((y - clip) * gain).clamp(0.0, 1.0)
}

/// Component-wise `out × (1 − t) + inc × t` on premultiplied values.
fn lerp_px(out: PremulRgba, inc: PremulRgba, t: f32) -> PremulRgba {
    PremulRgba {
        r: out.r * (1.0 - t) + inc.r * t,
        g: out.g * (1.0 - t) + inc.g * t,
        b: out.b * (1.0 - t) + inc.b * t,
        a: out.a * (1.0 - t) + inc.a * t,
    }
}

/// The ADR-0113 §2 merge written out per pixel: MIX `lerp(out, in, p)`, DIP
/// `lerp(lerp(out, colour, d), in, i)` with an opaque colour.
fn merge_px(m: SceneMerge, out: PremulRgba, inc: PremulRgba) -> PremulRgba {
    match m {
        SceneMerge::Mix { p } => lerp_px(out, inc, p),
        SceneMerge::Dip { colour, d, i } => {
            let c = PremulRgba {
                r: colour.r,
                g: colour.g,
                b: colour.b,
                a: 1.0,
            };
            lerp_px(lerp_px(out, c, d), inc, i)
        }
        _ => panic!("K1 builds MIX and DIP"),
    }
}

/// Encode within one code value (the merge is f32 arithmetic in another
/// association order than the kernel's fused multiply-adds).
fn assert_close(got: [u8; 3], want: [u8; 3], what: &str) {
    for (g, w) in got.iter().zip(want) {
        assert!(g.abs_diff(w) <= 1, "{what}: {got:?} vs {want:?}");
    }
}

#[test]
fn a_linear_self_key_uses_the_fill_alpha_once() {
    // A 0.5 alpha fill keyed linearly composes as 0.5 coverage, never 0.25.
    let pool = Arc::new(AlphaPlanePool::default());
    let fill = pool
        .attach(
            solid(4, 4, 200, 90, 160, bt709(ColorRange::Limited)),
            &[128_u8; 16],
        )
        .unwrap();
    let plain = solid(4, 4, 200, 90, 160, bt709(ColorRange::Limited));
    let keyed = comp(
        4,
        4,
        &[Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::linear())],
    );
    let half = comp(4, 4, &[Tile::placed(&plain, 0, 0, 128.0 / 255.0)]);
    assert_eq!(keyed, half);
    let quarter = comp(
        4,
        4,
        &[Tile::placed(
            &plain,
            0,
            0,
            (128.0 / 255.0) * (128.0 / 255.0),
        )],
    );
    assert_ne!(keyed, quarter, "alpha must not be applied twice");
    // And an opaque fill with a linear self key is a box: the plain tile.
    let boxed = comp(
        4,
        4,
        &[Tile::placed(&plain, 0, 0, 1.0).with_key(TileKey::linear())],
    );
    assert_eq!(boxed, comp(4, 4, &[Tile::placed(&plain, 0, 0, 1.0)]));
}

#[test]
fn a_linear_key_source_uses_its_luma_as_the_matte() {
    let fill = solid(4, 4, 180, 100, 150, bt709(ColorRange::Limited));
    // Full-range key at Y=128 → matte 128/255.
    let key = solid(4, 4, 128, 128, 128, bt709(ColorRange::Full));
    let keyed = comp(
        4,
        4,
        &[Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::linear().with_source(&key))],
    );
    assert_eq!(
        keyed,
        comp(4, 4, &[Tile::placed(&fill, 0, 0, 128.0 / 255.0)])
    );
    // Inverted: 1 − 128/255.
    let inv = comp(
        4,
        4,
        &[Tile::placed(&fill, 0, 0, 1.0)
            .with_key(TileKey::linear().with_source(&key).inverted(true))],
    );
    assert_eq!(
        inv,
        comp(4, 4, &[Tile::placed(&fill, 0, 0, 1.0 - 128.0 / 255.0)])
    );
}

#[test]
fn a_luma_self_key_applies_clip_gain_and_invert() {
    let c = bt709(ColorRange::Limited);
    for (y8, clip, gain, invert) in [
        (16_u8, 0.1_f32, 4.0_f32, false), // black: below clip → 0
        (235, 0.1, 4.0, false),           // white: saturates → 1
        (60, 0.1, 4.0, false),            // in the ramp
        (60, 0.1, 4.0, true),
        (126, 0.5, 1.5, false),
    ] {
        let fill = solid(4, 4, y8, 128, 128, c);
        let ynorm = (f32::from(y8) - 16.0) / 219.0;
        let m = matte(ynorm, clip, gain);
        let m = if invert { 1.0 - m } else { m };
        let keyed = comp(
            4,
            4,
            &[Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::luma(clip, gain).inverted(invert))],
        );
        assert_eq!(
            keyed,
            comp(4, 4, &[Tile::placed(&fill, 0, 0, m)]),
            "y8={y8} clip={clip} gain={gain} invert={invert}"
        );
    }
}

#[test]
fn a_luma_key_reads_the_key_source_not_the_fill() {
    let fill = solid(4, 4, 235, 128, 128, bt709(ColorRange::Limited));
    let key = solid(4, 4, 16, 128, 128, bt709(ColorRange::Limited)); // black key
    let keyed = comp(
        4,
        4,
        &[Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::luma(0.1, 4.0).with_source(&key))],
    );
    assert_eq!(
        keyed,
        comp(4, 4, &[]),
        "a black key signal keys the fill out"
    );
}

#[test]
fn the_mask_is_in_fill_source_pixels_and_travels_with_a_flying_key() {
    let fill = solid(8, 8, 200, 128, 128, bt709(ColorRange::Limited));
    // Mask the left half of the SOURCE, then fly the fill 2x down-scaled to
    // (4, 2) 4x4 on an 8x8 canvas: only destination columns 4..6 are keyed.
    let k = TileKey::linear().masked(KeyMask {
        x: 0,
        y: 0,
        w: 4,
        h: 8,
    });
    let out = comp(8, 8, &[Tile::scaled(&fill, 4, 2, 4, 4, 1.0).with_key(k)]);
    let full = comp(8, 8, &[Tile::scaled(&fill, 4, 2, 4, 4, 1.0)]);
    let bg = comp(8, 8, &[]);
    for y in 0..8 {
        for x in 0..8 {
            let inside_fly = (4..8).contains(&x) && (2..6).contains(&y);
            let inside_mask = inside_fly && x < 6;
            let want = if inside_mask {
                luma(&full, x, y)
            } else {
                luma(&bg, x, y)
            };
            assert_eq!(luma(&out, x, y), want, "pixel ({x},{y})");
        }
    }
}

#[test]
fn fill_and_key_of_unequal_sizes_are_sampled_independently() {
    let c = bt709(ColorRange::Full);
    let fill = solid(2, 2, 200, 128, 128, bt709(ColorRange::Limited));
    let ys = [0_u8, 64, 128, 255, 32, 96, 160, 224];
    // Key up-scaled (4 → 4 is 1:1 against the destination; fill 2 → 4 is up).
    let key4 = columns(4, 2, &ys[..4], c);
    // Key down-scaled (8 → 4 samples columns 0, 2, 4, 6).
    let key8 = columns(8, 2, &ys, c);
    for (key, cols) in [(&key4, [0_usize, 1, 2, 3]), (&key8, [0, 2, 4, 6])] {
        let out = comp(
            4,
            2,
            &[Tile::scaled(&fill, 0, 0, 4, 2, 1.0).with_key(TileKey::linear().with_source(key))],
        );
        for (x, &kc) in cols.iter().enumerate() {
            let m = f32::from(ys[kc]) / 255.0;
            let want = comp(4, 2, &[Tile::scaled(&fill, 0, 0, 4, 2, m)]);
            let x = u32::try_from(x).unwrap();
            assert_eq!(luma(&out, x, 0), luma(&want, x, 0), "column {x}");
        }
    }
}

#[test]
fn key_and_fill_in_mixed_range_expand_each_by_its_own_range() {
    // The same 8-bit code 126 is 0.5 in limited range and 126/255 in full.
    for (fill_range, key_range, want) in [
        (
            ColorRange::Full,
            ColorRange::Limited,
            (126.0 - 16.0) / 219.0,
        ),
        (ColorRange::Limited, ColorRange::Full, 126.0 / 255.0),
    ] {
        let fill = solid(4, 4, 180, 110, 140, bt709(fill_range));
        let key = solid(4, 4, 126, 128, 128, bt709(key_range));
        let keyed = comp(
            4,
            4,
            &[Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::linear().with_source(&key))],
        );
        assert_eq!(keyed, comp(4, 4, &[Tile::placed(&fill, 0, 0, want)]));
    }
}

#[test]
fn key_alpha_is_premultiplied_once_after_primaries() {
    // A PQ / BT.2020 fill: the key enters at the same single premultiply step
    // as opacity, after the primaries convert, so a key of m equals opacity m.
    let fill = solid(4, 4, 120, 100, 150, pq2020());
    let key = solid(4, 4, 191, 128, 128, bt709(ColorRange::Full));
    let m = 191.0 / 255.0;
    let keyed = comp(
        4,
        4,
        &[Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::linear().with_source(&key))],
    );
    assert_eq!(keyed, comp(4, 4, &[Tile::placed(&fill, 0, 0, m)]));
    // Independent per-pixel expectation: premultiply the post-primaries linear
    // colour once by m and fold over the background.
    let want = encode(over(premul(lin(&fill, 0, 0), m), BLACK.premultiplied()));
    assert_eq!(yuv(&keyed, 1, 1), want);
}

#[test]
fn weight_is_the_tile_opacity_times_the_key() {
    let fill = solid(4, 4, 200, 128, 128, bt709(ColorRange::Limited));
    let key = solid(4, 4, 255, 128, 128, bt709(ColorRange::Full));
    let out = comp(
        4,
        4,
        &[Tile::placed(&fill, 0, 0, 0.25).with_key(TileKey::linear().with_source(&key))],
    );
    assert_eq!(out, comp(4, 4, &[Tile::placed(&fill, 0, 0, 0.25)]));
}

proptest! {
    #[test]
    fn the_luma_matte_is_monotonic_and_bounded(
        a in -0.2_f32..1.2, b in -0.2_f32..1.2, clip in -0.5_f32..1.5, gain in -2.0_f32..50.0,
    ) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let (mlo, mhi) = (
            multiview_compositor::key::luma_matte(lo, clip, gain),
            multiview_compositor::key::luma_matte(hi, clip, gain),
        );
        prop_assert!(mlo <= mhi);
        prop_assert!((0.0..=1.0).contains(&mlo) && (0.0..=1.0).contains(&mhi));
    }
}

// --- scene merge (ADR-0113 §2) ----------------------------------------------

fn mix(p: f32) -> SceneMerge {
    SceneMerge::Mix { p }
}

fn scene(member: SceneMember, merge: SceneMerge) -> TileGroup {
    TileGroup::Scene { member, merge }
}

#[test]
fn an_opaque_key_going_on_mixes_exactly() {
    // Key-only transition at p = 0.5: A in both scenes, K only in the incoming
    // one. Under K the result is 0.5·K + 0.5·A (a flat over would give
    // 0.5·K + 0.25·A + 0.25·B — the review finding).
    let c = bt709(ColorRange::Limited);
    let a = solid(8, 4, 120, 90, 170, c);
    let k = solid(4, 4, 220, 160, 100, c);
    let m = mix(0.5);
    let out = comp(
        8,
        4,
        &[
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Both, m)),
            Tile::placed(&k, 0, 0, 1.0)
                .with_key(TileKey::linear())
                .in_group(scene(SceneMember::In, m)),
        ],
    );
    let pa = premul(lin(&a, 0, 0), 1.0);
    let pk = premul(lin(&k, 0, 0), 1.0);
    assert_close(
        yuv(&out, 1, 1),
        encode(merge_px(m, pa, over(pk, pa))),
        "under the key",
    );
    // Independent of the merge: 0.5·K + 0.5·A under an opaque key.
    let half = lerp_px(pa, pk, 0.5);
    assert_close(yuv(&out, 1, 1), encode(half), "0.5·K + 0.5·A");
    assert_close(yuv(&out, 5, 1), encode(pa), "outside the key: A");
}

#[test]
fn a_background_and_key_transition_merges_both_scenes() {
    // MIX from A to B+K (K going on), and the reverse with K going off.
    let c = bt709(ColorRange::Limited);
    let a = solid(4, 4, 80, 128, 128, c);
    let b = solid(4, 4, 160, 100, 150, c);
    let k = solid(4, 4, 30, 200, 60, c);
    for p in [0.0_f32, 0.25, 0.5, 1.0] {
        let m = mix(p);
        let on = comp(
            4,
            4,
            &[
                Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Out, m)),
                Tile::placed(&b, 0, 0, 1.0).in_group(scene(SceneMember::In, m)),
                Tile::placed(&k, 0, 0, 1.0)
                    .with_key(TileKey::luma(0.0, 1.0))
                    .in_group(scene(SceneMember::In, m)),
            ],
        );
        let km = matte((30.0 - 16.0) / 219.0, 0.0, 1.0);
        let (pa, pb, pk) = (
            premul(lin(&a, 0, 0), 1.0),
            premul(lin(&b, 0, 0), 1.0),
            premul(lin(&k, 0, 0), km),
        );
        assert_close(
            yuv(&on, 0, 0),
            encode(merge_px(m, pa, over(pk, pb))),
            &format!("on p={p}"),
        );
        let off = comp(
            4,
            4,
            &[
                Tile::placed(&b, 0, 0, 1.0).in_group(scene(SceneMember::Out, m)),
                Tile::placed(&k, 0, 0, 1.0)
                    .with_key(TileKey::luma(0.0, 1.0))
                    .in_group(scene(SceneMember::Out, m)),
                Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::In, m)),
            ],
        );
        assert_close(
            yuv(&off, 0, 0),
            encode(merge_px(m, over(pk, pb), pa)),
            &format!("off p={p}"),
        );
    }
}

#[test]
fn unarmed_keys_below_and_above_and_a_dsk_fold_in_the_right_place() {
    // A (both), U1 unarmed below (both), K armed going on (in), then U2 an
    // unarmed USK above and a DSK D (top, folded once onto the merge).
    let c = bt709(ColorRange::Limited);
    let a = solid(4, 4, 100, 128, 128, c);
    let u1 = solid(4, 4, 140, 90, 90, c);
    let k = solid(4, 4, 200, 170, 120, c);
    let u2 = solid(4, 4, 60, 140, 200, c);
    let d = solid(4, 4, 230, 128, 128, c);
    let m = mix(0.3);
    let out = comp(
        4,
        4,
        &[
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Both, m)),
            Tile::placed(&u1, 0, 0, 0.5)
                .with_key(TileKey::linear())
                .in_group(scene(SceneMember::Both, m)),
            Tile::placed(&k, 0, 0, 0.8)
                .with_key(TileKey::linear())
                .in_group(scene(SceneMember::In, m)),
            Tile::placed(&u2, 0, 0, 0.4).with_key(TileKey::linear()),
            Tile::placed(&d, 0, 0, 0.6).with_key(TileKey::linear()),
        ],
    );
    let base = over(premul(lin(&u1, 0, 0), 0.5), premul(lin(&a, 0, 0), 1.0));
    let merged = merge_px(m, base, over(premul(lin(&k, 0, 0), 0.8), base));
    let top = over(
        premul(lin(&d, 0, 0), 0.6),
        over(premul(lin(&u2, 0, 0), 0.4), merged),
    );
    assert_close(yuv(&out, 2, 2), encode(top), "U1, K merged, U2, D");
}

#[test]
fn a_dip_merge_colours_every_pixel_even_where_no_tile_lies() {
    let c = bt709(ColorRange::Limited);
    let a = solid(4, 4, 100, 128, 128, c);
    let k = solid(4, 4, 200, 100, 150, c);
    let colour = LinearRgba::opaque(0.2, 0.05, 0.6);
    let m = SceneMerge::Dip {
        colour,
        d: 1.0,
        i: 0.5,
    };
    // A 4x4 background in the corner of an 8x4 canvas; K comes in over it.
    let out = comp(
        8,
        4,
        &[
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Both, m)),
            Tile::placed(&k, 0, 0, 1.0)
                .with_key(TileKey::linear())
                .in_group(scene(SceneMember::In, m)),
        ],
    );
    // With d = 1 the outgoing scene is fully replaced by the colour, so an
    // uncovered pixel is exactly 0.5·colour + 0.5·background (both scenes
    // there are the black background).
    let c = PremulRgba {
        r: 0.2,
        g: 0.05,
        b: 0.6,
        a: 1.0,
    };
    let bg = BLACK.premultiplied();
    assert_close(
        yuv(&out, 6, 2),
        encode(lerp_px(c, bg, 0.5)),
        "uncovered pixel dips too",
    );
    let pa = premul(lin(&a, 0, 0), 1.0);
    let pk = premul(lin(&k, 0, 0), 1.0);
    assert_close(
        yuv(&out, 1, 1),
        encode(lerp_px(c, pk, 0.5)),
        "K over A at i",
    );
    assert_close(
        yuv(&out, 1, 1),
        encode(merge_px(m, pa, over(pk, pa))),
        "merge formula",
    );
}

#[test]
fn a_dip_merge_before_the_switch_point_runs_the_outgoing_leg() {
    // First leg: d = 0.25, i = 0 — out towards the colour, incoming unseen.
    let c = bt709(ColorRange::Limited);
    let a = solid(4, 4, 150, 110, 140, c);
    let k = solid(4, 4, 40, 180, 90, c);
    let m = SceneMerge::Dip {
        colour: LinearRgba::opaque(0.7, 0.1, 0.1),
        d: 0.25,
        i: 0.0,
    };
    let out = comp(
        4,
        4,
        &[
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Out, m)),
            Tile::placed(&k, 0, 0, 1.0).in_group(scene(SceneMember::In, m)),
        ],
    );
    let pa = premul(lin(&a, 0, 0), 1.0);
    let col = PremulRgba {
        r: 0.7,
        g: 0.1,
        b: 0.1,
        a: 1.0,
    };
    assert_close(
        yuv(&out, 1, 1),
        encode(lerp_px(pa, col, 0.25)),
        "0.75·A + 0.25·colour",
    );
}

#[test]
fn a_nan_weight_reads_as_zero_and_never_poisons_the_layers_beneath() {
    let c = bt709(ColorRange::Limited);
    let bg = solid(4, 4, 140, 100, 160, c);
    let k = solid(4, 4, 220, 128, 128, c);
    let base = comp(4, 4, &[Tile::placed(&bg, 0, 0, 1.0)]);
    for key in [TileKey::linear(), TileKey::luma(0.1, 2.0)] {
        let out = comp(
            4,
            4,
            &[
                Tile::placed(&bg, 0, 0, 1.0),
                Tile::placed(&k, 0, 0, f32::NAN).with_key(key),
            ],
        );
        assert_eq!(out, base, "NaN-weight keyer contributes nothing");
    }
}

/// Masks a caller could build from public fields that no `WipeMask::new` for
/// an 8x8 canvas produces (merge reviews R-1..R-5).
fn malformed_masks(
    good: multiview_compositor::mask::WipeMask,
) -> Vec<multiview_compositor::mask::WipeMask> {
    use multiview_compositor::mask::WipeMask;
    vec![
        WipeMask {
            pinned: Some(f32::NAN),
            ..good
        },
        WipeMask {
            pinned: Some(2.0),
            ..good
        },
        WipeMask {
            pinned: Some(-1.0),
            ..good
        },
        WipeMask {
            radius: f32::NAN,
            ..good
        },
        WipeMask {
            softness: f32::INFINITY,
            ..good
        },
        WipeMask {
            origin: [f32::NAN, 0.0],
            ..good
        },
        WipeMask {
            pinned: Some(0.5),
            ..good
        },
        WipeMask { width: 0, ..good },
        WipeMask { height: 0, ..good },
        WipeMask {
            aspect: 0.0,
            ..good
        },
        WipeMask {
            aspect: -1.0,
            ..good
        },
        WipeMask {
            aspect: 2.0,
            ..good
        },
        WipeMask {
            width: 16,
            height: 16,
            ..good
        },
    ]
}

#[test]
fn a_malformed_wipe_mask_covers_nothing_and_never_poisons_the_layers_beneath() {
    // Merge review R-1: `WipeMask` has public fields, so a NaN or out-of-range
    // field must not reach the premultiply. A malformed mask covers nothing,
    // on a plain tile and on a keyed one, on both kernels.
    use multiview_compositor::mask::{MaskShape, WipeMask};
    let c = bt709(ColorRange::Limited);
    let bg = solid(8, 8, 140, 100, 160, c);
    let k = solid(8, 8, 220, 128, 128, c);
    let base = comp(8, 8, &[Tile::placed(&bg, 0, 0, 1.0)]);
    let good = WipeMask::new(
        MaskShape::Horizontal,
        0.5,
        8,
        8,
        [0.0, 0.0],
        false,
        0.1,
        0.0,
    );
    assert!(good.is_valid_for(8, 8));
    let malformed = malformed_masks(good);
    for mask in malformed {
        assert!(!mask.is_valid_for(8, 8), "{mask:?}");
        for keyed in [false, true] {
            let tile = Tile::placed(&k, 0, 0, 1.0).with_mask(mask);
            let tile = if keyed {
                tile.with_key(TileKey::linear())
            } else {
                tile
            };
            let tiles = [Tile::placed(&bg, 0, 0, 1.0), tile];
            let out = comp(8, 8, &tiles);
            assert_eq!(
                out, base,
                "malformed mask {mask:?} (keyed {keyed}) covers nothing"
            );
            let reference = composite_reference(8, 8, canvas(), BLACK, &tiles, true).unwrap();
            assert_eq!(reference, base, "reference kernel agrees for {mask:?}");
        }
    }
    // A well-formed keyed + masked tile is drawn through its mask: the left
    // half shows the keyer, the right half the background.
    let tiles = [
        Tile::placed(&bg, 0, 0, 1.0),
        Tile::placed(&k, 0, 0, 1.0)
            .with_key(TileKey::linear())
            .with_mask(WipeMask::new(
                MaskShape::Horizontal,
                0.5,
                8,
                8,
                [0.0, 0.0],
                false,
                0.0,
                0.0,
            )),
    ];
    let out = comp(8, 8, &tiles);
    assert_eq!(
        luma(&out, 0, 0),
        luma(&comp(8, 8, &[Tile::placed(&k, 0, 0, 1.0)]), 0, 0)
    );
    assert_eq!(luma(&out, 7, 0), luma(&base, 7, 0));
}

#[test]
fn only_background_changing_with_an_opaque_b_equals_the_incoming_tile_at_p() {
    // ADR-0113 §2: the merge reduces to today's incoming-tile-at-weight render.
    let c = bt709(ColorRange::Limited);
    let a = solid(4, 4, 80, 120, 140, c);
    let b = solid(4, 4, 190, 140, 110, c);
    let p = 0.375_f32;
    let grouped = comp(
        4,
        4,
        &[
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Out, mix(p))),
            Tile::placed(&b, 0, 0, 1.0).in_group(scene(SceneMember::In, mix(p))),
        ],
    );
    let flat = comp(
        4,
        4,
        &[Tile::placed(&a, 0, 0, 1.0), Tile::placed(&b, 0, 0, p)],
    );
    for (g, f) in grouped.y_plane().iter().zip(flat.y_plane()) {
        assert!(g.abs_diff(*f) <= 1, "{g} vs {f}");
    }
}

// --- production kernel == oracle; pooled, no per-tick growth ------------------

fn varied(w: u32, h: u32, seed: u32, color: ColorInfo) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let s = seed as usize;
    let y = (0..wu * hu)
        .map(|i| u8::try_from((i * 37 + s * 11) % 220 + 16).unwrap())
        .collect();
    let uv = (0..wu * hu / 2)
        .map(|i| u8::try_from((i * 53 + s * 7) % 200 + 28).unwrap())
        .collect();
    Nv12Image::new(w, h, y, uv, color).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn the_tile_driven_kernel_equals_the_oracle_with_keys_and_groups(
        seed in 0_u32..1000,
        fx in 0_u32..12, fy in 0_u32..6,
        clip in 0.0_f32..0.6, gain in 0.0_f32..8.0, invert in any::<bool>(),
        p in 0.0_f32..1.0, member in 0_u8..3, grouped in any::<bool>(), dip in any::<bool>(),
        threads in 1_usize..5,
    ) {
        let c = bt709(ColorRange::Limited);
        let bg_img = varied(32, 16, seed, c);
        let fill = varied(8, 6, seed + 1, c);
        let key = varied(10, 4, seed + 2, bt709(ColorRange::Full));
        let merge = if dip {
            SceneMerge::Dip { colour: LinearRgba::opaque(0.1, 0.3, 0.2), d: p, i: 1.0 - p }
        } else {
            SceneMerge::Mix { p }
        };
        let member = [SceneMember::Both, SceneMember::Out, SceneMember::In][usize::from(member)];
        let g = |m| if grouped { scene(m, merge) } else { TileGroup::Top };
        let tiles = [
            Tile::placed(&bg_img, 0, 0, 1.0).in_group(g(SceneMember::Both)),
            Tile::scaled(&fill, fx * 2, fy * 2, 12, 8, 0.9)
                .with_key(TileKey::luma(clip, gain).with_source(&key).inverted(invert)
                    .masked(KeyMask { x: 1, y: 0, w: 6, h: 5 }))
                .in_group(g(member)),
            Tile::scaled(&fill, 2, 2, 6, 4, 0.7).with_key(TileKey::linear().with_source(&key)),
        ];
        let oracle = composite_reference(32, 16, canvas(), BLACK, &tiles, true).unwrap();
        let fast = composite_with_threads(32, 16, canvas(), BLACK, &tiles, true, threads).unwrap();
        prop_assert_eq!(oracle, fast);
    }
}

#[test]
fn grouped_composites_reuse_the_pooled_scene_accumulators() {
    let c = bt709(ColorRange::Limited);
    let a = varied(64, 32, 1, c);
    let k = varied(32, 16, 2, c);
    let backend = RunBackend::cpu();
    let run = |p: f32| {
        let m = mix(p);
        let tiles = [
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Both, m)),
            Tile::placed(&k, 8, 8, 1.0)
                .with_key(TileKey::luma(0.2, 3.0))
                .in_group(scene(SceneMember::In, m)),
        ];
        backend
            .composite_with_thread_count(64, 32, canvas(), BLACK, &tiles, 1)
            .unwrap()
    };
    drop(run(0.1));
    let warm = backend.scratch_alloc_count();
    for i in 0..50_u8 {
        drop(run(f32::from(i) / 50.0));
    }
    assert_eq!(
        backend.scratch_alloc_count(),
        warm,
        "no growth after warmup"
    );
}

#[test]
fn keys_and_groups_across_band_boundaries_match_the_oracle_on_every_thread_count() {
    // 320x256 is above the parallel threshold, so 2-4 threads really split the
    // canvas into bands; the keyed and grouped tiles straddle band edges.
    let c = bt709(ColorRange::Limited);
    let (w, h) = (320_u32, 256_u32);
    let a = varied(w, h, 3, c);
    let b = varied(w, h, 4, c);
    let fill = varied(64, 48, 5, c);
    let key = varied(80, 40, 6, bt709(ColorRange::Full));
    for (merge, p) in [
        (SceneMerge::Mix { p: 0.4 }, 0.4_f32),
        (
            SceneMerge::Dip {
                colour: LinearRgba::opaque(0.1, 0.3, 0.2),
                d: 1.0,
                i: 0.3,
            },
            0.3,
        ),
    ] {
        let tiles = [
            Tile::placed(&a, 0, 0, 1.0).in_group(scene(SceneMember::Out, merge)),
            Tile::placed(&b, 0, 0, 1.0).in_group(scene(SceneMember::In, merge)),
            Tile::scaled(&fill, 40, 60, 200, 150, 0.9)
                .with_key(TileKey::luma(0.1, 3.0).with_source(&key).masked(KeyMask {
                    x: 4,
                    y: 2,
                    w: 50,
                    h: 40,
                }))
                .in_group(scene(SceneMember::In, merge)),
            Tile::scaled(&fill, 100, 20, 128, 220, p).with_key(TileKey::linear().with_source(&key)),
        ];
        let oracle = composite_reference(w, h, canvas(), BLACK, &tiles, true).unwrap();
        let backend = RunBackend::cpu();
        for threads in 1..=4 {
            let fast = backend
                .composite_with_thread_count(w, h, canvas(), BLACK, &tiles, threads)
                .unwrap();
            assert_eq!(fast, oracle, "threads={threads}");
        }
        assert!(backend.band_count() >= 2, "the parallel band path ran");
        let warm = backend.scratch_alloc_count();
        for _ in 0..5 {
            drop(
                backend
                    .composite_with_thread_count(w, h, canvas(), BLACK, &tiles, 4)
                    .unwrap(),
            );
        }
        assert_eq!(
            backend.scratch_alloc_count(),
            warm,
            "no growth after warmup"
        );
    }
}
