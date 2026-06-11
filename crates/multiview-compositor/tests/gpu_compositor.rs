//! GPU compositor vs CPU reference, validated by SSIM/PSNR (GPU is never
//! bit-exact; core-engine §19 testing tiers).
//!
//! These tests REQUIRE a GPU. In a GPU-free environment (this devcontainer,
//! most CI runners) [`GpuCompositor::new`] returns a typed
//! `Error::NoAdapter`/`Error::DeviceRequest`, and each test SKIPS gracefully
//! (prints and returns) rather than failing — so the suite is green on
//! GPU-less machines while still exercising the real GPU path where one exists.
#![cfg(feature = "wgpu")]
// Test-only relaxations: integration tests are not covered by clippy.toml's
// `allow-*-in-tests`, so the denied lints are re-allowed here per the
// guardrails header convention. The numeric `cast_*` allows apply only to the
// throwaway test-fixture generators (gradient tiles) and the SSIM/PSNR scoring
// math, never to product code.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::error::Error;
use multiview_compositor::gpu::GpuCompositor;
use multiview_compositor::pipeline::{composite as cpu_composite, CanvasColor, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

/// Acquire a GPU compositor, or `None` (with a printed reason) when there is no
/// usable adapter — the graceful-skip path for GPU-free CI.
fn try_gpu() -> Option<GpuCompositor> {
    match GpuCompositor::new(None) {
        Ok(gpu) => Some(gpu),
        Err(Error::NoAdapter(reason) | Error::DeviceRequest(reason)) => {
            println!("SKIP: no usable GPU adapter ({reason})");
            None
        }
        Err(other) => panic!("unexpected GPU init error (not a no-adapter skip): {other}"),
    }
}

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn bt601_limited_525() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt601_525,
        transfer: TransferCharacteristic::Bt601,
        matrix: MatrixCoefficients::Bt601,
        range: ColorRange::Limited,
    }
}

/// Build a smooth-gradient NV12 tile (gradients exercise the per-pixel color
/// math far better than flat fills, which can hide matrix/transfer errors).
fn gradient_tile(w: u32, h: u32, color: ColorInfo) -> Nv12Image {
    let wu = w as usize;
    let hu = h as usize;
    let mut y = vec![0u8; wu * hu];
    for (i, px) in y.iter_mut().enumerate() {
        let x = (i % wu) as u32;
        let row = (i / wu) as u32;
        // Diagonal luma ramp across the limited-range band 16..=235.
        let t = (x + row) as f32 / ((w + h) as f32).max(1.0);
        *px = (16.0 + t * 219.0).round().clamp(16.0, 235.0) as u8;
    }
    let mut uv = vec![0u8; wu * hu / 2];
    for (j, pair) in uv.chunks_exact_mut(2).enumerate() {
        let cx = (j % (wu / 2)) as u32;
        let cy = (j / (wu / 2)) as u32;
        // Cb ramps with x, Cr ramps with y, around the neutral 128.
        let cb = 64.0 + (cx as f32 / (w as f32 / 2.0).max(1.0)) * 128.0;
        let cr = 64.0 + (cy as f32 / (h as f32 / 2.0).max(1.0)) * 128.0;
        pair[0] = cb.round().clamp(16.0, 240.0) as u8;
        pair[1] = cr.round().clamp(16.0, 240.0) as u8;
    }
    Nv12Image::new(w, h, y, uv, color).expect("gradient tile geometry")
}

/// Population PSNR over the Y plane in dB (`f64::INFINITY` for identical input).
fn psnr_y(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "plane length mismatch");
    let mut sse = 0.0_f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = f64::from(i32::from(*x) - i32::from(*y));
        sse += d * d;
    }
    if sse == 0.0 {
        return f64::INFINITY;
    }
    let mse = sse / (a.len() as f64);
    10.0 * (255.0 * 255.0 / mse).log10()
}

/// Global SSIM over the Y plane (single-window, the standard luminance/
/// contrast/structure product with the usual `C1`/`C2` stabilizers). 1.0 means
/// identical; >= 0.98 is the GPU-vs-CPU acceptance floor here.
fn ssim_y(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "plane length mismatch");
    let n = a.len() as f64;
    let mean = |s: &[u8]| s.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
    let mu_a = mean(a);
    let mu_b = mean(b);
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    let mut cov = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let da = f64::from(*x) - mu_a;
        let db = f64::from(*y) - mu_b;
        var_a += da * da;
        var_b += db * db;
        cov += da * db;
    }
    var_a /= n;
    var_b /= n;
    cov /= n;
    let c1 = (0.01 * 255.0_f64).powi(2);
    let c2 = (0.03 * 255.0_f64).powi(2);
    ((2.0 * mu_a * mu_b + c1) * (2.0 * cov + c2))
        / ((mu_a * mu_a + mu_b * mu_b + c1) * (var_a + var_b + c2))
}

/// Run the SAME composite on the GPU and the CPU reference and assert the GPU
/// output matches the oracle within the SSIM/PSNR floor.
fn assert_gpu_matches_cpu(
    gpu: &GpuCompositor,
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
) {
    let cpu = cpu_composite(canvas_w, canvas_h, canvas, background, tiles)
        .expect("CPU reference composite");
    let got = gpu
        .composite(canvas_w, canvas_h, canvas, background, tiles)
        .expect("GPU composite");

    assert_eq!(got.width(), cpu.width());
    assert_eq!(got.height(), cpu.height());
    assert_eq!(got.color(), cpu.color(), "output must carry the canvas tag");

    let y_ssim = ssim_y(got.y_plane(), cpu.y_plane());
    let y_psnr = psnr_y(got.y_plane(), cpu.y_plane());
    let uv_ssim = ssim_y(got.uv_plane(), cpu.uv_plane());
    let uv_psnr = psnr_y(got.uv_plane(), cpu.uv_plane());
    println!(
        "GPU vs CPU: Y SSIM={y_ssim:.5} PSNR={y_psnr:.2}dB | UV SSIM={uv_ssim:.5} PSNR={uv_psnr:.2}dB"
    );

    assert!(y_ssim >= 0.98, "Y SSIM {y_ssim:.5} below 0.98 floor");
    assert!(y_psnr >= 40.0, "Y PSNR {y_psnr:.2}dB below 40 dB floor");
    assert!(uv_ssim >= 0.98, "UV SSIM {uv_ssim:.5} below 0.98 floor");
    assert!(uv_psnr >= 40.0, "UV PSNR {uv_psnr:.2}dB below 40 dB floor");
}

#[test]
fn gpu_matches_cpu_single_bt709_gradient_tile() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    let tile_img = gradient_tile(64, 64, bt709_limited());
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    assert_gpu_matches_cpu(&gpu, 64, 64, canvas, LinearRgba::TRANSPARENT, &tiles);
}

#[test]
fn gpu_matches_cpu_cross_geometry_scaled_tiles() {
    // RT-6 / ADR-0034 scale-at-composite parity: the GPU must scale a source into
    // a differently-sized destination cell with the same nearest-neighbour mapping
    // as the CPU reference. A DOWNSCALE (64x64 source -> 24x24 cell) and an
    // UPSCALE (16x16 source -> 40x40 cell) together exercise both directions; the
    // SSIM floor confirms the GPU resample tracks the CPU oracle.
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    let big = gradient_tile(64, 64, bt709_limited());
    let small = gradient_tile(16, 16, bt709_limited());
    let bg = LinearRgba::opaque(0.02, 0.02, 0.02);
    let tiles = [
        Tile::scaled(&big, 0, 0, 24, 24, 1.0), // downscale into a 24x24 cell
        Tile::scaled(&small, 24, 24, 40, 40, 1.0), // upscale into a 40x40 cell
    ];
    assert_gpu_matches_cpu(&gpu, 64, 64, canvas, bg, &tiles);
}

#[test]
fn gpu_matches_cpu_mixed_colorspace_quad() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    // Two tiles in DIFFERENT color spaces (BT.709 and BT.601-525) exercise the
    // per-tile matrix + primaries conversion the GPU must reproduce.
    let a = gradient_tile(32, 32, bt709_limited());
    let b = gradient_tile(32, 32, bt601_limited_525());
    let bg = LinearRgba::opaque(0.05, 0.05, 0.05);
    let tiles = [Tile::placed(&a, 0, 0, 1.0), Tile::placed(&b, 32, 32, 1.0)];
    assert_gpu_matches_cpu(&gpu, 64, 64, canvas, bg, &tiles);
}

#[test]
fn gpu_matches_cpu_with_partial_opacity_overlap() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    // Overlapping tiles with partial opacity exercise premultiplied-alpha
    // source-over in LINEAR light (the step most sensitive to doing the blend
    // in the wrong space).
    let bottom = gradient_tile(48, 48, bt709_limited());
    let top = gradient_tile(48, 48, bt709_limited());
    let bg = LinearRgba::opaque(0.1, 0.2, 0.3);
    let tiles = [
        Tile::placed(&bottom, 0, 0, 1.0),
        Tile::placed(&top, 16, 16, 0.5),
    ];
    assert_gpu_matches_cpu(&gpu, 64, 64, canvas, bg, &tiles);
}

#[test]
fn gpu_rejects_too_many_tiles() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let img = gradient_tile(4, 4, bt709_limited());
    let many: Vec<Tile<'_>> = (0..=(multiview_compositor::gpu::MAX_TILES as usize))
        .map(|_| Tile::placed(&img, 0, 0, 1.0))
        .collect();
    let err = gpu
        .composite(8, 8, CanvasColor::default(), LinearRgba::TRANSPARENT, &many)
        .expect_err("over MAX_TILES must be rejected");
    assert!(matches!(err, Error::GpuLimit(_)), "got {err:?}");
}
