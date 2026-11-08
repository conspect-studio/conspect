#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::HashMap;
use std::sync::Arc;

fn scene(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: id.into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            source: Some(id.into()),
            ..Cell::default()
        }],
    })
}
fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::from_nanos(i64::try_from(index).unwrap() * 40_000_000),
    }
}
fn new() -> (
    SwitcherControl,
    multiview_engine::switcher_control::SwitcherTickDriver,
    Arc<TileStore<Nv12Image>>,
    Arc<TileStore<Nv12Image>>,
) {
    let thresholds = TileThresholds::from_millis(500, 2000, 10000).unwrap();
    let a = Arc::new(TileStore::new("a", thresholds, NoSignalPolicy::Slate));
    let b = Arc::new(TileStore::new("b", thresholds, NoSignalPolicy::Slate));
    a.publish(
        Nv12Image::solid(
            64,
            64,
            200,
            128,
            128,
            ColorInfo::default().resolve_defaults(64, 64),
        )
        .unwrap(),
        MediaTime::ZERO,
    );
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    driver.set_prime_stores(HashMap::from([
        ("a".into(), Arc::clone(&a)),
        ("b".into(), Arc::clone(&b)),
    ]));
    (handle, driver, a, b)
}
#[test]
fn cold_preview_hold_preserves_program_cadence_then_exact_tick_cut() {
    let (handle, mut driver, _a, b) = new();
    let mut events = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "cut".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
        })
        .unwrap();
    for index in 0..9 {
        driver.drain_at_tick(tick(index));
        assert_eq!(handle.snapshot().unwrap().tick, index);
        assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
        assert!(
            events.try_recv().is_err(),
            "unprimed take cannot claim applied"
        );
    }
    b.publish(
        Nv12Image::solid(
            64,
            64,
            60,
            128,
            128,
            ColorInfo::default().resolve_defaults(64, 64),
        )
        .unwrap(),
        MediaTime::ZERO,
    );
    driver.drain_at_tick(tick(9));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
    assert_eq!(handle.snapshot().unwrap().preview.as_str(), "a");
    let outcome = events.try_recv().unwrap();
    assert_eq!(outcome.event.tick, 9);
    assert!(outcome.event.completed);
    for index in 10..20 {
        driver.drain_at_tick(tick(index));
        assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
    }
}
#[test]
fn cold_preview_deadline_reports_failure_without_pgm_swap() {
    let (handle, mut driver, _a, _b) = new();
    let mut events = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "hold".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
        })
        .unwrap();
    for index in 0..51 {
        driver.drain_at_tick(tick(index));
    }
    let outcome = events.try_recv().unwrap();
    assert_eq!(outcome.event.tick, 50);
    assert!(!outcome.event.completed);
    assert!(outcome.event.error.as_ref().unwrap().contains("not primed"));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
}
