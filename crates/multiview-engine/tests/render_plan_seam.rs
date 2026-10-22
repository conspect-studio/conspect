#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::{ManualTimeSource, Tick, TimeSource};
use multiview_engine::drive::{Placement, SceneStates, SwitcherTickState};
use multiview_engine::{CompositorDrive, EnginePublisher, EngineRuntime, OutputClock, StopSignal};
use multiview_framestore::TileStore;

fn drive() -> CompositorDrive<Nv12Image> {
    let color = ColorInfo::default().resolve_defaults(64, 48);
    let slate = Nv12Image::solid(16, 16, 100, 128, 128, color).unwrap();
    let layout = Layout {
        name: "resolver-test".into(),
        canvas: Canvas { width: 16, height: 16, fps_num: 60, fps_den: 1 },
        cells: vec![Cell { w: 1.0, h: 1.0, ..Cell::default() }],
    };
    CompositorDrive::new(Arc::new(layout), HashMap::<String, Arc<TileStore<Nv12Image>>>::new(), slate, CanvasColor::default(), LinearRgba::TRANSPARENT).unwrap()
}

fn alternate(_scenes: &SceneStates<'_>, _state: &SwitcherTickState, tick: Tick, placements: &mut Vec<Placement>) {
    for placement in placements {
        placement.opacity = if tick.index % 2 == 0 { 1.0 } else { 0.0 };
    }
}

#[tokio::test]
async fn tick_aware_hook_and_resolver_replay_identically_without_pool_growth() {
    async fn replay() -> Vec<(u64, Vec<u8>)> {
        let drive = drive().with_render_plan_resolver(alternate);
        let time = Arc::new(ManualTimeSource::new());
        let mut runtime = EngineRuntime::new(
            OutputClock::new(Rational::FPS_60).unwrap(),
            drive,
            Arc::clone(&time) as Arc<dyn TimeSource>,
            multiview_engine::CooperativePacer,
        );
        let publisher = EnginePublisher::<u64, ()>::new(1);
        let mut frames = Vec::new();
        let outcome = runtime.run_for_with_tick_control(
            &publisher, &StopSignal::new(), 8,
            |frame| { frames.push((frame.tick.index, frame.canvas.y_plane().to_vec())); frame.tick.index },
            |_| None,
            move |_drive, tick| {
                assert!(tick.index < 8);
                time.advance(Duration::from_nanos(16_666_667));
            },
        ).await.unwrap();
        assert_eq!(outcome.ticks, 8);
        frames
    }
    let first = replay().await;
    let second = replay().await;
    assert_eq!(first, second);
    assert_ne!(first[0].1, first[1].1);

}

#[tokio::test]
async fn wedged_downstream_consumer_does_not_stall_tick_aware_hook() {
    let time = Arc::new(ManualTimeSource::new());
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::FPS_60).unwrap(), drive(),
        Arc::clone(&time) as Arc<dyn TimeSource>,
        multiview_engine::CooperativePacer,
    );
    let publisher = EnginePublisher::<u64, ()>::new(1);
    // No receiver polls the state or event lane: a permanently wedged consumer.
    let outcome = runtime.run_for_with_tick_control(
        &publisher, &StopSignal::new(), 50,
        |frame| frame.tick.index,
        |frame| Some(frame.tick.index).map(|_| ()),
        move |_, _| time.advance(Duration::from_nanos(16_666_667)),
    ).await.unwrap();
    assert_eq!(outcome.ticks, 50);
    assert_eq!(runtime.frames_repeated(), 0);
}
