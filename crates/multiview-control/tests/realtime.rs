//! Realtime tests: the snapshot-then-delta + resume-by-seq envelope flow driven
//! against a synthetic engine event source, plus the isolation property — a
//! never-reading client lags rather than back-pressuring the publisher.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_control::SessionStream;
use multiview_engine::EnginePublisher;
use multiview_events::{
    Alert, AlertSeverity, Event, FrameKind, InputConnection, LifecycleState, Topic,
};

type Publisher = EnginePublisher<serde_json::Value, Event>;

fn alert(key: &str) -> Event {
    Event::AlertRaised(Alert {
        key: key.to_owned(),
        severity: AlertSeverity::Warning,
        title: "test".to_owned(),
        detail: None,
        active: true,
    })
}

#[tokio::test]
async fn snapshot_precedes_deltas_with_monotonic_connection_seq() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-1", None);

    // Snapshot first, on the $control topic, at connection seq 0.
    let snap = session.snapshot_frame(engine.state.sequence());
    assert_eq!(snap.kind, FrameKind::Snapshot);
    assert_eq!(snap.envelope.topic, Topic::Control);
    assert_eq!(snap.envelope.seq.get(), 0);
    assert!(matches!(snap.envelope.payload, Event::Hello(_)));

    // Engine publishes two events; the session emits them as deltas with
    // strictly increasing per-connection seqs (1, 2) on their topics.
    engine.publish_event(alert("a"));
    engine.publish_event(Event::InputConnection(InputConnection {
        state: LifecycleState::Live,
        attempt: None,
    }));

    let d1 = session
        .next_delta()
        .await
        .unwrap()
        .expect("first delta present");
    assert_eq!(d1.kind, FrameKind::Delta);
    assert_eq!(d1.envelope.seq.get(), 1);
    assert_eq!(d1.envelope.topic, Topic::Alerts);

    let d2 = session
        .next_delta()
        .await
        .unwrap()
        .expect("second delta present");
    assert_eq!(d2.envelope.seq.get(), 2);
    assert_eq!(d2.envelope.topic, Topic::Inputs);

    // The wire form round-trips through serde.
    let text = d2.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["topic"], "inputs");
    assert_eq!(parsed["t"], "input.connection");
}

#[tokio::test]
async fn resume_after_skips_already_observed_events() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));

    // Publish three events while a subscriber is live so they are buffered.
    let sub = engine.subscribe();
    let s1 = engine.publish_event(alert("one"));
    let _s2 = engine.publish_event(alert("two"));
    let s3 = engine.publish_event(alert("three"));
    assert!(s3 > s1);

    // The client resumes after the first engine seq: it should NOT re-receive
    // event one, but should receive two and three.
    let mut session = SessionStream::new(sub, "sess-resume", Some(s1));

    // First poll: event one is skipped (Ok(None)).
    assert_eq!(session.next_delta().await.unwrap(), None);
    // Next polls deliver two then three.
    let d_two = session.next_delta().await.unwrap().expect("event two");
    let two_key = match &d_two.envelope.payload {
        Event::AlertRaised(a) => a.key.clone(),
        other => panic!("expected alert, got {other:?}"),
    };
    assert_eq!(two_key, "two");

    let d_three = session.next_delta().await.unwrap().expect("event three");
    let three_key = match &d_three.envelope.payload {
        Event::AlertRaised(a) => a.key.clone(),
        other => panic!("expected alert, got {other:?}"),
    };
    assert_eq!(three_key, "three");
}

#[tokio::test]
async fn slow_client_lags_without_back_pressuring_the_publisher() {
    // A small ring; the engine publishes far more than capacity while the
    // session never drains. Each publish must return promptly (non-blocking)
    // and the lagging session recovers via resubscribe (lagged-skip), never
    // forcing the engine to wait. This is the invariant #10 chaos property.
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(4));
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-slow", None);

    // Overflow the ring many times over. publish_event is wait-free; if it
    // could block on a slow consumer this whole loop would hang — completing it
    // is the proof the engine is never back-pressured (invariant #10).
    for i in 0..1000 {
        let seq = engine.publish_event(alert(&format!("evt-{i}")));
        assert_eq!(seq, u64::try_from(i + 1).unwrap());
    }

    // The far-behind session recovers via lagged-skip: its next poll observes
    // the overflow and re-subscribes (Ok(None)) rather than erroring or hanging.
    // A timeout guards against a regression that would block here.
    let recovery = tokio::time::timeout(std::time::Duration::from_secs(5), session.next_delta())
        .await
        .expect("lagged recovery must not block")
        .expect("lagged recovery is not a stream error");
    assert_eq!(
        recovery, None,
        "a far-behind client observes a lagged-skip recovery, not back-pressure"
    );

    // After recovery the session resumes cleanly: an event published now is
    // delivered as the next delta.
    engine.publish_event(alert("after-recovery"));
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), session.next_delta())
        .await
        .expect("post-recovery delivery must not block")
        .expect("post-recovery delivery is not a stream error")
        .expect("an event published after recovery is delivered");
    match &next.envelope.payload {
        Event::AlertRaised(a) => assert_eq!(a.key, "after-recovery"),
        other => panic!("expected the post-recovery alert, got {other:?}"),
    }
}
