// Shared color math for the fixed-order compositor pipeline (invariant #8).
//
// This prelude is concatenated ahead of `composite.wgsl` and `encode.wgsl`
// (see `crate::gpu::shader`). It mirrors the pure-Rust CPU reference in
// `crate::transfer` / `crate::matrix` / `crate::primaries` so the GPU output
// matches the CPU oracle within an SSIM/PSNR threshold (GPU is never
// bit-exact). Transfer-function ids match `crate::gpu::TransferId`.

const DISPLAY_GAMMA: f32 = 2.4;

fn signed_pow(v: f32, e: f32) -> f32 {
    if v < 0.0 {
        return -pow(-v, e);
    }
    return pow(v, e);
}

// BT.1886 display EOTF (code -> linear), modeled as a pure 2.4 power, used to
// decode SDR BT.709/BT.601/BT.2020 video (mirrors crate::transfer::bt709_eotf).
fn bt1886_eotf(c: f32) -> f32 {
    return signed_pow(c, DISPLAY_GAMMA);
}

fn bt1886_oetf_inverse(l: f32) -> f32 {
    return signed_pow(l, 1.0 / DISPLAY_GAMMA);
}

fn srgb_eotf(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn srgb_oetf(l: f32) -> f32 {
    if l <= 0.0031308 {
        return 12.92 * l;
    }
    return 1.055 * pow(l, 1.0 / 2.4) - 0.055;
}

const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

fn pq_eotf(e_in: f32) -> f32 {
    let e = clamp(e_in, 0.0, 1.0);
    let ep = pow(e, 1.0 / PQ_M2);
    let num = max(ep - PQ_C1, 0.0);
    let den = PQ_C2 - PQ_C3 * ep;
    if den <= 0.0 {
        return 0.0;
    }
    return pow(num / den, 1.0 / PQ_M1);
}

fn pq_oetf(l_in: f32) -> f32 {
    let l = clamp(l_in, 0.0, 1.0);
    let lm = pow(l, PQ_M1);
    return pow((PQ_C1 + PQ_C2 * lm) / (1.0 + PQ_C3 * lm), PQ_M2);
}

const HLG_A: f32 = 0.17883277;
const HLG_B: f32 = 0.28466892;
const HLG_C: f32 = 0.55991073;

// HLG inverse-OETF (code -> scene linear); mirrors crate::transfer::hlg_eotf.
fn hlg_eotf(v_in: f32) -> f32 {
    let v = max(v_in, 0.0);
    if v <= 0.5 {
        return v * v / 3.0;
    }
    return (exp((v - HLG_C) / HLG_A) + HLG_B) / 12.0;
}

// HLG OETF (scene linear -> code); mirrors crate::transfer::hlg_oetf.
fn hlg_oetf(l_in: f32) -> f32 {
    let l = max(l_in, 0.0);
    if l <= 1.0 / 12.0 {
        return sqrt(3.0 * l);
    }
    return HLG_A * log(12.0 * l - HLG_B) + HLG_C;
}

// EOTF dispatch by id (mirrors crate::transfer::eotf). Default = BT.1886.
fn eotf(c: f32, id: u32) -> f32 {
    switch id {
        case 1u: { return srgb_eotf(c); }
        case 2u: { return pq_eotf(c); }
        case 3u: { return hlg_eotf(c); }
        default: { return bt1886_eotf(c); }
    }
}

// OETF dispatch by id (mirrors crate::transfer::oetf). Default = BT.1886.
fn oetf(l: f32, id: u32) -> f32 {
    switch id {
        case 1u: { return srgb_oetf(l); }
        case 2u: { return pq_oetf(l); }
        case 3u: { return hlg_oetf(l); }
        default: { return bt1886_oetf_inverse(l); }
    }
}

fn mat3_apply(r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        r0.x * v.x + r0.y * v.y + r0.z * v.z,
        r1.x * v.x + r1.y * v.y + r1.z * v.z,
        r2.x * v.x + r2.y * v.y + r2.z * v.z,
    );
}

// Round half away from zero, clamp to [0,255], then normalize to [0,1] for an
// r8/rg8 unorm store (mirrors crate::range::quantize_u8 + the u8 encoding).
fn quantize_unorm(code: f32) -> f32 {
    let r = clamp(round(code), 0.0, 255.0);
    return r / 255.0;
}
