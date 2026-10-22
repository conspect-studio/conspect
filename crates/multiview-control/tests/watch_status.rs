//! `GET /api/v1/config/watch-status` (ADR-W020): the read-only config-file
//! watch surface — whether a watcher is active, the watched path, the last
//! applied/rejected loads, and the restart-pending sections.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::ConfigWatchStatus;
use support::{body_bytes, get, harness, harness_with, send, VIEWER_TOKEN};

#[tokio::test]
async fn watch_status_requires_authentication() {
    let h = harness();
    let response = send(
        &h.router,
        axum::http::Request::builder()
            .method("GET")
            .uri("/api/v1/config/watch-status")
            .body(axum::body::Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn watch_status_defaults_to_inactive() {
    let h = harness();
    let response = send(&h.router, get("/api/v1/config/watch-status", VIEWER_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes(response).await).expect("json body");
    assert_eq!(body.get("active"), Some(&serde_json::json!(false)));
    assert_eq!(body.get("path"), Some(&serde_json::Value::Null));
    assert_eq!(body.get("applied_count"), Some(&serde_json::json!(0)));
    assert_eq!(body.get("last_applied"), Some(&serde_json::Value::Null));
    assert_eq!(body.get("last_rejected"), Some(&serde_json::Value::Null));
    assert_eq!(
        body.get("restart_pending"),
        Some(&serde_json::json!([])),
        "no watcher => nothing pending"
    );
}

#[tokio::test]
async fn watch_status_reflects_recorded_watch_activity() {
    let status = Arc::new(ConfigWatchStatus::new());
    status.mark_active("/etc/multiview/multiview.toml");
    status.record_applied(1_718_000_000_123, "sources: in_a changed; live9 added");
    status.record_rejected(1_718_000_050_456, "TOML parse error at line 3, column 9");
    status.add_restart_pending(["outputs".to_owned(), "canvas".to_owned()]);
    let h = harness_with(|state| state.with_config_watch(Arc::clone(&status)));

    let response = send(&h.router, get("/api/v1/config/watch-status", VIEWER_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes(response).await).expect("json body");
    assert_eq!(body.get("active"), Some(&serde_json::json!(true)));
    assert_eq!(
        body.get("path"),
        Some(&serde_json::json!("/etc/multiview/multiview.toml"))
    );
    assert_eq!(body.get("applied_count"), Some(&serde_json::json!(1)));
    let applied = body.get("last_applied").expect("last_applied present");
    assert_eq!(
        applied.get("at_ms"),
        Some(&serde_json::json!(1_718_000_000_123_i64))
    );
    assert!(
        applied
            .get("detail")
            .and_then(|d| d.as_str())
            .is_some_and(|d| d.contains("live9")),
        "the applied detail names the change"
    );
    let rejected = body.get("last_rejected").expect("last_rejected present");
    assert_eq!(
        rejected.get("at_ms"),
        Some(&serde_json::json!(1_718_000_050_456_i64))
    );
    assert!(
        rejected
            .get("detail")
            .and_then(|d| d.as_str())
            .is_some_and(|d| d.contains("parse error")),
        "the rejected detail carries the reason"
    );
    // Sorted, deduplicated section names.
    assert_eq!(
        body.get("restart_pending"),
        Some(&serde_json::json!(["canvas", "outputs"]))
    );
}
