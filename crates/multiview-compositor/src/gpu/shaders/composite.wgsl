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
// `common.wgsl` is prepended at load time (transfer fns, mat3_apply, ids).

struct TileParams {
    // x,y: destination top-left on canvas (pixels). z,w: source w,h (pixels).
    placement: vec4<u32>,
    // x,y: destination rect w,h on canvas (pixels). z,w: padding. The source is
    // scaled into this rect (scale-at-composite); equal to src w,h for a 1:1 tile.
    dst_size: vec4<u32>,
    // x: opacity [0,1]. y: tile transfer id. z,w: padding.
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

    let count = comp.canvas.z;
    for (var i: u32 = 0u; i < count; i = i + 1u) {
        let t = tiles[i];
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

        // 5. premultiplied-alpha source-over in linear light.
        let a = clamp(t.opacity_transfer.x, 0.0, 1.0);
        let src = vec4<f32>(lin * a, a);
        let inv = 1.0 - src.a;
        acc = src + acc * inv;
    }

    // Store the straight (un-premultiplied) linear RGBA for the encode pass.
    var straight = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    if acc.a != 0.0 {
        straight = vec4<f32>(acc.rgb / acc.a, acc.a);
    }
    textureStore(canvas_out, vec2<i32>(i32(px), i32(py)), straight);
}
