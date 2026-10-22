//! # multiview-control
//!
//! The axum REST + WebSocket + SSE control API for the Multiview engine: the
//! command-bus shell, `OpenAPI` 3.1 (utoipa + Scalar), API-key + RBAC auth,
//! optimistic concurrency (`ETag`/`If-Match`), idempotent operational commands,
//! the realtime event fan-out, and the resource repository. The library target
//! is `multiview_control`.
//!
//! ## Isolation (invariant #10) is structural
//!
//! The control plane is best-effort and **physically incapable of
//! back-pressuring the engine**:
//!
//! * It reads engine state through the wait-free latest-state slot and the
//!   drop-oldest event broadcast of [`multiview_engine::EnginePublisher`] — both
//!   publish paths cannot be blocked by any consumer.
//! * The realtime reader uses lagged-skip semantics
//!   ([`realtime::SessionStream::next_delta`]): a slow client lags/re-subscribes
//!   and is dropped, never blocking the engine.
//! * The only channel *to* the engine is the bounded, **non-blocking** command
//!   bus ([`command::command_bus`]): control submits with `try_submit` (sheds to
//!   `503` when full); the engine drains at its leisure.
//!
//! ## Conventions (conventions §6)
//!
//! REST base **`/api/v1`**; RFC 9457 `application/problem+json` errors;
//! long-running ops return `202 Accepted` + an operation id (outcome on the
//! realtime stream); `ETag`/`If-Match` → `412`; `Idempotency-Key` on
//! start/stop/swap; WebSocket primary at `/api/v1/ws`, SSE fallback at
//! `/api/v1/events`; `OpenAPI` at `/api/v1/openapi.json` with Scalar at `/docs`.
//!
//! ## Default build is pure Rust
//!
//! The default features build with no native libraries. The `sqlite` feature
//! (off by default) adds an `sqlx`/SQLite-backed [`repository::Repository`];
//! `SQLite`'s license is outside the cargo-deny allowlist, so it is never part of
//! the CI-green default build.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod account_audit;
pub mod alarm_ingest;
pub mod alarm_store;
pub mod audio_routing;
pub mod audit;
pub mod auth;
pub mod boot_model;
pub mod command;
pub mod concurrency;
pub mod config_lock;
pub mod config_watch;
pub mod cors;
pub mod devices;
pub mod error;
pub mod is07;
pub mod jwt;
/// Management-plane rate limiting (SEC-14): the keyed token-bucket + the
/// request-concurrency + rate middleware backing the control-plane `DoS` floor.
pub(crate) mod limits;
pub mod live_apply;
pub mod nmos;
pub mod notify;
pub mod pending_actions;
pub mod preview;
pub mod problem;
pub mod realtime;
pub mod repository;
pub mod resource_store;
pub mod router;
pub mod routes;
pub mod routing;
pub mod salvo_store;
/// The shutdown-aware, guard-owning transport wrapper the hand-rolled serve loop wraps
/// each accepted connection in (SEC-14 #126 R2): it carries the accept-level admission
/// guard across an HTTP/1 upgrade (so a live WebSocket keeps its population-cap slot)
/// and drains the connection when serve shuts down.
mod serve_stream;
pub mod state;
pub mod support_bundle;
pub mod support_store;
pub mod system;
pub mod tally_ingest;
pub mod tally_state;
pub mod telemetry_consent;
pub(crate) mod typed_resources;
pub mod versioning;
pub mod warning_ingest;
pub mod warning_store;
pub mod watch_status;
pub mod whep_output;
pub mod whip;

#[cfg(feature = "embed-web")]
pub mod spa;

#[cfg(feature = "openapi")]
pub mod openapi;

#[cfg(feature = "openapi")]
pub mod openapi_schemas;

#[cfg(feature = "sqlite")]
pub mod sqlite;

use axum::routing::{get, post};
use axum::Router;

use serve_stream::TrackedStream;

pub use account_audit::{
    AccountAuditEntry, AccountAuditKind, AccountAuditPage, AccountAuditRepository,
    AccountAuditStore, InMemoryAccountAudit, ACCOUNT_AUDIT_KIND, DEFAULT_ACCOUNT_AUDIT_CAPACITY,
};
pub use alarm_ingest::{alarm_transition, ingest_step, run_alarm_ingest, IngestStep};
pub use alarm_store::{
    AlarmFilter, AlarmRepository, InMemoryAlarmStore, VersionedAlarm, ALARM_KIND,
};
pub use audio_routing::{AudioRoutingStore, AUDIO_ROUTING_ID, AUDIO_ROUTING_KIND};
pub use audit::{
    AuditAction, AuditEntry, AuditLog, AuditRepository, InMemoryAuditLog, AUDIT_KIND,
    DEFAULT_AUDIT_CAPACITY,
};
pub use auth::{
    authorize_object, authorize_output, provision_admin_keys, Action, ApiKeyStore, Principal, Role,
};
pub use command::{
    command_bus, resolve_layout_document, Command, CommandReceiver, CommandSender,
    MediaTransportVerb, OperationId, ResolvedLayout, SubmitError,
};
pub use concurrency::{IdempotencyKey, IdempotencyStore, IfMatch, Reservation, Version};
pub use devices::{
    DeviceBroadcaster, DeviceDriverRegistry, DeviceLifecycle, DeviceStatusRegistry, LifecycleEvent,
    ModeConvergence, OutputTarget, SourceCandidate, WorkMode, ZowietekDriver,
};
pub use error::{ControlError, ControlResult};
pub use is07::{
    tally_color_from_is07, tally_color_to_is07, tally_event_to_is07, GpiEvent, Is07Command,
    Is07EventType, Is07Message, Is07Payload, Is07Subscription, Is07Timing,
};
pub use jwt::{JwtError, JwtValidator, SignatureAlgorithm};
pub use live_apply::{LiveApplyCaps, OverlayLiveCapability};
pub use nmos::is04::{Device, MediaFormat, Node, Receiver, Registration, ResourceCore, Sender};
pub use nmos::is05::{
    parse_sdp_transport, Activation, ActivationMode, ConnectionRequest, ConnectionState,
    TransportParams,
};
pub use nmos::is08::{ChannelMap, ChannelSource, MappingError};
pub use nmos::is10::{Is10Claims, Is10Error, NmosAccess, NmosApiClaim};
pub use nmos::{nmos_router, NmosRegistry, NMOS_RECEIVER_KIND};
pub use notify::email::{EmailEnvelope, EmailMessage};
pub use notify::webhook::{WebhookPayload, WebhookRequest};
pub use notify::{AlarmTransitionKind, Destination, RoutingRule, SeverityRouter};
pub use pending_actions::{
    CancelOutcome, ExecuteOutcome, InMemoryPendingActions, PendingAction, PendingActionKind,
    PendingActionRepository, PendingActionState, PendingActionStore,
    DEFAULT_PENDING_ACTION_CAPACITY, PENDING_ACTION_KIND,
};
pub use preview::{
    no_preview, no_whep, FocusCaps, GatedWhep, NoPreview, NoWhep, PreviewCapabilities,
    PreviewProvider, ProgramFidelity, ScopeCapabilities, ScopeCapability, SharedPreview,
    SharedWhep, WhepAnswer, WhepProvider, WhepReject, WhepScope,
};
pub use problem::{Problem, PROBLEM_JSON};
pub use realtime::{
    AllowedOrigins, CorrKey, CorrRegistry, RealtimeFrame, ReauthOutcome, SessionStream,
    WsTicketResponse, WsTicketStore, WS_TICKET_CAPACITY, WS_TICKET_TTL,
};
pub use repository::{InMemoryRepository, Layout, LayoutInput, Repository, VersionedLayout};
pub use resource_store::{
    DeviceKind, InMemoryDeviceStore, InMemoryMediaPlayerStore, InMemoryOutputStore,
    InMemoryOverlayStore, InMemoryResourceStore, InMemorySourceStore, InMemorySyncGroupStore,
    MediaPlayerKind, OutputKind, OverlayKind, Resource, ResourceInput, ResourceKind,
    ResourceRepository, SourceKind, SyncGroupKind, VersionedResource, DEVICE_KIND,
    MEDIA_PLAYER_KIND, OUTPUT_KIND, OVERLAY_KIND, SOURCE_KIND, SYNC_GROUP_KIND,
};
pub use router::{
    ingest_route, route_follow, route_follow_all, RouteBinding, RouteFollowUpdate, RouteTable,
    RouterRoute,
};
pub use routing::{classify, DestinationProfile, RouteClass, RoutePlan, RouteRequest, RouteTarget};
pub use salvo_store::{InMemorySalvoStore, SalvoRepository, VersionedSalvo, SALVO_KIND};
pub use state::{
    seed_resources, AckClock, AppState, EngineStateSnapshot, LicenceState, LiveSourceCapability,
    SeededResources,
};
pub use support_bundle::{
    compose_bundle, redact_config, redact_config_for_export, Bundle, BundleInclude,
    BundleRepository, BundleRequest, BundleStore, BundleWindow, ConfigSources, InMemoryBundles,
    Redaction, RedactionReason, EXPORT_REDACTED_SENTINEL,
};
pub use support_store::{
    support_entitlement, support_route, CloseOutcome, DataRequest, DataRequestOutcome,
    DataRequestRepository, DataRequestState, DataRequestStore, FirstLine, InMemoryDataRequests,
    InMemoryTickets, NewTicket, ReplyOutcome, SupportEntitlement, SupportRoute, Ticket,
    TicketContext, TicketRepository, TicketSeverity, TicketState, TicketStore, TicketSummary,
};
pub use tally_ingest::{run_tally_ingest, tally_ingest_step, TallyIngestStep};
pub use tally_state::{
    tally_observation, target_key, InMemoryProfileStore, OverrideRegistry, TallyEntry, TallyMirror,
    TallyProfileRepository, VersionedProfile, TALLY_PROFILE_KIND,
};
pub use telemetry_consent::{
    ConsentActor, ConsentRecord, ConsentState, DiagnosticsSnapshotStore, SnapshotStatus,
    DEFAULT_SNAPSHOT_CAPACITY,
};
pub use versioning::{
    diff_documents, ConfigRevision, ConfigVersionStore, DocumentDiff, InMemoryConfigVersionStore,
    RevisionId, CONFIG_REVISION_KIND,
};
pub use warning_ingest::{
    emit_capability_warnings, run_warning_ingest, warning_ingest_step, warning_transition,
    CompositeMismatchView, WarningIngestStep,
};
pub use warning_store::{InMemoryWarningStore, WarningFilter, WarningRepository, WARNING_KIND};
pub use watch_status::{ConfigWatchStatus, WatchStamp, WatchStatusBody};
pub use whep_output::{
    no_whep_output, NoWhepOutput, SharedWhepOutput, WhepOutputAnswer, WhepOutputAuth,
    WhepOutputProvider, WhepOutputReject,
};
pub use whip::{no_whip, NoWhip, SharedWhip, WhipAnswer, WhipAuth, WhipProvider, WhipReject};

/// Build the complete control-plane [`Router`] for the given [`AppState`].
///
/// Wires, under base path `/api/v1`:
/// * the resource + command routes ([`routes::api_router`]);
/// * the realtime WebSocket (`/ws`) and SSE (`/events`) transports;
///
/// and, when the `openapi` feature is on, the `OpenAPI` JSON and Scalar docs.
///
/// The returned router carries `AppState`, so it is ready to serve.
pub fn router(state: AppState) -> Router {
    // SEC-14: install the management-plane request-concurrency + rate caps only when
    // the operator has limits enabled (the default). Disabled ⇒ the router is exactly
    // what it was before — no middleware, no overhead.
    let limits_enabled = state.limiters.is_enabled();

    let api = routes::api_router()
        // The WebRTC media-signalling routes, wrapped with their own CORS layer
        // (ADR-0048 §9): `webrtc.cors_allow_origins` applies **only** here — to
        // WHIP / WHEP-serve / preview-WHEP / capabilities — so a browser served
        // from a web origin can publish and play cross-origin, while the resource
        // CRUD and realtime surfaces stay outside the media-CORS scope.
        .merge(cors::with_signalling_cors(
            routes::signalling_router(),
            state.clone(),
        ))
        .route("/ws", get(realtime::ws_handler))
        // Mint a short-lived single-use realtime auth ticket (ADR-RT011): the
        // browser path that keeps the durable bearer out of the WS/SSE URL (SEC-01).
        .route("/ws/ticket", post(realtime::ws_ticket_handler))
        .route("/events", get(realtime::sse_handler))
        // Unauthenticated auth-mode discovery: the SPA reads this before it has a
        // token, to decide whether to show a login gate (and to validate a key).
        .route("/auth/status", get(realtime::auth_status_handler))
        // The Conspect config-lock interceptor (S2 backend, ADR-0050 §5): one
        // additive guard over the whole `/api/v1` surface that refuses
        // *configuration* mutations with a `409 config_locked` problem when the
        // entitlement ladder is locked. Reads + operational continuity + the
        // licence recovery path pass through. It is wait-free (a lock-free store
        // read) and holds no engine handle — a locked config never stops a running
        // program (invariant #1/#10).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            config_lock::config_lock_guard,
        ));

    // SEC-14 post-auth per-API-key rate limit on the authenticated `/api/v1`
    // surface: resolves the presented `Bearer` credential and limits a *validated*
    // key by its `key_id` (unauthenticated requests pass through — they are covered
    // by the outer per-IP + concurrency caps). Applied inside those outer caps.
    let api = if limits_enabled {
        api.layer(axum::middleware::from_fn_with_state(
            limits::PerKeyLimitState {
                limiters: state.limiters.clone(),
                api_keys: state.api_keys.clone(),
            },
            limits::per_api_key_rate_limit,
        ))
    } else {
        api
    };

    let app = Router::new()
        .nest("/api/v1", api)
        // The NMOS Node API lives under its own standardised `/x-nmos` base, not
        // under `/api/v1` — it is a separate, AMWA-specified surface.
        .merge(nmos::nmos_router());

    #[cfg(feature = "openapi")]
    let app = app.merge(openapi::openapi_router());

    // The embedded web UI is the fallback: it runs only for requests no route
    // above matched, so it never shadows `/api/v1`, `/x-nmos`, or the docs.
    #[cfg(feature = "embed-web")]
    let app = app.fallback(spa::spa_fallback);

    // Capture the shared limiters before `state` is consumed by `with_state`.
    let limiters = state.limiters.clone();
    let app = app.with_state(state);

    // SEC-14 outer caps over the WHOLE surface (`/api/v1`, `/x-nmos`, docs, SPA):
    // the per-IP pre-auth rate limit is OUTERMOST — an abusive source IP is shed
    // with `429` before it can consume a concurrency permit — then the global
    // concurrent-request cap sheds `503`. Both carry their own state and shed
    // rather than queue, so they never back-pressure the engine (invariant #10).
    if limits_enabled {
        app.layer(axum::middleware::from_fn_with_state(
            limiters.clone(),
            limits::concurrency_cap,
        ))
        .layer(axum::middleware::from_fn_with_state(
            limiters,
            limits::per_ip_rate_limit,
        ))
    } else {
        app
    }
}

/// The default control-plane **header-read timeout** ([`ServeOptions`]): the
/// maximum time the server waits to read a request's full header block before it
/// drops the connection. Generous for any legitimate client (headers arrive in
/// milliseconds) while bounding a slow-header ("slowloris") client that dribbles
/// headers to pin a connection open. Mirrors the `[control.limits]
/// header_read_timeout` config default.
pub const DEFAULT_HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The default accept-level **global connection cap** ([`ServeOptions`]): the maximum
/// number of connections the serve loop holds open across all peers, enforced at
/// accept before any request headers parse. Mirrors the `[control.limits]
/// max_connections` config default.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// The default accept-level **per-source-IP connection cap** ([`ServeOptions`]): the
/// maximum number of connections one peer IP may hold, so a single source cannot
/// monopolise [`DEFAULT_MAX_CONNECTIONS`]. Mirrors the `[control.limits]
/// max_connections_per_ip` config default.
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 256;

/// The default graceful-drain ceiling ([`ServeOptions`]): after shutdown is signalled,
/// in-flight connections get up to this long to finish before the remaining ones are
/// **aborted**. Normal teardown (the engine's broadcast close ends the WS/SSE sessions)
/// completes well within it — this only bounds a genuinely stuck client so a restart
/// cannot hang forever (safety §3).
pub const DEFAULT_GRACEFUL_SHUTDOWN_CEILING: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Serve-loop tuning for the control-plane HTTP servers ([`serve_with`] /
/// [`serve_router_with`] and their TLS siblings).
///
/// The control plane is served **HTTP/1-only** (SEC-14 #126 R2 /
/// [ADR-W031](../../docs/decisions/ADR-W031.md)): hyper's HTTP/2 server has no
/// header-read timeout, so an HTTP/2 connection could pin a slot forever — serving
/// HTTP/1-only bounds every slow-header connection by the deadlines below, and the h2
/// preface (a complete but invalid HTTP/1 request line) is rejected immediately by the
/// parser rather than negotiated into a timeout-free h2 session. This struct carries:
///
/// * the **header-read timeout** — the request-concurrency + rate caps engage only
///   *after* a request's headers are parsed, so they bound in-flight requests but not
///   a half-open, slow-header connection (slowloris); the header-read timeout closes
///   that hole. `None` = unbounded (front the plane with a reverse proxy instead).
/// * the **TLS handshake timeout** — the header-read timeout is post-handshake, so it
///   cannot catch a client that completes TCP then stalls mid-handshake; this bounds
///   the handshake so a stalled connection recycles its population-cap slot. `None` =
///   unbounded.
/// * the accept-level **connection population caps** — `max_connections` (global) and
///   `max_connections_per_ip`, enforced *at accept* before any header parse (the
///   population bound the request-level caps miss). `None` = no in-process cap.
/// * the **graceful-shutdown ceiling** — how long tracked (non-upgraded) connections get
///   to drain before they are aborted, so no tracked task outlives `serve`; an upgraded
///   WebSocket is detached and instead drains cooperatively via the shutdown-aware
///   transport (F3, ADR-W031 §4).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ServeOptions {
    /// Maximum time to read a request's full header block before the connection is
    /// dropped. `None` = unbounded (no in-process slowloris guard).
    pub header_read_timeout: Option<std::time::Duration>,
    /// Maximum time to complete the TLS handshake before the connection is dropped
    /// (TLS serve paths only). `None` = unbounded.
    pub tls_handshake_timeout: Option<std::time::Duration>,
    /// Accept-level global connection cap across all peers. `None` = no in-process cap.
    pub max_connections: Option<usize>,
    /// Accept-level per-source-IP connection cap. `None` = no in-process per-IP cap.
    /// Ignored when `max_connections` is `None`.
    pub max_connections_per_ip: Option<usize>,
    /// How long in-flight connections get to drain at shutdown before they are aborted.
    pub graceful_shutdown_ceiling: std::time::Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            header_read_timeout: Some(DEFAULT_HEADER_READ_TIMEOUT),
            tls_handshake_timeout: Some(DEFAULT_HEADER_READ_TIMEOUT),
            max_connections: Some(DEFAULT_MAX_CONNECTIONS),
            max_connections_per_ip: Some(DEFAULT_MAX_CONNECTIONS_PER_IP),
            graceful_shutdown_ceiling: DEFAULT_GRACEFUL_SHUTDOWN_CEILING,
        }
    }
}

impl ServeOptions {
    /// Set the header-read timeout (`None` disables it), chainable from
    /// [`ServeOptions::default`].
    #[must_use]
    pub fn with_header_read_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    /// Set the TLS handshake timeout (`None` disables it), chainable.
    #[must_use]
    pub fn with_tls_handshake_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.tls_handshake_timeout = timeout;
        self
    }

    /// Set the accept-level global connection cap (`None` disables it), chainable.
    #[must_use]
    pub fn with_max_connections(mut self, max_connections: Option<usize>) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Set the accept-level per-source-IP connection cap (`None` disables it),
    /// chainable.
    #[must_use]
    pub fn with_max_connections_per_ip(mut self, max_connections_per_ip: Option<usize>) -> Self {
        self.max_connections_per_ip = max_connections_per_ip;
        self
    }

    /// Set the graceful-shutdown drain ceiling, chainable.
    #[must_use]
    pub fn with_graceful_shutdown_ceiling(mut self, ceiling: std::time::Duration) -> Self {
        self.graceful_shutdown_ceiling = ceiling;
        self
    }

    /// Build the accept-level [`ConnectionAdmission`] gate from these options, or
    /// `None` when no global cap is configured. The per-IP cap defaults to the global
    /// cap when unset (so a lone `max_connections` still bounds a single source).
    fn admission(&self) -> Option<std::sync::Arc<crate::limits::ConnectionAdmission>> {
        self.max_connections.map(|max_connections| {
            crate::limits::ConnectionAdmission::new(
                max_connections,
                self.max_connections_per_ip.unwrap_or(max_connections),
            )
        })
    }
}

/// Serve the control-plane [`router`] on an already-bound
/// [`tokio::net::TcpListener`], shutting down gracefully when `shutdown` resolves.
///
/// Binding is the caller's responsibility — so it can choose the address, log the
/// resolved port, or hand in a socket inherited from a supervisor — and the
/// caller's `shutdown` future (typically the engine's stop signal) drives a clean
/// drain of in-flight requests before this returns. Everything served here is
/// isolation-safe (invariant #10): the router only reads the engine's wait-free
/// latest-state slot and drop-oldest event broadcast and submits to the
/// non-blocking command bus, so no client it serves can back-pressure the engine.
///
/// # Errors
/// Propagates any I/O error from the underlying accept loop.
pub async fn serve<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_router(listener, router(state), shutdown).await
}

/// Serve the control-plane [`router`] with explicit [`ServeOptions`] (the
/// header-read timeout) — the [`serve`] variant that threads the SEC-14 slowloris
/// guard from `[control.limits] header_read_timeout`.
///
/// # Errors
/// Propagates any I/O error from the underlying accept loop.
pub async fn serve_with<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    options: ServeOptions,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_router_with(listener, router(state), options, shutdown).await
}

/// Serve an already-built [`axum::Router`] on an already-bound
/// [`tokio::net::TcpListener`], shutting down gracefully when `shutdown`
/// resolves.
///
/// The composition seam behind [`serve`]: a caller that needs extra routes
/// alongside the control plane — e.g. `multiview run` nesting each configured
/// HLS output's delivery router under `/hls/{output-id}` (DEV-D1) — builds the
/// app from [`router`] plus its mounts and serves it here, so there is exactly
/// one accept-loop/graceful-shutdown implementation. The same isolation
/// contract as [`serve`] applies: anything mounted must be best-effort and
/// physically incapable of back-pressuring the engine (invariant #10).
///
/// [`serve_router_with`] with the default [`ServeOptions`] — so it installs the
/// [`DEFAULT_HEADER_READ_TIMEOUT`] slowloris guard and each connection's peer
/// `SocketAddr` as a `ConnectInfo` extension (the SEC-14 per-IP key). Route
/// control-plane traffic through this helper (or [`serve`]) to keep both active.
///
/// # Errors
/// Propagates any I/O error from the underlying accept loop.
pub async fn serve_router<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_router_with(listener, app, ServeOptions::default(), shutdown).await
}

/// Build the shared **HTTP/1** connection builder from [`ServeOptions`].
///
/// The control plane is served HTTP/1-only (SEC-14 #126 R2 / ADR-W031): hyper's HTTP/2
/// server has no header-read timeout, so an HTTP/2 connection could pin a slot forever.
/// Serving on [`hyper::server::conn::http1::Builder`] subjects every **slow-header**
/// connection to `options.header_read_timeout`; a connection that opens with the HTTP/2
/// preface is a complete but invalid HTTP/1 request line, so the parser rejects it
/// immediately (it never negotiates a timeout-free h2 session). The Tokio timer is
/// installed unconditionally because `header_read_timeout` panics when armed without one.
fn http1_builder(options: &ServeOptions) -> hyper::server::conn::http1::Builder {
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.timer(hyper_util::rt::TokioTimer::new());
    if let Some(timeout) = options.header_read_timeout {
        builder.header_read_timeout(timeout);
    }
    builder
}

/// Serve one accepted connection to completion on the HTTP/1 `builder`, installing the
/// peer `SocketAddr` as a `ConnectInfo` extension (the SEC-14 per-IP key) and upgrading
/// `/api/v1/ws`. When `shutdown` signals, the connection begins a graceful shutdown and
/// is then driven to completion; the drain loop aborts it if it outlives the ceiling (F3).
///
/// Generic over the transport IO so the plaintext (`TokioIo<TcpStream>`) and TLS
/// (`TokioIo<TlsStream<…>>`) serve loops share one connection driver.
async fn drive_connection<I>(
    builder: hyper::server::conn::http1::Builder,
    io: I,
    app: Router,
    peer_addr: std::net::SocketAddr,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    use axum::extract::ConnectInfo;
    use tower::ServiceExt as _;

    // Per-connection service: install the peer `SocketAddr` as a `ConnectInfo` extension
    // (what `into_make_service_with_connect_info` does — the SEC-14 per-IP key) then
    // dispatch to the shared router. `Router` is always ready, so `oneshot` is safe.
    let service =
        hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
            let mut request = request.map(axum::body::Body::new);
            request.extensions_mut().insert(ConnectInfo(peer_addr));
            app.clone().oneshot(request)
        });

    let connection = builder.serve_connection(io, service).with_upgrades();
    let mut connection = std::pin::pin!(connection);

    // Serve the connection, watching for the shutdown signal. `changed()` fires only on
    // the real signal — a cloned receiver has already seen the initial `false`, only
    // `true` is ever sent, and the sender outlives every connection task (the drain holds
    // it) — so a spurious early shutdown is not possible; the `borrow_and_update` guard
    // makes that explicit and keeps serving on any non-`true` value.
    let mut draining = false;
    loop {
        tokio::select! {
            result = connection.as_mut() => {
                if let Err(error) = result {
                    tracing::debug!(error = %error, "control-plane connection ended with error");
                }
                return;
            }
            changed = shutdown.changed(), if !draining => {
                let shutdown_now = match changed {
                    Ok(()) => *shutdown.borrow_and_update(),
                    // The sender was dropped — serve() is tearing down.
                    Err(_) => true,
                };
                if shutdown_now {
                    // http1's `UpgradeableConnection` does not implement hyper-util's
                    // `GracefulConnection`, so drive the graceful shutdown directly; the
                    // loop then polls the connection to completion.
                    connection.as_mut().graceful_shutdown();
                    draining = true;
                }
            }
        }
    }
}

/// Drain the **tracked** (non-upgraded) connection tasks at shutdown: signal them to begin
/// a graceful shutdown, then wait up to `ceiling`; any still running are **aborted** and
/// reaped so no tracked task outlives `serve` (F3, safety §3). An upgraded WebSocket is a
/// detached task the `JoinSet` does not track; it drains cooperatively via the
/// shutdown-aware transport (ADR-W031 §4), not by a join here.
async fn drain_connections(
    connections: &mut tokio::task::JoinSet<()>,
    shutdown: &tokio::sync::watch::Sender<bool>,
    ceiling: std::time::Duration,
) {
    // Signal every connection task to begin its graceful shutdown.
    let _ = shutdown.send(true);
    tokio::select! {
        () = async { while connections.join_next().await.is_some() {} } => {}
        () = tokio::time::sleep(ceiling) => {
            tracing::debug!(
                "control-plane graceful-shutdown ceiling reached; aborting remaining connections"
            );
            connections.abort_all();
            // Reap the aborted tasks so none outlives serve().
            while connections.join_next().await.is_some() {}
        }
    }
}

/// Serve an already-built [`Router`] with explicit [`ServeOptions`], shutting down
/// gracefully when `shutdown` resolves. [`serve_router`] is this with
/// [`ServeOptions::default`].
///
/// The composition seam the CLI uses to thread the configured [`ServeOptions`] through
/// the mounted app (control plane + per-output HLS routers).
///
/// `axum::serve` builds the hyper connection `Builder` internally and exposes no
/// header-read timeout, and its `auto` builder negotiates HTTP/2 — which has no
/// header-read timeout at all. So this drives the accept loop directly on
/// [`hyper::server::conn::http1::Builder`], **HTTP/1-only**, so every connection is
/// bounded by `options.header_read_timeout` (SEC-14 #126 R2 / ADR-W031). Each accepted
/// connection is first admitted through the accept-level [`ConnectionAdmission`]
/// population cap (`options.max_connections` / `max_connections_per_ip`) — over-cap
/// connections are dropped at accept, before any header parse — then served with
/// `serve_connection(..).with_upgrades()` (so `/api/v1/ws` still upgrades) with the peer
/// `SocketAddr` installed as a `ConnectInfo` request extension exactly as
/// `into_make_service_with_connect_info` would (the SEC-14 pre-auth per-IP rate limit
/// keys on it). Non-upgraded connections are tracked in a
/// [`JoinSet`](tokio::task::JoinSet) and aborted after
/// `options.graceful_shutdown_ceiling`, so no tracked task outlives this function; an
/// upgraded WebSocket is a detached task that instead drains cooperatively via the
/// shutdown-aware transport (ADR-W031 §4), not synchronously joined here.
///
/// Same isolation contract as [`serve_router`] (invariant #10): the loop only serves
/// HTTP and never awaits the engine, so no client it accepts can back-pressure the data
/// plane.
///
/// # Errors
/// Propagates any I/O error from the underlying accept loop.
pub async fn serve_router_with<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    options: ServeOptions,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use hyper_util::rt::TokioIo;

    let builder = http1_builder(&options);
    let admission = options.admission();
    let (shutdown_signal, shutdown_watch) = tokio::sync::watch::channel(false);
    let mut connections = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer_addr) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        // A transient accept error (e.g. EMFILE under load) must not tear
                        // down the control plane; log and keep serving, with a brief
                        // backoff so a persistent error does not busy-spin.
                        tracing::debug!(%error, "control-plane accept error; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                // Accept-level admission (SEC-14 #126 R2 F2): drop an over-cap connection
                // here, before any request headers parse — the population bound the
                // request-level caps miss. `None` admission ⇒ no cap configured.
                let guard = if let Some(admission) = admission.as_ref() {
                    let Some(guard) = admission.try_admit(peer_addr.ip()) else {
                        tracing::debug!(
                            peer = %peer_addr,
                            "control-plane connection refused at accept (population cap)"
                        );
                        continue;
                    };
                    Some(guard)
                } else {
                    None
                };
                let builder = builder.clone();
                let app = app.clone();
                let conn_shutdown = shutdown_watch.clone();
                connections.spawn(async move {
                    // Wrap the accepted stream so the admission `guard` rides the whole
                    // connection — including into an upgraded WebSocket, since hyper moves
                    // the IO into its `Upgraded` — and releases only when the socket finally
                    // closes (so a live WS keeps occupying its population-cap slot); and so
                    // the serve-shutdown signal reaches even the detached upgraded socket
                    // through the transport (SEC-14 #126 R2 F1).
                    let io = TokioIo::new(TrackedStream::new(stream, guard, conn_shutdown.clone()));
                    drive_connection(builder, io, app, peer_addr, conn_shutdown).await;
                });
            }
            () = &mut shutdown => {
                // Stop accepting; drain the in-flight connections below.
                drop(listener);
                break;
            }
        }
    }

    drain_connections(
        &mut connections,
        &shutdown_signal,
        options.graceful_shutdown_ceiling,
    )
    .await;
    Ok(())
}

/// Loaded, ready-to-serve rustls TLS material for the control plane (TLS-0,
/// [ADR-W029](../../docs/decisions/ADR-W029.md)).
///
/// An opaque wrapper over the shared [`rustls::ServerConfig`] — ALPN pinned to
/// `http/1.1` (the control plane is served HTTP/1-only; SEC-14 #126 R2 / ADR-W031) — so
/// callers (`multiview-cli`) hold it across the bind→serve seam without naming `rustls`.
/// Built by [`load_tls_material`] and consumed by [`serve_tls`] / [`serve_router_tls`],
/// which wrap it in a [`tokio_rustls::TlsAcceptor`].
#[cfg(feature = "tls")]
#[derive(Clone)]
pub struct RustlsMaterial(std::sync::Arc<rustls::ServerConfig>);

/// Failure loading operator TLS material for [`serve_tls`] (TLS-0, ADR-W029).
///
/// Surfaced at **startup** (a bad certificate aborts the run with a clear
/// message) rather than as a panic on the serve path (safety §3).
#[cfg(feature = "tls")]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsSetupError {
    /// Reading or PEM-parsing the certificate chain file failed.
    #[error("reading/parsing the TLS certificate {path:?}: {source}")]
    Certificate {
        /// The certificate path that failed.
        path: std::path::PathBuf,
        /// The underlying PEM/IO error.
        source: rustls_pki_types::pem::Error,
    },
    /// The certificate file contained no PEM certificates.
    #[error("the TLS certificate file {0:?} contained no certificates")]
    NoCertificates(std::path::PathBuf),
    /// Reading or PEM-parsing the private-key file failed (including no key
    /// present).
    #[error("reading/parsing the TLS private key {path:?}: {source}")]
    PrivateKey {
        /// The private-key path that failed.
        path: std::path::PathBuf,
        /// The underlying PEM/IO error.
        source: rustls_pki_types::pem::Error,
    },
    /// rustls rejected the certificate + key (e.g. the key does not match the
    /// leaf certificate, or the chain is malformed).
    #[error("building the rustls server configuration: {0}")]
    Rustls(#[from] rustls::Error),
    /// The configured `mode` is not one this build can serve (e.g. a future ACME
    /// mode fed to a static-certificate-only build). Fail-closed forward-compat —
    /// a `[control.tls]` config always parses in a newer schema, so an older
    /// binary rejects a mode it does not implement rather than misbehaving.
    #[error(
        "the configured control.tls mode is not supported by this build (TLS-0 serves \
             `mode = \"static\"` only)"
    )]
    UnsupportedMode,
}

/// Load the operator's [`TlsConfig`](multiview_config::TlsConfig) into
/// ready-to-serve [`RustlsMaterial`] (TLS-0, ADR-W029).
///
/// Reads the PEM certificate chain (leaf first) + private key and builds a rustls
/// [`ServerConfig`](rustls::ServerConfig) with an **explicit `aws-lc-rs` crypto
/// provider** — never the process-default provider, which panics when both
/// `ring` and `aws-lc-rs` are linked into one binary (a `full` build). This is
/// the exact idiom the cast driver uses (`devices/cast/net.rs`).
///
/// Deliberately a **separate, synchronous step from serving**: the binary calls
/// it at startup and fails loudly on a missing/garbage certificate, rather than
/// discovering the fault inside the spawned server task.
///
/// # Errors
/// [`TlsSetupError`] if a PEM file cannot be read/parsed, contains no
/// certificate / no private key, or the cert+key is rejected by rustls.
#[cfg(feature = "tls")]
pub fn load_tls_material(
    tls: &multiview_config::TlsConfig,
) -> Result<RustlsMaterial, TlsSetupError> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    // `TlsConfig` is `#[non_exhaustive]` cross-crate; TLS-0 has only `static`. A
    // future `mode` (e.g. ACME) fed to this build is rejected, never panicked.
    // Rebind the `cert_file`/`key_file` paths to short locals for the loader body.
    let multiview_config::TlsConfig::Static {
        cert_file: cert,
        key_file: key,
    } = tls
    else {
        return Err(TlsSetupError::UnsupportedMode);
    };

    // Certificate chain (leaf first, then intermediates).
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|source| TlsSetupError::Certificate {
            path: cert.clone(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsSetupError::Certificate {
            path: cert.clone(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsSetupError::NoCertificates(cert.clone()));
    }

    // Private key (PKCS#8 / PKCS#1 / SEC1). `from_pem_file` errors if none found.
    let key_der =
        PrivateKeyDer::from_pem_file(key).map_err(|source| TlsSetupError::PrivateKey {
            path: key.clone(),
            source,
        })?;

    // Build the ServerConfig with an EXPLICIT aws-lc-rs provider (never the
    // process default — see the function doc). Propagate any rustls error
    // (`#[from]`), never panic on the control-plane path (safety §3).
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key_der)?;

    // ALPN: offer HTTP/1.1 ONLY. The control plane is served HTTP/1-only (SEC-14 #126 R2
    // / ADR-W031) because hyper's HTTP/2 server has no header-read timeout, so an ALPN-
    // respecting client negotiates h1. Belt-and-braces: even a client that ignores ALPN
    // and sends the h2 preface post-handshake sends a complete but invalid HTTP/1 request
    // line, which the HTTP/1-only serve loop rejects immediately (never a timeout-free h2
    // session).
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(RustlsMaterial(std::sync::Arc::new(config)))
}

/// Serve an already-built [`axum::Router`] over **TLS** on an already-bound
/// [`tokio::net::TcpListener`], terminating rustls with `material` and shutting
/// down gracefully when `shutdown` resolves (TLS-0, ADR-W029) — the TLS sibling of
/// [`serve_router`], with the default [`ServeOptions`] (a
/// [`DEFAULT_HEADER_READ_TIMEOUT`] slowloris guard).
///
/// # Errors
/// Propagates any I/O error from the underlying accept/serve loop.
#[cfg(feature = "tls")]
pub async fn serve_router_tls<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    material: RustlsMaterial,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_router_tls_with(listener, app, material, ServeOptions::default(), shutdown).await
}

/// [`serve_router_tls`] with explicit [`ServeOptions`] — the TLS sibling of
/// [`serve_router_with`].
///
/// Identical isolation contract (invariant #10) and the **same** `ConnectInfo`
/// peer-`SocketAddr` wiring (via the shared [`drive_connection`]), so the SEC-14 pre-auth
/// per-IP rate limit ([`crate::limits`]) keeps its peer-IP key under HTTPS exactly as over
/// plain HTTP. This drives the accept loop directly: it wraps `material` in a
/// [`tokio_rustls::TlsAcceptor`] and, for each accepted connection, admits it through the
/// accept-level [`ConnectionAdmission`] population cap **before** the (costly) handshake,
/// performs the TLS handshake bounded by `options.tls_handshake_timeout` (so a client that
/// stalls mid-handshake recycles its cap slot rather than pinning it — the header-read
/// timeout is post-handshake and cannot catch that), then serves it **HTTP/1-only** via
/// [`drive_connection`] exactly like [`serve_router_with`]. The header-read timeout and the
/// bounded [`JoinSet`](tokio::task::JoinSet) drain apply identically; ALPN is pinned to
/// `http/1.1` in [`load_tls_material`] (SEC-14 #126 R2 / ADR-W031).
///
/// # Errors
/// Propagates any I/O error from the underlying accept loop.
#[cfg(feature = "tls")]
pub async fn serve_router_tls_with<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    material: RustlsMaterial,
    options: ServeOptions,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use hyper_util::rt::TokioIo;

    let acceptor = tokio_rustls::TlsAcceptor::from(material.0);
    let builder = http1_builder(&options);
    let admission = options.admission();
    let handshake_timeout = options.tls_handshake_timeout;
    let (shutdown_signal, shutdown_watch) = tokio::sync::watch::channel(false);
    let mut connections = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer_addr) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::debug!(%error, "control-plane TLS accept error; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                // Accept-level admission (SEC-14 #126 R2 F2): drop an over-cap connection
                // BEFORE the costly TLS handshake. `None` admission ⇒ no cap configured.
                let guard = if let Some(admission) = admission.as_ref() {
                    let Some(guard) = admission.try_admit(peer_addr.ip()) else {
                        tracing::debug!(
                            peer = %peer_addr,
                            "control-plane TLS connection refused at accept (population cap)"
                        );
                        continue;
                    };
                    Some(guard)
                } else {
                    None
                };
                let acceptor = acceptor.clone();
                let builder = builder.clone();
                let app = app.clone();
                let conn_shutdown = shutdown_watch.clone();
                connections.spawn(async move {
                    // `guard` is owned by this task, so it holds the admission slot across
                    // the (costly) handshake; a handshake failure/timeout `return`s and
                    // drops it, recycling the slot. On success it is handed to the
                    // `TrackedStream` below so it rides the served connection — including
                    // into an upgraded WebSocket — and releases only when the socket finally
                    // closes (SEC-14 #126 R2 F1).
                    let tls_stream = match handshake_timeout {
                        Some(timeout) => {
                            match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
                                Ok(Ok(tls_stream)) => tls_stream,
                                Ok(Err(error)) => {
                                    tracing::debug!(
                                        peer = %peer_addr, error = %error,
                                        "control-plane TLS handshake failed"
                                    );
                                    return;
                                }
                                Err(_) => {
                                    tracing::debug!(
                                        peer = %peer_addr,
                                        "control-plane TLS handshake timed out"
                                    );
                                    return;
                                }
                            }
                        }
                        None => match acceptor.accept(stream).await {
                            Ok(tls_stream) => tls_stream,
                            Err(error) => {
                                tracing::debug!(
                                    peer = %peer_addr, error = %error,
                                    "control-plane TLS handshake failed"
                                );
                                return;
                            }
                        },
                    };
                    let io =
                        TokioIo::new(TrackedStream::new(tls_stream, guard, conn_shutdown.clone()));
                    drive_connection(builder, io, app, peer_addr, conn_shutdown).await;
                });
            }
            () = &mut shutdown => {
                drop(listener);
                break;
            }
        }
    }

    drain_connections(
        &mut connections,
        &shutdown_signal,
        options.graceful_shutdown_ceiling,
    )
    .await;
    Ok(())
}

/// Serve the control-plane [`router`] over **TLS** on an already-bound listener
/// (TLS-0, ADR-W029): the TLS sibling of [`serve`].
///
/// # Errors
/// Propagates any I/O error from the underlying accept/serve loop.
#[cfg(feature = "tls")]
pub async fn serve_tls<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    material: RustlsMaterial,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_router_tls(listener, router(state), material, shutdown).await
}
