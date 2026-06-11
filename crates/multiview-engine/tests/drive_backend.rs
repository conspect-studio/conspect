//! The compositor-drive **backend seam** — invariant #1 (degradation-safe).
//!
//! The drive composites one frame per tick. By default it uses the CPU
//! reference; a run may inject a GPU-preferred [`RunBackend`] via
//! [`CompositorDrive::with_backend`]. These tests pin:
//!
//! 1. the DEFAULT drive backend is CPU (no behavior change for existing runs);
//! 2. injecting a GPU-PREFERRED backend on this GPU-free box still composes a
//!    valid frame on time (the GPU init fell back to CPU — invariant #1: a
//!    missing GPU never stalls/crashes the run);
//! 3. the injected-CPU path is byte-identical to the default path (the seam did
//!    not change the golden CPU output).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::backend::{GpuTarget, RunBackend, RunBackendKind};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::Tick;
use multiview_engine::{CompositorDrive, OutputClock};
use multiview_framestore::TileStore;

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, resolved_color()).unwrap()
}

fn two_cell_layout(w: u32, h: u32) -> Layout {
    Layout {
        name: "test".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![
            Cell {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            },
            Cell {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-b".to_owned()),
                ..Cell::default()
            },
        ],
    }
}

fn make_drive(w: u32, h: u32) -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    store_a.publish(solid(w / 2, h, 200), multiview_core::time::MediaTime::ZERO);
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    store_b.publish(solid(w / 2, h, 80), multiview_core::time::MediaTime::ZERO);
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);
    CompositorDrive::new(
        Arc::new(two_cell_layout(w, h)),
        stores,
        solid(w, h, 16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

fn tick_at(index: u64) -> Tick {
    let clock = OutputClock::new(Rational::FPS_60).unwrap();
    Tick {
        index,
        pts: clock.pts_at(index),
    }
}

#[test]
fn default_drive_backend_is_cpu() {
    let drive = make_drive(640, 480);
    assert_eq!(drive.backend_kind(), RunBackendKind::Cpu);
}

#[test]
fn gpu_preferred_backend_still_composes_a_valid_frame() {
    // On this GPU-free devcontainer, preferring the GPU must fall back to CPU
    // (RunBackend::select) and STILL produce one valid frame on time — a missing
    // GPU can never stall or crash the run (invariant #1).
    let drive = make_drive(640, 480).with_backend(RunBackend::select(Some(GpuTarget::none())));
    let frame = drive.compose(tick_at(0)).unwrap();
    assert_eq!(frame.canvas.width(), 640);
    assert_eq!(frame.canvas.height(), 480);
    assert_eq!(frame.tick.index, 0);
    assert_eq!(
        frame.canvas.color(),
        CanvasColor::default().output_tag(),
        "output must carry the canvas tag"
    );
}

#[test]
fn injected_cpu_backend_matches_default_byte_for_byte() {
    // Injecting an explicit CPU backend must not change the golden output: the
    // composed canvas is byte-identical to the default (free-function) path.
    let default_frame = make_drive(640, 480).compose(tick_at(0)).unwrap();
    let injected_frame = make_drive(640, 480)
        .with_backend(RunBackend::cpu())
        .compose(tick_at(0))
        .unwrap();
    assert_eq!(
        default_frame.canvas.y_plane(),
        injected_frame.canvas.y_plane(),
        "Y plane must be byte-identical to the default path"
    );
    assert_eq!(
        default_frame.canvas.uv_plane(),
        injected_frame.canvas.uv_plane(),
        "UV plane must be byte-identical to the default path"
    );
}
