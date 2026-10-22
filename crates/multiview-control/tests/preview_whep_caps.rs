//! WHEP concurrent-focus cap tests (PRV-3): the `FocusGate`, wired into the
//! negotiate path via [`GatedWhep`], bounds concurrent focus sessions and sheds
//! the overflow to the existing `503 fallback: jpeg` shape.
//!
//! These drive the real route layer (axum `oneshot`) over an in-memory fake
//! transport, never a real str0m engine (PRV-1b). They re-assert invariant #10:
//! a focus that cannot be admitted is **rejected**, never queued, and the gate
//! holds only its own counters — it never touches the engine command bus.
//!
//! NOTE (honest scope): live media is PRV-1b. The fake transport mints a
//! deterministic SDP answer; the bytes are not a real WebRTC session. What is
//! proven here is the *admission* contract: caps, the 503 fallback shape,
//! per-scope independence, and slot release on DELETE.
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
    command_bus, ApiKeyStore, AppState, FocusCaps, GatedWhep, InMemoryRepository, Principal, Role,
    WhepAnswer, WhepProvider, WhepReject, WhepScope,
};
use multiview_engine::EnginePublisher;

mod support;
use support::{body_json, send, OPERATOR_TOKEN, PEPPER};

/// A minimal, well-formed SDP offer advertising H.264 on the video m-line.
const H264_OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

/// An in-memory fake [`WhepProvider`]: it accepts every well-formed offer with a
/// deterministic answer and refcounts live sessions. It has NO cap of its own —
/// the cap is enforced entirely by the [`GatedWhep`] wrapper under test, so this
/// fake proves the gate is what bounds concurrency (not the transport).
#[derive(Default)]
struct UncappedWhep {
    sessions: Mutex<Vec<(String, String)>>,
    next: AtomicUsize,
}

impl WhepProvider for UncappedWhep {
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject> {
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

/// Build a router whose WHEP seam is `inner` wrapped by a [`GatedWhep`] with the
/// given caps and the `jpeg` fallback hint.
fn router_with_caps(inner: Arc<UncappedWhep>, caps: FocusCaps) -> axum::Router {
    let gated = Arc::new(GatedWhep::new(inner, caps, "jpeg"));
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
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    )
    .with_whep(gated);
    multiview_control::router(state)
}

fn post_sdp(path: &str, token: &str, offer: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(Body::from(offer.to_owned()))
        .expect("request builds")
}

fn delete(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds")
}

/// Open a focus and return its `Location` (the per-session WHEP resource URL).
async fn open_focus(router: &axum::Router, path: &str) -> String {
    let resp = send(router, post_sdp(path, OPERATOR_TOKEN, H264_OFFER)).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "focus opened for {path}"
    );
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location at the session resource")
}

#[tokio::test]
async fn focuses_up_to_the_global_cap_succeed_then_overflow_is_503_with_fallback() {
    // Global cap of 3, generous per-scope cap so the GLOBAL cap is the binding
    // one across distinct scopes.
    let router = router_with_caps(Arc::new(UncappedWhep::default()), FocusCaps::new(3, 3));
    // Three DIFFERENT scopes, all admitted up to the global cap.
    for path in [
        "/api/v1/preview/program/whep",
        "/api/v1/preview/inputs/cam-1/whep",
        "/api/v1/preview/outputs/wall-1/whep",
    ] {
        let resp = send(&router, post_sdp(path, OPERATOR_TOKEN, H264_OFFER)).await;
        assert_eq!(resp.status(), StatusCode::CREATED, "admitted: {path}");
    }
    // The 4th focus exceeds the global cap → 503 with the honest fallback hint,
    // NOT queued (invariant #10): the existing `503 fallback: jpeg` shape.
    let over = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/cam-2/whep",
            OPERATOR_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_json(over).await;
    assert_eq!(problem["status"], 503);
    assert_eq!(problem["fallback"], "jpeg");
}

#[tokio::test]
async fn releasing_a_focus_frees_a_slot() {
    let router = router_with_caps(Arc::new(UncappedWhep::default()), FocusCaps::new(1, 1));
    // The one slot is taken.
    let location = open_focus(&router, "/api/v1/preview/program/whep").await;
    // A second focus (any scope) is shed: at capacity.
    let blocked = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/cam-1/whep",
            OPERATOR_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Release the first focus → the slot is freed.
    let released = send(&router, delete(&location, OPERATOR_TOKEN)).await;
    assert_eq!(released.status(), StatusCode::NO_CONTENT);

    // Now a fresh focus is admitted again.
    let resp = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/cam-1/whep",
            OPERATOR_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "slot freed, re-admitted"
    );
}

#[tokio::test]
async fn per_scope_caps_are_independent() {
    // Global is generous (4); per-scope cap is 1, so each scope bounds itself but
    // a different scope is unaffected.
    let router = router_with_caps(Arc::new(UncappedWhep::default()), FocusCaps::new(4, 1));
    // First program focus: admitted.
    let _ = open_focus(&router, "/api/v1/preview/program/whep").await;
    // A SECOND program focus hits the per-scope cap of 1 → shed.
    let second_program = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, H264_OFFER),
    )
    .await;
    assert_eq!(
        second_program.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the program scope is at its per-scope cap"
    );
    let problem = body_json(second_program).await;
    assert_eq!(problem["fallback"], "jpeg");
    // A DIFFERENT scope is independent: still admitted.
    let resp = send(
        &router,
        post_sdp(
            "/api/v1/preview/inputs/cam-1/whep",
            OPERATOR_TOKEN,
            H264_OFFER,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "a different scope is independent of the program cap"
    );
}

#[tokio::test]
async fn an_inner_refusal_does_not_consume_a_slot() {
    // The gate admits, but the inner transport refuses the (admitted) offer:
    // the slot must be returned so a later valid focus is still admitted. With a
    // global cap of 1, a leaked slot would wrongly block the next focus.
    let router = router_with_caps(Arc::new(UncappedWhep::default()), FocusCaps::new(1, 1));
    // A malformed offer is admitted by the gate but refused by the transport
    // (400). The slot must NOT be consumed.
    let bad = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, "not sdp"),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    // A subsequent valid focus is still admitted — the slot was freed.
    let ok = send(
        &router,
        post_sdp("/api/v1/preview/program/whep", OPERATOR_TOKEN, H264_OFFER),
    )
    .await;
    assert_eq!(
        ok.status(),
        StatusCode::CREATED,
        "the refused offer did not leak the slot"
    );
}
