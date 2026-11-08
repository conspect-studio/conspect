#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb, COMMAND_CAPACITY,
    MAX_COMMANDS_PER_TICK,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::HashMap;
use std::sync::Arc;

fn primed_stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["a", "b"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
            store.publish(
                Nv12Image::solid(
                    64,
                    64,
                    128,
                    128,
                    128,
                    ColorInfo::default().resolve_defaults(64, 64),
                )
                .unwrap(),
                MediaTime::ZERO,
            );
            (id.to_owned(), store)
        })
        .collect()
}

fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::ZERO,
    }
}
fn source(id: &str) -> SourceRef {
    SourceRef::new(id)
}
fn scene(sources: &[&str]) -> Arc<Layout> {
    Arc::new(Layout {
        name: "test".into(),
        canvas: Canvas {
            width: 640,
            height: 360,
            fps_num: 25,
            fps_den: 1,
        },
        cells: sources
            .iter()
            .map(|id| Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                source: Some((*id).to_owned()),
                ..Cell::default()
            })
            .collect(),
    })
}
fn set_scenes(handle: &SwitcherControl) {
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene(&["a"]),
            preview: scene(&["b"]),
        })
        .unwrap();
}
fn command(op: usize, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op.to_string(),
        scene_version: 1,
        verb,
    }
}
fn pair() -> (
    SwitcherControl,
    multiview_engine::switcher_control::SwitcherTickDriver,
) {
    let (handle, mut driver) =
        SwitcherControl::new(SwitcherState::new(source("a"), source("b"), vec![]));
    driver.set_prime_stores(primed_stores());
    (handle, driver)
}

#[test]
fn commands_apply_only_at_exact_tick_and_reject_overload_before_202() {
    let (handle, mut driver) = pair();
    assert!(handle.snapshot().is_none());
    set_scenes(&handle);
    for i in 0..COMMAND_CAPACITY {
        handle.try_send(command(i, SwitcherVerb::Cut)).unwrap();
    }
    assert!(handle
        .try_send(command(COMMAND_CAPACITY, SwitcherVerb::Cut))
        .is_err());
    driver.drain_at_tick(tick(5));
    assert!(
        handle.snapshot().is_none(),
        "staging never claims an on-air frame"
    );
    driver.commit_composed_tick(tick(5));
    assert_eq!(handle.snapshot().unwrap().tick, 5);
    assert_eq!(
        handle.snapshot().unwrap().completion_count,
        MAX_COMMANDS_PER_TICK as u64
    );
    driver.drain_at_tick(tick(6));
    driver.commit_composed_tick(tick(6));
    assert_eq!(
        handle.snapshot().unwrap().completion_count,
        (MAX_COMMANDS_PER_TICK * 2) as u64
    );
}

#[test]
fn auto_is_correlated_only_at_terminal_tick_and_rejection_changes_no_bus() {
    let (handle, mut driver) = pair();
    set_scenes(&handle);
    let mut outcomes = handle.outcomes();
    let rate = FrameCount::try_from(25).unwrap();
    handle
        .try_send(command(1, SwitcherVerb::Auto(rate)))
        .unwrap();
    driver.drain_at_tick(tick(0));
    assert!(
        outcomes.try_recv().is_err(),
        "arming is not a completed operation"
    );
    handle
        .try_send(command(2, SwitcherVerb::SelectPreview(source("b"))))
        .unwrap();
    driver.drain_at_tick(tick(1));
    driver.commit_composed_tick(tick(1));
    let rejection = outcomes.try_recv().unwrap();
    assert_eq!(rejection.event.operation_id, "2");
    assert!(!rejection.event.completed);
    assert!(rejection
        .event
        .error
        .as_ref()
        .unwrap()
        .contains("active take"));
    assert_eq!(handle.snapshot().unwrap().preview.as_str(), "b");
    driver.drain_at_tick(tick(25));
    driver.commit_composed_tick(tick(25));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
    let completion = outcomes.try_recv().unwrap();
    assert_eq!(completion.event.operation_id, "1");
    assert_eq!(completion.event.tick, 25);
    assert!(completion.event.completed);
    assert!(completion.event.error.is_none());
    assert!(outcomes.try_recv().is_err(), "AUTO settles exactly once");
}

#[test]
fn no_provider_stale_scene_and_overlapping_mix_fail_closed() {
    let (handle, mut driver) = pair();
    let mut outcomes = handle.outcomes();
    handle.try_send(command(1, SwitcherVerb::Cut)).unwrap();
    driver.drain_at_tick(tick(0));
    driver.commit_composed_tick(tick(0));
    assert!(!outcomes.try_recv().unwrap().event.completed);
    assert_eq!(handle.snapshot().unwrap().completion_count, 0);
    set_scenes(&handle);
    handle
        .set_scenes(ScenePair {
            version: 2,
            program: scene(&["a"]),
            preview: scene(&["b"]),
        })
        .unwrap();
    handle.try_send(command(2, SwitcherVerb::Cut)).unwrap();
    driver.drain_at_tick(tick(1));
    assert!(outcomes
        .try_recv()
        .unwrap()
        .event
        .error
        .as_ref()
        .unwrap()
        .contains("scene version changed"));
    assert_eq!(handle.snapshot().unwrap().completion_count, 0);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene(&["a", "b"]),
            preview: scene(&["b"]),
        })
        .unwrap();
    handle
        .try_send(command(
            3,
            SwitcherVerb::Auto(FrameCount::try_from(4).unwrap()),
        ))
        .unwrap();
    driver.drain_at_tick(tick(2));
    let failure = outcomes.try_recv().unwrap();
    assert!(!failure.event.completed);
    assert!(failure
        .event
        .error
        .as_ref()
        .unwrap()
        .contains("overlapping"));
    assert_eq!(handle.snapshot().unwrap().completion_count, 0);
}

#[test]
fn cut_interrupts_auto_with_correlated_failure_and_new_success() {
    let (handle, mut driver) = pair();
    set_scenes(&handle);
    let mut outcomes = handle.outcomes();
    handle
        .try_send(command(
            1,
            SwitcherVerb::Auto(FrameCount::try_from(25).unwrap()),
        ))
        .unwrap();
    driver.drain_at_tick(tick(0));
    handle.try_send(command(2, SwitcherVerb::Cut)).unwrap();
    driver.drain_at_tick(tick(3));
    driver.commit_composed_tick(tick(3));
    let cancelled = outcomes.try_recv().unwrap();
    assert_eq!(cancelled.event.operation_id, "1");
    assert!(!cancelled.event.completed);
    assert!(cancelled.event.error.is_some());
    let cut = outcomes.try_recv().unwrap();
    assert_eq!(cut.event.operation_id, "2");
    assert!(cut.event.completed);
    driver.drain_at_tick(tick(25));
    assert!(outcomes.try_recv().is_err());
}
