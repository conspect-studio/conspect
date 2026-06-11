//! End-to-end tests for the layouts resource: CRUD, `ETag` round-trip, and
//! `If-Match` optimistic concurrency (`412`) — driven through the real router.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{body_json, get, harness, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN};

fn layout_body(name: &str) -> serde_json::Value {
    json!({ "name": name, "body": { "canvas": { "w": 1920, "h": 1080 }, "cells": [] } })
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    // Create.
    let resp = send(
        &h.router,
        post_json("/api/v1/layouts/main", OPERATOR_TOKEN, &layout_body("Main")),
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
    assert_eq!(created["id"], "main");
    assert_eq!(created["name"], "Main");

    // Get returns the same body and ETag.
    let resp = send(&h.router, get("/api/v1/layouts/main", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let fetched = body_json(resp).await;
    assert_eq!(fetched["name"], "Main");
}

#[tokio::test]
async fn get_unknown_layout_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/layouts/missing", OPERATOR_TOKEN)).await;
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
        post_json("/api/v1/layouts/main", OPERATOR_TOKEN, &layout_body("Main")),
    )
    .await;

    // Update with the correct If-Match (version 1).
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/layouts/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &layout_body("Renamed"),
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
        post_json("/api/v1/layouts/main", OPERATOR_TOKEN, &layout_body("Main")),
    )
    .await;
    // First update -> now version 2.
    send(
        &h.router,
        put_json(
            "/api/v1/layouts/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &layout_body("V2"),
        ),
    )
    .await;

    // A second operator still holding version 1 must be rejected.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/layouts/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &layout_body("Clobber"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");

    // The stale write did NOT take effect.
    let resp = send(&h.router, get("/api/v1/layouts/main", OPERATOR_TOKEN)).await;
    let current = body_json(resp).await;
    assert_eq!(current["name"], "V2", "the clobbering write was rejected");
}

#[tokio::test]
async fn update_without_if_match_is_precondition_required() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/layouts/main", OPERATOR_TOKEN, &layout_body("Main")),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/layouts/main",
            OPERATOR_TOKEN,
            None,
            &layout_body("X"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn list_returns_created_layouts_sorted() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/layouts/bbb", OPERATOR_TOKEN, &layout_body("B")),
    )
    .await;
    send(
        &h.router,
        post_json("/api/v1/layouts/aaa", OPERATOR_TOKEN, &layout_body("A")),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/layouts", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let arr = list.as_array().expect("list is an array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "aaa", "id-sorted order");
    assert_eq!(arr[1]["id"], "bbb");
}

#[tokio::test]
async fn delete_requires_admin_role() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/layouts/main", ADMIN_TOKEN, &layout_body("Main")),
    )
    .await;
    // Operator may not delete (Administer action).
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/layouts/main")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Admin may delete.
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/layouts/main")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // It is gone.
    let resp = send(&h.router, get("/api/v1/layouts/main", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
