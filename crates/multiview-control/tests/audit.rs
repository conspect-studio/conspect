//! Change-audit-log tests: every mutation is recorded (who/what/when/object),
//! the log is queryable through a read-only route, and the route refuses writes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_control::{AuditAction, AuditLog, InMemoryAuditLog};
use multiview_core::time::MediaTime;
use serde_json::json;
use support::{
    body_json, delete_if_match, etag, get, harness, post_json, put_json, send, ADMIN_TOKEN,
    OPERATOR_TOKEN, VIEWER_TOKEN,
};

#[test]
fn audit_log_records_and_lists_in_reverse_chronological_order() {
    let log = InMemoryAuditLog::new();
    log.record(
        "admin-key",
        AuditAction::Create,
        "layout",
        "alpha",
        MediaTime::from_nanos(10),
        Some(json!({ "name": "A" })),
    );
    log.record(
        "operator-key",
        AuditAction::Update,
        "layout",
        "alpha",
        MediaTime::from_nanos(20),
        None,
    );

    let all = log.list(None).unwrap();
    assert_eq!(all.len(), 2);
    // Newest first.
    assert_eq!(all[0].actor, "operator-key");
    assert_eq!(all[0].action, AuditAction::Update);
    assert_eq!(all[1].actor, "admin-key");
    assert_eq!(all[1].action, AuditAction::Create);

    // Filtering by object id narrows the slice.
    let only_alpha = log.list(Some("alpha")).unwrap();
    assert_eq!(only_alpha.len(), 2);
    let none = log.list(Some("nonexistent")).unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn mutation_through_the_router_is_audited() {
    let h = harness();

    // A create mutation.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/wall-1",
            ADMIN_TOKEN,
            &json!({ "name": "Wall 1", "body": {} }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The audit route shows the create, attributed to the admin key.
    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let entries = body_json(resp).await;
    let arr = entries.as_array().unwrap();
    assert!(!arr.is_empty(), "the create must be audited");
    let newest = &arr[0];
    assert_eq!(newest["actor"], "admin-key");
    assert_eq!(newest["action"], "create");
    assert_eq!(newest["object_kind"], "layout");
    assert_eq!(newest["object_id"], "wall-1");

    // An update mutation is also audited.
    let etag_val =
        etag(&send(&h.router, get("/api/v1/layouts/wall-1", ADMIN_TOKEN)).await).unwrap();
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/layouts/wall-1",
            ADMIN_TOKEN,
            Some(&etag_val),
            &json!({ "name": "Wall 1 (edited)", "body": {} }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    let entries = body_json(resp).await;
    let arr = entries.as_array().unwrap();
    assert!(
        arr.len() >= 2,
        "create + update must both be audited, got {}",
        arr.len()
    );
    assert_eq!(arr[0]["action"], "update");
}

#[tokio::test]
async fn audit_route_is_read_only_and_role_gated() {
    let h = harness();

    // The audit route accepts only GET — a POST is method-not-allowed.
    let resp = send(
        &h.router,
        post_json("/api/v1/audit", ADMIN_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    // A viewer (read) may read the audit log.
    let resp = send(&h.router, get("/api/v1/audit", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn failed_mutation_is_not_audited() {
    let h = harness();

    // An operator update against a nonexistent layout fails (404) and must NOT
    // produce an audit entry — only successful mutations are recorded.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/layouts/ghost",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "ghost", "body": {} }),
        ),
    )
    .await;
    assert!(resp.status().is_client_error());

    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    let entries = body_json(resp).await;
    assert!(
        entries.as_array().unwrap().is_empty(),
        "a failed mutation must not be audited"
    );

    // A successful delete IS audited.
    send(
        &h.router,
        post_json(
            "/api/v1/layouts/real",
            ADMIN_TOKEN,
            &json!({ "name": "real", "body": {} }),
        ),
    )
    .await;
    let etag_val = etag(&send(&h.router, get("/api/v1/layouts/real", ADMIN_TOKEN)).await).unwrap();
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/layouts/real", ADMIN_TOKEN, Some(&etag_val)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    let entries = body_json(resp).await;
    let arr = entries.as_array().unwrap();
    // Reverse-chronological: the delete (newest) is first, the create second.
    let actions: Vec<&str> = arr.iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, vec!["delete", "create"]);
    assert_eq!(arr[0]["object_id"], "real");
}
