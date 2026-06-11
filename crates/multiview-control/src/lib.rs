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
pub mod command;
pub mod concurrency;
pub mod config_lock;
pub mod devices;
pub mod error;
pub mod is07;
pub mod jwt;
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
pub mod state;
pub mod tally_ingest;
pub mod tally_state;
pub(crate) mod typed_resources;
pub mod versioning;
pub mod warning_ingest;
pub mod warning_store;

#[cfg(feature = "embed-web")]
pub mod spa;

#[cfg(feature = "openapi")]
pub mod openapi;

#[cfg(feature = "openapi")]
pub mod openapi_schemas;

#[cfg(feature = "sqlite")]
pub mod sqlite;

use axum::routing::get;
use axum::Router;

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
    command_bus, Command, CommandReceiver, CommandSender, OperationId, ResolvedLayout, SubmitError,
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
    no_preview, no_whep, FocusCaps, GatedWhep, NoPreview, NoWhep, PreviewProvider, SharedPreview,
    SharedWhep, WhepAnswer, WhepProvider, WhepReject, WhepScope,
};
pub use problem::{Problem, PROBLEM_JSON};
pub use realtime::{CorrKey, CorrRegistry, RealtimeFrame, SessionStream};
pub use repository::{InMemoryRepository, Layout, LayoutInput, Repository, VersionedLayout};
pub use resource_store::{
    DeviceKind, InMemoryDeviceStore, InMemoryOutputStore, InMemoryOverlayStore,
    InMemoryResourceStore, InMemorySourceStore, InMemorySyncGroupStore, OutputKind, OverlayKind,
    Resource, ResourceInput, ResourceKind, ResourceRepository, SourceKind, SyncGroupKind,
    VersionedResource, DEVICE_KIND, OUTPUT_KIND, OVERLAY_KIND, SOURCE_KIND, SYNC_GROUP_KIND,
};
pub use router::{
    ingest_route, route_follow, route_follow_all, RouteBinding, RouteFollowUpdate, RouteTable,
    RouterRoute,
};
pub use routing::{classify, DestinationProfile, RouteClass, RoutePlan, RouteRequest, RouteTarget};
pub use salvo_store::{InMemorySalvoStore, SalvoRepository, VersionedSalvo, SALVO_KIND};
pub use state::{
    seed_resources, AckClock, AppState, EngineStateSnapshot, LicenceState, SeededResources,
};
pub use tally_ingest::{run_tally_ingest, tally_ingest_step, TallyIngestStep};
pub use tally_state::{
    tally_observation, target_key, InMemoryProfileStore, OverrideRegistry, TallyEntry, TallyMirror,
    TallyProfileRepository, VersionedProfile, TALLY_PROFILE_KIND,
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
    let api = routes::api_router()
        .route("/ws", get(realtime::ws_handler))
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

    app.with_state(state)
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
/// Propagates any I/O error from the underlying [`axum::serve`] accept loop.
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
/// # Errors
/// Propagates any I/O error from the underlying [`axum::serve`] accept loop.
pub async fn serve_router<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
}
