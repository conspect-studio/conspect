//! Keyers K1 (ADR-0113): until the GPU key path lands (after #395), a frame
//! with a keyed or scene-grouped tile is a typed `GpuLimit` on the GPU and
//! `RunBackend` composes it on the CPU reference — byte-identical to the CPU
//! oracle, never drawn without its key. Requires a GPU adapter (a software
//! one such as lavapipe is enough); skips with a note when there is none.
#![cfg(feature = "wgpu")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{composite, CanvasColor, Nv12Image, Tile};
use multiview_compositor::{Error, GpuCompositor, GpuTarget, RunBackend, RunBackendKind, TileKey};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

fn color() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

#[test]
fn a_keyed_frame_is_composed_on_the_cpu_until_the_gpu_key_path_lands() {
    let gpu = match GpuCompositor::new(None) {
        Ok(gpu) => gpu,
        Err(Error::NoAdapter(reason) | Error::DeviceRequest(reason)) => {
            println!("SKIP: no usable GPU adapter ({reason})");
            return;
        }
        Err(other) => panic!("unexpected GPU init error: {other}"),
    };
    let bg = Nv12Image::solid(16, 8, 90, 128, 128, color()).unwrap();
    let fill = Nv12Image::solid(8, 8, 200, 100, 150, color()).unwrap();
    let tiles = [
        Tile::placed(&bg, 0, 0, 1.0),
        Tile::placed(&fill, 4, 0, 1.0).with_key(TileKey::luma(0.1, 2.0)),
    ];
    let black = LinearRgba::opaque(0.0, 0.0, 0.0);
    let refused = gpu.composite(16, 8, CanvasColor::default(), black, &tiles);
    assert!(matches!(refused, Err(Error::GpuLimit(_))), "{refused:?}");

    let backend = RunBackend::select(Some(GpuTarget::none()));
    if backend.kind() != RunBackendKind::Gpu {
        println!("SKIP: RunBackend did not select the GPU");
        return;
    }
    let out = backend
        .composite(16, 8, CanvasColor::default(), black, &tiles)
        .unwrap();
    let oracle = composite(16, 8, CanvasColor::default(), black, &tiles).unwrap();
    assert_eq!(out, oracle);
    assert_eq!(backend.gpu_cpu_fallback_frames(), 1);
}
