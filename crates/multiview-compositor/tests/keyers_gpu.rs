//! Keyers K1 GPU key path (#386, ADR-0113 §3.3): the WGSL composite draws
//! keyed tiles (luma / linear, separate key source of its own size and range,
//! invert, source-space mask) and scene groups (two-accumulator MIX / DIP
//! merge) itself, matched against the CPU oracle by SSIM >= 0.98 and
//! PSNR >= 40 dB on Y and UV (design note §11; GPU is never bit-exact).
//!
//! These tests REQUIRE a GPU adapter (a software one such as lavapipe is
//! enough). Without one `GpuContext::new` returns `NoAdapter`/`DeviceRequest`
//! and each test SKIPS (prints and returns), the pattern of
//! `nv12_alpha_gpu.rs`, so CI without a GPU stays green.
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
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)] // Test fixtures: one table of cases, short pixel-math names.

use std::sync::Arc;

use multiview_compositor::backend::{GpuTarget, RunBackend, RunBackendKind};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::error::Error;
use multiview_compositor::gpu::{GpuCompositor, GpuContext};
use multiview_compositor::pipeline::{
    composite as cpu_composite, AlphaPlanePool, CanvasColor, Nv12Image, Tile,
};
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

fn color(range: ColorRange) -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range,
    }
}

/// A diagonal luma ramp over the full code range with a chroma gradient: a
/// luma key on it has a long soft edge plus hard clipped regions.
fn gradient(w: u32, h: u32, range: ColorRange, phase: u32) -> Nv12Image {
    let (wu, hu) = (w as usize, h as usize);
    let mut y = vec![0u8; wu * hu];
    for (i, px) in y.iter_mut().enumerate() {
        let (c, r) = (i % wu, i / wu);
        let t = ((c + r + phase as usize) % (wu + hu)) as f32 / (wu + hu) as f32;
        *px = (t * 255.0).round() as u8;
    }
    let mut uv = vec![0u8; wu * hu / 2];
    for (j, pair) in uv.chunks_exact_mut(2).enumerate() {
        let cx = (j % (wu / 2)) as f32 / (wu as f32 / 2.0);
        let cy = (j / (wu / 2)) as f32 / (hu as f32 / 2.0);
        pair[0] = (64.0 + cx * 128.0).round() as u8;
        pair[1] = (64.0 + cy * 128.0).round() as u8;
    }
    Nv12Image::new(w, h, y, uv, color(range)).expect("gradient geometry")
}

/// A key signal with hard vertical bars (white / black / grey) so a key
/// sampled with the fill's rounding instead of its own differs visibly.
fn bars(w: u32, h: u32, range: ColorRange) -> Nv12Image {
    let wu = w as usize;
    let levels = [235u8, 16, 128, 255, 0, 180];
    let mut y = vec![0u8; wu * h as usize];
    for (i, px) in y.iter_mut().enumerate() {
        *px = levels[(i % wu) % levels.len()];
    }
    let uv = vec![128u8; wu * h as usize / 2];
    Nv12Image::new(w, h, y, uv, color(range)).expect("bars geometry")
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

/// Composite `tiles` on the GPU and on the CPU oracle and hold them to the
/// §11 floors. Returns `(min SSIM, min PSNR)` over Y and UV.
fn gate(gpu: &GpuCompositor, name: &str, tiles: &[Tile<'_>]) -> (f64, f64) {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.05, 0.1);
    let cpu_out = cpu_composite(W, H, canvas, bg, tiles).expect("CPU oracle");
    let gpu_out = gpu
        .composite(W, H, canvas, bg, tiles)
        .expect("GPU composite");
    assert_eq!(gpu_out.color(), cpu_out.color());
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
    (ys.min(us), yp.min(up))
}

/// The unkeyed composite of the same tiles must differ from the keyed one,
/// so a shader that silently ignored the key could not pass the gate.
fn assert_key_visible(name: &str, keyed: &[Tile<'_>]) {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.05, 0.1);
    let unkeyed: Vec<Tile<'_>> = keyed
        .iter()
        .map(|t| {
            let mut t = t.clone();
            t.key = None;
            t.group = TileGroup::Top;
            t
        })
        .collect();
    let a = cpu_composite(W, H, canvas, bg, keyed).unwrap();
    let b = cpu_composite(W, H, canvas, bg, &unkeyed).unwrap();
    let changed = a
        .y_plane()
        .iter()
        .zip(b.y_plane())
        .filter(|(x, y)| x.abs_diff(**y) > 8)
        .count();
    assert!(
        changed >= 32,
        "fixture {name}: the key must change the picture ({changed} px changed)"
    );
}

#[test]
fn gpu_key_path_matches_the_cpu_oracle() {
    let Some(gpu) = try_gpu() else { return };
    let limited = ColorRange::Limited;
    let full = ColorRange::Full;
    let bg = gradient(W, H, limited, 7);
    let fill = gradient(32, 24, limited, 0);
    // A key source of a different size AND range than the fill (§3.1).
    let key_big = bars(48, 36, full);
    let key_small = bars(12, 10, limited);
    let pool = Arc::new(AlphaPlanePool::default());
    let alpha: Vec<u8> = (0..32 * 24).map(|i| ((i % 32) * 8) as u8).collect();
    let fill_alpha = pool.attach(gradient(32, 24, limited, 3), &alpha).unwrap();

    let mut floor = (f64::INFINITY, f64::INFINITY);
    let mut check = |name: &str, tiles: &[Tile<'_>]| {
        let (s, p) = gate(&gpu, name, tiles);
        floor = (floor.0.min(s), floor.1.min(p));
    };

    let base = Tile::placed(&bg, 0, 0, 1.0);
    let cases: Vec<(&str, Vec<Tile<'_>>)> = vec![
        (
            "luma self key",
            vec![
                base.clone(),
                Tile::placed(&fill, 8, 6, 1.0).with_key(TileKey::luma(0.3, 3.0)),
            ],
        ),
        (
            "luma, key source of another size (upscaled tile)",
            vec![
                base.clone(),
                Tile::scaled(&fill, 4, 4, 48, 36, 0.9)
                    .with_key(TileKey::luma(0.2, 2.5).with_source(&key_big)),
            ],
        ),
        (
            "luma, small key source, downscaled tile",
            vec![
                base.clone(),
                Tile::scaled(&fill, 10, 8, 22, 18, 1.0)
                    .with_key(TileKey::luma(0.4, 4.0).with_source(&key_small)),
            ],
        ),
        (
            "linear with key source",
            vec![
                base.clone(),
                Tile::scaled(&fill, 0, 0, W, H, 1.0)
                    .with_key(TileKey::linear().with_source(&key_big)),
            ],
        ),
        (
            "linear self key with fill alpha (invert ignored)",
            vec![
                base.clone(),
                Tile::placed(&fill_alpha, 16, 12, 0.8).with_key(TileKey::linear().inverted(true)),
            ],
        ),
        (
            "luma invert",
            vec![
                base.clone(),
                Tile::placed(&fill, 8, 6, 1.0).with_key(TileKey::luma(0.25, 2.0).inverted(true)),
            ],
        ),
        (
            "linear invert with key source",
            vec![
                base.clone(),
                Tile::scaled(&fill, 0, 0, W, H, 1.0)
                    .with_key(TileKey::linear().with_source(&key_small).inverted(true)),
            ],
        ),
        (
            "mask on a scaled (flying) luma key",
            vec![
                base.clone(),
                Tile::scaled(&fill, 6, 2, 48, 40, 1.0).with_key(
                    TileKey::luma(0.0, 1.0).inverted(true).masked(KeyMask {
                        x: 4,
                        y: 3,
                        w: 20,
                        h: u32::MAX,
                    }),
                ),
            ],
        ),
        (
            "keyed tile at non-unit scale, two keyers",
            vec![
                base.clone(),
                Tile::scaled(&fill, 2, 2, 40, 30, 1.0)
                    .with_key(TileKey::luma(0.35, 5.0).with_source(&key_big)),
                Tile::scaled(&fill_alpha, 30, 20, 30, 26, 0.7).with_key(TileKey::luma(0.1, 1.5)),
            ],
        ),
        (
            "degenerate params: NaN / negative gain, NaN clip, inf clip, NaN weight",
            vec![
                base.clone(),
                Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::luma(0.2, f32::NAN)),
                Tile::placed(&fill, 32, 0, 1.0).with_key(TileKey::luma(0.2, -3.0).inverted(true)),
                Tile::placed(&fill, 0, 24, 1.0).with_key(TileKey::luma(f32::NAN, 2.0)),
                Tile::placed(&fill, 32, 24, 1.0)
                    .with_key(TileKey::luma(f32::NEG_INFINITY, f32::INFINITY)),
                Tile::placed(&fill, 16, 12, f32::NAN).with_key(TileKey::luma(0.1, 1.0)),
            ],
        ),
    ];
    for (name, tiles) in &cases {
        // A linear self key's matte is 1 by definition (its fill alpha
        // shapes it), and the degenerate case only proves totality.
        if !name.starts_with("degenerate") && !name.contains("invert ignored") {
            assert_key_visible(name, tiles);
        }
        check(name, tiles);
    }

    // Scene groups: a key-changing transition (an opaque incoming key going
    // on over a changing background, an outgoing key going off) under MIX
    // and DIP at several p, plus a DSK on top of the merge.
    let incoming_bg = gradient(W, H, full, 31);
    let dsk = gradient(16, 12, limited, 5);
    let group = |member, merge| TileGroup::Scene { member, merge };
    let merges = [
        SceneMerge::Mix { p: 0.0 },
        SceneMerge::Mix { p: 0.3 },
        SceneMerge::Mix { p: 0.5 },
        SceneMerge::Mix { p: 1.0 },
        SceneMerge::Dip {
            colour: LinearRgba::opaque(0.8, 0.1, 0.05),
            d: 0.6,
            i: 0.0,
        },
        SceneMerge::Dip {
            colour: LinearRgba::opaque(0.8, 0.1, 0.05),
            d: 1.0,
            i: 0.4,
        },
        SceneMerge::Dip {
            colour: LinearRgba::opaque(0.0, 0.0, 0.0),
            d: 1.0,
            i: 1.0,
        },
    ];
    for m in merges {
        let tiles = vec![
            Tile::placed(&bg, 0, 0, 1.0).in_group(group(SceneMember::Out, m)),
            Tile::placed(&incoming_bg, 0, 0, 1.0).in_group(group(SceneMember::In, m)),
            Tile::scaled(&fill, 4, 4, 30, 24, 1.0)
                .with_key(TileKey::luma(0.3, 3.0))
                .in_group(group(SceneMember::Both, m)),
            Tile::scaled(&fill, 30, 10, 30, 30, 1.0)
                .with_key(TileKey::linear().with_source(&key_small))
                .in_group(group(SceneMember::In, m)),
            Tile::placed(&fill_alpha, 20, 20, 1.0)
                .with_key(TileKey::linear())
                .in_group(group(SceneMember::Out, m)),
            // A DSK above the merge (top), keyed.
            Tile::placed(&dsk, 44, 32, 1.0).with_key(TileKey::luma(0.2, 2.0)),
        ];
        check(&format!("scene {m:?}"), &tiles);
    }

    // Two segments with a top tile between; a grouped tile covering nothing
    // still carries the segment's merge; the last grouped merge wins.
    let mix = SceneMerge::Mix { p: 0.25 };
    let dip = SceneMerge::Dip {
        colour: LinearRgba::opaque(0.2, 0.6, 0.9),
        d: 0.5,
        i: 0.0,
    };
    let tiles = vec![
        Tile::placed(&fill, 0, 0, 1.0).in_group(group(SceneMember::In, mix)),
        Tile::placed(&dsk, 40, 30, 1.0),
        Tile::placed(&fill, 20, 10, 1.0).in_group(group(SceneMember::Out, mix)),
        // Off canvas: covers nothing, but its DIP merge wins the segment.
        Tile::placed(&dsk, W + 100, 0, 1.0).in_group(group(SceneMember::Both, dip)),
    ];
    check("two segments, off-canvas grouped tile's merge wins", &tiles);

    println!(
        "minimum over all cases: SSIM={:.5} PSNR={:.2}dB",
        floor.0, floor.1
    );
}

#[test]
fn a_keyed_frame_on_the_gpu_backend_does_not_fall_back_and_allocates_nothing_new() {
    if try_gpu().is_none() {
        return;
    }
    let backend = RunBackend::select(Some(GpuTarget::none()));
    if backend.kind() != RunBackendKind::Gpu {
        println!("SKIP: RunBackend did not select the GPU");
        return;
    }
    let limited = ColorRange::Limited;
    let bg = gradient(W, H, limited, 7);
    let fill = gradient(32, 24, limited, 0);
    let key = bars(48, 36, ColorRange::Full);
    let canvas = CanvasColor::default();
    let black = LinearRgba::opaque(0.0, 0.0, 0.0);
    let m = SceneMerge::Mix { p: 0.5 };
    let keyed = [
        Tile::placed(&bg, 0, 0, 1.0),
        Tile::scaled(&fill, 4, 4, 48, 36, 1.0).with_key(TileKey::luma(0.2, 2.0).with_source(&key)),
    ];
    let grouped = [
        Tile::placed(&bg, 0, 0, 1.0).in_group(TileGroup::Scene {
            member: SceneMember::Both,
            merge: m,
        }),
        Tile::placed(&fill, 4, 0, 1.0)
            .with_key(TileKey::luma(0.1, 2.0))
            .in_group(TileGroup::Scene {
                member: SceneMember::In,
                merge: m,
            }),
    ];
    let first = backend.composite(W, H, canvas, black, &keyed).unwrap();
    let oracle = cpu_composite(W, H, canvas, black, &keyed).unwrap();
    assert!(ssim(first.y_plane(), oracle.y_plane()) >= 0.98);
    assert!(psnr(first.y_plane(), oracle.y_plane()) >= 40.0);
    drop(backend.composite(W, H, canvas, black, &grouped).unwrap());
    assert_eq!(
        backend.gpu_cpu_fallback_frames(),
        0,
        "keyed and grouped frames are drawn on the GPU, not the CPU fallback"
    );
    assert_eq!(
        backend.scratch_alloc_count(),
        0,
        "the CPU fallback never ran"
    );

    // No GPU buffer growth over N keyed composites once warm: the key params
    // buffer is allocated once and refilled, key sources reuse the pooled Y
    // array.
    let gpu = try_gpu().unwrap();
    drop(gpu.composite(W, H, canvas, black, &keyed).unwrap());
    drop(gpu.composite(W, H, canvas, black, &grouped).unwrap());
    let warm = gpu.gpu_allocation_count();
    assert!(warm > 0);
    for i in 0..30 {
        let frame = if i % 2 == 0 { &keyed[..] } else { &grouped[..] };
        drop(gpu.composite(W, H, canvas, black, frame).unwrap());
    }
    assert_eq!(gpu.gpu_allocation_count(), warm, "no GPU pool growth");
}

#[test]
fn a_frame_past_the_layer_limit_is_refused_and_falls_back_to_the_oracle() {
    let Some(gpu) = try_gpu() else { return };
    let fill = gradient(8, 8, ColorRange::Limited, 0);
    let key = bars(8, 8, ColorRange::Limited);
    // 40 tiles with a key source each need 80 Y layers > MAX_TILES (64).
    let tiles: Vec<Tile<'_>> = (0..40)
        .map(|i| {
            Tile::placed(&fill, (i % 8) * 8, (i / 8) * 8, 1.0)
                .with_key(TileKey::luma(0.2, 2.0).with_source(&key))
        })
        .collect();
    let canvas = CanvasColor::default();
    let black = LinearRgba::opaque(0.0, 0.0, 0.0);
    let refused = gpu.composite(W, H, canvas, black, &tiles);
    assert!(matches!(refused, Err(Error::GpuLimit(_))), "{refused:?}");

    let backend = RunBackend::select(Some(GpuTarget::none()));
    if backend.kind() != RunBackendKind::Gpu {
        println!("SKIP: RunBackend did not select the GPU");
        return;
    }
    let out = backend.composite(W, H, canvas, black, &tiles).unwrap();
    let oracle = cpu_composite(W, H, canvas, black, &tiles).unwrap();
    assert_eq!(out, oracle, "the fallback is the CPU oracle, keyed");
    assert_eq!(backend.gpu_cpu_fallback_frames(), 1);
}

/// Composite on both paths, hold the §11 floors AND require no pixel to
/// differ by more than `max_dy` on Y: a matte flipped at the step of a huge or
/// infinite gain moves a whole pixel by far more than float fuzz does.
fn gate_exact(gpu: &GpuCompositor, name: &str, tiles: &[Tile<'_>], max_dy: u8) -> (f64, f64) {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.05, 0.1);
    let cpu_out = cpu_composite(W, H, canvas, bg, tiles).expect("CPU oracle");
    let gpu_out = gpu
        .composite(W, H, canvas, bg, tiles)
        .expect("GPU composite");
    let worst = gpu_out
        .y_plane()
        .iter()
        .zip(cpu_out.y_plane())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        worst <= max_dy,
        "{name}: a GPU pixel differs from the oracle by {worst} > {max_dy} on Y"
    );
    gate(gpu, name, tiles)
}

/// Y pixels (beyond 8 codes) that differ between two CPU composites.
fn changed_px(a: &[Tile<'_>], b: &[Tile<'_>]) -> usize {
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.05, 0.1);
    let a = cpu_composite(W, H, canvas, bg, a).unwrap();
    let b = cpu_composite(W, H, canvas, bg, b).unwrap();
    a.y_plane()
        .iter()
        .zip(b.y_plane())
        .filter(|(x, y)| x.abs_diff(**y) > 8)
        .count()
}

/// Review MEDIA-1: a positive excess times an infinite gain is a matte of 1
/// on the CPU oracle; the GPU must not approximate `+inf` (with `f32::MAX`,
/// `1e-40 × f32::MAX ≈ 0.034`, or 0 when subnormals flush).
#[test]
fn an_infinite_gain_on_a_subnormal_excess_keys_exactly_like_the_oracle() {
    let Some(gpu) = try_gpu() else { return };
    let bg = gradient(W, H, ColorRange::Limited, 7);
    let white = Nv12Image::solid(W, H, 235, 128, 128, color(ColorRange::Limited)).unwrap();
    // Full-range black key source: y = 0 exactly, so y − clip = +1e-40.
    let black_key = Nv12Image::solid(W, H, 0, 128, 128, color(ColorRange::Full)).unwrap();
    let clip = -1e-40_f32;
    assert!(clip.is_subnormal());
    let mut floor = (f64::INFINITY, f64::INFINITY);
    for invert in [false, true] {
        let tiles = [
            Tile::placed(&bg, 0, 0, 1.0),
            Tile::placed(&white, 0, 0, 1.0).with_key(
                TileKey::luma(clip, f32::INFINITY)
                    .with_source(&black_key)
                    .inverted(invert),
            ),
        ];
        // The key is visible: matte 1 shows the white fill over the gradient
        // (the picture differs from the background alone); inverted, matte 0
        // hides it (the picture differs from the un-keyed fill).
        let reference: Vec<Tile<'_>> = if invert {
            vec![tiles[0].clone(), Tile::placed(&white, 0, 0, 1.0)]
        } else {
            vec![tiles[0].clone()]
        };
        let changed = changed_px(&tiles, &reference);
        assert!(
            changed >= (W * H / 2) as usize,
            "invert={invert}: the key must decide the picture ({changed} px)"
        );
        let (s, p) = gate_exact(&gpu, &format!("MEDIA-1 invert={invert}"), &tiles, 2);
        floor = (floor.0.min(s), floor.1.min(p));
    }
    println!("MEDIA-1 minimum: SSIM={:.5} PSNR={:.2}dB", floor.0, floor.1);
}

/// The luma matte of a huge or infinite gain is a step at the clip: a
/// limited-range key code that lands exactly on the clip (matte 0: zero
/// excess, and `0 × inf` is NaN read as 0), one code above (1) and one below
/// (0) must key exactly as the oracle does, inverted or not.
#[test]
fn a_step_matte_at_the_clip_keys_exactly_like_the_oracle() {
    let Some(gpu) = try_gpu() else { return };
    let limited = ColorRange::Limited;
    let bg = gradient(W, H, limited, 7);
    let white = Nv12Image::solid(W, H, 235, 128, 128, color(limited)).unwrap();
    // Columns cycle below / at / above the clip code.
    let codes = [125u8, 126, 127];
    let wu = W as usize;
    let y: Vec<u8> = (0..wu * H as usize).map(|i| codes[(i % wu) % 3]).collect();
    let key = Nv12Image::new(W, H, y, vec![128u8; wu * H as usize / 2], color(limited)).unwrap();
    let clip = multiview_compositor::range::expand_luma(126, limited);
    let mut floor = (f64::INFINITY, f64::INFINITY);
    for gain in [1e30_f32, f32::INFINITY] {
        for invert in [false, true] {
            let tiles = [
                Tile::placed(&bg, 0, 0, 1.0),
                Tile::placed(&white, 0, 0, 1.0)
                    .with_key(TileKey::luma(clip, gain).with_source(&key).inverted(invert)),
            ];
            assert_key_visible("step", &tiles);
            let (s, p) = gate_exact(
                &gpu,
                &format!("step gain={gain:e} invert={invert}"),
                &tiles,
                2,
            );
            floor = (floor.0.min(s), floor.1.min(p));
        }
    }
    println!("step minimum: SSIM={:.5} PSNR={:.2}dB", floor.0, floor.1);
}
