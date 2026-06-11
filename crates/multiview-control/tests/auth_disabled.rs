//! The opt-in **auth-disable** mode and the unauthenticated
//! `GET /api/v1/auth/status` discovery endpoint (the SPA reads it to decide
//! whether to prompt for an API key).
//!
//! Secure default: auth is REQUIRED — a protected route without a token is 401.
//! With auth disabled, every request runs as a local admin (200, no token).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_control::{command_bus, AppState, EngineStateSnapshot, InMemoryRepository};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_json, get, seeded_keys, send, ADMIN_TOKEN};

/// A router whose auth is enabled (`disabled = false`, the secure default) or
/// disabled (`true`). Minimal state — only the auth path is exercised here.
fn router_with_auth(disabled: bool) -> axum::Router {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (tx, _rx) = command_bus(4);
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(seeded_keys()),
    )
    .with_auth_disabled(disabled);
    multiview_control::router(state)
}

/// A GET request with **no** `Authorization` header.
fn no_token_get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn auth_on_by_default_rejects_unauthenticated_and_reports_status() {
    let router = router_with_auth(false);

    // A protected route without a token is refused (401) — secure default.
    let resp = send(&router, no_token_get("/api/v1/layouts")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The status endpoint is unauthenticated and reports "auth required, not
    // authenticated" so the SPA knows to show a login gate.
    let resp = send(&router, no_token_get("/api/v1/auth/status")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["auth_required"], serde_json::json!(true));
    assert_eq!(body["authenticated"], serde_json::json!(false));

    // Presenting a valid token reports authenticated — the SPA validates an
    // entered key this way before storing it.
    let resp = send(&router, get("/api/v1/auth/status", ADMIN_TOKEN)).await;
    let body = body_json(resp).await;
    assert_eq!(body["auth_required"], serde_json::json!(true));
    assert_eq!(body["authenticated"], serde_json::json!(true));
}

#[tokio::test]
async fn auth_disabled_opens_the_api_without_a_token() {
    let router = router_with_auth(true);

    // With auth disabled, a protected route succeeds with NO token (local admin).
    let resp = send(&router, no_token_get("/api/v1/layouts")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The status endpoint reports "auth not required, authenticated".
    let resp = send(&router, no_token_get("/api/v1/auth/status")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["auth_required"], serde_json::json!(false));
    assert_eq!(body["authenticated"], serde_json::json!(true));
}
