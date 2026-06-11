//! End-to-end tests for the overlays resource: CRUD, `ETag` round-trip,
//! `If-Match` optimistic concurrency (`412`), and RBAC — driven through the real
//! router. Mirrors `tests/layouts.rs`.
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
    body_json, get, harness, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN,
};

fn overlay_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": { "id": "clk", "kind": "clock", "target": "canvas", "z": 10 }
    })
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .expect("create must return an ETag")
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(etag, "W/\"1\"", "a fresh resource is version 1");
    let created = body_json(resp).await;
    assert_eq!(created["id"], "clk");
    assert_eq!(created["name"], "Clock");
    assert_eq!(created["body"]["kind"], "clock");

    let resp = send(&h.router, get("/api/v1/overlays/clk", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let fetched = body_json(resp).await;
    assert_eq!(fetched["name"], "Clock");
}

#[tokio::test]
async fn get_unknown_overlay_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/overlays/missing", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["type"], "/problems/not-found");
}

#[tokio::test]
async fn update_with_matching_if_match_succeeds_and_bumps_version() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &overlay_body("Renamed"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"2\"",
        "a successful update bumps the version"
    );
    let updated = body_json(resp).await;
    assert_eq!(updated["name"], "Renamed");
}

#[tokio::test]
async fn update_with_stale_if_match_is_412() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &overlay_body("V2"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &overlay_body("Clobber"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");

    let resp = send(&h.router, get("/api/v1/overlays/clk", OPERATOR_TOKEN)).await;
    let current = body_json(resp).await;
    assert_eq!(current["name"], "V2", "the clobbering write was rejected");
}

#[tokio::test]
async fn update_without_if_match_is_precondition_required() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            None,
            &overlay_body("X"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn list_returns_created_overlays_sorted() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/bbb",
            OPERATOR_TOKEN,
            &json!({ "name": "B", "body": { "kind": "clock", "target": "canvas" } }),
        ),
    )
    .await;
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/aaa",
            OPERATOR_TOKEN,
            &json!({ "name": "A", "body": { "kind": "clock", "target": "canvas" } }),
        ),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/overlays", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let arr = list.as_array().expect("list is an array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "aaa", "id-sorted order");
    assert_eq!(arr[1]["id"], "bbb");
}

#[tokio::test]
async fn list_requires_authentication() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/overlays")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 401);
    assert_eq!(problem["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn viewer_may_not_create() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/overlays/clk", VIEWER_TOKEN, &overlay_body("Clock")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_requires_admin_role() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/overlays/clk", ADMIN_TOKEN, &overlay_body("Clock")),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/overlays/clk")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/overlays/clk")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(&h.router, get("/api/v1/overlays/clk", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
