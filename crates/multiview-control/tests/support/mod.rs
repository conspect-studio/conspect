//! Shared test scaffolding: build an `AppState` + router with a seeded API-key
//! store and an in-memory repository, and helpers to drive real HTTP requests
//! through the router via `tower::ServiceExt::oneshot`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// This module is shared by several integration-test binaries; each binary uses
// a different subset of the helpers, so any given binary sees some as unused.
// These allows are scoped to the test support module only.
#![allow(dead_code, unreachable_pub)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, Response};
use axum::Router;
use http_body_util::BodyExt;
use multiview_control::{
    command_bus, AlarmRepository, ApiKeyStore, AppState, CommandReceiver, EngineStateSnapshot,
    InMemoryAlarmStore, InMemoryRepository, InMemorySalvoStore, InMemoryWarningStore, NmosRegistry,
    Principal, Role, SalvoRepository, TallyMirror, WarningRepository,
};
use multiview_core::time::MediaTime;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tower::ServiceExt;

/// The HMAC pepper used by the test API-key store.
pub const PEPPER: &[u8] = b"test-pepper-do-not-use-in-prod";

/// Test API keys: `<key_id>.<secret>` Bearer tokens with known roles.
pub const ADMIN_TOKEN: &str = "admin-key.admin-secret-abc";
pub const OPERATOR_TOKEN: &str = "operator-key.operator-secret-def";
pub const VIEWER_TOKEN: &str = "viewer-key.viewer-secret-ghi";
/// An operator scoped to a single object id (`scoped-layout`) for BOLA tests.
pub const SCOPED_TOKEN: &str = "scoped-key.scoped-secret-jkl";
/// An operator scoped to a single **output** id (`wall-1`) for per-output BOLA
/// tests: it may address head `wall-1` but is denied any other head.
pub const OUTPUT_SCOPED_TOKEN: &str = "out-scoped-key.out-scoped-secret-mno";

/// A fixed, deterministic acknowledgement timestamp used in tests.
pub const ACK_NANOS: i64 = 1_700_000_000_000_000_000;

/// A built test harness: the router plus the engine publisher and command
/// receiver so a test can drive both sides of the isolation channels.
pub struct Harness {
    pub router: Router,
    pub engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    pub commands: CommandReceiver,
    /// The shared alarm store the router reads/writes — also the ingest sink, so
    /// a test can publish an engine alarm event and observe it over HTTP.
    pub alarms: Arc<dyn AlarmRepository>,
    /// The shared health-warning store the router reads — also the warning-ingest
    /// sink, so a test can publish an engine `health.warning.*` event and read it
    /// over `GET /api/v1/health`.
    pub warnings: Arc<dyn WarningRepository>,
    /// The shared salvo store the router reads/writes (so a test can seed a
    /// definition and exercise CRUD + arm/take over HTTP).
    pub salvos: Arc<dyn SalvoRepository>,
    /// The shared resolved-tally mirror the router reads — also the tally ingest
    /// sink, so a test can publish an engine tally event and read it over HTTP.
    pub tally: Arc<TallyMirror>,
    /// The shared NMOS registry the router serves, so a test can seed
    /// node/device/sender/receiver resources and exercise the NMOS Node API.
    pub nmos: Arc<NmosRegistry>,
}

/// Build an API-key store seeded with the four known test keys.
pub fn seeded_keys() -> ApiKeyStore {
    let mut keys = ApiKeyStore::new(PEPPER.to_vec());
    keys.register(
        "admin-key",
        "admin-secret-abc",
        Principal {
            key_id: "admin-key".to_owned(),
            role: Role::Admin,
            scoped_object_ids: None,
            scoped_output_ids: None,
        },
    );
    keys.register(
        "operator-key",
        "operator-secret-def",
        Principal {
            key_id: "operator-key".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: None,
        },
    );
    keys.register(
        "viewer-key",
        "viewer-secret-ghi",
        Principal {
            key_id: "viewer-key".to_owned(),
            role: Role::Viewer,
            scoped_object_ids: None,
            scoped_output_ids: None,
        },
    );
    keys.register(
        "scoped-key",
        "scoped-secret-jkl",
        Principal {
            key_id: "scoped-key".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(vec!["scoped-layout".to_owned()]),
            scoped_output_ids: None,
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
        },
    );
    keys
}

/// Build a fresh harness with a small command-bus capacity (so overflow is
/// testable) and a seeded key store.
pub fn harness() -> Harness {
    harness_with_capacity(4)
}

/// Build a harness whose [`AppState`] is customized before the router is
/// built (e.g. `with_base_document`, `with_working_layout_id`).
pub fn harness_with(customize: impl FnOnce(AppState) -> AppState) -> Harness {
    harness_customized(4, customize)
}

/// Build a harness with a specific command-bus capacity.
pub fn harness_with_capacity(capacity: usize) -> Harness {
    harness_customized(capacity, |state| state)
}

/// Build a harness with a specific command-bus capacity and a state customizer
/// applied before the router is constructed.
pub fn harness_customized(
    capacity: usize,
    customize: impl FnOnce(AppState) -> AppState,
) -> Harness {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (tx, rx) = command_bus(capacity);
    let alarms: Arc<dyn AlarmRepository> = Arc::new(InMemoryAlarmStore::new());
    let warnings: Arc<dyn WarningRepository> = Arc::new(InMemoryWarningStore::new());
    let salvos: Arc<dyn SalvoRepository> = Arc::new(InMemorySalvoStore::new());
    let tally = Arc::new(TallyMirror::new());
    let nmos = Arc::new(NmosRegistry::new());
    let state = AppState::new(
        Arc::clone(&engine),
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(seeded_keys()),
    )
    .with_alarm_store(Arc::clone(&alarms))
    .with_warning_store(Arc::clone(&warnings))
    .with_salvo_store(Arc::clone(&salvos))
    .with_tally_mirror(Arc::clone(&tally))
    .with_nmos(Arc::clone(&nmos))
    .with_ack_clock(Arc::new(|| MediaTime::from_nanos(ACK_NANOS)));
    let state = customize(state);
    Harness {
        router: multiview_control::router(state),
        engine,
        commands: rx,
        alarms,
        warnings,
        salvos,
        tally,
        nmos,
    }
}

/// Send a request through the router, returning the response.
pub async fn send(router: &Router, request: Request<Body>) -> Response<Body> {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router should produce a response")
}

/// Read the entire response body into bytes.
pub async fn body_bytes(response: Response<Body>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("body should collect")
        .to_bytes()
        .to_vec()
}

/// Read the response body as parsed JSON.
pub async fn body_json(response: Response<Body>) -> serde_json::Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).expect("body should be JSON")
}

/// Build a `GET` request with a Bearer token.
pub fn get(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request should build")
}

/// Build a `POST` request with a Bearer token and JSON body.
pub fn post_json(path: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("request should build")
}

/// Build a `PATCH` request with a Bearer token and JSON body.
pub fn patch_json(path: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("request should build")
}

/// Read the `ETag` header off a response, if present.
pub fn etag(response: &Response<Body>) -> Option<String> {
    response
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Build a bodyless `POST` request with a Bearer token and an optional
/// `If-Match` header (used for the alarm acknowledge endpoint).
pub fn post_if_match(path: &str, token: &str, if_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(value) = if_match {
        builder = builder.header(header::IF_MATCH, value);
    }
    builder.body(Body::empty()).expect("request should build")
}

/// Build a bodyless `DELETE` request with a Bearer token and optional `If-Match`.
pub fn delete_if_match(path: &str, token: &str, if_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(value) = if_match {
        builder = builder.header(header::IF_MATCH, value);
    }
    builder.body(Body::empty()).expect("request should build")
}

/// Build a `DELETE` request with a Bearer token and a JSON body.
pub fn delete_json(path: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("request should build")
}

/// Build a `PUT` request with a Bearer token, optional `If-Match`, and JSON body.
pub fn put_json(
    path: &str,
    token: &str,
    if_match: Option<&str>,
    body: &serde_json::Value,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = if_match {
        builder = builder.header(header::IF_MATCH, value);
    }
    builder
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("request should build")
}
