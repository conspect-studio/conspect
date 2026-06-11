//! GPU surface-pool allocation-count gate (EFF-0).
//!
//! The wgpu compositor must allocate its per-tick GPU surfaces (canvas, tile
//! arrays, output planes, readback + uniform buffers) **once** and reuse them
//! every tick — never per frame (safety rule §5: "frame buffers come from
//! per-device pools allocated at start, never per-frame"). This test drives N
//! composite ticks at a fixed geometry and asserts the number of GPU
//! buffer/texture CREATIONS is bounded (≈ one-time setup), NOT proportional to
//! N. A second steady-state check resizes once then reuses, proving a resize is
//! rare (not per-tick) and reuse resumes after it.
//!
//! These tests REQUIRE a GPU. In a GPU-free environment (this devcontainer,
//! most CI runners) [`GpuCompositor::new`] returns a typed
//! `Error::NoAdapter`/`Error::DeviceRequest`, and each test SKIPS gracefully
//! (prints and returns) — so the suite is green on GPU-less machines while
//! still exercising the real GPU path where one exists. The allocation-count
//! *logic* itself is also unit-tested GPU-free inside `gpu::pool`.
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

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::error::Error;
use multiview_compositor::gpu::GpuCompositor;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image, Tile};
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

/// A flat NV12 tile (content is irrelevant to the allocation accounting).
fn flat_tile(w: u32, h: u32) -> Nv12Image {
    let wu = w as usize;
    let hu = h as usize;
    let y = vec![140u8; wu * hu];
    let uv = vec![128u8; wu * hu / 2];
    Nv12Image::new(w, h, y, uv, bt709_limited()).expect("flat tile geometry")
}

/// Number of steady-state ticks driven after the one-time warmup.
const TICKS: u32 = 32;

#[test]
fn steady_state_composite_does_not_allocate_per_tick() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.02, 0.02);
    let a = flat_tile(64, 64);
    let b = flat_tile(32, 32);
    let tiles = [Tile::placed(&a, 0, 0, 1.0), Tile::placed(&b, 64, 0, 1.0)];

    // First tick: one-time pool fill (allowed to allocate).
    gpu.composite(128, 64, canvas, bg, &tiles)
        .expect("first composite");
    let after_warmup = gpu.gpu_allocation_count();

    // Drive many more ticks at the SAME geometry: steady state must reuse.
    for _ in 0..TICKS {
        gpu.composite(128, 64, canvas, bg, &tiles)
            .expect("steady composite");
    }
    let after_steady = gpu.gpu_allocation_count();

    let per_tick = after_steady - after_warmup;
    println!(
        "GPU allocations: warmup={after_warmup} steady_total={after_steady} over {TICKS} ticks = {per_tick}"
    );
    assert_eq!(
        per_tick, 0,
        "steady-state composite must NOT allocate GPU surfaces per tick (got {per_tick} over {TICKS} ticks)"
    );
}

#[test]
fn allocation_count_is_bounded_not_proportional_to_ticks() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    let bg = LinearRgba::TRANSPARENT;
    let img = flat_tile(48, 48);
    let tiles = [Tile::placed(&img, 0, 0, 1.0)];

    let runs = [4_u32, 40_u32];
    let mut totals = Vec::new();
    for &n in &runs {
        // A fresh compositor per run so the per-run total reflects exactly that
        // run's allocations (the first `gpu` proved an adapter exists).
        let Ok(gpu2) = GpuCompositor::new(None) else {
            return; // adapter vanished mid-test; skip
        };
        let _ = &gpu;
        for _ in 0..n {
            gpu2.composite(96, 96, canvas, bg, &tiles)
                .expect("composite");
        }
        totals.push(gpu2.gpu_allocation_count());
    }
    println!("allocations for {runs:?} ticks = {totals:?}");
    // If allocation were per-tick, the 40-tick run would allocate ~10x the
    // 4-tick run. Pooling makes the total identical (one-time setup only).
    assert_eq!(
        totals[0], totals[1],
        "GPU allocation count must be bounded (one-time setup), not proportional to tick count"
    );
}

#[test]
fn resize_then_reuse_resumes_pooling_after_one_resize() {
    let Some(gpu) = try_gpu() else {
        return;
    };
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.01, 0.01, 0.01);
    let small = flat_tile(32, 32);
    let big = flat_tile(64, 64);

    // Warm up at geometry A.
    let tiles_a = [Tile::placed(&small, 0, 0, 1.0)];
    gpu.composite(64, 64, canvas, bg, &tiles_a)
        .expect("warmup A");
    let before_resize = gpu.gpu_allocation_count();

    // A LARGER geometry forces a (rare) reallocation of the canvas-sized pool.
    let tiles_b = [Tile::placed(&big, 0, 0, 1.0)];
    gpu.composite(256, 256, canvas, bg, &tiles_b)
        .expect("resize B");
    let after_resize = gpu.gpu_allocation_count();
    assert!(
        after_resize > before_resize,
        "a genuine resize SHOULD reallocate the canvas-sized pool"
    );

    // Steady state at the NEW geometry must reuse again (no per-tick alloc).
    for _ in 0..16 {
        gpu.composite(256, 256, canvas, bg, &tiles_b)
            .expect("steady B");
    }
    let after_steady_b = gpu.gpu_allocation_count();
    assert_eq!(
        after_steady_b, after_resize,
        "after a resize the pool must resume reuse (0 alloc/tick at the new geometry)"
    );
}
