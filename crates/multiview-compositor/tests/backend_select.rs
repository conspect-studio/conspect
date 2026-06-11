//! The compositor **backend-selection seam** (GPU-vs-CPU with degradation-safe
//! fallback).
//!
//! These tests run on the DEFAULT build and (a superset) under `wgpu`. They
//! pin the load-bearing degradation contract: the run path may *prefer* the GPU
//! compositor, but when no GPU adapter initializes the selector MUST fall back
//! to the CPU reference — never panic, never return an error, never block
//! (invariant #1: a missing/failed GPU can never stall or crash the run).
//!
//! This devcontainer has no GPU adapter, so `select(prefer_gpu = true)` is
//! expected to resolve to the CPU backend here. On real hardware it would
//! resolve to the GPU backend; the SSIM-vs-CPU equivalence of that path is
//! covered by `gpu_compositor.rs` (which skips here).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::backend::{GpuTarget, RunBackend, RunBackendKind};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{composite as cpu_composite, CanvasColor, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

/// A flat NV12 tile (mid-gray luma, neutral chroma) of the given even geometry.
fn flat_tile(w: u32, h: u32) -> Nv12Image {
    let pixels = usize::try_from(w).expect("width fits usize")
        * usize::try_from(h).expect("height fits usize");
    let y = vec![128u8; pixels];
    let uv = vec![128u8; pixels / 2];
    Nv12Image::new(w, h, y, uv, bt709_limited()).expect("flat tile geometry")
}

#[test]
fn default_backend_is_cpu() {
    let backend = RunBackend::cpu();
    assert_eq!(backend.kind(), RunBackendKind::Cpu);
}

#[test]
fn select_without_gpu_preference_is_cpu() {
    // Not preferring the GPU (`None` target) always yields the CPU reference,
    // regardless of whether the `wgpu` feature is compiled in.
    let backend = RunBackend::select(None);
    assert_eq!(backend.kind(), RunBackendKind::Cpu);
}

#[test]
fn select_falls_back_to_cpu_when_no_gpu_adapter() {
    // The degradation contract: preferring the GPU must never panic and must
    // resolve to *a* usable backend. On this GPU-free devcontainer that is the
    // CPU reference (GPU init returns `NoAdapter`, the selector falls back).
    // Without the `wgpu` feature there is no GPU variant at all, so this is also
    // CPU. Either way the selector returns a working backend, never an error.
    // `Some(GpuTarget::none())` is "prefer the GPU at the default adapter" (an
    // inert pin) — the exact prefer-GPU semantics, with no specific device named.
    let backend = RunBackend::select(Some(GpuTarget::none()));

    // We cannot assert *which* backend on hardware that has a GPU, but in THIS
    // environment there is no adapter, so it must be CPU. The crucial property
    // is that selection succeeded (no panic/abort) and produced a usable kind.
    #[cfg(not(feature = "wgpu"))]
    assert_eq!(backend.kind(), RunBackendKind::Cpu);

    // The backend must actually composite a frame (proving it is wired, not a
    // dead placeholder) without blocking or erroring on a valid request.
    let tile_img = flat_tile(16, 16);
    let tiles = [Tile::placed(&tile_img, 0, 0, 1.0)];
    let out = backend
        .composite(
            16,
            16,
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
            &tiles,
        )
        .expect("selected backend must composite a valid request");
    assert_eq!(out.width(), 16);
    assert_eq!(out.height(), 16);
    assert_eq!(
        out.color(),
        CanvasColor::default().output_tag(),
        "output must carry the canvas tag (inv #8 tag-output)"
    );
}

#[test]
fn cpu_backend_matches_free_function_byte_for_byte() {
    // The CPU backend variant is a thin dispatch to `pipeline::composite`; it
    // must be byte-identical to calling the free function directly (no
    // accidental re-encode / reorder). This guards the seam from silently
    // diverging from the golden CPU oracle.
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.1, 0.2, 0.3);
    let a = flat_tile(16, 16);
    let b = flat_tile(8, 8);
    let tiles = [Tile::placed(&a, 0, 0, 1.0), Tile::placed(&b, 4, 4, 0.5)];

    let via_backend = RunBackend::cpu()
        .composite(16, 16, canvas, bg, &tiles)
        .expect("cpu backend composite");
    let via_free = cpu_composite(16, 16, canvas, bg, &tiles).expect("free-function composite");

    assert_eq!(via_backend.width(), via_free.width());
    assert_eq!(via_backend.height(), via_free.height());
    assert_eq!(via_backend.color(), via_free.color());
    assert_eq!(
        via_backend.y_plane(),
        via_free.y_plane(),
        "Y plane must be byte-identical to the free function"
    );
    assert_eq!(
        via_backend.uv_plane(),
        via_free.uv_plane(),
        "UV plane must be byte-identical to the free function"
    );
}
