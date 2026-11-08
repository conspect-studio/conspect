#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::sync::Arc;
fn layout(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: "program".into(),
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
#[tokio::test]
async fn owned_hook_cut_changes_actual_composited_program_without_false_202() {
    let color = ColorInfo::default().resolve_defaults(64, 64);
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    a.publish(
        Nv12Image::solid(64, 64, 200, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    b.publish(
        Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    let stores = HashMap::from([("a".into(), a), ("b".into(), b)]);
    let drive = CompositorDrive::new(
        layout("a"),
        stores.clone(),
        Nv12Image::solid(64, 64, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("pgm".into())]);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: layout("a"),
            preview: layout("b"),
        })
        .unwrap();
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut results = Vec::new();
    let mut outcomes = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "one".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
        })
        .unwrap();
    let pace = Arc::clone(&clock);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            4,
            |frame| {
                results.push(frame.canvas.sample(8, 8).unwrap().0);
                pace.advance(std::time::Duration::from_millis(40));
                frame.canvas.sample(8, 8).unwrap().0
            },
            |_| None,
            SwitcherFrameHook::new(
                driver,
                "pgm".into(),
                |_: &mut CompositorDrive<Nv12Image>| {},
            ),
        )
        .await
        .unwrap();
    assert!(
        results.iter().all(|&y| y < 100),
        "first and all subsequent PGM pictures must show selected b: {results:?}"
    );
    let outcome = outcomes.try_recv().unwrap();
    assert_eq!(outcome.event.tick, 0);
    assert!(outcome.event.completed);
    assert!(outcomes.try_recv().is_err());
}
