//! WHEP-serve output route tests (ADR-0049 §5.1): the WHEP signalling surface —
//! `POST` offer→answer, `DELETE` teardown, `PATCH` 405, `OPTIONS` preflight, the
//! per-output-token / View-API-key auth model, the `503` capacity rule, the `404`
//! unknown-output, and the RFC 9457 problem bodies — against an in-memory fake
//! [`WhepOutputProvider`] (never a real str0m engine; that is the cli wiring).
//! Re-asserts invariant #10: a WHEP viewer is a real-output consumer that can
//! never back-pressure the engine, and the control plane stays native-free via
//! the provider seam.
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
    command_bus, ApiKeyStore, AppState, InMemoryRepository, Principal, Role, WhepOutputAnswer,
    WhepOutputAuth, WhepOutputProvider, WhepOutputReject,
};
use multiview_engine::EnginePublisher;

mod support;
use support::{body_json, send, OPERATOR_TOKEN, OUTPUT_SCOPED_TOKEN, PEPPER, VIEWER_TOKEN};

/// A minimal, well-formed WHEP recvonly SDP offer advertising H.264.
const OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP6 ::\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=recvonly\r\n";

/// The per-output bearer the fake provider accepts for `pgm`.
const OUTPUT_TOKEN: &str = "s3cret-pgm";

/// The bearer for a control-plane API key output-scoped to `pgm` — the in-scope
/// positive control for the SEC-08 API-key path.
const PGM_SCOPED_TOKEN: &str = "pgm-scoped-key.pgm-scoped-secret";

/// An in-memory fake [`WhepOutputProvider`]: accepts the per-output token or a
/// View principal, enforces a per-output `max_viewers`, and records sessions. It
/// never touches an engine — the invariant-#10 stand-in for the str0m endpoint.
#[derive(Default)]
struct FakeWhep {
    sessions: Mutex<Vec<(String, String)>>,
    next: AtomicUsize,
    /// The per-output viewer cap (0 ⇒ unlimited for the fake).
    max_viewers: usize,
}

impl FakeWhep {
    fn capped(max: usize) -> Self {
        Self {
            max_viewers: max,
            ..Self::default()
        }
    }
}

impl WhepOutputProvider for FakeWhep {
    fn negotiate(
        &self,
        output_id: &str,
        offer: &str,
        auth: &WhepOutputAuth,
    ) -> Result<WhepOutputAnswer, WhepOutputReject> {
        // Only `pgm` is a configured output.
        if output_id != "pgm" {
            return Err(WhepOutputReject::NotFound);
        }
        // Auth: a View+-scope API key OR the per-output bearer token.
        let token_ok = auth.bearer.as_deref() == Some(OUTPUT_TOKEN);
        if !auth.view_key && !token_ok {
            return if auth.bearer.is_none() {
                Err(WhepOutputReject::Unauthorized)
            } else {
                Err(WhepOutputReject::Forbidden)
            };
        }
        if !offer.contains("v=0") {
            return Err(WhepOutputReject::Malformed(
                "no SDP version line".to_owned(),
            ));
        }
        if self.max_viewers != 0 && self.sessions.lock().unwrap().len() >= self.max_viewers {
            return Err(WhepOutputReject::Unavailable);
        }
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let session_id = format!("whep-sess-{n}");
        self.sessions
            .lock()
            .unwrap()
            .push((output_id.to_owned(), session_id.clone()));
        Ok(WhepOutputAnswer {
            session_id,
            sdp: "v=0\r\no=- 0 0 IN IP6 ::\r\ns=-\r\nt=0 0\r\n\
a=group:BUNDLE 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=ice-ufrag:abcd\r\na=rtpmap:96 H264/90000\r\na=sendonly\r\na=setup:passive\r\n"
                .to_owned(),
        })
    }

    fn release(&self, output_id: &str, session_id: &str, _auth: &WhepOutputAuth) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|(o, id)| !(o == output_id && id == session_id));
        sessions.len() != before
    }

    fn active_sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

fn router_with_whep(whep: Arc<FakeWhep>) -> axum::Router {
    let engine = Arc::new(EnginePublisher::new(64));
    let (tx, _rx) = command_bus(4);
    let mut keys = ApiKeyStore::new(PEPPER.to_vec());
    for (id, secret, role) in [
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
    // Scoped keys for the SEC-08 API-key-path BOLA tests: one output-scoped to
    // "wall-1" (out of scope for the fake's "pgm" output) and one to "pgm" (in
    // scope). `OUTPUT_SCOPED_TOKEN` matches `support::seeded_keys`.
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
    keys.register(
        "pgm-scoped-key",
        "pgm-scoped-secret",
        Principal {
            key_id: "pgm-scoped-key".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: Some(vec!["pgm".to_owned()]),
            scoped_discovery_domains: None,
        },
    );
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    )
    .with_whep_output(whep);
    multiview_control::router(state)
}

fn post_sdp(path: &str, bearer: Option<&str>, offer: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/sdp");
    if let Some(token) = bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    b.body(Body::from(offer.to_owned()))
        .expect("request builds")
}

#[tokio::test]
async fn output_token_view_yields_created_with_location() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(ct.starts_with("application/sdp"), "answer is SDP");
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location points at the session resource");
    assert!(
        location.starts_with("/api/v1/whep/pgm/sessions/"),
        "Location is the session resource: {location}"
    );
    assert_eq!(whep.active_sessions(), 1);
}

#[tokio::test]
async fn view_api_key_with_view_scope_is_accepted() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // A Viewer (View/Read scope) API key suffices for read-shaped viewing.
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(VIEWER_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn operator_key_also_views() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OPERATOR_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn no_credentials_is_unauthorized() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let resp = send(&router, post_sdp("/api/v1/whep/pgm", None, OFFER)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key(header::WWW_AUTHENTICATE),
        "a 401 carries WWW-Authenticate: Bearer"
    );
}

#[tokio::test]
async fn unknown_output_is_not_found() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/nope", Some(OPERATOR_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn over_capacity_is_service_unavailable_with_retry_after() {
    let whep = Arc::new(FakeWhep::capped(1));
    let router = router_with_whep(Arc::clone(&whep));
    let first = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_TOKEN), OFFER),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let second = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_TOKEN), OFFER),
    )
    .await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        second.headers().contains_key(header::RETRY_AFTER),
        "a 503 carries Retry-After"
    );
}

#[tokio::test]
async fn wrong_content_type_is_unsupported_media_type() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/whep/pgm")
        .header(header::AUTHORIZATION, format!("Bearer {OUTPUT_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn patch_is_method_not_allowed_with_allow_header() {
    let router = router_with_whep(Arc::new(FakeWhep::default()));
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/v1/whep/pgm/sessions/whep-sess-0")
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        resp.headers()
            .get(header::ALLOW)
            .and_then(|v| v.to_str().ok()),
        Some("DELETE, OPTIONS")
    );
}

#[tokio::test]
async fn delete_releases_then_404s_unknown() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let created = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_TOKEN), OFFER),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap();
    let del = Request::builder()
        .method("DELETE")
        .uri(&location)
        .header(header::AUTHORIZATION, format!("Bearer {OUTPUT_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, del).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(whep.active_sessions(), 0);

    // A second DELETE of the (now-unknown) session is a 404.
    let del2 = Request::builder()
        .method("DELETE")
        .uri(&location)
        .header(header::AUTHORIZATION, format!("Bearer {OUTPUT_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp2 = send(&router, del2).await;
    assert_eq!(resp2.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn default_provider_refuses_503() {
    // No `with_whep_output`: the default `NoWhepOutput` answers 503 (routes
    // present + authz-enforced), never 404/panic.
    let engine = Arc::new(EnginePublisher::new(64));
    let (tx, _rx) = command_bus(4);
    let keys = ApiKeyStore::new(PEPPER.to_vec());
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    );
    let router = multiview_control::router(state);
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ---- SEC-08 (BOLA, ADR-W005/W026): the API-key path honors output scope ----
//
// A WHEP-serve output is an output (rendition) id. The alternative View-API-key
// path authorized by ROLE only, so a scoped key could view / tear down ANY
// output. These pin per-output authz on the API-key path (POST + DELETE) with
// zero provider side effect; the per-output *token* path is unchanged.

#[tokio::test]
async fn scoped_view_key_is_denied_viewing_an_out_of_scope_output() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // out-scoped-key is output-scoped to "wall-1"; pgm is out of scope.
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_SCOPED_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    assert_eq!(
        whep.active_sessions(),
        0,
        "no viewer session on a denied view"
    );
}

#[tokio::test]
async fn scoped_view_key_may_view_its_in_scope_output() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let resp = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(PGM_SCOPED_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(whep.active_sessions(), 1);
}

#[tokio::test]
async fn scoped_view_key_is_denied_tearing_down_an_out_of_scope_session() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    // A viewer session on pgm opened via the per-output token.
    let created = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(OUTPUT_TOKEN), OFFER),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    // The output-scoped key (wall-1) may not tear down pgm's viewer session.
    let del = Request::builder()
        .method("DELETE")
        .uri(&location)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {OUTPUT_SCOPED_TOKEN}"),
        )
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, del).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    assert_eq!(
        whep.active_sessions(),
        1,
        "the out-of-scope viewer session is untouched on a denied teardown"
    );
}

#[tokio::test]
async fn scoped_view_key_may_tear_down_its_in_scope_session() {
    let whep = Arc::new(FakeWhep::default());
    let router = router_with_whep(Arc::clone(&whep));
    let created = send(
        &router,
        post_sdp("/api/v1/whep/pgm", Some(PGM_SCOPED_TOKEN), OFFER),
    )
    .await;
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    let del = Request::builder()
        .method("DELETE")
        .uri(&location)
        .header(header::AUTHORIZATION, format!("Bearer {PGM_SCOPED_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, del).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        whep.active_sessions(),
        0,
        "the in-scope viewer session is freed"
    );
}
