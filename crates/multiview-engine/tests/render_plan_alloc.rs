#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Zero steady-state allocation gate for the render-plan resolver seam (SW-3).
//!
//! Measures real heap allocations (a counting global allocator) around warmed
//! resolver calls, not the scratch pool's own reservation odometer — a resolver
//! that allocates a temporary or replaces the pooled placements vector must fail.
//! Kept as the only test in this binary so no concurrent test allocates while the
//! counter is sampled.

use std::alloc::System;
use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::Tick;
use multiview_engine::drive::{Placement, RenderPlanResolver, SceneStates, SwitcherTickState};
use multiview_engine::{CompositorDrive, OutputClock};
use multiview_framestore::TileStore;
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn drive(resolver: RenderPlanResolver) -> CompositorDrive<Nv12Image> {
    let color = ColorInfo::default().resolve_defaults(64, 48);
    let slate = Nv12Image::solid(16, 16, 100, 128, 128, color).unwrap();
    let cell = |source: &str, x: f32| Cell {
        x,
        w: 0.5,
        h: 1.0,
        source: Some(source.into()),
        ..Cell::default()
    };
    let layout = Layout {
        name: "resolver-alloc".into(),
        canvas: Canvas {
            width: 16,
            height: 16,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![cell("a", 0.0), cell("b", 0.5)],
    };
    let mut stores = HashMap::new();
    for id in ["a", "b"] {
        stores.insert(
            id.to_owned(),
            Arc::new(TileStore::<Nv12Image>::with_defaults(id)),
        );
    }
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        slate,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_render_plan_resolver(resolver)
}

fn alternate(
    scenes: &SceneStates<'_>,
    _state: &SwitcherTickState,
    tick: Tick,
    placements: &mut Vec<Placement>,
) {
    for (image_index, _) in scenes.order.iter().enumerate() {
        placements.push(Placement {
            image_index,
            dst_x: 0,
            dst_y: 0,
            dst_w: scenes.layout.canvas.width,
            dst_h: scenes.layout.canvas.height,
            opacity: if tick.index % 2 == 0 { 1.0 } else { 0.0 },
        });
    }
}

/// Negative control: replaces the pooled vector every tick.
fn replacing(
    scenes: &SceneStates<'_>,
    state: &SwitcherTickState,
    tick: Tick,
    placements: &mut Vec<Placement>,
) {
    let mut fresh = Vec::with_capacity(scenes.order.len());
    alternate(scenes, state, tick, &mut fresh);
    *placements = fresh;
}

/// Allocations performed by `ticks` warmed resolver calls.
fn steady_state_allocations(resolver: RenderPlanResolver, ticks: usize) -> usize {
    let drive = drive(resolver);
    let mut clock = OutputClock::new(Rational::FPS_60).unwrap();
    // Warm-up: the first resolve sizes the pool, and a full compose exercises
    // the seam end to end.
    drive.compose(clock.tick()).unwrap();
    let warm_tick = clock.tick();
    assert_eq!(drive.resolve_render_plan(warm_tick, <[Placement]>::len), 2);
    let mut visible = 0usize;
    let region = Region::new(GLOBAL);
    for _ in 0..ticks {
        let tick = clock.tick();
        visible +=
            drive.resolve_render_plan(tick, |p| p.iter().filter(|p| p.opacity > 0.5).count());
    }
    let stats = region.change();
    // Every other tick is opaque: the resolver genuinely ran each tick.
    assert_eq!(visible, ticks);
    stats.allocations + stats.reallocations
}

#[test]
fn warmed_resolver_performs_zero_steady_state_allocations() {
    assert_eq!(steady_state_allocations(alternate, 256), 0);
    // The gate measures the resolver itself: an allocating resolver is caught.
    assert!(steady_state_allocations(replacing, 256) >= 256);
}
