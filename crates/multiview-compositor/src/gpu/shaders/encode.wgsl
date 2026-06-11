// Encode pass — back half of the fixed pipeline (invariant #8 steps 6-8):
//
//   canvas OETF (linear -> gamma) -> RGB->YUV (canvas matrix) -> range
//   compress -> write NV12 (Y full-res R8 plane, UV half-res Rg8 interleaved).
//
// Reads the straight linear Rgba16Float canvas produced by the composite pass.
// `common.wgsl` is prepended at load time (transfer fns, mat3_apply,
// quantize_unorm). Output stays NV12 (invariant #5).

struct EncodeUniforms {
    // x,y: canvas width,height (pixels). z: canvas transfer id. w: padding.
    canvas: vec4<u32>,
    // Canvas RGB->YUV 3x3 (row-major), padded to 3x vec4.
    rgb2yuv0: vec4<f32>,
    rgb2yuv1: vec4<f32>,
    rgb2yuv2: vec4<f32>,
    // Range compress: Y8 = luma*luma_scale + luma_off,
    //                 C8 = chroma*chroma_scale + 128.
    // x: luma_scale, y: luma_off, z: chroma_scale, w: padding.
    range: vec4<f32>,
};

@group(0) @binding(0) var<uniform> enc: EncodeUniforms;
@group(0) @binding(1) var canvas_in: texture_2d<f32>;
@group(0) @binding(2) var y_out: texture_storage_2d<r8unorm, write>;
@group(0) @binding(3) var uv_out: texture_storage_2d<rg8unorm, write>;

// Run steps 6-8 at one canvas pixel, returning 8-bit code values (pre-quantize).
fn encode_pixel_yuv(px: i32, py: i32) -> vec3<f32> {
    let lin = textureLoad(canvas_in, vec2<i32>(px, py), 0).rgb;
    let tid = enc.canvas.z;
    // 6. canvas OETF (linear -> gamma code values).
    let gamma = vec3<f32>(oetf(lin.x, tid), oetf(lin.y, tid), oetf(lin.z, tid));
    // 7. RGB -> YUV with the canvas matrix.
    let yuv = mat3_apply(enc.rgb2yuv0, enc.rgb2yuv1, enc.rgb2yuv2, gamma);
    // 8. range compress to 8-bit code values.
    let ls = enc.range.x;
    let lo = enc.range.y;
    let cs = enc.range.z;
    return vec3<f32>(
        yuv.x * ls + lo,
        yuv.y * cs + 128.0,
        yuv.z * cs + 128.0,
    );
}

@compute @workgroup_size(8, 8, 1)
fn encode_y_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cw = enc.canvas.x;
    let ch = enc.canvas.y;
    if gid.x >= cw || gid.y >= ch {
        return;
    }
    let yuv = encode_pixel_yuv(i32(gid.x), i32(gid.y));
    textureStore(
        y_out,
        vec2<i32>(i32(gid.x), i32(gid.y)),
        vec4<f32>(quantize_unorm(yuv.x), 0.0, 0.0, 0.0),
    );
}

@compute @workgroup_size(8, 8, 1)
fn encode_uv_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let half_w = enc.canvas.x / 2u;
    let half_h = enc.canvas.y / 2u;
    if gid.x >= half_w || gid.y >= half_h {
        return;
    }
    // Match the CPU reference's last-writer-wins within each 2x2 block: in
    // raster order the bottom-right pixel (odd x, odd y) writes the chroma pair.
    let px = i32(gid.x * 2u + 1u);
    let py = i32(gid.y * 2u + 1u);
    let yuv = encode_pixel_yuv(px, py);
    textureStore(
        uv_out,
        vec2<i32>(i32(gid.x), i32(gid.y)),
        vec4<f32>(quantize_unorm(yuv.y), quantize_unorm(yuv.z), 0.0, 0.0),
    );
}
