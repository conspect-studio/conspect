//! SW-8b (#109, ADR-0058): GPU per-tile alpha vs the CPU oracle (SW-8a).
//!
//! NV12+A tiles carry a full-resolution STRAIGHT alpha plane. The GPU samples it
//! from a pooled R8 texture-array and multiplies it into the tile opacity at the
//! single premultiply step, in linear light — exactly like the CPU reference.
//! Validated by SSIM/PSNR (GPU is never bit-exact) at the same floors as
//! `gpu_compositor.rs` (SSIM >= 0.98, PSNR >= 40 dB on Y and UV), plus a tight
//! per-pixel bound on a flat half-alpha tile that a double premultiply fails.
//!
//! These tests REQUIRE a GPU adapter. GPU-free (CI, this devcontainer without a
//! Vulkan ICD) `GpuCompositor::new` returns `NoAdapter`/`DeviceRequest` and each
//! test SKIPS (prints and returns), following the `gpu_compositor.rs` pattern.
#![cfg(feature = "wgpu")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Arc;

use multiview_compositor::backend::{GpuTarget, RunBackend};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::error::Error;
use multiview_compositor::gpu::GpuCompositor;
use multiview_compositor::pipeline::{
    composite as cpu_composite, AlphaPlanePool, CanvasColor, Nv12Image, Tile,
};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

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

fn gradient_tile(w: u32, h: u32) -> Nv12Image {
    let wu = w as usize;
    let hu = h as usize;
    let mut y = vec![0u8; wu * hu];
    for (i, px) in y.iter_mut().enumerate() {
        let t = ((i % wu) + (i / wu)) as f32 / (wu + hu) as f32;
        *px = (16.0 + t * 219.0).round() as u8;
    }
    let mut uv = vec![0u8; wu * hu / 2];
    for (j, pair) in uv.chunks_exact_mut(2).enumerate() {
        let cx = (j % (wu / 2)) as f32 / (wu as f32 / 2.0);
        let cy = (j / (wu / 2)) as f32 / (hu as f32 / 2.0);
        pair[0] = (64.0 + cx * 128.0).round() as u8;
        pair[1] = (64.0 + cy * 128.0).round() as u8;
    }
    Nv12Image::new(w, h, y, uv, bt709_limited()).expect("gradient geometry")
}

/// Straight alpha: a horizontal 0..255 ramp with a hard-edged transparent hole
/// (the key edge a graphic or VT clip with alpha produces).
fn alpha_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut a = Vec::with_capacity((w * h) as usize);
    for row in 0..h {
        for col in 0..w {
            let hole = col > w / 4 && col < w / 2 && row > h / 4 && row < h / 2;
            a.push(if hole {
                0
            } else {
                (col * 255 / (w - 1).max(1)) as u8
            });
        }
    }
    a
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sse: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(i32::from(*x) - i32::from(*y)).powi(2))
        .sum();
    if sse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 / (sse / a.len() as f64)).log10()
}

fn ssim(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let n = a.len() as f64;
    let mean = |s: &[u8]| s.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
    let (ma, mb) = (mean(a), mean(b));
    let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        let (da, db) = (f64::from(*x) - ma, f64::from(*y) - mb);
        va += da * da;
        vb += db * db;
        cov += da * db;
    }
    let (va, vb, cov) = (va / n, vb / n, cov / n);
    let c1 = (0.01 * 255.0_f64).powi(2);
    let c2 = (0.03 * 255.0_f64).powi(2);
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

fn assert_matches_oracle(gpu_out: &Nv12Image, cpu_out: &Nv12Image) {
    assert_eq!(gpu_out.color(), cpu_out.color());
    let ys = ssim(gpu_out.y_plane(), cpu_out.y_plane());
    let yp = psnr(gpu_out.y_plane(), cpu_out.y_plane());
    let us = ssim(gpu_out.uv_plane(), cpu_out.uv_plane());
    let up = psnr(gpu_out.uv_plane(), cpu_out.uv_plane());
    println!("alpha GPU vs CPU: Y SSIM={ys:.5} PSNR={yp:.2}dB | UV SSIM={us:.5} PSNR={up:.2}dB");
    assert!(ys >= 0.98, "Y SSIM {ys:.5} below 0.98 floor");
    assert!(yp >= 40.0, "Y PSNR {yp:.2}dB below 40 dB floor");
    assert!(us >= 0.98, "UV SSIM {us:.5} below 0.98 floor");
    assert!(up >= 40.0, "UV PSNR {up:.2}dB below 40 dB floor");
}

fn gate(gpu: &GpuCompositor, w: u32, h: u32, bg: LinearRgba, tiles: &[Tile<'_>]) {
    let canvas = CanvasColor::default();
    let cpu_out = cpu_composite(w, h, canvas, bg, tiles).expect("CPU oracle");
    let gpu_out = gpu
        .composite(w, h, canvas, bg, tiles)
        .expect("GPU must composite alpha tiles itself (no CPU fallback)");
    assert_matches_oracle(&gpu_out, &cpu_out);
}

#[test]
fn gpu_alpha_gradient_tile_matches_cpu_oracle() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let pool = Arc::new(AlphaPlanePool::default());
    let fill = pool
        .attach(gradient_tile(64, 64), &alpha_pattern(64, 64))
        .unwrap();
    let bg = LinearRgba::opaque(0.1, 0.2, 0.3);
    gate(&gpu, 64, 64, bg, &[Tile::placed(&fill, 0, 0, 1.0)]);
}

#[test]
fn gpu_alpha_over_opaque_with_partial_opacity_and_scaling_matches_cpu_oracle() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let pool = Arc::new(AlphaPlanePool::default());
    let under = gradient_tile(96, 64);
    // Alpha sampled at the SAME nearest-neighbour source texel as the colour.
    let down = pool
        .attach(gradient_tile(64, 64), &alpha_pattern(64, 64))
        .unwrap();
    let up = pool
        .attach(gradient_tile(16, 16), &alpha_pattern(16, 16))
        .unwrap();
    let tiles = [
        Tile::placed(&under, 0, 0, 1.0),
        Tile::scaled(&down, 0, 0, 40, 40, 0.6),
        Tile::scaled(&up, 40, 16, 48, 48, 1.0),
    ];
    gate(&gpu, 96, 64, LinearRgba::opaque(0.02, 0.02, 0.02), &tiles);
}

#[test]
fn gpu_six_alpha_tiles_among_opaque_matches_cpu_oracle() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let pool = Arc::new(AlphaPlanePool::default());
    let opaque = [
        gradient_tile(32, 32),
        gradient_tile(32, 32),
        gradient_tile(32, 32),
    ];
    let alpha: Vec<Nv12Image> = (0..6)
        .map(|i| {
            let side = 16 + 8 * (i % 3);
            pool.attach(gradient_tile(side, side), &alpha_pattern(side, side))
                .unwrap()
        })
        .collect();
    let mut tiles: Vec<Tile<'_>> = opaque
        .iter()
        .enumerate()
        .map(|(i, img)| Tile::placed(img, 32 * i as u32, 0, 1.0))
        .collect();
    for (i, img) in alpha.iter().enumerate() {
        tiles.push(Tile::scaled(img, 16 * i as u32, 8, 32, 32, 0.9));
    }
    gate(&gpu, 96, 48, LinearRgba::opaque(0.05, 0.05, 0.05), &tiles);
}

#[test]
fn gpu_premultiplies_straight_alpha_exactly_once() {
    // A flat tile at straight alpha 128 x opacity 0.5 over a flat background:
    // the CPU oracle premultiplies once. A double premultiply on the GPU
    // (alpha^2) would miss the oracle by far more than this f32-drift bound.
    let Some(gpu) = try_gpu() else {
        return;
    };
    let pool = Arc::new(AlphaPlanePool::default());
    let fill = pool
        .attach(
            Nv12Image::solid(32, 32, 220, 90, 170, bt709_limited()).unwrap(),
            &[128; 32 * 32],
        )
        .unwrap();
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.3, 0.05);
    let tiles = [Tile::placed(&fill, 0, 0, 0.5)];
    let cpu_out = cpu_composite(32, 32, canvas, bg, &tiles).unwrap();
    let gpu_out = gpu.composite(32, 32, canvas, bg, &tiles).unwrap();
    for (plane, (g, c)) in [
        (gpu_out.y_plane(), cpu_out.y_plane()),
        (gpu_out.uv_plane(), cpu_out.uv_plane()),
    ]
    .into_iter()
    .enumerate()
    {
        let max = g
            .iter()
            .zip(c)
            .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
            .max()
            .unwrap();
        assert!(max <= 1, "plane {plane}: max |GPU-CPU| = {max} code values");
    }
}

#[test]
fn alpha_frames_stay_on_gpu_without_per_tick_allocation() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let pool = Arc::new(AlphaPlanePool::default());
    let opaque = gradient_tile(32, 32);
    let fill = pool
        .attach(gradient_tile(32, 32), &alpha_pattern(32, 32))
        .unwrap();
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.0, 0.0, 0.0);
    let alpha_tiles = [
        Tile::placed(&opaque, 0, 0, 1.0),
        Tile::placed(&fill, 16, 16, 1.0),
    ];
    let opaque_tiles = [Tile::placed(&opaque, 0, 0, 1.0)];
    // Warm with the alpha scene, then alternate alpha/opaque frames: both must
    // reuse the pooled surfaces (the alpha array is grow-only, so an opaque
    // frame between alpha frames neither frees nor reallocates it).
    gpu.composite(64, 64, canvas, bg, &alpha_tiles).unwrap();
    let warm = gpu.gpu_allocation_count();
    for i in 0..32 {
        let tiles: &[Tile<'_>] = if i % 2 == 0 {
            &alpha_tiles
        } else {
            &opaque_tiles
        };
        gpu.composite(64, 64, canvas, bg, tiles).unwrap();
    }
    assert_eq!(
        gpu.gpu_allocation_count(),
        warm,
        "steady-state alpha compositing must not allocate GPU surfaces per tick"
    );
    assert_eq!(gpu.cpu_fallback_frames(), 0);
}

#[test]
fn run_backend_composites_alpha_on_gpu_with_no_cpu_fallback() {
    let backend = RunBackend::select(Some(GpuTarget::none()));
    if !backend.is_gpu() {
        println!("SKIP: run backend resolved to CPU (no usable GPU adapter)");
        return;
    }
    let pool = Arc::new(AlphaPlanePool::default());
    let fill = pool
        .attach(gradient_tile(32, 32), &alpha_pattern(32, 32))
        .unwrap();
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.1, 0.1, 0.1);
    let tiles = [Tile::placed(&fill, 0, 0, 1.0)];
    let out = backend.composite(32, 32, canvas, bg, &tiles).unwrap();
    assert_eq!(
        backend.gpu_cpu_fallback_frames(),
        0,
        "an alpha tile must not move the frame to the CPU"
    );
    let cpu_out = cpu_composite(32, 32, canvas, bg, &tiles).unwrap();
    assert_matches_oracle(&out, &cpu_out);
}
