// Composite pass — front half of the fixed pipeline + premultiplied-alpha
// blend in linear light (invariant #8 steps 1-5).
//
//   range-expand -> YUV->RGB matrix -> linearize (tile EOTF) -> primaries
//   convert into the canvas gamut (linear) -> premultiplied-alpha source-over.
//
// NV12-throughout (invariant #5): each tile is two textures (R8 luma plane,
// Rg8 interleaved chroma plane), one array layer per tile; YUV->RGB happens
// here at sample time, never materializing an RGBA tile. Output is the single
// linear Rgba16Float canvas (straight, un-premultiplied, for the encode pass).
//
// Per-pixel alpha (NV12+A, ADR-0058): a tile with an alpha plane carries its
// full-resolution STRAIGHT R8 alpha in one layer of `a_planes` (only alpha
// tiles consume a layer). It is multiplied into the uniform opacity at the
// single premultiply step below, so alpha is premultiplied exactly once, in
// linear light, after primaries conversion (invariant #8 order untouched).
// Opaque tiles never touch `a_planes`.
//
// Keyers (ADR-0113, design note switcher-keyers.md §3): `keys[i]` runs
// parallel to `tiles[i]`. A keyed tile's coverage multiplies
// mask(sx, sy) × matte(kx, ky) into the same single premultiply; the matte
// reads only post-range-expand Y (step 1), of the fill or of a separate key
// source resident in its own `y_planes` layer, sampled with its own
// nearest-neighbour map (kx = floor(ddx × key_w / dst_w)). Mirrors
// `key::key_factor`. Scene groups fold into a second pair of vec4 registers
// (outgoing / incoming) merged at the next top tile or the end of the list,
// mirroring the CPU oracle's segment semantics (the last grouped tile's
// merge wins).
//
// `common.wgsl` is prepended at load time (transfer fns, mat3_apply, ids).

struct TileParams {
    // x,y: destination top-left on canvas (pixels). z,w: source w,h (pixels).
    placement: vec4<u32>,
    // x,y: destination rect w,h on canvas (pixels). The source is scaled into
    // this rect (scale-at-composite); equal to src w,h for a 1:1 tile.
    // z: has-alpha flag (1 = sample `a_planes`, 0 = opaque). w: alpha layer.
    dst_size: vec4<u32>,
    // x: opacity [0,1]. y: tile transfer id. z: wipe shape id (0 = none).
    // w: wipe reverse. The .w lanes of range/yuv2rgb*/prim0..1 carry the rest
    // of the wipe mask (softness, origin, extent, radius, expand).
    opacity_transfer: vec4<f32>,
    // Range expand: luma = (Y*255 - luma_off)/luma_scale,
    //               chroma = (C*255 - 128)/chroma_scale.
    // x: luma_scale, y: luma_off, z: chroma_scale, w: padding.
    range: vec4<f32>,
    // YUV'->R'G'B' 3x3 (row-major), padded to 3x vec4.
    yuv2rgb0: vec4<f32>,
    yuv2rgb1: vec4<f32>,
    yuv2rgb2: vec4<f32>,
    // Primaries source->canvas 3x3 (linear), padded to 3x vec4.
    prim0: vec4<f32>,
    prim1: vec4<f32>,
    prim2: vec4<f32>,
};

// Mirrors `uniforms::KeyParams`; all zero = an ordinary top tile, no key.
struct KeyParams {
    // x: key layer in `y_planes`, y,z: key source w,h, w: flags (KEY_* bits;
    // bits 8-9 the scene group: 0 top, 1 both, 2 out, 3 in).
    head: vec4<u32>,
    // x: base of this tile's 256-entry matte table in `mattes` (valid under
    // KEY_HAS_TABLE); y,z,w: padding.
    table: vec4<u32>,
    // Mask rectangle in fill-source pixels: x, y, w, h (half-open).
    mask: vec4<u32>,
    // x: merge kind (0 MIX, 1 DIP); y: p | d; z: i (all in [0, 1]).
    merge: vec4<f32>,
    // Opaque DIP colour (linear, canvas gamut), a = 1.
    dip_colour: vec4<f32>,
};

const KEY_KIND_BITS: u32 = 3u;
const KEY_KIND_LINEAR: u32 = 1u;
const KEY_INVERT: u32 = 4u;
const KEY_HAS_SOURCE: u32 = 8u;
const KEY_HAS_MASK: u32 = 16u;
const KEY_HAS_TABLE: u32 = 32u;
const KEY_GROUP_SHIFT: u32 = 8u;

struct CompositeUniforms {
    // x,y: canvas width,height (pixels). z: tile count. w: padding.
    canvas: vec4<u32>,
    // Background straight-alpha linear RGBA in the canvas gamut.
    background: vec4<f32>,
};

@group(0) @binding(0) var<uniform> comp: CompositeUniforms;
@group(0) @binding(1) var<storage, read> tiles: array<TileParams>;
@group(0) @binding(2) var y_planes: texture_2d_array<f32>;
@group(0) @binding(3) var uv_planes: texture_2d_array<f32>;
@group(0) @binding(4) var canvas_out: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var a_planes: texture_2d_array<f32>;
@group(0) @binding(6) var<storage, read> keys: array<KeyParams>;
// Per-key matte tables (review MEDIA-1): `mattes[base + c]` is the final
// matte (after invert) of 8-bit Y code c, built on the CPU with the oracle's
// own functions, so a step matte (a huge or infinite gain) is exact.
@group(0) @binding(7) var<storage, read> mattes: array<f32>;

// Nearest-neighbour index of destination offset d (< dst) on a source axis of
// src samples: floor(d × src / dst), clamped. Mirrors `key::axis`. d < dst and
// src are bounded by the texture dimension limit (<= 16384), so the u32
// product cannot overflow.
fn key_axis(d: u32, src: u32, dst: u32) -> u32 {
    if dst == 0u || src == 0u {
        return 0u;
    }
    return min((d * src) / dst, src - 1u);
}

// The integer 8-bit code of a code value v8 = unorm texel × 255 (texture-to-
// float fuzz is far below 0.5): round, clamped to [0, 255].
fn y_code(v8: f32) -> u32 {
    return u32(clamp(round(v8), 0.0, 255.0));
}

// The straight key factor mask × matte (before the single premultiply).
// Mirrors `key::key_factor`: a pixel outside the source-space mask is 0; a
// linear self key is 1 and ignores invert; every other matte is a pure
// function of the signal's 8-bit Y code (the key source's, else the fill's),
// looked up in the tile's table (`KeyParams::build` / `push_matte_table`).
fn key_factor(k: KeyParams, sx: u32, sy: u32, ddx: u32, ddy: u32, dst_w: u32, dst_h: u32,
              fill_y8: f32) -> f32 {
    let flags = k.head.w;
    let kind = flags & KEY_KIND_BITS;
    if kind == 0u {
        return 1.0;
    }
    if (flags & KEY_HAS_MASK) != 0u {
        // Half-open and overflow safe: compare the offsets, never x + w.
        let m = k.mask;
        if sx < m.x || sy < m.y || (sx - m.x) >= m.z || (sy - m.y) >= m.w {
            return 0.0;
        }
    }
    if (flags & KEY_HAS_TABLE) == 0u {
        // A linear self key: no signal, matte 1.
        return 1.0;
    }
    var code = y_code(fill_y8);
    if (flags & KEY_HAS_SOURCE) != 0u {
        let kx = key_axis(ddx, k.head.y, dst_w);
        let ky = key_axis(ddy, k.head.z, dst_h);
        code = y_code(textureLoad(y_planes, vec2<i32>(i32(kx), i32(ky)), i32(k.head.x), 0).r * 255.0);
    }
    return mattes[k.table.x + code];
}

// Merge the two scene accumulators (premultiplied linear). Mirrors
// `SceneMerge::apply`: MIX lerp(out, in, p); DIP lerp(lerp(out, c, d), in, i).
fn scene_merge(o: vec4<f32>, inc: vec4<f32>, m: vec4<f32>, dip: vec4<f32>) -> vec4<f32> {
    if m.x > 0.5 {
        let c = vec4<f32>(dip.rgb, 1.0);
        let od = o * (1.0 - m.y) + c * m.y;
        return od * (1.0 - m.z) + inc * m.z;
    }
    return o * (1.0 - m.y) + inc * m.y;
}

// Straight per-pixel alpha at integer tile-local (sx, sy), full resolution
// (no subsampling), or 1.0 for an opaque tile. Mirrors `pipeline::tile_alpha`.
fn sample_tile_alpha(t: TileParams, sx: u32, sy: u32) -> f32 {
    if t.dst_size.z == 0u {
        return 1.0;
    }
    return textureLoad(a_planes, vec2<i32>(i32(sx), i32(sy)), i32(t.dst_size.w), 0).r;
}

// Wipe-mask coverage at canvas pixel (px, py) (ADR-0112 §3). Mirrors
// `mask::WipeMask::coverage` exactly; shape id 0 is "no mask". Parameters ride
// the spare .z/.w lanes of TileParams (see `TileParams::with_mask`).
fn wipe_coverage(t: TileParams, px: u32, py: u32, cw: u32, ch: u32) -> f32 {
    let shape = u32(t.opacity_transfer.z + 0.5);
    if shape == 0u {
        return 1.0;
    }
    let aspect = f32(max(cw, 1u)) / f32(max(ch, 1u));
    let u = (f32(px) + 0.5) / f32(max(cw, 1u));
    let v = (f32(py) + 0.5) / f32(max(ch, 1u));
    let origin = vec2<f32>(t.yuv2rgb0.w, t.yuv2rgb1.w);
    let extent = t.yuv2rgb2.w;
    var radius = t.prim0.w;
    var sign = 1.0;
    if t.opacity_transfer.w > 0.5 {
        radius = extent - radius;
        sign = -1.0;
    }
    let inv_sqrt2 = 0.70710678118654752;
    // Signed distance to the edge in canvas-height units, positive inside
    // (mirrors `mask::inside`).
    var inside = 0.0;
    switch shape {
        case 1u: { inside = radius - u * aspect; }
        case 2u: { inside = radius - v; }
        case 3u: { inside = radius - (u * aspect + v) * inv_sqrt2; }
        case 4u: {
            inside = min(aspect * (radius - abs(u - origin.x)), radius - abs(v - origin.y));
        }
        default: {
            let dx = (u - origin.x) * aspect;
            let dy = v - origin.y;
            inside = radius - sqrt(dx * dx + dy * dy);
        }
    }
    let d = sign * inside;
    let softness = t.range.w;
    let expand = t.prim1.w;
    // A border band (expand > 0) is the grown edge attenuated by the edge's
    // own coverage: smooth on both sides, never the revealed interior
    // (mirrors `mask::coverage`).
    if expand > 0.0 {
        return wipe_edge(d + expand, softness) * (1.0 - wipe_edge(d, softness));
    }
    return wipe_edge(d, softness);
}

// Coverage of signed distance d through an edge of width softness (0 = hard).
// Mirrors `mask::edge`.
fn wipe_edge(d: f32, softness: f32) -> f32 {
    // A subnormal softness is a hard edge (mirrors `mask::edge`): the GPU may
    // flush it to zero, so both paths treat it so. 1.17549435e-38 is
    // f32::MIN_POSITIVE, the smallest normal f32.
    if softness >= 1.17549435e-38 {
        // Saturate before dividing (mirrors `mask::edge`): a tiny softness
        // would overflow `d / softness`. `2·d` is exact where `softness / 2`
        // can round, so this is the old clamp boundary; inside it the ratio
        // is <= 0.5.
        let d2 = d * 2.0;
        if d2 >= softness {
            return 1.0;
        }
        if d2 <= -softness {
            return 0.0;
        }
        return clamp(d / softness + 0.5, 0.0, 1.0);
    }
    return select(0.0, 1.0, d >= 0.0);
}

// Nearest-neighbour NV12 fetch at integer tile-local (sx, sy), matching the
// CPU reference's chroma replication (no siting interpolation in the oracle).
fn sample_tile_yuv(layer: i32, sx: u32, sy: u32) -> vec3<f32> {
    let y = textureLoad(y_planes, vec2<i32>(i32(sx), i32(sy)), layer, 0).r;
    let cx = i32(sx / 2u);
    let cy = i32(sy / 2u);
    let uv = textureLoad(uv_planes, vec2<i32>(cx, cy), layer, 0).rg;
    return vec3<f32>(y, uv.r, uv.g);
}

@compute @workgroup_size(8, 8, 1)
fn composite_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cw = comp.canvas.x;
    let ch = comp.canvas.y;
    if gid.x >= cw || gid.y >= ch {
        return;
    }
    let px = gid.x;
    let py = gid.y;

    // Accumulator in PREMULTIPLIED linear RGBA, seeded with the background.
    let bg = comp.background;
    var acc = vec4<f32>(bg.rgb * bg.a, bg.a);

    // The scene segment (ADR-0113 §2): two extra registers, open while a run
    // of grouped tiles is being folded.
    var seg_open = false;
    var seg_out = acc;
    var seg_in = acc;
    var seg_merge = vec4<f32>(0.0);
    var seg_dip = vec4<f32>(0.0);

    let count = comp.canvas.z;
    for (var i: u32 = 0u; i < count; i = i + 1u) {
        let t = tiles[i];
        let k = keys[i];
        let group = (k.head.w >> KEY_GROUP_SHIFT) & 3u;
        // Segment bookkeeping happens before the coverage test: a grouped
        // tile that covers nothing still opens (and re-merges) its segment,
        // and a top tile closes it, exactly as on the CPU oracle.
        if group == 0u {
            if seg_open {
                acc = scene_merge(seg_out, seg_in, seg_merge, seg_dip);
                seg_open = false;
            }
        } else {
            if !seg_open {
                seg_out = acc;
                seg_in = acc;
                seg_open = true;
            }
            seg_merge = k.merge;
            seg_dip = k.dip_colour;
        }
        let dst = t.placement.xy;
        let src_w = t.placement.z;
        let src_h = t.placement.w;
        // Destination rect extent (the source is scaled into it). A zero size
        // (defensive) falls back to the source size = 1:1 placement.
        var dst_w = t.dst_size.x;
        var dst_h = t.dst_size.y;
        if dst_w == 0u { dst_w = src_w; }
        if dst_h == 0u { dst_h = src_h; }
        if px < dst.x || py < dst.y {
            continue;
        }
        // Destination-rect-local offset.
        let ddx = px - dst.x;
        let ddy = py - dst.y;
        if ddx >= dst_w || ddy >= dst_h {
            continue;
        }
        // Scale-at-composite: nearest-neighbour map of the destination offset to
        // the source texel — floor(dd * src / dst), clamped to the last texel.
        // Matches the CPU reference `map_axis` (identity when dst == src).
        var sx = ddx;
        var sy = ddy;
        if dst_w != src_w {
            sx = min((ddx * src_w) / dst_w, src_w - 1u);
        }
        if dst_h != src_h {
            sy = min((ddy * src_h) / dst_h, src_h - 1u);
        }

        // 8-bit code values in [0,255] (textures deliver [0,1], scale up).
        let yuv8 = sample_tile_yuv(i32(i), sx, sy) * 255.0;

        // 1. range expand (code-value space).
        let luma_scale = t.range.x;
        let luma_off = t.range.y;
        let chroma_scale = t.range.z;
        let yexp = (yuv8.x - luma_off) / luma_scale;
        let cb = (yuv8.y - 128.0) / chroma_scale;
        let cr = (yuv8.z - 128.0) / chroma_scale;

        // 2. YUV' -> R'G'B' (gamma-encoded) with the tile matrix.
        let rgb_gamma = mat3_apply(t.yuv2rgb0, t.yuv2rgb1, t.yuv2rgb2, vec3<f32>(yexp, cb, cr));

        // 3. linearize via the tile EOTF.
        let tid = u32(t.opacity_transfer.y);
        let lin_tile = vec3<f32>(
            eotf(rgb_gamma.x, tid),
            eotf(rgb_gamma.y, tid),
            eotf(rgb_gamma.z, tid),
        );

        // 4. primaries convert into the canvas gamut (linear).
        let lin = mat3_apply(t.prim0, t.prim1, t.prim2, lin_tile);

        // 5. premultiplied-alpha source-over in linear light. Straight
        //    per-pixel alpha folds into the opacity here, premultiplied ONCE.
        //    A keyer's mask × matte (ADR-0113 §3) folds in at the same step;
        //    its matte is a table lookup on the fill's (or key source's) 8-bit
        //    Y code, the input of step 1 (never steps 2-4).
        let a = clamp(t.opacity_transfer.x, 0.0, 1.0) * sample_tile_alpha(t, sx, sy)
            * wipe_coverage(t, px, py, cw, ch)
            * key_factor(k, sx, sy, ddx, ddy, dst_w, dst_h, yuv8.x);
        let src = vec4<f32>(lin * a, a);
        let inv = 1.0 - src.a;
        switch group {
            case 0u: { acc = src + acc * inv; }
            case 2u: { seg_out = src + seg_out * inv; }
            case 3u: { seg_in = src + seg_in * inv; }
            default: {
                seg_out = src + seg_out * inv;
                seg_in = src + seg_in * inv;
            }
        }
    }
    if seg_open {
        acc = scene_merge(seg_out, seg_in, seg_merge, seg_dip);
    }

    // Store the straight (un-premultiplied) linear RGBA for the encode pass.
    var straight = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    if acc.a != 0.0 {
        straight = vec4<f32>(acc.rgb / acc.a, acc.a);
    }
    textureStore(canvas_out, vec2<i32>(i32(px), i32(py)), straight);
}
