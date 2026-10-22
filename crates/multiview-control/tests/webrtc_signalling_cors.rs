//! CORS on the WebRTC media-signalling routes (ADR-0048 §9 / ADR-T014 §2).
//!
//! `webrtc.cors_allow_origins` (default `"*"`) applies **only** to the
//! media-signalling routes — WHIP ingest (`/api/v1/whip/{source}`), WHEP-serve
//! output (`/api/v1/whep/{output}`), the preview-WHEP focus routes
//! (`/api/v1/preview/.../whep`), and preview capabilities — so a real browser
//! served from a web origin can publish (WHIP) and play (WHEP) cross-origin.
//!
//! The contract (ADR-0048 §9): a cross-origin request carrying an allowed
//! `Origin` gets `Access-Control-Allow-Origin` reflected, `Vary: Origin`,
//! `Access-Control-Expose-Headers: location, link` on the actual response, and a
//! preflight `OPTIONS` is answered `204` with `Access-Control-Allow-Methods` +
//! `Access-Control-Allow-Headers: authorization, content-type` — **without**
//! authentication (preflight is unauthenticated by browser construction). A
//! request with **no** `Origin` (a non-browser publisher/player) gets no CORS
//! headers. This proves the live browser-play defect (CORS headers never
//! emitted) cannot regress.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::{command_bus, ApiKeyStore, AppState, InMemoryRepository};
use multiview_engine::EnginePublisher;

mod support;
use support::send;

/// A wildcard-CORS router (the default `webrtc.cors_allow_origins = ["*"]`).
fn router_default_cors() -> axum::Router {
    let engine = std::sync::Arc::new(EnginePublisher::new(64));
    let (tx, _rx) = command_bus(4);
    let keys = ApiKeyStore::new(b"pepper-for-cors-tests".to_vec());
    let state = AppState::new(
        engine,
        tx,
        std::sync::Arc::new(InMemoryRepository::new()),
        std::sync::Arc::new(keys),
    );
    multiview_control::router(state)
}

/// A router whose CORS allow-list is a single concrete origin.
fn router_with_origins(origins: Vec<String>) -> axum::Router {
    let engine = std::sync::Arc::new(EnginePublisher::new(64));
    let (tx, _rx) = command_bus(4);
    let keys = ApiKeyStore::new(b"pepper-for-cors-tests".to_vec());
    let state = AppState::new(
        engine,
        tx,
        std::sync::Arc::new(InMemoryRepository::new()),
        std::sync::Arc::new(keys),
    )
    .with_cors_allow_origins(origins);
    multiview_control::router(state)
}

/// The browser-origin every preflight is sent from.
const ORIGIN: &str = "https://viewer.example.org";

fn options(path: &str) -> Request<Body> {
    Request::builder()
        .method("OPTIONS")
        .uri(path)
        .header(header::ORIGIN, ORIGIN)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .body(Body::empty())
        .expect("request builds")
}

/// Every media-signalling route answers a wildcard preflight `204` with the
/// reflected origin + the ADR-0048 §9 allow set — unauthenticated.
#[tokio::test]
async fn preflight_on_every_media_signalling_route_reflects_origin() {
    let router = router_default_cors();
    for path in [
        "/api/v1/whip/cam-1",
        "/api/v1/whep/pgm",
        "/api/v1/preview/program/whep",
        "/api/v1/preview/inputs/cam-1/whep",
        "/api/v1/preview/outputs/pgm/whep",
        "/api/v1/preview/capabilities",
    ] {
        let resp = send(&router, options(path)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NO_CONTENT,
            "{path}: preflight is 204 (unauthenticated)"
        );
        let h = resp.headers();
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some(ORIGIN),
            "{path}: reflects the allowed origin"
        );
        let methods = h
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            methods.contains("POST"),
            "{path}: allow-methods includes POST (got {methods:?})"
        );
        let allow_headers = h
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            allow_headers.contains("authorization") && allow_headers.contains("content-type"),
            "{path}: allow-headers carries authorization + content-type (got {allow_headers:?})"
        );
        assert_eq!(
            h.get(header::VARY).and_then(|v| v.to_str().ok()),
            Some("Origin"),
            "{path}: Vary: Origin"
        );
    }
}

/// The actual WHEP-serve response carries the reflected origin + the
/// `location, link` expose set (so the browser can read the WHEP `Location`).
#[tokio::test]
async fn whep_serve_response_exposes_location_and_reflects_origin() {
    let router = router_default_cors();
    // No provider wired ⇒ the default refuses 503, but the CORS headers must be
    // present on the response regardless (the layer wraps the route).
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/whep/pgm")
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(Body::from("v=0\r\n"))
        .expect("request builds");
    let resp = send(&router, req).await;
    let h = resp.headers();
    assert_eq!(
        h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN),
        "actual response reflects the origin"
    );
    let expose = h
        .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        expose.contains("location") && expose.contains("link"),
        "expose-headers carries location + link (got {expose:?})"
    );
}

/// A request with **no** `Origin` (a non-browser publisher/player) gets no
/// `Access-Control-Allow-Origin` — CORS headers are browser-only.
#[tokio::test]
async fn no_origin_request_gets_no_cors_headers() {
    let router = router_default_cors();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/whep/pgm")
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(Body::from("v=0\r\n"))
        .expect("request builds");
    let resp = send(&router, req).await;
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "a no-Origin request gets no allow-origin header"
    );
}

/// A concrete allow-list reflects a listed origin and refuses an unlisted one
/// (no `Access-Control-Allow-Origin` ⇒ the browser blocks it).
#[tokio::test]
async fn concrete_allow_list_gates_by_origin() {
    let router = router_with_origins(vec![ORIGIN.to_owned()]);

    let allowed = send(&router, options("/api/v1/whip/cam-1")).await;
    assert_eq!(
        allowed
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN),
        "a listed origin is reflected"
    );

    let denied_req = Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/whip/cam-1")
        .header(header::ORIGIN, "https://evil.example.com")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .expect("request builds");
    let denied = send(&router, denied_req).await;
    assert!(
        denied
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "an unlisted origin is not reflected (browser blocks)"
    );
}

/// CORS is scoped to the media-signalling surface: a non-signalling route (e.g.
/// the resource CRUD) does **not** get the WebRTC CORS allow-origin header, so
/// the change is contained to the documented routes (ADR-0048 §9).
#[tokio::test]
async fn non_signalling_routes_are_not_cors_wrapped() {
    let router = router_default_cors();
    let resp = send(&router, options("/api/v1/sources")).await;
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "non-signalling routes are outside the media-CORS scope"
    );
}
