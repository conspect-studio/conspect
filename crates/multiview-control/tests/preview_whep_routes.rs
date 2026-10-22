//! WHEP focus route tests (PRV-2): auth-gating, the offer→answer happy path
//! against an in-memory fake transport, and idempotent release.
//!
//! The control plane stays codec-free: it delegates negotiation/teardown to the
//! [`WhepProvider`] seam (a trait object), so these tests prove the *route layer*
//! — auth, body handling, status/Location/`application/sdp` semantics, and RFC
//! 9457 problem bodies — against a fake transport, never a real str0m engine
//! (that is PRV-1b). Re-asserts invariant #10: a focus session is best-effort
//! preview, strictly isolated, and never back-pressures the engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, InMemoryRepository, Principal, Role, WhepAnswer,
    WhepProvider, WhepReject, WhepScope,
};
use multiview_engine::EnginePublisher;

mod support;
use support::{
    body_bytes, body_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, OUTPUT_SCOPED_TOKEN, PEPPER,
    SCOPED_TOKEN, VIEWER_TOKEN,
};

/// A minimal, well-formed SDP offer advertising H.264 on the video m-line.
const H264_OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

/// An in-memory fake [`WhepProvider`]: it records every negotiated session,
/// returns a deterministic non-placeholder answer, and lets a test drive a
/// refusal (cap/unknown/malformed). It never touches an engine — it is the
/// invariant-#10 stand-in for the str0m transport (PRV-1b).
#[derive(Default)]
struct FakeWhep {
    /// Live sessions as `(scope_label, session_id)`.
    sessions: Mutex<Vec<(String, String)>>,
    /// Monotonic session-id source.
    next: AtomicUsize,
    /// When set, every negotiation is refused with this reason instead.
    refuse: Mutex<Option<WhepReject>>,
}

impl FakeWhep {
    fn refuse_with(reason: WhepReject) -> Self {
        Self {
            refuse: Mutex::new(Some(reason)),
            ..Self::default()
        }
    }
}

impl WhepProvider for FakeWhep {
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject> {
        if let Some(reason) = self.refuse.lock().unwrap().clone() {
            return Err(reason);
        }
        // Reject a body that is not even SDP, mirroring a real transport.
        if !offer.contains("v=0") {
            return Err(WhepReject::Malformed("no SDP version line".to_owned()));
        }
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let session_id = format!("sess-{n}");
        self.sessions
            .lock()
            .unwrap()
            .push((scope.label(), session_id.clone()));
        Ok(WhepAnswer {
            session_id,
            // A deterministic non-placeholder answer (real ICE ufrag/pwd come
            // from the transport in PRV-1b; the route only needs an SDP body).
            sdp: "v=0\r\no=multiview-preview 0 0 IN IP4 0.0.0.0\r\ns=multiview-preview\r\n\
t=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=ice-ufrag:abcd\r\n\
a=rtpmap:96 H264/90000\r\na=sendonly\r\n"
                .to_owned(),
        })
    }

    fn release(&self, scope: &WhepScope, session_id: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let label = scope.label();
        let before = sessions.len();
        sessions.retain(|(l, s)| !(l == &label && s == session_id));
        sessions.len() != before
    }

    fn active_sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// Build an `AppState` + router with the four seeded keys and the given fake
/// WHEP provider, so the route layer can be exercised end-to-end.
fn router_with_whep(whep: Arc<FakeWhep>) -> axum::Router {
    let engine = Arc::new(EnginePublisher::new(64));
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
    // Scoped operators for the per-scope BOLA tests (SEC-06/07): object-scoped to
    // `scoped-layout`, output-scoped to `wall-1` — matching `support::seeded_keys`
    // so `SCOPED_TOKEN` / `OUTPUT_SCOPED_TOKEN` authenticate.
    keys.register(
        "scoped-key",
        "scoped-secret-jkl",
        Principal {
            key_id: "scoped-key".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(vec!["scoped-layout".to_owned()]),
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        },
    );
    keys.register(
        "out-scoped-key",
        "out-scoped-secret-mno",
        Principal {
            key_id: "out-scoped-key".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: Some(vec!["wall-1".to_owned()]),
            scoped_discovery_domains: None,
        },
    );
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    )
    .with_whep(whep);
    multiview_control::router(state)
}

/// Build a `POST …/whep` request carrying an SDP offer body and a Bearer token.
fn post_sdp(path: &str, token: &str, offer: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(Body::from(offer.to_owned()))
        .expect("request builds")
}

/// Build a bodyless `DELETE` request with a Bearer token.
fn delete(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn missing_token_is_unauthorized() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/preview/program/whep")
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(Body::from(H264_OFFER))
        .unwrap();
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn view_token_is_forbidden() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let resp = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", VIEWER_TOKEN, H264_OFFER),
    )
    .await;
    // A View token can never open a focus session (Focus is a write action).
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
}

#[tokio::test]
async fn focus_offer_yields_created_answer_for_all_scopes() {
    for path in [
        "/api/v1/preview/program/whep",
        "/api/v1/preview/inputs/cam-1/whep",
        "/api/v1/preview/outputs/wall-1/whep",
    ] {
        let whep = Arc::new(FakeWhep::default());
        let router = router_with_whep(Arc::clone(&whep));
        let resp = send(&router, post_sdp(path, OPERATOR_TOKEN, H264_OFFER)).await;
        assert_eq!(resp.status(), StatusCode::CREATED, "201 for {path}");
        // The answer body is SDP and a Location points at the WHEP resource URL.
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(
            ct.starts_with("application/sdp"),
            "answer is SDP for {path}"
        );
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .expect("a Location header points at the session resource");
        assert!(
            location.starts_with(path) && location.len() > path.len(),
            "Location {location} extends {path} with a session id"
        );
        let body = String::from_utf8(body_bytes(resp).await).unwrap();
        assert!(
            body.contains("v=0"),
            "answer carries an SDP body for {path}"
        );
        assert_eq!(whep.active_sessions(), 1, "one live session for {path}");
    }
}

#[tokio::test]
async fn malformed_offer_is_bad_request() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let resp = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, "not sdp"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 400);
}

#[tokio::test]
async fn capacity_exceeded_is_service_unavailable_with_fallback() {
    let whep = Arc::new(FakeWhep::refuse_with(WhepReject::CapacityExceeded {
        fallback: "jpeg".to_owned(),
    }));
    let router = router_with_whep(whep);
    let resp = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, H264_OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_json(resp).await;
    // The fallback transport hint is surfaced so the UI degrades honestly.
    assert_eq!(problem["fallback"], "jpeg");
}

#[tokio::test]
async fn release_frees_the_session() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // Open a focus, then DELETE the Location the 201 handed back.
    let created = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, H264_OFFER),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    assert_eq!(whep.active_sessions(), 1);

    let released = send(&router, delete(&location, OPERATOR_TOKEN)).await;
    assert_eq!(released.status(), StatusCode::NO_CONTENT);
    assert_eq!(whep.active_sessions(), 0, "the session is freed");

    // Deleting an unknown session id is a 404 (nothing to free).
    let unknown = send(
        &router,
        delete(
            "/api/v1/preview/program/whep/does-not-exist",
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn release_requires_write_role() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let created = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", ADMIN_TOKEN, H264_OFFER),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    // A View token may not release a focus session.
    let resp = send(&router, delete(&location, VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(whep.active_sessions(), 1, "the session is untouched");
}

// ---- SEC-06/07 (BOLA, ADR-W005/W026): per-scope authorization on focus ----
//
// A focus session exposes the live pixels of exactly one entity, so opening OR
// closing one must be authorized on that entity's axis — enforced at the route
// layer because the `WhepProvider` seam is codec-only and never sees a
// `Principal`. `Input`→object scope, `Output`→output scope, `Program`→unrestricted
// (the canvas embeds every object/output). Each denial is a 403 problem+json with
// ZERO provider side effect (`active_sessions` unchanged).

#[tokio::test]
async fn input_focus_is_denied_for_an_out_of_scope_object() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // scoped-key is object-scoped to "scoped-layout"; cam-1 is out of scope.
    let resp = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/cam-1/whep",
            SCOPED_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
    assert_eq!(
        whep.active_sessions(),
        0,
        "no focus is negotiated when authorization is denied"
    );
}

#[tokio::test]
async fn input_focus_is_allowed_for_the_in_scope_object() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let resp = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/scoped-layout/whep",
            SCOPED_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(whep.active_sessions(), 1, "the in-scope input focus opens");
}

#[tokio::test]
async fn output_focus_is_denied_for_an_out_of_scope_output() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // out-scoped-key is output-scoped to "wall-1"; wall-2 is out of scope.
    let resp = send(
        &router,
        post_sdp(
            "/api/v1/preview/outputs/wall-2/whep",
            OUTPUT_SCOPED_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
    assert_eq!(whep.active_sessions(), 0);
}

#[tokio::test]
async fn output_focus_is_allowed_for_the_in_scope_output() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let resp = send(
        &router,
        post_sdp(
            "/api/v1/preview/outputs/wall-1/whep",
            OUTPUT_SCOPED_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(whep.active_sessions(), 1, "the in-scope output focus opens");
}

#[tokio::test]
async fn program_focus_is_denied_for_a_scoped_principal() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // The program canvas embeds every object/output, so only an UNRESTRICTED
    // principal may focus it. An object-scoped principal is denied...
    let resp = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", SCOPED_TOKEN, H264_OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
    assert_eq!(whep.active_sessions(), 0);
    // ...and so is an output-scoped principal (the whole-system gate is all-axes).
    let resp2 = send(
        &router,
        post_sdp(
            "/api/v1/preview/program/whep",
            OUTPUT_SCOPED_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(resp2.status(), StatusCode::FORBIDDEN);
    assert_eq!(whep.active_sessions(), 0);
}

#[tokio::test]
async fn program_focus_is_allowed_for_an_unscoped_operator() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let resp = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, H264_OFFER),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "an unscoped operator may focus the whole program canvas"
    );
}

#[tokio::test]
async fn release_is_denied_for_an_out_of_scope_input_session() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // Admin (unscoped) opens a focus on cam-1.
    let created = send(
        &router,
        post_sdp("/api/v1/preview/inputs/cam-1/whep", ADMIN_TOKEN, H264_OFFER),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    assert_eq!(whep.active_sessions(), 1);
    // The object-scoped principal (scoped-layout) may not tear down cam-1's focus.
    let resp = send(&router, delete(&location, SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
    assert_eq!(
        whep.active_sessions(),
        1,
        "the out-of-scope session is untouched on a denied release"
    );
}

#[tokio::test]
async fn release_is_allowed_for_the_in_scope_input_session() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // Admin opens a focus on scoped-layout; the scoped principal may release it.
    let created = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/scoped-layout/whep",
            ADMIN_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    let resp = send(&router, delete(&location, SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        whep.active_sessions(),
        0,
        "the in-scope session is freed by its authorized principal"
    );
}
