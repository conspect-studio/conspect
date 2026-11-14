//! ADR-0112 §3: the GPU wipe mask equals the CPU reference within the
//! existing GPU tolerance (SSIM/PSNR, as `gpu_compositor.rs`), for every
//! pattern at start, mid, half and end, with a soft edge and a border band.
//! Skips without an adapter (GPU-free CI), like every `gpu_*` test.
#![cfg(feature = "wgpu")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::error::Error;
use multiview_compositor::gpu::GpuCompositor;
use multiview_compositor::mask::{MaskShape, WipeMask};
use multiview_compositor::pipeline::{composite, CanvasColor, Nv12Image, Tile};
use multiview_core::color::ColorInfo;

const W: u32 = 128;
const H: u32 = 72;

fn try_gpu() -> Option<GpuCompositor> {
    match GpuCompositor::new(None) {
        Ok(gpu) => Some(gpu),
        Err(Error::NoAdapter(reason) | Error::DeviceRequest(reason)) => {
            println!("SKIP: no usable GPU adapter ({reason})");
            None
        }
        Err(other) => panic!("unexpected GPU init error: {other}"),
    }
}

fn solid(y: u8, cb: u8, cr: u8) -> Nv12Image {
    Nv12Image::solid(W, H, y, cb, cr, ColorInfo::default().resolve_defaults(W, H)).unwrap()
}

/// Fraction of luma samples that differ by more than `tol` code values.
fn mismatch(a: &[u8], b: &[u8], tol: u8) -> f64 {
    let bad = a
        .iter()
        .zip(b)
        .filter(|(x, y)| x.abs_diff(**y) > tol)
        .count();
    bad as f64 / a.len() as f64
}

#[test]
fn gpu_wipe_masks_match_the_cpu_reference() {
    let Some(gpu) = try_gpu() else { return };
    let out = solid(40, 110, 140);
    let inc = solid(200, 150, 100);
    let border = Nv12Image::solid_rgb(2, 2, 255, 200, 0, CanvasColor::default()).unwrap();
    for shape in [
        MaskShape::Horizontal,
        MaskShape::Vertical,
        MaskShape::Diagonal,
        MaskShape::Box,
        MaskShape::Circle,
    ] {
        for progress in [0.0_f32, 0.3, 0.5, 1.0] {
            for (soft, bw, reverse) in [(0.0, 0.0, false), (0.08, 0.03, true)] {
                let mask = WipeMask::new(shape, progress, W, H, [0.4, 0.6], reverse, soft, bw);
                let mut tiles = vec![Tile::scaled(&out, 0, 0, W, H, 1.0)];
                tiles.push(Tile::scaled(&inc, 0, 0, W, H, 1.0).with_mask(mask));
                if bw > 0.0 {
                    tiles.push(Tile::scaled(&border, 0, 0, W, H, 1.0).with_mask(mask.expanded(bw)));
                }
                let cpu = composite(
                    W,
                    H,
                    CanvasColor::default(),
                    LinearRgba::TRANSPARENT,
                    &tiles,
                )
                .unwrap();
                let got = gpu
                    .composite(
                        W,
                        H,
                        CanvasColor::default(),
                        LinearRgba::TRANSPARENT,
                        &tiles,
                    )
                    .unwrap();
                // Away from the edge the pictures are identical up to the
                // existing f32-vs-LUT tolerance; on a hard edge a pixel centre
                // may land on either side under float rounding, so at most
                // 1 % of luma samples may differ by more than 3 code values.
                let m = mismatch(cpu.y_plane(), got.y_plane(), 3);
                assert!(m <= 0.01, "{shape:?} p={progress} soft={soft}: {m}");
                let c = mismatch(cpu.uv_plane(), got.uv_plane(), 3);
                assert!(c <= 0.02, "{shape:?} p={progress} chroma: {c}");
            }
        }
    }
}

/// Masks a caller could build from public fields that `WipeMask::new` for a
/// `W x H` canvas never produces (PR #406 merge reviews R-1..R-6).
fn malformed(good: WipeMask) -> Vec<WipeMask> {
    vec![
        WipeMask {
            pinned: Some(f32::NAN),
            ..good
        },
        WipeMask {
            pinned: Some(2.0),
            ..good
        },
        WipeMask {
            radius: f32::NAN,
            ..good
        },
        WipeMask {
            softness: f32::INFINITY,
            ..good
        },
        WipeMask {
            pinned: Some(0.5),
            ..good
        },
        WipeMask { width: 0, ..good },
        WipeMask { height: 0, ..good },
        // R-6: finite fields whose intermediates overflow (`extent - radius`).
        WipeMask {
            shape: MaskShape::Circle,
            origin: [f32::MAX, 0.0],
            extent: f32::MAX,
            radius: -f32::MAX,
            reverse: true,
            softness: 1.0,
            ..good
        },
        WipeMask {
            radius: 1.0e30,
            ..good
        },
        WipeMask {
            softness: 2.0,
            ..good
        },
        WipeMask {
            aspect: 0.0,
            ..good
        },
        WipeMask {
            aspect: -1.0,
            ..good
        },
        WipeMask {
            aspect: 2.0,
            ..good
        },
        WipeMask {
            width: W * 2,
            height: H * 2,
            ..good
        },
    ]
}

/// Merge review R-1 (PR #406): a malformed public mask draws nothing on the
/// GPU, as on the CPU reference, and never poisons the layer beneath.
#[test]
fn gpu_malformed_wipe_mask_draws_nothing_like_the_cpu_reference() {
    let Some(gpu) = try_gpu() else { return };
    let out = solid(40, 110, 140);
    let inc = solid(200, 150, 100);
    let good = WipeMask::new(
        MaskShape::Horizontal,
        0.5,
        W,
        H,
        [0.0, 0.0],
        false,
        0.1,
        0.0,
    );
    let only_out = [Tile::scaled(&out, 0, 0, W, H, 1.0)];
    let base = composite(
        W,
        H,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &only_out,
    )
    .unwrap();
    for mask in malformed(good) {
        let tiles = [
            Tile::scaled(&out, 0, 0, W, H, 1.0),
            Tile::scaled(&inc, 0, 0, W, H, 1.0).with_mask(mask),
        ];
        let got = gpu
            .composite(
                W,
                H,
                CanvasColor::default(),
                LinearRgba::TRANSPARENT,
                &tiles,
            )
            .unwrap();
        assert!(!mask.is_valid_for(W, H), "{mask:?}");
        let m = mismatch(base.y_plane(), got.y_plane(), 3);
        assert!(m == 0.0, "GPU {mask:?}: {m}");
        let cpu = composite(
            W,
            H,
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
            &tiles,
        )
        .unwrap();
        assert_eq!(cpu, base, "CPU {mask:?}");
    }
}
