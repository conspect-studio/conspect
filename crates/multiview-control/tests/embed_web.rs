//! The embedded web UI (feature `embed-web`) is served as the router fallback —
//! the SPA loads at `/` and on deep links, without shadowing the API surface.
//!
//! Compiled only with `--features embed-web` (the built `web/dist` is inlined).
#![cfg(feature = "embed-web")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use support::{body_bytes, harness, send, ADMIN_TOKEN};

fn raw_get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn spa_index_is_served_at_root() {
    let h = harness();
    let response = send(&h.router, raw_get("/")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/html"),
        "root should serve HTML, got {content_type:?}"
    );
    let body = String::from_utf8_lossy(&body_bytes(response).await).into_owned();
    assert!(
        body.to_ascii_lowercase().contains("<!doctype html")
            || body.to_ascii_lowercase().contains("<html"),
        "root should serve the SPA index document"
    );
}

#[tokio::test]
async fn spa_deep_link_serves_index_for_client_routing() {
    // A client-side route (no such asset) must return index.html (200), not 404,
    // so the BrowserRouter can own deep links / page reloads.
    let h = harness();
    let response = send(&h.router, raw_get("/layouts/new")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&body_bytes(response).await).into_owned();
    assert!(
        body.to_ascii_lowercase().contains("<!doctype html")
            || body.to_ascii_lowercase().contains("<html"),
        "a deep link should serve the SPA index for client routing"
    );
}

#[tokio::test]
async fn spa_fallback_does_not_shadow_the_api() {
    // The API is reachable (here: authenticated, returns a JSON list), proving the
    // SPA fallback runs only for unmatched routes — it never swallows /api/v1.
    let h = harness();
    let response = send(
        &h.router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/layouts")
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the API must still answer under the SPA fallback"
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("application/json"),
        "the API route returns JSON, not the SPA HTML; got {content_type:?}"
    );
}

#[tokio::test]
async fn spa_does_not_shadow_the_openapi_docs() {
    let h = harness();
    let response = send(&h.router, raw_get("/api/v1/openapi.json")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&body_bytes(response).await).into_owned();
    assert!(
        body.contains("openapi"),
        "the OpenAPI document must still be served, not shadowed by the SPA"
    );
}
