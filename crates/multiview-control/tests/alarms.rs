//! Alarm REST surface tests (tower oneshot): list + filter, acknowledge with
//! `ETag`/`If-Match` → `412`, problem+json, RBAC (viewer reads, operator acks),
//! the engine→control ingest wiring, and the isolation property (a slow ingest
//! lags rather than back-pressuring the engine).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::{ingest_step, AlarmRepository, IngestStep};
use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;
use multiview_events::{AlarmTransition, Event};
use support::{
    body_json, etag, get, harness, post_if_match, send, ACK_NANOS, ADMIN_TOKEN, OPERATOR_TOKEN,
    VIEWER_TOKEN,
};

fn record(id: &str, severity: PerceivedSeverity, scope: AlarmScope) -> AlarmRecord {
    AlarmRecord::new(
        AlarmId::new(id),
        AlarmKind::Black,
        severity,
        scope,
        MediaTime::from_nanos(5),
    )
}

fn seed(store: &Arc<dyn AlarmRepository>, record: AlarmRecord) {
    store.upsert(record).expect("seed upsert");
}

#[tokio::test]
async fn list_returns_seeded_alarms_id_sorted_to_a_viewer() {
    let h = harness();
    seed(
        &h.alarms,
        record("zeta", PerceivedSeverity::Warning, AlarmScope::System),
    );
    seed(
        &h.alarms,
        record(
            "alpha",
            PerceivedSeverity::Major,
            AlarmScope::Tile { index: 1 },
        ),
    );

    // A viewer (read-only) may list alarms.
    let resp = send(&h.router, get("/api/v1/alarms", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let arr = body.as_array().expect("array body");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "alpha");
    assert_eq!(arr[1]["id"], "zeta");
    // The wire form is the core AlarmRecord serde shape.
    assert_eq!(arr[0]["severity"], "Major");
    assert_eq!(arr[0]["scope"]["kind"], "tile");
}

#[tokio::test]
async fn list_filters_by_severity_active_and_scope() {
    let h = harness();
    seed(
        &h.alarms,
        record(
            "tile-major",
            PerceivedSeverity::Major,
            AlarmScope::Tile { index: 1 },
        ),
    );
    seed(
        &h.alarms,
        record("sys-warn", PerceivedSeverity::Warning, AlarmScope::System),
    );
    seed(
        &h.alarms,
        record(
            "tile-cleared",
            PerceivedSeverity::Cleared,
            AlarmScope::Tile { index: 2 },
        ),
    );

    // ?severity=major (case-insensitive): only the tile-major alarm.
    let resp = send(
        &h.router,
        get("/api/v1/alarms?severity=major", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["tile-major"]);

    // ?active=true: excludes the cleared alarm.
    let resp = send(&h.router, get("/api/v1/alarms?active=true", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sys-warn", "tile-major"]);

    // ?scope=tile: the two tile-scoped alarms, id-sorted.
    let resp = send(&h.router, get("/api/v1/alarms?scope=tile", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["tile-cleared", "tile-major"]);
}

#[tokio::test]
async fn unknown_severity_filter_is_a_422_problem() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/alarms?severity=bogus", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn acknowledge_requires_matching_if_match_and_records_who_when() {
    let h = harness();
    seed(
        &h.alarms,
        record("a1", PerceivedSeverity::Major, AlarmScope::System),
    );

    // Fetch the current ETag via list/get is not exposed; the seeded version is
    // INITIAL == 1, whose weak ETag is W/"1".
    let resp = send(
        &h.router,
        post_if_match("/api/v1/alarms/a1/ack", OPERATOR_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // The acknowledged record bumps to version 2 and carries the ack identity.
    assert_eq!(etag(&resp).as_deref(), Some("W/\"2\""));
    let body = body_json(resp).await;
    assert_eq!(body["ack"]["state"], "Acked");
    assert_eq!(body["ack"]["who"], "operator-key");
    assert_eq!(body["ack"]["when"], ACK_NANOS);
}

#[tokio::test]
async fn acknowledge_with_stale_if_match_is_412() {
    let h = harness();
    seed(
        &h.alarms,
        record("a1", PerceivedSeverity::Major, AlarmScope::System),
    );
    // Present a stale version (5) that does not match the live version (1).
    let resp = send(
        &h.router,
        post_if_match("/api/v1/alarms/a1/ack", OPERATOR_TOKEN, Some("W/\"5\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/version-conflict");
}

#[tokio::test]
async fn acknowledge_without_if_match_is_428() {
    let h = harness();
    seed(
        &h.alarms,
        record("a1", PerceivedSeverity::Major, AlarmScope::System),
    );
    let resp = send(
        &h.router,
        post_if_match("/api/v1/alarms/a1/ack", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn acknowledge_unknown_alarm_is_404_problem() {
    let h = harness();
    let resp = send(
        &h.router,
        post_if_match(
            "/api/v1/alarms/missing/ack",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/not-found");
}

#[tokio::test]
async fn viewer_may_read_but_may_not_acknowledge() {
    let h = harness();
    seed(
        &h.alarms,
        record("a1", PerceivedSeverity::Major, AlarmScope::System),
    );

    // Read is allowed.
    let read = send(&h.router, get("/api/v1/alarms", VIEWER_TOKEN)).await;
    assert_eq!(read.status(), StatusCode::OK);

    // Acknowledge is a write: a viewer is forbidden.
    let ack = send(
        &h.router,
        post_if_match("/api/v1/alarms/a1/ack", VIEWER_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(ack.status(), StatusCode::FORBIDDEN);
    let problem = body_json(ack).await;
    assert_eq!(problem["type"], "/problems/forbidden");
}

#[tokio::test]
async fn admin_may_acknowledge_too() {
    let h = harness();
    seed(
        &h.alarms,
        record("a1", PerceivedSeverity::Critical, AlarmScope::System),
    );
    let resp = send(
        &h.router,
        post_if_match("/api/v1/alarms/a1/ack", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn unauthenticated_listing_is_401() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/alarms", "bogus.token")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn engine_raised_alarm_flows_through_ingest_and_is_listed_over_http() {
    // The end-to-end wiring: an engine alarm event, drained by the ingest into
    // the SHARED store the router reads, becomes visible over the REST list.
    let h = harness();
    let mut sub = h.engine.subscribe();

    h.engine
        .publish_event(Event::AlarmRaised(AlarmTransition::new(record(
            "live-1",
            PerceivedSeverity::Major,
            AlarmScope::Tile { index: 4 },
        ))));

    let step = ingest_step(&mut sub, h.alarms.as_ref()).await;
    assert_eq!(
        step,
        IngestStep::Applied(multiview_control::AlarmTransitionKind::Raised)
    );

    let resp = send(&h.router, get("/api/v1/alarms", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["live-1"]);
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_alarm_mirror_matches_the_core_record_serde_shape() {
    // The OpenAPI schema mirror (AlarmRecordDoc) must serialise to the SAME JSON
    // shape as the real core AlarmRecord it documents, or the published contract
    // would lie. We round-trip a real record's JSON THROUGH the mirror and back
    // and require byte-identical JSON.
    use multiview_control::openapi_schemas::AlarmRecordDoc;

    let mut rec = record(
        "doc-1",
        PerceivedSeverity::Critical,
        AlarmScope::Probe {
            id: "p7".to_owned(),
        },
    );
    rec.dwell = MediaTime::from_nanos(250);
    rec.latched = true;

    let core_json = serde_json::to_value(&rec).unwrap();
    // The mirror parses the core JSON (same field names/tags)...
    let doc: AlarmRecordDoc = serde_json::from_value(core_json.clone()).unwrap();
    // ...and re-serialises to the same JSON.
    let doc_json = serde_json::to_value(&doc).unwrap();
    assert_eq!(
        core_json, doc_json,
        "the OpenAPI mirror must match the core AlarmRecord serde shape"
    );
}

#[tokio::test]
async fn slow_ingest_lags_without_back_pressuring_the_engine() {
    // The chaos property (invariant #10): the engine publishes far more alarm
    // events than the ring while ingest never drains. publish_event must remain
    // wait-free; ingest recovers via lagged-skip. If publish could block on the
    // slow subscriber this loop would hang — completing it is the proof.
    let h = support::harness();
    let mut sub = h.engine.subscribe();

    for i in 0..2000 {
        let seq = h
            .engine
            .publish_event(Event::AlarmRaised(AlarmTransition::new(record(
                &format!("a{i}"),
                PerceivedSeverity::Major,
                AlarmScope::System,
            ))));
        assert_eq!(seq, u64::try_from(i + 1).unwrap());
    }

    // The far-behind ingest observes the overflow and resubscribes rather than
    // erroring or hanging.
    let step = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ingest_step(&mut sub, h.alarms.as_ref()),
    )
    .await
    .expect("lagged recovery must not block");
    assert_eq!(step, IngestStep::Lagged);
}
