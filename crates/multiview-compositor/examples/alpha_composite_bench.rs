//! SW-8b (#109): composite time with 0, 1 and 6 NV12+A alpha tiles, CPU vs GPU.
//!
//! A 1920x1080 3x3 multiview (nine 640x360 sources scaled into 640x360 cells);
//! N of the nine tiles carry a straight alpha plane. Reports the median and p95
//! wall time per composite (including GPU upload + readback) for the persistent
//! CPU backend and, when an adapter is present, the GPU backend. Before SW-8b any
//! alpha tile moved the WHOLE frame to the uncached CPU path; now the GPU column
//! for 1 and 6 alpha tiles must stay close to its 0-alpha time.
//!
//! Run (release, on the target host):
//!   `cargo run --release -p multiview-compositor --features wgpu --example alpha_composite_bench`
//! Optional args: `<iterations>` (default 60).
#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stdout
)] // Local-only benchmark harness: bounded fixtures and intentional output.

use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{AlphaPlanePool, CanvasColor, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

const CANVAS_W: u32 = 1920;
const CANVAS_H: u32 = 1080;
const CELL_W: u32 = 640;
const CELL_H: u32 = 360;

fn color() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn source(seed: u32) -> Nv12Image {
    let (w, h) = (CELL_W as usize, CELL_H as usize);
    let y = (0..w * h)
        .map(|i| (16 + ((i % w) as u32 + (i / w) as u32 + seed * 17) % 219) as u8)
        .collect();
    let uv = (0..w * h / 2)
        .map(|i| (64 + (i as u32 + seed * 31) % 128) as u8)
        .collect();
    Nv12Image::new(CELL_W, CELL_H, y, uv, color()).unwrap()
}

fn alpha_plane() -> Vec<u8> {
    (0..(CELL_W * CELL_H) as usize)
        .map(|i| ((i % CELL_W as usize) * 255 / (CELL_W as usize - 1)) as u8)
        .collect()
}

fn time_backend(
    backend: &RunBackend,
    tiles: &[Tile<'_>],
    iterations: usize,
) -> (Duration, Duration) {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.02, 0.02);
    for _ in 0..5 {
        backend
            .composite(CANVAS_W, CANVAS_H, canvas, bg, tiles)
            .expect("warm-up composite");
    }
    let mut samples: Vec<Duration> = (0..iterations)
        .map(|_| {
            let t = Instant::now();
            let out = backend
                .composite(CANVAS_W, CANVAS_H, canvas, bg, tiles)
                .expect("composite");
            std::hint::black_box(out);
            t.elapsed()
        })
        .collect();
    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    (samples[samples.len() / 2], p95)
}

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(60_usize)
        .max(1);
    let pool = Arc::new(AlphaPlanePool::default());
    let alpha = alpha_plane();
    let opaque: Vec<Nv12Image> = (0..9).map(source).collect();
    let with_alpha: Vec<Nv12Image> = (0..9)
        .map(|i| pool.attach(source(i), &alpha).unwrap())
        .collect();

    let cpu = RunBackend::cpu();
    #[cfg(feature = "wgpu")]
    let gpu = {
        let b = RunBackend::select(Some(multiview_compositor::backend::GpuTarget::none()));
        b.is_gpu().then_some(b)
    };
    #[cfg(not(feature = "wgpu"))]
    let gpu: Option<RunBackend> = None;
    if gpu.is_none() {
        println!("NOTE: no GPU backend (build with --features wgpu on a GPU host); CPU only");
    }

    println!("1920x1080, 9 tiles (640x360), {iterations} iterations; median / p95 ms");
    println!("alpha_tiles | CPU median | CPU p95 | GPU median | GPU p95 | GPU->CPU fallbacks");
    for n_alpha in [0_usize, 1, 6] {
        let tiles: Vec<Tile<'_>> = (0..9)
            .map(|i| {
                let img = if i < n_alpha {
                    &with_alpha[i]
                } else {
                    &opaque[i]
                };
                let x = (i as u32 % 3) * CELL_W;
                let y = (i as u32 / 3) * CELL_H;
                Tile::scaled(img, x, y, CELL_W, CELL_H, 1.0)
            })
            .collect();
        let (cm, cp) = time_backend(&cpu, &tiles, iterations);
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        match &gpu {
            Some(g) => {
                let (gm, gp) = time_backend(g, &tiles, iterations);
                println!(
                    "{n_alpha:>11} | {:>10.2} | {:>7.2} | {:>10.2} | {:>7.2} | {}",
                    ms(cm),
                    ms(cp),
                    ms(gm),
                    ms(gp),
                    g.gpu_cpu_fallback_frames()
                );
            }
            None => println!(
                "{n_alpha:>11} | {:>10.2} | {:>7.2} | {:>10} | {:>7} | -",
                ms(cm),
                ms(cp),
                "n/a",
                "n/a"
            ),
        }
    }
}
