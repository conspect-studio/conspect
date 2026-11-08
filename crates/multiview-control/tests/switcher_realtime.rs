#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_control::realtime::SessionStream;
use multiview_control::state::EngineStateSnapshot;
use multiview_control::switcher_events::forward_switcher_outcomes;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use std::sync::Arc;

fn scene(source: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: source.into(),
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
            source: Some(source.into()),
            ..Cell::default()
        }],
    })
}
#[tokio::test]
async fn terminal_only_corr_and_dual_scope_survive_bounded_relay() {
    let (control, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    control
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let mut allowed = SessionStream::new(publisher.subscribe(), "operator", None).with_scopes(
        Some(vec!["me1".into()]),
        Some(vec!["program:main".into()]),
        None,
    );
    let mut wrong_me = SessionStream::new(publisher.subscribe(), "other-me", None).with_scopes(
        Some(vec!["me2".into()]),
        Some(vec!["program:main".into()]),
        None,
    );
    let mut wrong_program = SessionStream::new(publisher.subscribe(), "other-program", None)
        .with_scopes(
            Some(vec!["me1".into()]),
            Some(vec!["program:other".into()]),
            None,
        );
    let relay = tokio::spawn(forward_switcher_outcomes(
        control.outcomes(),
        Arc::clone(&publisher),
        "me1".into(),
        "main".into(),
    ));
    control
        .try_send(SwitcherCommand {
            operation_id: "op1".into(),
            scene_version: 1,
            verb: SwitcherVerb::Auto(FrameCount::try_from(3).unwrap()),
        })
        .unwrap();
    for index in 0..3 {
        driver.drain_at_tick(Tick {
            index,
            pts: MediaTime::ZERO,
        });
    }
    assert_eq!(
        publisher.events.sequence(),
        0,
        "accepted AUTO cannot publish completion early"
    );
    driver.drain_at_tick(Tick {
        index: 3,
        pts: MediaTime::ZERO,
    });
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), allowed.next_delta())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.envelope.corr.as_deref(), Some("op1"));
    assert_eq!(frame.envelope.id.as_deref(), Some("me1"));
    match frame.envelope.payload {
        Event::SwitcherOperation(data) => {
            assert_eq!(data.tick, 3);
            assert!(data.completed);
            assert_eq!(data.operation_id, "op1");
        }
        other => panic!("wrong realtime event: {other:?}"),
    }
    assert!(
        wrong_me.next_delta().await.unwrap().is_none(),
        "M/E object-scope must filter operation"
    );
    assert!(
        wrong_program.next_delta().await.unwrap().is_none(),
        "program-scope must filter operation"
    );
    relay.abort();
}
