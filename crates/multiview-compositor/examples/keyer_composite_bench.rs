//! Keyers K1 (#386, ADR-0113 §9): composite time of a 1080p Program frame with
//! 0, 1 and 2 full-canvas keyers, 2 flying keyers, a large source in a tiny
//! fly, and a key-changing MIX (two scene accumulators). Reports median and
//! p95 per composite for the persistent CPU backend and, when an adapter is
//! present, the run backend (which composes keyed frames on the CPU until the
//! GPU key path lands after #395, so its fallback count is printed too).
//!
//! Run (release, on the target host):
//!   `cargo run --release -p multiview-compositor --features wgpu --example keyer_composite_bench`
//! Optional args: `<iterations>` (default 40).
#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::similar_names
)] // Local-only benchmark harness: bounded fixtures and intentional output.

use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{AlphaPlanePool, CanvasColor, Nv12Image, Tile};
use multiview_compositor::{SceneMember, SceneMerge, TileGroup, TileKey};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

const W: u32 = 1920;
const H: u32 = 1080;

fn color() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn source(w: u32, h: u32, seed: u32) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let y = (0..wu * hu)
        .map(|i| (16 + ((i % wu) as u32 + (i / wu) as u32 + seed * 17) % 219) as u8)
        .collect();
    let uv = (0..wu * hu / 2)
        .map(|i| (64 + (i as u32 + seed * 31) % 128) as u8)
        .collect();
    Nv12Image::new(w, h, y, uv, color()).unwrap()
}

fn time(backend: &RunBackend, tiles: &[Tile<'_>], iterations: usize) -> (Duration, Duration) {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.0, 0.0, 0.0);
    for _ in 0..3 {
        backend.composite(W, H, canvas, bg, tiles).expect("warm-up");
    }
    let mut samples: Vec<Duration> = (0..iterations)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(
                backend
                    .composite(W, H, canvas, bg, tiles)
                    .expect("composite"),
            );
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
        .unwrap_or(40_usize)
        .max(1);
    let pool = Arc::new(AlphaPlanePool::default());
    let a = source(W, H, 1);
    let b = source(W, H, 2);
    let alpha: Vec<u8> = (0..(W * H) as usize).map(|i| (i % 256) as u8).collect();
    let gfx = pool.attach(source(W, H, 3), &alpha).unwrap();
    let fill = source(W, H, 4);
    let key = source(W, H, 5);

    let mix = SceneMerge::Mix { p: 0.5 };
    let scene = |m| TileGroup::Scene {
        member: m,
        merge: mix,
    };
    let full = |img| Tile::placed(img, 0, 0, 1.0);
    let dsk = Tile::placed(&gfx, 0, 0, 1.0).with_key(TileKey::linear());
    let luma = Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::luma(0.1, 4.0).with_source(&key));
    let fly = |x| {
        Tile::scaled(&fill, x, 60, 576, 324, 1.0)
            .with_key(TileKey::luma(0.1, 4.0).with_source(&key))
    };
    let tiny = Tile::scaled(&fill, 1760, 40, 128, 72, 1.0).with_key(TileKey::linear());
    let cases: Vec<(&str, Vec<Tile<'_>>)> = vec![
        ("0 keyers", vec![full(&a)]),
        ("1 DSK full (alpha)", vec![full(&a), dsk.clone()]),
        (
            "2 full (DSK + luma fill/key)",
            vec![full(&a), luma.clone(), dsk.clone()],
        ),
        ("2 flying 576x324", vec![full(&a), fly(64), fly(1280)]),
        ("1080p source, 128x72 fly", vec![full(&a), tiny]),
        (
            "key-changing MIX + DSK",
            vec![
                full(&a).in_group(scene(SceneMember::Out)),
                full(&b).in_group(scene(SceneMember::In)),
                luma.in_group(scene(SceneMember::In)),
                dsk,
            ],
        ),
    ];

    let cpu = RunBackend::cpu();
    #[cfg(feature = "wgpu")]
    let run = {
        let r = RunBackend::select(Some(multiview_compositor::backend::GpuTarget::none()));
        r.is_gpu().then_some(r)
    };
    #[cfg(not(feature = "wgpu"))]
    let run: Option<RunBackend> = None;

    println!("{W}x{H}, {iterations} iterations; median / p95 ms");
    println!("case | CPU median | CPU p95 | run-backend median | p95 | GPU->CPU fallbacks");
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    for (name, tiles) in &cases {
        let (cm, cp) = time(&cpu, tiles, iterations);
        match &run {
            Some(r) => {
                let (gm, gp) = time(r, tiles, iterations);
                println!(
                    "{name} | {:.2} | {:.2} | {:.2} | {:.2} | {}",
                    ms(cm),
                    ms(cp),
                    ms(gm),
                    ms(gp),
                    r.gpu_cpu_fallback_frames()
                );
            }
            None => println!("{name} | {:.2} | {:.2} | n/a | n/a | -", ms(cm), ms(cp)),
        }
    }
}
