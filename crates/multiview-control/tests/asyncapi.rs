//! Tests that the `AsyncAPI` 3.0 document is served at `/asyncapi.json` and that
//! the channels + messages the realtime brief requires are present.
//!
//! The route mirrors `openapi_router()` (ADR-W002): the generated document lives
//! at `docs/api/asyncapi.json` (produced by `cargo xtask gen-asyncapi`) and is
//! embedded at compile time so the binary is self-contained.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use support::{body_json, harness, send};

/// The document is served at the root-level `/asyncapi.json` (no `/api/v1`
/// prefix — `AsyncAPI` lives alongside, not inside, the REST namespace).
#[tokio::test]
async fn asyncapi_json_is_served_with_200() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/asyncapi.json")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /asyncapi.json must return 200"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.starts_with("application/json"),
        "content-type must be application/json, got {ct}"
    );
}

/// The document declares `AsyncAPI` 3.0 and contains the two canonical channels
/// (`ws` and `sse`) plus `components.messages` including `Envelope`.
#[tokio::test]
async fn asyncapi_json_contains_channels_and_messages() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/asyncapi.json")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let doc = body_json(resp).await;

    // Verify the document is AsyncAPI 3.0.
    let version = doc["asyncapi"].as_str().expect("asyncapi version present");
    assert!(
        version.starts_with("3.0"),
        "expected AsyncAPI 3.0, got {version}"
    );

    // Both transport channels are declared.
    let channels = &doc["channels"];
    assert!(
        channels.get("ws").is_some(),
        "channels.ws must be present (WebSocket primary)"
    );
    assert!(
        channels.get("sse").is_some(),
        "channels.sse must be present (SSE fallback)"
    );

    // The `ws` channel must carry an address and at least one message reference.
    let ws = &channels["ws"];
    assert_eq!(
        ws["address"].as_str().unwrap_or_default(),
        "/api/v1/ws",
        "ws channel address must be /api/v1/ws"
    );

    // Reusable messages live under components.messages (AsyncAPI 3.0 — a
    // top-level `messages` block is invalid and was relocated here).
    let messages = &doc["components"]["messages"];
    assert!(
        messages.get("Envelope").is_some(),
        "components.messages.Envelope must be present"
    );
    // The tile-state event message must be documented (key realtime topic).
    assert!(
        messages.get("TileState").is_some(),
        "components.messages.TileState must be present"
    );
}

/// The document lists `/asyncapi.json` in the `ApiDoc::rest_routes` surface so
/// the contract is discoverable without a live server.
#[test]
fn asyncapi_json_is_declared_in_rest_routes() {
    use multiview_control::openapi::ApiDoc;

    let routes: Vec<(&str, &str)> = ApiDoc::rest_routes()
        .iter()
        .map(|(m, p)| (*m, *p))
        .collect();
    assert!(
        routes.contains(&("GET", "/asyncapi.json")),
        "ApiDoc::rest_routes must include GET /asyncapi.json; got: {routes:?}"
    );
}
