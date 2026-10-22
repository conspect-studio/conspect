//! Read-only structured log-tail REST surface (ADR-0060 §5.2): `GET
//! /api/v1/logs` returns recent buffered records from the bounded drop-oldest
//! `LogRing`, filterable by `resource_id` / `kind` / `level` / `since` / `limit`.
//! Role: read. RFC 9457 problems on bad filters / auth. The ring is bounded and
//! never back-pressures the engine (invariant #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_telemetry::{LogLevel, LogRecord, LogResourceKind};
use support::{
    body_json, get, harness, send, OPERATOR_TOKEN, OUTPUT_SCOPED_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

fn record(seq: u64, resource_id: Option<&str>, level: LogLevel, message: &str) -> LogRecord {
    LogRecord {
        seq,
        timestamp_ms: 1_700_000_000_000 + seq,
        level,
        target: "libav".to_owned(),
        message: message.to_owned(),
        run_id: Some("run-1".to_owned()),
        resource_kind: resource_id.map(|_| LogResourceKind::Source),
        resource_id: resource_id.map(str::to_owned),
        label: None,
        component: Some("hevc".to_owned()),
        repeated: None,
    }
}

/// A record with an explicit resource kind + id (SEC-12 kind-aware filter tests).
fn record_kind(
    seq: u64,
    kind: Option<LogResourceKind>,
    id: Option<&str>,
    message: &str,
) -> LogRecord {
    LogRecord {
        seq,
        timestamp_ms: 1_700_000_000_000 + seq,
        level: LogLevel::Info,
        target: "libav".to_owned(),
        message: message.to_owned(),
        run_id: Some("run-1".to_owned()),
        resource_kind: kind,
        resource_id: id.map(str::to_owned),
        label: None,
        component: Some("hevc".to_owned()),
        repeated: None,
    }
}

#[tokio::test]
async fn lists_recent_records_for_a_viewer() {
    let h = harness();
    h.logs
        .push(record(0, Some("cnn"), LogLevel::Info, "cnn opening"));
    h.logs
        .push(record(1, Some("cnn"), LogLevel::Error, "cnn RPS error"));

    let resp = send(&h.router, get("/api/v1/logs", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["resource_id"], "cnn");
    assert_eq!(arr[1]["level"], "error");
    assert_eq!(arr[1]["component"], "hevc");
}

#[tokio::test]
async fn filters_by_resource_id_and_minimum_level() {
    let h = harness();
    h.logs
        .push(record(0, Some("cnn"), LogLevel::Info, "cnn info"));
    h.logs
        .push(record(1, Some("cnn"), LogLevel::Error, "cnn error"));
    h.logs
        .push(record(2, Some("bbc"), LogLevel::Error, "bbc error"));

    let resp = send(
        &h.router,
        get("/api/v1/logs?resource_id=cnn&level=warn", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{arr:?}");
    assert_eq!(arr[0]["message"], "cnn error");
}

#[tokio::test]
async fn filters_by_since_cursor() {
    let h = harness();
    h.logs.push(record(0, None, LogLevel::Info, "first"));
    h.logs.push(record(1, None, LogLevel::Info, "second"));
    h.logs.push(record(2, None, LogLevel::Info, "third"));

    let resp = send(&h.router, get("/api/v1/logs?since=0", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 2, "records strictly after seq 0");
    assert_eq!(arr[0]["message"], "second");
}

#[tokio::test]
async fn limit_caps_the_returned_count() {
    let h = harness();
    for i in 0..10 {
        h.logs
            .push(record(i, None, LogLevel::Info, &format!("line {i}")));
    }
    let resp = send(&h.router, get("/api/v1/logs?limit=3", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[2]["message"], "line 9");
}

#[tokio::test]
async fn unknown_level_is_a_422_problem() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs?level=bogus", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn unknown_kind_is_a_422_problem() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs?kind=bogus", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn unauthenticated_listing_is_401() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs", "bogus.token")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn empty_ring_returns_empty_list() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    assert!(arr.as_array().unwrap().is_empty());
}

// ---- SEC-12 (BOLA, ADR-W005/W025/W026): kind-aware log-tail scope filter ----
//
// GET /api/v1/logs returned EVERY buffered record regardless of the caller's
// scope, so a scoped principal could read logs (config bodies, error detail) for
// out-of-scope resources. The filter must be kind-aware: Source/Layout/Device →
// object axis, Output → output axis, Program → unrestricted-only; and an
// unattributed / unknown-kind record fails closed for a scoped principal. An
// explicit resource_id query is a per-object probe (403 out of scope; an
// ambiguous no-kind query must clear BOTH axes). No-op for an unscoped principal.

#[tokio::test]
async fn scoped_principal_sees_only_in_scope_object_logs() {
    let h = harness();
    // scoped-key is object-scoped to "scoped-layout".
    h.logs.push(record_kind(
        0,
        Some(LogResourceKind::Source),
        Some("scoped-layout"),
        "in scope",
    ));
    h.logs.push(record_kind(
        1,
        Some(LogResourceKind::Source),
        Some("cnn"),
        "out of scope",
    ));
    h.logs.push(record_kind(
        2,
        Some(LogResourceKind::Program),
        None,
        "program",
    ));
    h.logs.push(record_kind(3, None, None, "unattributed"));

    let resp = send(&h.router, get("/api/v1/logs", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "only the in-scope source record is visible (cnn, program, unattributed dropped): {arr:?}"
    );
    assert_eq!(arr[0]["resource_id"], "scoped-layout");
}

#[tokio::test]
async fn output_scoped_principal_filters_by_the_output_axis() {
    let h = harness();
    // out-scoped-key is output-scoped to "wall-1".
    h.logs.push(record_kind(
        0,
        Some(LogResourceKind::Output),
        Some("wall-1"),
        "in scope output",
    ));
    h.logs.push(record_kind(
        1,
        Some(LogResourceKind::Output),
        Some("wall-2"),
        "out of scope output",
    ));
    h.logs.push(record_kind(
        2,
        Some(LogResourceKind::Program),
        None,
        "program",
    ));

    let resp = send(&h.router, get("/api/v1/logs", OUTPUT_SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "only the in-scope output record is visible (wall-2 + program dropped): {arr:?}"
    );
    assert_eq!(arr[0]["resource_id"], "wall-1");
}

#[tokio::test]
async fn explicit_out_of_scope_resource_id_query_is_forbidden() {
    let h = harness();
    h.logs.push(record_kind(
        0,
        Some(LogResourceKind::Source),
        Some("cnn"),
        "cnn",
    ));
    // A per-object probe for an out-of-scope id is denied, exactly as a
    // single-object GET of that id would be.
    let resp = send(
        &h.router,
        get("/api/v1/logs?resource_id=cnn&kind=source", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
}

#[tokio::test]
async fn ambiguous_resource_id_query_without_kind_fails_closed() {
    let h = harness();
    // No `kind`: the id is ambiguous across the object and output axes, so a
    // scoped principal must clear BOTH — an out-of-scope object id is denied.
    let resp = send(&h.router, get("/api/v1/logs?resource_id=cnn", SCOPED_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an ambiguous id probe fails closed on the object axis"
    );
}

#[tokio::test]
async fn explicit_in_scope_resource_id_query_is_allowed() {
    let h = harness();
    h.logs.push(record_kind(
        0,
        Some(LogResourceKind::Source),
        Some("scoped-layout"),
        "in scope",
    ));
    let resp = send(
        &h.router,
        get(
            "/api/v1/logs?resource_id=scoped-layout&kind=source",
            SCOPED_TOKEN,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn unscoped_operator_sees_every_record_including_program_and_unattributed() {
    let h = harness();
    h.logs.push(record_kind(
        0,
        Some(LogResourceKind::Source),
        Some("cnn"),
        "s",
    ));
    h.logs
        .push(record_kind(1, Some(LogResourceKind::Program), None, "p"));
    h.logs.push(record_kind(2, None, None, "u"));
    let resp = send(&h.router, get("/api/v1/logs", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await.as_array().unwrap().len(),
        3,
        "an unscoped principal sees every record (the fix is a no-op for it)"
    );
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_log_record_mirror_matches_the_telemetry_serde_shape() {
    // The OpenAPI mirror (LogRecordDoc) must serialise to the SAME JSON shape as
    // the real telemetry LogRecord it documents, or the published contract would
    // lie. Round-trip a real record's JSON THROUGH the mirror — including the
    // unattributed case (resource_id omitted) and the libav case (component set).
    use multiview_control::openapi_schemas::LogRecordDoc;

    let attributed = record(7, Some("cnn"), LogLevel::Error, "cnn RPS error");
    let unattributed = record(8, None, LogLevel::Info, "context-free line");
    for real in [attributed, unattributed] {
        let real_json = serde_json::to_value(&real).unwrap();
        let doc: LogRecordDoc = serde_json::from_value(real_json.clone()).unwrap();
        let doc_json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            real_json, doc_json,
            "the OpenAPI mirror must match the telemetry LogRecord serde shape"
        );
    }
}
