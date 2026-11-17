//! Keyers K7 (#392, ADR-C007 §3) GPU parity: the WGSL composite computes the
//! chroma matte in-shader from the fill's tile-gamut linear RGB and the
//! CPU-converted reference, matched against the CPU oracle by SSIM >= 0.98 and
//! PSNR >= 40 dB on Y and UV (GPU is never bit-exact).
//!
//! These tests REQUIRE a GPU adapter (a software one such as lavapipe is
//! enough). Without one each test SKIPS (prints and returns), the pattern of
//! `keyers_gpu.rs`, so CI without a GPU stays green.
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
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::many_single_char_names
)] // Test fixtures: one table of cases, short pixel-math names.

use multiview_compositor::backend::{GpuTarget, RunBackend, RunBackendKind};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::error::Error;
use multiview_compositor::gpu::{GpuCompositor, GpuContext};
use multiview_compositor::pipeline::{composite as cpu_composite, CanvasColor, Nv12Image, Tile};
use multiview_compositor::{KeyMask, SceneMember, SceneMerge, TileGroup, TileKey};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

const W: u32 = 64;
const H: u32 = 48;

fn try_gpu() -> Option<GpuCompositor> {
    match GpuContext::new(None) {
        Ok(ctx) => {
            let info = ctx.adapter().get_info();
            println!("GPU adapter: {} ({:?})", info.name, info.backend);
            Some(GpuCompositor::with_context(ctx).expect("pipelines"))
        }
        Err(Error::NoAdapter(reason) | Error::DeviceRequest(reason)) => {
            println!("SKIP: no usable GPU adapter ({reason})");
            None
        }
        Err(other) => panic!("unexpected GPU init error (not a no-adapter skip): {other}"),
    }
}

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

/// A green screen: chroma sweeps around the studio green (Cb/Cr near
/// 96/55) across the frame, with luma bands, so the soft edge covers many
/// pixels and some blocks sit far away (subject colours).
fn screen(w: u32, h: u32, color: ColorInfo, phase: u32) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let mut y = vec![0_u8; wu * hu];
    for (i, px) in y.iter_mut().enumerate() {
        let (c, r) = (i % wu, i / wu);
        *px = (100 + ((c * 3 + r + phase as usize) % 60)) as u8;
    }
    let mut uv = vec![0_u8; wu * hu / 2];
    for (j, pair) in uv.chunks_exact_mut(2).enumerate() {
        let cx = j % (wu / 2);
        let cy = j / (wu / 2);
        if (cx + cy + phase as usize) % 7 == 0 {
            // A subject block: skin / magenta.
            pair[0] = 110;
            pair[1] = 170;
        } else {
            pair[0] = (80 + (cx * 40) / (wu / 2)) as u8;
            pair[1] = (40 + (cy * 40) / (hu / 2)) as u8;
        }
    }
    Nv12Image::new(w, h, y, uv, color).expect("screen geometry")
}

/// A plain ramp background.
fn ramp(w: u32, h: u32) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let y = (0..wu * hu)
        .map(|i| (16 + ((i % wu) * 219) / wu) as u8)
        .collect();
    let uv = (0..wu * hu / 2).map(|i| (100 + i % 60) as u8).collect();
    Nv12Image::new(w, h, y, uv, bt709(ColorRange::Limited)).unwrap()
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
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

fn bg() -> LinearRgba {
    LinearRgba::opaque(0.02, 0.05, 0.1)
}

/// GPU vs CPU oracle on the §11 floors; returns `(min SSIM, min PSNR, max |dY|)`.
fn gate(gpu: &GpuCompositor, name: &str, tiles: &[Tile<'_>]) -> (f64, f64, u8) {
    let canvas = CanvasColor::default();
    let cpu_out = cpu_composite(W, H, canvas, bg(), tiles).expect("CPU oracle");
    let gpu_out = gpu
        .composite(W, H, canvas, bg(), tiles)
        .expect("GPU composite");
    let ys = ssim(gpu_out.y_plane(), cpu_out.y_plane());
    let yp = psnr(gpu_out.y_plane(), cpu_out.y_plane());
    let us = ssim(gpu_out.uv_plane(), cpu_out.uv_plane());
    let up = psnr(gpu_out.uv_plane(), cpu_out.uv_plane());
    let max_dy = gpu_out
        .y_plane()
        .iter()
        .zip(cpu_out.y_plane())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    println!(
        "{name}: Y SSIM={ys:.5} PSNR={yp:.2}dB | UV SSIM={us:.5} PSNR={up:.2}dB | max|dY|={max_dy}"
    );
    assert!(ys >= 0.98, "{name}: Y SSIM {ys:.5} below 0.98");
    assert!(yp >= 40.0, "{name}: Y PSNR {yp:.2}dB below 40 dB");
    assert!(us >= 0.98, "{name}: UV SSIM {us:.5} below 0.98");
    assert!(up >= 40.0, "{name}: UV PSNR {up:.2}dB below 40 dB");
    (ys.min(us), yp.min(up), max_dy)
}

/// The key must visibly change the picture, so a shader that ignored it
/// could not pass.
fn assert_key_visible(name: &str, keyed: &[Tile<'_>]) {
    let canvas = CanvasColor::default();
    let unkeyed: Vec<Tile<'_>> = keyed
        .iter()
        .map(|t| {
            let mut t = t.clone();
            t.key = None;
            t.group = TileGroup::Top;
            t
        })
        .collect();
    let a = cpu_composite(W, H, canvas, bg(), keyed).unwrap();
    let b = cpu_composite(W, H, canvas, bg(), &unkeyed).unwrap();
    let changed = a
        .y_plane()
        .iter()
        .zip(b.y_plane())
        .filter(|(x, y)| x.abs_diff(**y) > 8)
        .count();
    assert!(
        changed >= 32,
        "fixture {name}: the key must change the picture ({changed} px)"
    );
}

#[test]
fn gpu_chroma_key_matches_the_cpu_oracle() {
    let Some(gpu) = try_gpu() else { return };
    let back = ramp(W, H);
    let fill = screen(W, H, bt709(ColorRange::Limited), 0);
    let fill_small = screen(32, 24, bt709(ColorRange::Full), 3);
    let fill_hdr = screen(40, 30, pq2020(), 5);
    let green = [0x00, 0xb1, 0x40];
    let m = SceneMerge::Mix { p: 0.4 };
    let nan = f32::NAN;
    let inf = f32::INFINITY;
    let cases: Vec<(&str, Vec<Tile<'_>>, bool)> = vec![
        (
            "chroma soft edge",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma(green, 0.02, 6.0)),
            ],
            true,
        ),
        (
            "chroma invert",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::placed(&fill, 0, 0, 1.0)
                    .with_key(TileKey::chroma(green, 0.02, 6.0).inverted(true)),
            ],
            true,
        ),
        (
            "chroma clip 0, gain 1 (pure distance)",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma(green, 0.0, 1.0)),
            ],
            true,
        ),
        (
            "chroma upscaled full-range fill, masked, weight 0.7",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::scaled(&fill_small, 4, 2, 56, 42, 0.7).with_key(
                    TileKey::chroma([0x20, 0xa0, 0x30], 0.05, 4.0).masked(KeyMask {
                        x: 2,
                        y: 2,
                        w: 26,
                        h: 18,
                    }),
                ),
            ],
            true,
        ),
        (
            "chroma PQ/BT.2020 fill (tile-gamut distance), downscaled",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::scaled(&fill_hdr, 8, 6, 32, 24, 1.0).with_key(TileKey::chroma(
                    [0x50, 0x88, 0x60],
                    0.01,
                    40.0,
                )),
            ],
            true,
        ),
        (
            "chroma in a MIX scene group",
            vec![
                Tile::placed(&back, 0, 0, 1.0).in_group(TileGroup::Scene {
                    member: SceneMember::Both,
                    merge: m,
                }),
                Tile::placed(&fill, 0, 0, 1.0)
                    .with_key(TileKey::chroma(green, 0.02, 6.0))
                    .in_group(TileGroup::Scene {
                        member: SceneMember::In,
                        merge: m,
                    }),
            ],
            false,
        ),
        (
            "chroma degenerate: NaN clip, NaN gain, +inf clip, -inf clip",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::scaled(&fill, 0, 0, 16, 48, 1.0).with_key(TileKey::chroma(green, nan, 1.0)),
                Tile::scaled(&fill, 16, 0, 16, 48, 1.0).with_key(TileKey::chroma(green, 0.1, nan)),
                Tile::scaled(&fill, 32, 0, 16, 48, 1.0).with_key(TileKey::chroma(green, inf, 1.0)),
                Tile::scaled(&fill, 48, 0, 16, 48, 1.0)
                    .with_key(TileKey::chroma(green, -inf, 1.0).inverted(true)),
            ],
            true,
        ),
        (
            "chroma step: infinite gain, subnormal clip, f32::MAX gain",
            vec![
                Tile::placed(&back, 0, 0, 1.0),
                Tile::scaled(&fill, 0, 0, 32, 48, 1.0).with_key(TileKey::chroma(green, 0.03, inf)),
                Tile::scaled(&fill, 32, 0, 32, 48, 1.0)
                    .with_key(TileKey::chroma(green, -1e-40, f32::MAX).inverted(true)),
            ],
            false,
        ),
    ];
    let (mut min_s, mut min_p, mut max_dy) = (f64::INFINITY, f64::INFINITY, 0_u8);
    for (name, tiles, visible) in &cases {
        if *visible {
            assert_key_visible(name, tiles);
        }
        let (s, p, d) = gate(&gpu, name, tiles);
        min_s = min_s.min(s);
        min_p = min_p.min(p);
        max_dy = max_dy.max(d);
    }
    println!(
        "chroma minima over {} frames: SSIM={min_s:.5} PSNR={min_p:.2}dB max|dY|={max_dy}",
        cases.len()
    );
}

#[test]
fn the_exact_reference_colour_is_keyed_out_on_the_gpu() {
    let Some(gpu) = try_gpu() else { return };
    // Full-range BT.709 black (Y 0, neutral chroma) decodes to R'G'B'
    // (0, 0, 0) and white (Y 255) to (255, 255, 255), so the distance is 0
    // (to LUT precision) and a soft edge from clip 0 keys the fill out.
    let canvas = CanvasColor::default();
    for (y8, reference) in [(0_u8, [0_u8, 0, 0]), (255, [255, 255, 255])] {
        let fill = Nv12Image::solid(W, H, y8, 128, 128, bt709(ColorRange::Full)).unwrap();
        let tiles = [Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma(reference, 0.0, 8.0))];
        let cpu_out = cpu_composite(W, H, canvas, bg(), &tiles).unwrap();
        let gpu_out = gpu.composite(W, H, canvas, bg(), &tiles).unwrap();
        let empty = cpu_composite(W, H, canvas, bg(), &[]).unwrap();
        assert!(
            cpu_out.y_plane() == empty.y_plane() && cpu_out.uv_plane() == empty.uv_plane(),
            "the oracle keys the reference out"
        );
        let dy = gpu_out
            .y_plane()
            .iter()
            .zip(empty.y_plane())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(dy <= 1, "GPU keyed the reference out (max |dY| {dy})");
    }
}

#[test]
fn a_chroma_frame_on_the_gpu_backend_does_not_fall_back_and_allocates_nothing_new() {
    if try_gpu().is_none() {
        return;
    }
    let backend = RunBackend::select(Some(GpuTarget::none()));
    if backend.kind() != RunBackendKind::Gpu {
        println!("SKIP: RunBackend did not select the GPU");
        return;
    }
    let back = ramp(W, H);
    let fill = screen(W, H, bt709(ColorRange::Limited), 1);
    let canvas = CanvasColor::default();
    let tiles = [
        Tile::placed(&back, 0, 0, 1.0),
        Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma([0, 0xb1, 0x40], 0.02, 6.0)),
    ];
    let out = backend.composite(W, H, canvas, bg(), &tiles).unwrap();
    let oracle = cpu_composite(W, H, canvas, bg(), &tiles).unwrap();
    assert!(ssim(out.y_plane(), oracle.y_plane()) >= 0.98);
    assert!(psnr(out.y_plane(), oracle.y_plane()) >= 40.0);
    assert_eq!(backend.gpu_cpu_fallback_frames(), 0, "drawn on the GPU");

    let gpu = try_gpu().unwrap();
    drop(gpu.composite(W, H, canvas, bg(), &tiles).unwrap());
    let warm = gpu.gpu_allocation_count();
    for _ in 0..30 {
        drop(gpu.composite(W, H, canvas, bg(), &tiles).unwrap());
    }
    assert_eq!(gpu.gpu_allocation_count(), warm, "no GPU pool growth");
}

/// Review K7 MEDIA-1 on the GPU: pixels whose codes equal the reference's
/// are keyed out at gain 1e9, `f32::MAX` and +inf, exactly like the CPU with
/// and without LUTs; neighbouring codes are kept (the step stays sharp).
#[test]
fn an_exact_reference_is_keyed_out_on_the_gpu_at_a_huge_gain() {
    let Some(gpu) = try_gpu() else { return };
    let canvas = CanvasColor::default();
    let black = LinearRgba::opaque(0.0, 0.0, 0.0);
    // Columns of 2x2 blocks: grey 128 (the reference) next to 127 and 129.
    let full_range = bt709(ColorRange::Full);
    let (wu, hu) = (W as usize, H as usize);
    let levels = [128_u8, 127, 128, 129];
    let y: Vec<u8> = (0..wu * hu)
        .map(|i| levels[(i % wu) / 2 % levels.len()])
        .collect();
    let uv = vec![128_u8; wu * hu / 2];
    let fill = Nv12Image::new(W, H, y, uv, full_range).unwrap();
    for gain in [1e9_f32, f32::MAX, f32::INFINITY] {
        let tiles =
            [
                Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::chroma(
                    [128, 128, 128],
                    0.0,
                    gain,
                )),
            ];
        let gpu_out = gpu.composite(W, H, canvas, black, &tiles).unwrap();
        for lut in [false, true] {
            let cpu_out =
                multiview_compositor::pipeline::composite_with(W, H, canvas, black, &tiles, lut)
                    .unwrap();
            let dy = gpu_out
                .y_plane()
                .iter()
                .zip(cpu_out.y_plane())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap();
            assert!(dy <= 2, "gain {gain} lut {lut}: GPU vs CPU max |dY| {dy}");
        }
        // Reference columns are background (black, Y 16); 127/129 are kept.
        for (col, level) in levels.iter().enumerate() {
            let got = gpu_out
                .sample(u32::try_from(col * 2).unwrap(), 0)
                .unwrap()
                .0;
            if *level == 128 {
                assert_eq!(got, 16, "gain {gain}: reference column {col} keyed out");
            } else {
                assert!(
                    got > 100,
                    "gain {gain}: column {col} ({level}) kept, got {got}"
                );
            }
        }
    }
}
