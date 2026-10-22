//! `GET /api/v1/preview/capabilities` (ADR-P006 move 6): the SPA reads this to
//! pick its transport (WHEP → JPEG ladder, ADR-W020) **before** issuing an offer,
//! so it never discovers WHEP absence by failing a POST.
//!
//! The shape (ADR-P006):
//! ```json
//! { "webrtc": bool,
//!   "scopes": {
//!     "program": { "whep": bool, "fidelity": "real-encoded-output" | "pre-encode-canvas-approx" },
//!     "inputs":  { "whep": bool },
//!     "outputs": { "whep": bool } },
//!   "fallback": "jpeg" }
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, GatedWhep, InMemoryRepository, Principal, Role, WhepAnswer,
    WhepProvider, WhepReject, WhepScope,
};
use multiview_engine::EnginePublisher;

mod support;
use support::{body_json, send, PEPPER, VIEWER_TOKEN};

/// A WHEP provider that reports WebRTC AVAILABLE on all scopes (the live-build
/// shape). Negotiation itself is irrelevant here — only `capabilities` matters.
#[derive(Default)]
struct AvailableWhep;

impl WhepProvider for AvailableWhep {
    fn negotiate(&self, _scope: &WhepScope, _offer: &str) -> Result<WhepAnswer, WhepReject> {
        Err(WhepReject::CapacityExceeded {
            fallback: "jpeg".to_owned(),
        })
    }
    fn release(&self, _scope: &WhepScope, _session_id: &str) -> bool {
        false
    }
    fn active_sessions(&self) -> usize {
        0
    }
    fn webrtc_available(&self) -> bool {
        true
    }
}

fn router(whep: Option<std::sync::Arc<dyn WhepProvider>>) -> axum::Router {
    let engine = std::sync::Arc::new(EnginePublisher::new(64));
    let (tx, _rx) = command_bus(4);
    let mut keys = ApiKeyStore::new(PEPPER.to_vec());
    for (id, secret, role) in [
        ("admin-key", "admin-secret-abc", Role::Admin),
        ("operator-key", "operator-secret-def", Role::Operator),
        ("viewer-key", "viewer-secret-ghi", Role::Viewer),
    ] {
        keys.register(
            id,
            secret,
            Principal {
                key_id: id.to_owned(),
                role,
                scoped_object_ids: None,
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
        );
    }
    let mut state = AppState::new(
        engine,
        tx,
        std::sync::Arc::new(InMemoryRepository::new()),
        std::sync::Arc::new(keys),
    );
    if let Some(w) = whep {
        state = state.with_whep(w);
    }
    multiview_control::router(state)
}

fn get(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn capabilities_reports_no_webrtc_and_jpeg_fallback_on_a_pure_build() {
    // The default (no WHEP transport wired / pure build): webrtc=false on every
    // scope, fallback "jpeg" — the SPA stays on the JPEG ladder.
    let router = router(None);
    let resp = send(&router, get("/api/v1/preview/capabilities", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let caps = body_json(resp).await;
    assert_eq!(caps["webrtc"], false);
    assert_eq!(caps["fallback"], "jpeg");
    assert_eq!(caps["scopes"]["program"]["whep"], false);
    assert_eq!(caps["scopes"]["inputs"]["whep"], false);
    assert_eq!(caps["scopes"]["outputs"]["whep"], false);
    // The program scope always carries a fidelity label (ADR-P005/P006).
    assert!(caps["scopes"]["program"]["fidelity"].is_string());
}

#[tokio::test]
async fn capabilities_reports_webrtc_when_a_native_transport_is_wired() {
    let whep: std::sync::Arc<dyn WhepProvider> =
        std::sync::Arc::new(GatedWhep::with_defaults(std::sync::Arc::new(AvailableWhep)));
    let router = router(Some(whep));
    let resp = send(&router, get("/api/v1/preview/capabilities", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let caps = body_json(resp).await;
    assert_eq!(caps["webrtc"], true);
    assert_eq!(caps["scopes"]["program"]["whep"], true);
    assert_eq!(caps["scopes"]["inputs"]["whep"], true);
    assert_eq!(caps["scopes"]["outputs"]["whep"], true);
    assert_eq!(caps["fallback"], "jpeg");
}

#[tokio::test]
async fn capabilities_requires_authentication() {
    let router = router(None);
    let resp = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/preview/capabilities")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
