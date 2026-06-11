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
    ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN,
};

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
