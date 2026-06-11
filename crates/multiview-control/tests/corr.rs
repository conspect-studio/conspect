//! CTL-5: command-outcome correlation on the realtime stream.
//!
//! An accepted command returns `202 Accepted` + an [`OperationId`] immediately;
//! its eventual outcome [`Event`] is delivered on the realtime stream. These
//! tests pin the ADR-W008 contract that the outcome envelope carries the SAME
//! correlation id (`Envelope.corr`) the request was 202'd with, so a UI can
//! match the async outcome to its request — and that an event with no
//! originating command carries no spurious `corr`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use multiview_control::realtime::{CorrKey, CorrRegistry};
use multiview_control::{AppState, Command, OperationId, SessionStream};
use multiview_engine::EnginePublisher;
use multiview_events::{
    Alert, AlertSeverity, Event, OutputRunState, OutputStatus, SalvoEvent, SalvoPhase,
};
use serde_json::json;
use support::{body_json, post_json, seeded_keys, send, OPERATOR_TOKEN};

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

/// A salvo take's outcome envelope carries the same `corr` the take was
/// accepted with: the registry records `op` under the command's outcome key at
/// 202 time, and the realtime projection stamps it onto the matching outcome.
#[tokio::test]
async fn salvo_take_outcome_carries_corr() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(CorrRegistry::new(64));

    // The command surface 202's a TakeSalvo and records the correlation by the
    // outcome key the engine will produce.
    let op = OperationId::new();
    let command = Command::TakeSalvo {
        op: op.clone(),
        salvo: Some("salvo_one".to_owned()),
        head: None,
    };
    let key = CorrKey::for_command(&command).expect("a take command has an outcome key");
    registry.record(key, op.clone());

    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-corr", None).with_corr_registry(registry);

    // The engine applies the take and publishes its outcome event.
    engine.publish_event(Event::SalvoTaken(SalvoEvent::new(
        "salvo_one",
        SalvoPhase::Taken,
    )));

    let delta = session
        .next_delta()
        .await
        .unwrap()
        .expect("the take outcome is delivered");
    assert!(
        matches!(&delta.envelope.payload, Event::SalvoTaken(e) if e.salvo == "salvo_one"),
        "expected the SalvoTaken outcome, got {:?}",
        delta.envelope.payload
    );
    assert_eq!(
        delta.envelope.corr.as_deref(),
        Some(op.as_str()),
        "the outcome envelope must echo the accepted op id as corr"
    );
}

/// A Start command's `OutputStatus{Running}` outcome carries the start's corr.
#[tokio::test]
async fn start_outcome_carries_corr() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(CorrRegistry::new(64));

    let op = OperationId::new();
    let command = Command::Start { op: op.clone() };
    let key = CorrKey::for_command(&command).expect("a start command has an outcome key");
    registry.record(key, op.clone());

    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-start", None).with_corr_registry(registry);

    engine.publish_event(Event::OutputStatus(OutputStatus {
        state: OutputRunState::Running,
        bitrate_bps: None,
        clients: None,
    }));

    let delta = session
        .next_delta()
        .await
        .unwrap()
        .expect("the start outcome is delivered");
    assert_eq!(
        delta.envelope.corr.as_deref(),
        Some(op.as_str()),
        "the running OutputStatus must echo the start op id as corr"
    );
}

/// An event with no originating command carries no `corr`: an unrelated alert
/// (never recorded in the registry) is delivered with `corr: None`, and a
/// recorded key is not consumed by a non-matching event.
#[tokio::test]
async fn event_without_command_has_no_corr() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(CorrRegistry::new(64));

    // Record a take correlation; it must NOT be stamped onto an unrelated alert.
    let op = OperationId::new();
    registry.record(
        CorrKey::for_command(&Command::TakeSalvo {
            op: op.clone(),
            salvo: Some("salvo_one".to_owned()),
            head: None,
        })
        .expect("take key"),
        op,
    );

    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-none", None).with_corr_registry(registry);

    engine.publish_event(alert("unrelated"));

    let delta = session
        .next_delta()
        .await
        .unwrap()
        .expect("the alert is delivered");
    assert!(
        matches!(&delta.envelope.payload, Event::AlertRaised(a) if a.key == "unrelated"),
        "expected the unrelated alert, got {:?}",
        delta.envelope.payload
    );
    assert_eq!(
        delta.envelope.corr, None,
        "an event with no originating command must carry no corr"
    );
}

/// A correlation is consumed once: the second matching outcome (e.g. a stop
/// after a start, or a re-emitted event) carries no stale corr.
#[tokio::test]
async fn corr_is_consumed_once() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(CorrRegistry::new(64));

    let op = OperationId::new();
    registry.record(
        CorrKey::for_command(&Command::Start { op: op.clone() }).expect("start key"),
        op.clone(),
    );

    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-once", None).with_corr_registry(registry);

    // Two identical Running statuses; only the first matches the single recorded
    // correlation. The second has no stale corr.
    engine.publish_event(Event::OutputStatus(OutputStatus {
        state: OutputRunState::Running,
        bitrate_bps: None,
        clients: None,
    }));
    engine.publish_event(Event::OutputStatus(OutputStatus {
        state: OutputRunState::Running,
        bitrate_bps: None,
        clients: None,
    }));

    let first = session.next_delta().await.unwrap().expect("first running");
    assert_eq!(first.envelope.corr.as_deref(), Some(op.as_str()));
    let second = session.next_delta().await.unwrap().expect("second running");
    assert_eq!(
        second.envelope.corr, None,
        "a consumed correlation must not be re-stamped onto a later event"
    );
}

/// Invariant #10 re-asserted with the registry installed: a never-reading
/// client lags rather than back-pressuring the publisher, and recovers via the
/// lagged-skip resubscribe — the correlation registry never blocks publishing.
#[tokio::test]
async fn corr_registry_preserves_lagged_skip_isolation() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(4));
    let registry = Arc::new(CorrRegistry::new(64));
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-slow", None).with_corr_registry(registry);

    // Overflow the ring many times over while the session never drains. Each
    // publish must return promptly (wait-free); completing the loop is the proof
    // the engine is never back-pressured (invariant #10) even with a registry.
    for i in 0..1000 {
        let seq = engine.publish_event(alert(&format!("evt-{i}")));
        assert_eq!(seq, u64::try_from(i + 1).unwrap());
    }

    let recovery = tokio::time::timeout(std::time::Duration::from_secs(5), session.next_delta())
        .await
        .expect("lagged recovery must not block")
        .expect("lagged recovery is not a stream error");
    assert_eq!(
        recovery, None,
        "a far-behind client observes a lagged-skip recovery, not back-pressure"
    );
}

/// End-to-end: a `POST /api/v1/commands/start` that returns `202` + an op id
/// records that op into the live `AppState` correlation registry, so the
/// realtime projection — built over the SAME registry — stamps it as `corr` on
/// the start's `OutputStatus{Running}` outcome event. This exercises the live
/// route → registry → realtime wiring, not just the seam in isolation.
#[tokio::test]
async fn posted_start_command_correlates_its_realtime_outcome() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let (commands, _rx) = multiview_control::command_bus(8);
    let state = AppState::new(
        Arc::clone(&engine),
        commands,
        Arc::new(multiview_control::InMemoryRepository::new()),
        Arc::new(seeded_keys()),
    );
    // Hold the live registry the router will record into.
    let corr = Arc::clone(&state.corr);
    let router = multiview_control::router(state);

    // A genuine operator POST: 202 Accepted + an operation id.
    let resp = send(
        &router,
        post_json("/api/v1/commands/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let op = body_json(resp).await["operation_id"]
        .as_str()
        .expect("operation_id present")
        .to_owned();
    assert!(!op.is_empty());

    // The realtime projection over the same registry stamps the recorded op onto
    // the start's outcome event.
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-e2e", None).with_corr_registry(corr);
    engine.publish_event(Event::OutputStatus(OutputStatus {
        state: OutputRunState::Running,
        bitrate_bps: None,
        clients: None,
    }));

    let delta = session
        .next_delta()
        .await
        .unwrap()
        .expect("the start outcome is delivered");
    assert_eq!(
        delta.envelope.corr.as_deref(),
        Some(op.as_str()),
        "the posted start's op id must echo as corr on its realtime outcome"
    );

    // The wire form carries `corr` as the same id.
    let text = delta.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["corr"], op, "the JSON envelope carries corr == op");
    assert_eq!(parsed["t"], "output.status");
}
