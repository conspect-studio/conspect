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
fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::ZERO,
    }
}
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
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
            source: Some(id.into()),
            ..Cell::default()
        }],
    })
}
#[test]
fn repeat_ticks_never_claim_cut_before_fresh_frame() {
    let th = TileThresholds::from_millis(500, 2000, 10000).unwrap();
    let stores: HashMap<String, Arc<TileStore<Nv12Image>>> = ["a", "b"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, th, NoSignalPolicy::Slate));
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
        .collect();
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    driver.drain_at_tick(tick(0));
    driver.commit_composed_tick(tick(0));
    let mut outcome = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "cut-repeat".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
        })
        .unwrap();
    for i in 1..20 {
        driver.repeat_tick(tick(i));
        assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
        assert!(outcome.try_recv().is_err());
    }
    driver.drain_at_tick(tick(20));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
    assert!(outcome.try_recv().is_err());
    driver.commit_composed_tick(tick(20));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
    assert_eq!(outcome.try_recv().unwrap().event.tick, 20);
}
