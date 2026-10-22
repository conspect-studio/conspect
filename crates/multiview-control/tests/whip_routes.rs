//! WHIP ingest route tests (ADR-T014 §2/§3): the RFC 9725 signalling surface —
//! `POST` offer→answer, `DELETE` teardown, `PATCH` 405, `OPTIONS` preflight,
//! the per-source-token / Write-API-key auth model, the 409 one-publisher rule,
//! and the RFC 9457 problem bodies — exercised against an in-memory fake
//! [`WhipProvider`] (never a real str0m engine; that is the cli wiring). Re-asserts
//! invariant #10: a WHIP publisher is an ingest source that can never
//! back-pressure the engine, and the control plane stays native-free via the
//! provider seam.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, InMemoryRepository, Principal, Role, WhipAnswer, WhipAuth,
    WhipProvider, WhipReject,
};
use multiview_engine::EnginePublisher;

mod support;
use support::{body_json, send, OPERATOR_TOKEN, PEPPER, SCOPED_TOKEN, VIEWER_TOKEN};

/// A minimal, well-formed SDP offer advertising H.264 + Opus.
const OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP6 ::\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendonly\r\n";

/// The per-source bearer the fake provider accepts for `cam-1`.
const SOURCE_TOKEN: &str = "s3cret-cam-1";

/// An in-memory fake [`WhipProvider`]: accepts the per-source token or a Write
/// principal, enforces one-publisher-per-source (409), and records sessions. It
/// never touches an engine — the invariant-#10 stand-in for the str0m endpoint.
#[derive(Default)]
struct FakeWhip {
    /// Live sessions as `(source_id, session_id)`.
    sessions: Mutex<Vec<(String, String)>>,
    next: AtomicUsize,
    /// When set, every negotiation is refused with this reason.
    refuse: Mutex<Option<WhipReject>>,
}

impl FakeWhip {
    fn refuse_with(reason: WhipReject) -> Self {
        Self {
            refuse: Mutex::new(Some(reason)),
            ..Self::default()
        }
    }
}

impl WhipProvider for FakeWhip {
    fn negotiate(
        &self,
        source_id: &str,
        offer: &str,
        auth: &WhipAuth,
    ) -> Result<WhipAnswer, WhipReject> {
        if let Some(reason) = self.refuse.lock().unwrap().clone() {
            return Err(reason);
        }
        // Auth: a Write-scope API key, OR the per-source bearer token. `cam-1`
        // has a token; an unknown source is treated as token-less (Write-only).
        let token_ok = source_id == "cam-1" && auth.bearer.as_deref() == Some(SOURCE_TOKEN);
        if !auth.write_key && !token_ok {
            // No credential at all is 401; a wrong/insufficient one is 403.
            return if auth.bearer.is_none() {
                Err(WhipReject::Unauthorized)
            } else {
                Err(WhipReject::Forbidden)
            };
        }
        if !offer.contains("v=0") {
            return Err(WhipReject::Malformed("no SDP version line".to_owned()));
        }
        // One publisher per source.
        if self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .any(|(s, _)| s == source_id)
        {
            return Err(WhipReject::Conflict);
        }
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let session_id = format!("whip-sess-{n}");
        self.sessions
            .lock()
            .unwrap()
            .push((source_id.to_owned(), session_id.clone()));
        Ok(WhipAnswer {
            session_id,
            sdp: "v=0\r\no=- 0 0 IN IP6 ::\r\ns=-\r\nt=0 0\r\n\
a=group:BUNDLE 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=ice-ufrag:abcd\r\na=rtpmap:96 H264/90000\r\na=recvonly\r\n"
                .to_owned(),
        })
    }

    fn release(&self, source_id: &str, session_id: &str, _auth: &WhipAuth) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|(s, id)| !(s == source_id && id == session_id));
        sessions.len() != before
    }

    fn active_sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

fn router_with_whip(whip: Arc<FakeWhip>) -> axum::Router {
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
    // A scoped Operator key (object-scoped to "scoped-layout") for the SEC-08
    // API-key-path BOLA tests; matches `support::seeded_keys` so `SCOPED_TOKEN`
    // authenticates.
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
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    )
    .with_whip(whip);
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
async fn source_token_publish_yields_created_with_location() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
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
        location.starts_with("/api/v1/whip/cam-1/sessions/"),
        "Location is the session resource: {location}"
    );
    assert_eq!(whip.active_sessions(), 1);
}

#[tokio::test]
async fn write_api_key_publish_is_accepted() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    // An Operator (Write) API key publishes to a token-less source.
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/no-token-src", Some(OPERATOR_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn no_credentials_is_unauthorized() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    let resp = send(&router, post_sdp("/api/v1/whip/cam-1", None, OFFER)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key(header::WWW_AUTHENTICATE),
        "a 401 carries WWW-Authenticate: Bearer"
    );
}

#[tokio::test]
async fn wrong_token_is_forbidden() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    // A View API key is a valid credential but lacks Write scope, and is not the
    // source token -> 403.
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(VIEWER_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wrong_content_type_is_unsupported_media_type() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/whip/cam-1")
        .header(header::AUTHORIZATION, format!("Bearer {SOURCE_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn malformed_offer_is_bad_request() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), "not sdp at all"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn second_publisher_is_conflict() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    let first = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let second = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn codec_incompatible_offer_is_not_acceptable() {
    let whip = Arc::new(FakeWhip::refuse_with(WhipReject::NoCompatibleCodec));
    let router = router_with_whip(whip);
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn unavailable_is_service_unavailable_with_retry_after() {
    let whip = Arc::new(FakeWhip::refuse_with(WhipReject::Unavailable));
    let router = router_with_whip(whip);
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        resp.headers().contains_key(header::RETRY_AFTER),
        "503 carries Retry-After"
    );
}

#[tokio::test]
async fn delete_is_idempotent_and_404_for_unknown() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    let created = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
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
        .header(header::AUTHORIZATION, format!("Bearer {SOURCE_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, del).await;
    assert_eq!(resp.status(), StatusCode::OK, "DELETE returns 200");
    assert_eq!(whip.active_sessions(), 0);

    // An unknown session id is a 404.
    let unknown = Request::builder()
        .method("DELETE")
        .uri("/api/v1/whip/cam-1/sessions/never-existed")
        .header(header::AUTHORIZATION, format!("Bearer {SOURCE_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, unknown).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_is_method_not_allowed_with_allow_header() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    let req = Request::builder()
        .method(Method::PATCH)
        .uri("/api/v1/whip/cam-1/sessions/whatever")
        .header(header::AUTHORIZATION, format!("Bearer {SOURCE_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/trickle-ice-sdpfrag")
        .body(Body::from("a=candidate..."))
        .unwrap();
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = resp
        .headers()
        .get(header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        allow.contains("DELETE") && allow.contains("OPTIONS"),
        "Allow advertises DELETE, OPTIONS: {allow}"
    );
}

#[tokio::test]
async fn options_preflight_advertises_accept_post() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/v1/whip/cam-1")
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, req).await;
    // OPTIONS is unauthenticated (browser preflight) and advertises Accept-Post.
    assert!(
        resp.status() == StatusCode::NO_CONTENT || resp.status() == StatusCode::OK,
        "OPTIONS preflight is 2xx, got {}",
        resp.status()
    );
    let accept_post = resp
        .headers()
        .get("accept-post")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        accept_post.contains("application/sdp"),
        "Accept-Post: application/sdp, got {accept_post:?}"
    );
}

#[tokio::test]
async fn problem_body_is_rfc9457() {
    let router = router_with_whip(Arc::new(FakeWhip::default()));
    let resp = send(&router, post_sdp("/api/v1/whip/cam-1", None, OFFER)).await;
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 401);
}

// ---- SEC-08 (BOLA, ADR-W005): the API-key credential path honors object scope --
//
// A WHIP source is an object (source) id. The alternative Write-API-key path
// authorized by ROLE only, so a scoped Operator key could publish to / tear down
// ANY source. These pin per-object authz on the API-key path (POST + DELETE)
// with zero provider side effect; the per-source *token* path is unchanged
// (the existing token tests still pass).

#[tokio::test]
async fn scoped_write_key_is_denied_publishing_to_an_out_of_scope_source() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    // scoped-key is object-scoped to "scoped-layout"; cam-1 is out of scope.
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SCOPED_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    assert_eq!(
        whip.active_sessions(),
        0,
        "no session is negotiated on a denied publish"
    );
}

#[tokio::test]
async fn scoped_write_key_may_publish_to_its_in_scope_source() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    // "scoped-layout" is a token-less source in the fake, so the Write-key path
    // is exercised; it is in scope, so the publish is accepted.
    let resp = send(
        &router,
        post_sdp("/api/v1/whip/scoped-layout", Some(SCOPED_TOKEN), OFFER),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(whip.active_sessions(), 1);
}

#[tokio::test]
async fn scoped_write_key_is_denied_tearing_down_an_out_of_scope_session() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    // A session on cam-1 opened via the per-source token (out of the scoped key's
    // object scope).
    let created = send(
        &router,
        post_sdp("/api/v1/whip/cam-1", Some(SOURCE_TOKEN), OFFER),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let location = created
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location present");
    // The scoped key (scoped-layout) may not tear down cam-1's session.
    let del = Request::builder()
        .method("DELETE")
        .uri(&location)
        .header(header::AUTHORIZATION, format!("Bearer {SCOPED_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, del).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    assert_eq!(
        whip.active_sessions(),
        1,
        "the out-of-scope session is untouched on a denied teardown"
    );
}

#[tokio::test]
async fn scoped_write_key_may_tear_down_its_in_scope_session() {
    let whip = Arc::new(FakeWhip::default());
    let router = router_with_whip(Arc::clone(&whip));
    // Open + release a session on the in-scope, token-less source "scoped-layout".
    let created = send(
        &router,
        post_sdp("/api/v1/whip/scoped-layout", Some(SCOPED_TOKEN), OFFER),
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
        .header(header::AUTHORIZATION, format!("Bearer {SCOPED_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, del).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(whip.active_sessions(), 0, "the in-scope session is freed");
}
