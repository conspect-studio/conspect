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
    OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
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

/// BOLA re-disclosure through the audit log (OWASP API1, ADR-W005/ADR-W025): the
/// audit history carries every mutation's `object_id` AND a `detail` body with
/// full resource contents (device ids, `device_ref`, sync-group members). A
/// scoped principal reading `GET /audit` unfiltered re-enumerates every
/// out-of-scope object id and the device refs redacted elsewhere. So a scoped
/// principal must see ONLY entries for objects in its allowlist, and those
/// entries' detail bodies must still redact out-of-scope device refs.
///
/// `SCOPED_TOKEN` (allowlist `["scoped-layout"]`). Admin creates an out-of-scope
/// device `dev-other` (audited under `object_id` `dev-other`) and an in-scope
/// source `scoped-layout` whose `device_ref` is `dev-other` (audited under
/// `object_id` `scoped-layout`, detail body carrying the `device_ref`). The
/// scoped `/audit` must show only the `scoped-layout` entry, with its detail
/// `device_ref` redacted; never an entry whose `object_id` is `dev-other`.
#[tokio::test]
async fn audit_list_filters_entries_and_redacts_detail_for_a_scoped_principal() {
    let h = harness();

    // An out-of-scope device mutation (object_id = dev-other).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-other",
            ADMIN_TOKEN,
            &json!({
                "name": "Theirs",
                "body": { "id": "dev-other", "driver": "zowietek", "address": "http://[fd00:db8::9]" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // An in-scope source mutation whose body embeds the out-of-scope device_ref.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/scoped-layout",
            ADMIN_TOKEN,
            &json!({
                "name": "Mine",
                "body": {
                    "id": "scoped-layout",
                    "kind": "rtsp",
                    "url": "rtsp://[fd00:db8::1]/mine",
                    "device_ref": "dev-other"
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The scoped principal's audit listing: ONLY in-scope object ids appear.
    let resp = send(&h.router, get("/api/v1/audit", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let entries = body_json(resp).await;
    let object_ids: Vec<&str> = entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["object_id"].as_str().unwrap())
        .collect();
    assert!(
        object_ids.iter().all(|id| *id == "scoped-layout"),
        "a scoped principal must not see audit entries for out-of-scope objects (BOLA): {object_ids:?}"
    );
    assert!(
        object_ids.contains(&"scoped-layout"),
        "the scoped principal's own in-scope entry must still be visible: {object_ids:?}"
    );
    // The surviving in-scope entry's detail body must redact the out-of-scope
    // device_ref it carries.
    for entry in entries.as_array().unwrap() {
        if let Some(detail) = entry.get("detail") {
            assert!(
                detail.get("device_ref").is_none(),
                "an audit detail must not disclose an out-of-scope device_ref: {entry}"
            );
        }
    }

    // An admin sees every entry (both object ids) — no over-restriction.
    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    let admin_ids: Vec<String> = body_json(resp)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["object_id"].as_str().unwrap().to_owned())
        .collect();
    assert!(admin_ids.iter().any(|id| id == "dev-other"));
    assert!(admin_ids.iter().any(|id| id == "scoped-layout"));
}

/// A scoped principal that explicitly queries an out-of-scope `?object_id=` is
/// denied `403` — the per-object BOLA gate, exactly as a single-object `GET` of
/// that id would `403` (ADR-W005/ADR-W025). An in-scope `?object_id=` is allowed.
#[tokio::test]
async fn audit_object_id_query_is_object_scoped() {
    let h = harness();
    // Seed an out-of-scope device so its audit history exists.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-other",
            ADMIN_TOKEN,
            &json!({
                "name": "Theirs",
                "body": { "id": "dev-other", "driver": "zowietek", "address": "http://[fd00:db8::9]" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The scoped principal explicitly probing the out-of-scope object id: 403.
    let resp = send(
        &h.router,
        get("/api/v1/audit?object_id=dev-other", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "querying an out-of-scope object_id must be denied (BOLA probe)"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");

    // The scoped principal querying its OWN in-scope object id is allowed.
    let resp = send(
        &h.router,
        get("/api/v1/audit?object_id=scoped-layout", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "querying an in-scope object_id is allowed (the guard does not over-restrict)"
    );
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
