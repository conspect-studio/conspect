//! End-to-end tests for the sync-groups resource (ADR-M008/M010): `/sync-groups`
//! CRUD with `ETag`/`If-Match` (`412`), typed-body validation (`422`), `404`
//! problem documents, and the `POST /sync-groups/{id}/measure` action (`202` +
//! operation id). Driven through the real router via `tower::oneshot`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{
    body_json, delete_if_match, get, harness, post_if_match, post_json, put_json, send,
    ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

/// Collect the `device` values present across a sync-group body's members.
fn member_devices(body: &serde_json::Value) -> Vec<String> {
    body["body"]["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("device").and_then(|d| d.as_str()).map(str::to_owned))
        .collect()
}

/// A valid sync-group body (the canonical `multiview_config::SyncGroup` shape).
fn group_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": {
            "id": "lobby-wall",
            "mode": "auto",
            "target_skew_ms": 50,
            "members": [
                { "device": "dev-node-left", "offset_ms": 0 },
                { "device": "dev-node-right", "offset_ms": 0 }
            ]
        }
    })
}

/// BOLA embedded-reference leak (OWASP API1, ADR-W005/ADR-W025): a sync group
/// carries managed **device ids** in `members[].device`. A scoped principal that
/// is authorized for the GROUP (its own id is in scope) must NOT learn the ids of
/// out-of-scope member devices — those must be redacted, by parity with a
/// single-device `GET` `403`'ing the out-of-scope device.
///
/// The group id `scoped-layout` is in `SCOPED_TOKEN`'s allowlist (so the group is
/// readable); its members are `scoped-layout` (in scope) and `dev-other` (out).
/// The scoped read must show only the in-scope member device; admin sees both.
#[tokio::test]
async fn sync_group_members_redact_out_of_scope_device_ids() {
    let h = harness();
    // Admin creates a group keyed by the scoped principal's allowed id, mixing an
    // in-scope and an out-of-scope member device.
    let body = json!({
        "name": "Scoped wall",
        "body": {
            "id": "scoped-layout",
            "target_skew_ms": 50,
            "members": [
                { "device": "scoped-layout", "offset_ms": 0 },
                { "device": "dev-other", "offset_ms": 0 }
            ]
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/sync-groups/scoped-layout", ADMIN_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Single GET (group in scope): the out-of-scope member device id is redacted.
    let resp = send(
        &h.router,
        get("/api/v1/sync-groups/scoped-layout", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let group = body_json(resp).await;
    assert_eq!(
        member_devices(&group),
        vec!["scoped-layout".to_owned()],
        "a scoped principal must not see an out-of-scope member device id (BOLA)"
    );
    // The member entry itself is preserved (count unchanged) — only the device id
    // is dropped, so the group's structure stays intact.
    assert_eq!(
        group["body"]["members"].as_array().map(Vec::len),
        Some(2),
        "the out-of-scope member entry stays; only its device id is redacted"
    );

    // The list view redacts identically.
    let resp = send(&h.router, get("/api/v1/sync-groups", SCOPED_TOKEN)).await;
    let list = body_json(resp).await;
    let scoped_row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "scoped-layout")
        .expect("the scoped group is listed");
    assert_eq!(member_devices(scoped_row), vec!["scoped-layout".to_owned()]);

    // An unscoped admin sees BOTH member device ids (no redaction).
    let resp = send(
        &h.router,
        get("/api/v1/sync-groups/scoped-layout", ADMIN_TOKEN),
    )
    .await;
    let group = body_json(resp).await;
    assert_eq!(
        member_devices(&group),
        vec!["scoped-layout".to_owned(), "dev-other".to_owned()],
        "an unscoped admin sees every member device id"
    );
}

/// BOLA ROW enumeration (OWASP API1, ADR-W005/ADR-W025): a sync group is itself
/// object-scoped by its OWN id (`get_sync_group` 403s an out-of-scope id), so
/// `list_sync_groups` MUST filter ROWS to the principal's allowlist. Redacting
/// only the embedded `members[].device` (round-3) still leaked out-of-scope
/// GROUP ids through the list.
///
/// `SCOPED_TOKEN` (allowlist `["scoped-layout"]`) lists with an in-scope group
/// (`scoped-layout`, a member device out of scope) and an out-of-scope group
/// (`other-group`). The scoped list must contain ONLY `scoped-layout`, and that
/// row's out-of-scope member device must still be redacted.
#[tokio::test]
async fn list_filters_sync_group_rows_to_the_scoped_allowlist() {
    let h = harness();
    // In-scope group (own id allowlisted) with an out-of-scope member device.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/scoped-layout",
            ADMIN_TOKEN,
            &json!({
                "name": "Mine",
                "body": {
                    "id": "scoped-layout",
                    "target_skew_ms": 50,
                    "members": [
                        { "device": "scoped-layout", "offset_ms": 0 },
                        { "device": "dev-other", "offset_ms": 0 }
                    ]
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Out-of-scope group (own id NOT allowlisted).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/other-group",
            ADMIN_TOKEN,
            &json!({
                "name": "Theirs",
                "body": {
                    "id": "other-group",
                    "target_skew_ms": 50,
                    "members": [{ "device": "dev-x", "offset_ms": 0 }]
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(&h.router, get("/api/v1/sync-groups", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout"],
        "a scoped principal must see ONLY its allowlisted sync-group rows, never enumerate others (BOLA)"
    );
    // …and the surviving in-scope group's out-of-scope member device is redacted.
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "scoped-layout")
        .unwrap();
    assert_eq!(
        member_devices(row),
        vec!["scoped-layout".to_owned()],
        "the surviving row still redacts its out-of-scope member device: {row}"
    );

    let resp = send(&h.router, get("/api/v1/sync-groups", ADMIN_TOKEN)).await;
    assert_eq!(
        body_json(resp).await.as_array().unwrap().len(),
        2,
        "an unscoped admin still sees every sync group"
    );
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            &group_body("Lobby wall"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(support::etag(&resp).as_deref(), Some("W/\"1\""));
    let created = body_json(resp).await;
    assert_eq!(created["id"], "lobby-wall");
    assert_eq!(created["body"]["target_skew_ms"], 50);

    let resp = send(
        &h.router,
        get("/api/v1/sync-groups/lobby-wall", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn get_unknown_group_is_404_problem_json() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/sync-groups/missing", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["type"], "/problems/not-found");
}

#[tokio::test]
async fn update_with_stale_if_match_is_412() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            &group_body("Lobby wall"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &group_body("Lobby wall v2"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &group_body("Stale"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn empty_member_list_is_422() {
    let h = harness();
    let body = json!({
        "name": "Bad",
        "body": { "id": "bad", "target_skew_ms": 50, "members": [] }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/sync-groups/bad", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 422);
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn delete_requires_admin_and_if_match() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            &group_body("Lobby wall"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        delete_if_match(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(
        &h.router,
        delete_if_match(
            "/api/v1/sync-groups/lobby-wall",
            ADMIN_TOKEN,
            Some("W/\"1\""),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn measure_returns_202_with_operation_id() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            &group_body("Lobby wall"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        post_if_match(
            "/api/v1/sync-groups/lobby-wall/measure",
            OPERATOR_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert!(
        body["operation_id"].as_str().is_some(),
        "measure returns an operation id: {body}"
    );
}

#[tokio::test]
async fn measure_of_unknown_group_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        post_if_match("/api/v1/sync-groups/missing/measure", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_is_id_sorted() {
    let h = harness();
    for id in ["zeta", "alpha"] {
        let body = json!({
            "name": id,
            "body": {
                "id": id,
                "target_skew_ms": 50,
                "members": [ { "device": "dev-a" } ]
            }
        });
        send(
            &h.router,
            post_json(&format!("/api/v1/sync-groups/{id}"), OPERATOR_TOKEN, &body),
        )
        .await;
    }
    let resp = send(&h.router, get("/api/v1/sync-groups", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["alpha", "zeta"]);
    let _ = header::ETAG;
}
