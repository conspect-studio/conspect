//! The shared [`AppState`] the axum router carries.
//!
//! Per ADR-W001/W008 state sharing is an idiomatic `Arc<AppState>` holding the
//! engine's outbound subscription, the inbound command bus sender, the resource
//! repository, the auth store, and the idempotency/operation bookkeeping. Every
//! handle here is either control-only state or one of the engine's
//! isolation-safe channels — nothing in `AppState` can back-pressure the engine
//! (invariant #10).
use std::sync::Arc;

use multiview_config::MultiviewConfig;
use multiview_core::time::MediaTime;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_licence::verify::PinnedKey;
use multiview_licence::{ChallengeFile, LeaseStore};

use crate::account_audit::{AccountAuditRepository, InMemoryAccountAudit};
use crate::alarm_store::{AlarmRepository, InMemoryAlarmStore};
use crate::audio_routing::AudioRoutingStore;
use crate::audit::{AuditRepository, InMemoryAuditLog};
use crate::auth::ApiKeyStore;
use crate::command::CommandSender;
use crate::concurrency::IdempotencyStore;
use crate::devices::discovery::{DiscoveryBrowser, DiscoveryInventory, NullBrowser, ScanGate};
use crate::devices::{DeviceDriverRegistry, DevicePollerRegistry, DeviceStatusRegistry};
use crate::error::{ControlError, ControlResult};
use crate::nmos::NmosRegistry;
use crate::pending_actions::{InMemoryPendingActions, PendingActionRepository};
use crate::repository::{InMemoryRepository, LayoutInput, Repository};
use crate::resource_store::{
    InMemoryDeviceStore, InMemoryOutputStore, InMemoryOverlayStore, InMemoryProbeStore,
    InMemorySourceStore, InMemorySyncGroupStore, ResourceInput, ResourceRepository,
};
use crate::router::RouteTable;
use crate::salvo_store::{InMemorySalvoStore, SalvoRepository};
use crate::tally_state::{
    InMemoryProfileStore, OverrideRegistry, TallyMirror, TallyProfileRepository,
};
use crate::versioning::{ConfigVersionStore, InMemoryConfigVersionStore};
use crate::warning_store::{InMemoryWarningStore, WarningRepository};

/// A monotonic source of acknowledgement timestamps on the media timeline.
///
/// Acknowledging an alarm records *when* an operator acked it. The control plane
/// is not on the media clock, so it injects a clock here; the default reads the
/// system clock as nanoseconds-since-Unix-epoch (saturating into `i64`), and
/// tests inject a deterministic clock. The clock is read **off** the engine and
/// never touches the data plane.
pub type AckClock = Arc<dyn Fn() -> MediaTime + Send + Sync>;

/// The control-plane handle on the **local entitlement plane** (Conspect,
/// ADR-0050 / CONSPECT-1).
///
/// Bundles the in-memory verified-lease [`LeaseStore`] the `GET /api/v1/licence`
/// resource renders with the optional [`PinnedKey`] the
/// `POST /api/v1/licence/lease` install path verifies a presented binding
/// against. Both are control-plane-only: the store holds an `RwLock` over a
/// single verified entitlement read off the hot loop, and verification is pure
/// signature math — neither holds an engine handle, so the entitlement plane is
/// **physically incapable of back-pressuring the engine** (invariant #10) or of
/// taking a running program off air (invariant #1). Enforcement is **data** the
/// surface renders, never a control-flow decision here.
///
/// The `pinned` key is `None` for deployments that have not pinned an issuer
/// public key (e.g. an unconfigured machine or a store-only test); the install
/// route then refuses with a typed problem rather than installing an
/// unverifiable binding.
#[derive(Clone)]
pub struct LicenceState {
    /// The in-memory verified-lease store the licence resource reads.
    pub store: Arc<LeaseStore>,
    /// The pinned issuer verifying key the install path checks a binding against,
    /// or `None` when no key has been pinned (install is then refused).
    pub pinned: Option<PinnedKey>,
    /// The salted-digest + counter challenge the cli assembles for this machine
    /// (brief §3/§8), or `None` until the cli has gathered it (CONSPECT-10). The
    /// `GET /api/v1/licence/challenge` endpoint serves this verbatim; when `None`
    /// it serves an empty-but-well-formed challenge (zeroed counters, no digests)
    /// so the endpoint never fails and never leaks an identifier.
    pub challenge: Option<ChallengeFile>,
}

impl LicenceState {
    /// A licence state holding `store` and pinning `pinned` (or no key when
    /// `None`), with no challenge document gathered yet.
    #[must_use]
    pub fn new(store: Arc<LeaseStore>, pinned: Option<PinnedKey>) -> Self {
        Self {
            store,
            pinned,
            challenge: None,
        }
    }

    /// Attach the salted-digest + counter challenge the cli assembled for this
    /// machine (brief §3/§8). The control plane only renders it; it never gathers
    /// raw identifiers itself (data minimisation).
    #[must_use]
    pub fn with_challenge(mut self, challenge: ChallengeFile) -> Self {
        self.challenge = Some(challenge);
        self
    }
}

impl Default for LicenceState {
    /// An empty, unpinned licence state: a fresh [`LeaseStore`] (no lease
    /// installed), no pinned key, and no challenge gathered. This is the secure,
    /// never-off-air default — the resource reports "unlicensed" data and the
    /// install path refuses until a key is pinned.
    fn default() -> Self {
        Self::new(Arc::new(LeaseStore::new()), None)
    }
}

/// The engine state-snapshot type the realtime layer republishes.
///
/// The engine publishes its newest snapshot into the wait-free latest-state slot
/// (`EnginePublisher::state`); the control plane reads it to build the
/// subscribe-time snapshot frame. It is intentionally an opaque JSON value here
/// so this crate does not couple to the engine's internal state shape.
pub type EngineStateSnapshot = serde_json::Value;

/// The control-plane resource stores seeded from a loaded [`MultiviewConfig`].
///
/// Produced by [`seed_resources`] and installed onto an [`AppState`] with
/// [`AppState::with_seeded_resources`], so the web UI
/// Sources/Outputs/Overlays/Probes (and layout) pages are non-empty under a
/// live `multiview run` instead of starting blank. The stores are ordinary in-memory control-plane state:
/// read-mostly, never on the engine's data plane, so they cannot back-pressure
/// the engine (invariant #10). Seeding happens once at bind time, off the
/// per-tick hot loop.
pub struct SeededResources {
    /// The `sources` store, one resource per `config.sources`.
    pub sources: Arc<dyn ResourceRepository>,
    /// The `outputs` store, one resource per `config.outputs`.
    pub outputs: Arc<dyn ResourceRepository>,
    /// The `overlays` store, one resource per `config.overlays`.
    pub overlays: Arc<dyn ResourceRepository>,
    /// The `probes` store, one resource per `config.probes` (per-cell
    /// fail-state detection: black / freeze / silence / loudness).
    pub probes: Arc<dyn ResourceRepository>,
    /// The `devices` store, one resource per `config.devices` (the managed-device
    /// registry, ADR-M008).
    pub devices: Arc<dyn ResourceRepository>,
    /// The `sync-groups` store, one resource per `config.sync_groups`
    /// (presentation-sync groups, ADR-M008/M010).
    pub sync_groups: Arc<dyn ResourceRepository>,
    /// The device **status** registry, seeded with one `ADOPTING` runtime row
    /// per `config.devices` so a freshly-booted control plane answers
    /// `GET /devices/{id}/status` before any driver probe. Runtime state only —
    /// never persisted/exported.
    pub device_status: Arc<DeviceStatusRegistry>,
    /// The audio-routing singleton store, seeded from the config's optional
    /// `[audio]` block (unconfigured when the config carries none).
    pub audio: Arc<AudioRoutingStore>,
    /// The layout store carrying the single working layout (canvas + cells).
    pub layouts: Arc<dyn Repository>,
    /// The id the working layout was seeded under (the solved layout's name,
    /// else `"working"`) — the layout `GET /api/v1/config/export` composes
    /// canvas/layout/cells from.
    pub working_layout_id: String,
    /// The running session's **pinned canvas** (geometry + cadence), captured
    /// immutably from the loaded config at seed time (ADR-W019 / ADR-R004).
    /// The apply-layout route's Class-1 gate compares against THIS — never the
    /// mutable layouts repository, which any operator `PUT` can rewrite.
    pub running_canvas: multiview_config::LayoutCanvas,
}

/// Map a `serde_json` serialization fault to a repository error.
///
/// Serializing the config-as-code value types ([`Source`](multiview_config::Source)
/// / [`Output`](multiview_config::Output) / [`Overlay`](multiview_config::Overlay))
/// has no failing path in practice (plain derived `Serialize`, no non-string map
/// keys), but the guardrails forbid `unwrap`/`expect`, so the `Result` is
/// propagated rather than panicked.
fn to_body(value: &impl serde::Serialize) -> ControlResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|e| ControlError::Repository(format!("serializing a config resource body: {e}")))
}

/// Build fresh in-memory resource stores seeded from `config`, mirroring one
/// resource per `config.sources` / `config.outputs` / `config.overlays` plus the
/// single working layout (canvas + cells) into the layout store.
///
/// Each resource's `body` is the typed config value serialized to canonical JSON
/// (`serde_json::to_value`), so it round-trips back to the config type — engine-
/// side validation still happens at apply time; the store keeps the document
/// opaque (`resource_store` doc). Ids:
/// * **sources** / **overlays** use their intrinsic config `id`;
/// * **outputs** have no intrinsic id in the schema, so a stable, index-derived
///   `output-{n}` id is assigned in config order (deterministic across runs of
///   the same config).
///
/// The function never fails on an otherwise-runnable config: config validation
/// (run before this) already enforces unique source ids, so the `create` calls
/// cannot collide; a serialization fault (not expected for these derived types)
/// surfaces as [`ControlError::Repository`] rather than a panic.
///
/// Isolation (invariant #10): this allocates plain control-plane stores and runs
/// once at bind time — it touches no engine channel and is off the hot loop.
///
/// # Errors
///
/// [`ControlError::Repository`] if a config value fails to serialize, and any
/// [`ControlError`] a backing `create` surfaces (e.g. a duplicate id — not
/// expected for a validated config).
pub fn seed_resources(config: &MultiviewConfig) -> ControlResult<SeededResources> {
    let sources = InMemorySourceStore::new();
    for source in &config.sources {
        let name = source
            .display_name
            .clone()
            .unwrap_or_else(|| source.id.clone());
        sources.create(
            &source.id,
            ResourceInput {
                name,
                body: to_body(source)?,
            },
        )?;
    }

    let overlays = InMemoryOverlayStore::new();
    for overlay in &config.overlays {
        overlays.create(
            &overlay.id,
            ResourceInput {
                name: overlay.id.clone(),
                body: to_body(overlay)?,
            },
        )?;
    }

    let probes = InMemoryProbeStore::new();
    for probe in &config.probes {
        probes.create(
            &probe.id,
            ResourceInput {
                name: probe.id.clone(),
                body: to_body(probe)?,
            },
        )?;
    }

    // Managed devices: one resource per `config.devices`, body the typed config
    // value serialized to canonical JSON (round-trips back to `Device`). The
    // status registry is seeded in parallel with one ADOPTING runtime row per
    // device — runtime state only, never persisted/exported.
    let devices = InMemoryDeviceStore::new();
    let device_status = DeviceStatusRegistry::new();
    for device in &config.devices {
        let name = device
            .display_name
            .clone()
            .unwrap_or_else(|| device.id.clone());
        devices.create(
            &device.id,
            ResourceInput {
                name,
                body: to_body(device)?,
            },
        )?;
        device_status.ensure(&device.id);
    }

    // Presentation-sync groups: one resource per `config.sync_groups`.
    let sync_groups = InMemorySyncGroupStore::new();
    for group in &config.sync_groups {
        sync_groups.create(
            &group.id,
            ResourceInput {
                name: group.id.clone(),
                body: to_body(group)?,
            },
        )?;
    }

    let outputs = InMemoryOutputStore::new();
    for (index, output) in config.outputs.iter().enumerate() {
        // Outputs carry no intrinsic id in the config schema; assign a stable,
        // config-order id so the resource is addressable. The `kind` tag (read
        // back from the serialized body) is the human-friendly name.
        let body = to_body(output)?;
        let kind = body
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("output")
            .to_owned();
        let id = format!("output-{index}");
        outputs.create(&id, ResourceInput { name: kind, body })?;
    }

    let layouts = InMemoryRepository::new();
    let working_layout_id = seed_working_layout(config, &layouts)?;

    Ok(SeededResources {
        sources: Arc::new(sources),
        outputs: Arc::new(outputs),
        overlays: Arc::new(overlays),
        probes: Arc::new(probes),
        devices: Arc::new(devices),
        sync_groups: Arc::new(sync_groups),
        device_status: Arc::new(device_status),
        audio: Arc::new(AudioRoutingStore::seeded(config.audio.clone())),
        layouts: Arc::new(layouts),
        working_layout_id,
        // Snapshot the pinned canvas from the LOADED CONFIG (the geometry +
        // cadence the engine session is actually built with), immutably — a
        // later repository edit cannot move this (ADR-W019 MAJOR-1).
        running_canvas: multiview_config::LayoutCanvas::new(
            config.canvas.width,
            config.canvas.height,
            config.canvas.fps,
        ),
    })
}

/// Seed the single working layout (canvas + grid/layout strategy + cells) into
/// the layout store so the web UI layout page is non-empty under a live run.
///
/// The body is the authored shape — `{ canvas, layout, cells }` as canonical
/// JSON — kept opaque exactly like every other layout document; the editor reads
/// and the engine validates it on apply. The id/name is the solved working
/// layout's name when the config solves, else the stable fallback `"working"`
/// (seeding must not fail just because a config would not yet solve — it still
/// mirrors the authored cells).
fn seed_working_layout(
    config: &MultiviewConfig,
    layouts: &InMemoryRepository,
) -> ControlResult<String> {
    let id = config
        .solve_layout()
        .ok()
        .map_or_else(|| "working".to_owned(), |layout| layout.name);
    let body = serde_json::json!({
        "canvas": to_body(&config.canvas)?,
        "layout": to_body(&config.layout)?,
        "cells": to_body(&config.cells)?,
    });
    layouts.create_layout(
        &id,
        LayoutInput {
            name: id.clone(),
            body,
        },
    )?;
    Ok(id)
}

/// The shared application state.
///
/// Cloned cheaply (`Arc`) into every handler via axum's `State` extractor. It
/// bundles:
///
/// * `engine` — the engine's outbound [`EnginePublisher`]: a wait-free
///   latest-state slot plus a drop-oldest event broadcast. The control plane
///   only ever **reads/subscribes**, never sends on a path the engine awaits.
/// * `commands` — the inbound [`CommandSender`] (bounded, non-blocking
///   `try_submit`): the only channel control->engine, designed so it can never
///   block the engine.
/// * `repository` — the resource store (in-memory by default).
/// * `api_keys` — the API-key/RBAC store.
/// * `idempotency` — the `Idempotency-Key` dedupe store.
#[derive(Clone)]
pub struct AppState {
    /// The engine's outbound publisher (state slot + event broadcast).
    pub engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    /// The inbound, bounded, non-blocking command bus sender.
    pub commands: CommandSender,
    /// The resource repository (CRUD persistence).
    pub repository: Arc<dyn Repository>,
    /// The sources store (versioned CRUD over config-as-code managed inputs).
    pub sources: Arc<dyn ResourceRepository>,
    /// The outputs store (versioned CRUD over config-as-code managed sinks).
    pub outputs: Arc<dyn ResourceRepository>,
    /// The overlays store (versioned CRUD over config-as-code overlay layers).
    pub overlays: Arc<dyn ResourceRepository>,
    /// The probes store (versioned CRUD over config-as-code per-cell
    /// fail-state detectors: black / freeze / silence / loudness).
    pub probes: Arc<dyn ResourceRepository>,
    /// The devices store (versioned CRUD over the config-as-code managed-device
    /// registry, ADR-M008). The body is a `multiview_config::Device`.
    pub devices: Arc<dyn ResourceRepository>,
    /// The sync-groups store (versioned CRUD over config-as-code
    /// presentation-sync groups, ADR-M008/M010). The body is a
    /// `multiview_config::SyncGroup`.
    pub sync_groups: Arc<dyn ResourceRepository>,
    /// The latest-wins device **status** registry (runtime state, never
    /// persisted/exported): the conflated `device.status` lane's backing store
    /// and `GET /devices/{id}/status`'s cold-snapshot source. Bounded, control-
    /// plane-only, latest-wins — it can never back-pressure the engine
    /// (invariant #10).
    pub device_status: Arc<DeviceStatusRegistry>,
    /// The **untrusted** mDNS-discovery inventory (DEV-A5 / ADR-M008 §6 /
    /// ADR-0041): a bounded, TTL-expiring, dedup-keyed list of services found on
    /// the LAN. It is runtime state, never persisted/exported, and it is **never**
    /// the device registry — its rows are hints requiring explicit confirm-adopt
    /// (`POST /devices/{id}`). Bounded drop-oldest, control-plane-only — it can
    /// never back-pressure the engine (invariant #10).
    pub discovery: Arc<DiscoveryInventory>,
    /// The mDNS browse seam (DEV-A5): the only socket-touching part of discovery.
    /// The default ([`NullBrowser`]) finds nothing (the pure default build has no
    /// mDNS socket); the binary swaps in the real `mdns-sd`-backed browser behind
    /// the `discovery` feature, and tests inject a `StaticBrowser`. A scan runs
    /// this on a bounded control-plane task and publishes `device.discovered`
    /// (drop-oldest) — it never awaits a client (invariant #10).
    pub discovery_browser: Arc<dyn DiscoveryBrowser>,
    /// Single-flight admission for the discovery scan: **one in-flight mDNS
    /// browse** (concurrent `mdns-sd` browses of the same type overwrite each
    /// other's listeners, and either scan's `stop_browse` removes the other's
    /// live querier). A concurrent scan request attaches to the running scan's
    /// operation id. Also the scan rate limit (ADR-M008).
    pub discovery_scan_gate: Arc<ScanGate>,
    /// The `[discovery]` browse configuration (managed-devices brief §6): the
    /// operator-configured zowietek-control service type (the vendor's type is
    /// unverified — never fabricated) and any extra DNS-SD types to browse.
    /// Defaults to the empty section (built-in Cast + NDI types only).
    pub discovery_config: Arc<multiview_config::DiscoveryConfig>,
    /// The latest-wins device **driver** registry (runtime state, never
    /// persisted/exported): the source-candidate / output-target facets each
    /// driver (DEV-A4 `zowietek`, …) enumerated for its device, read by the
    /// `GET /devices/{id}/source-candidates` and `/output-targets` routes
    /// (ADR-M009). Empty until a driver enumerates — the routes' honest-empty
    /// fallback. Bounded, control-plane-only — it can never back-pressure the
    /// engine (invariant #10).
    pub device_drivers: Arc<DeviceDriverRegistry>,
    /// The runtime registry of **spawned** device poller actors (DEV-A4): adopt
    /// starts one for a `zowietek` device, delete stops it, and `set-mode`
    /// dispatches a convergence to the running actor. The default build uses the
    /// no-op factory (no live transport → no poller spawned, projection routes
    /// stay honestly empty); the binary installs the reqwest-backed factory
    /// behind the `zowietek` feature. Control-plane-only, `Mutex`-guarded handle
    /// map — it can never back-pressure the engine (invariant #10).
    pub device_pollers: Arc<DevicePollerRegistry>,
    /// The audio-routing singleton store (the document-level `[audio]` block:
    /// program-bus membership/gains and discrete-track wiring), managed over
    /// `GET`/`PUT /api/v1/audio-routing` and overlaid into the config export.
    pub audio_routing: Arc<AudioRoutingStore>,
    /// The alarm mirror store (versioned, fed from the engine event stream).
    pub alarms: Arc<dyn AlarmRepository>,
    /// The health-warning mirror store (SA-0 / ADR-0035): active capability
    /// mismatches (e.g. GPU present but compositing fell back to CPU) with their
    /// remediation, fed from the engine event stream via `warning_ingest`.
    pub warnings: Arc<dyn WarningRepository>,
    /// The salvo definition store (versioned CRUD over config-as-code salvos).
    pub salvos: Arc<dyn SalvoRepository>,
    /// The resolved-tally mirror (latest-wins, fed from the engine event stream).
    pub tally: Arc<TallyMirror>,
    /// The operator manual-tally-override registry (the control-plane record of
    /// override requests submitted to the engine).
    pub tally_overrides: Arc<OverrideRegistry>,
    /// The tally-profile store (versioned CRUD over config-as-code profiles).
    pub tally_profiles: Arc<dyn TallyProfileRepository>,
    /// The NMOS resource registry (node/device/sender/receiver + IS-05
    /// connection state) served by the NMOS Node API.
    pub nmos: Arc<NmosRegistry>,
    /// The router crosspoint mirror feeding route-follow (control-plane only).
    pub routes: Arc<RouteTable>,
    /// The API-key + RBAC store.
    pub api_keys: Arc<ApiKeyStore>,
    /// The optional `OAuth2`/JWT validator. When configured, a `Bearer` token that
    /// is not a native API key is validated as an IS-10-aligned JWT (signature +
    /// issuer/audience/expiry, `alg=none` refused) and its claims mapped to a
    /// [`Role`](crate::auth::Role). `None` means JWT auth is disabled (native
    /// API keys only). This is an **alternative** authn path, not a replacement:
    /// per-object/per-output authorization (BOLA defense) is enforced
    /// identically for both.
    pub jwt: Option<Arc<crate::jwt::JwtValidator>>,
    /// The audience name (resource server id) a JWT must target, and the NMOS
    /// API name the claim's grant is read against when mapping to a role. Unused
    /// when [`AppState::jwt`] is `None`.
    pub jwt_api_name: String,
    /// The change audit log: every successful mutation is recorded here
    /// (who/what/when) and queryable read-only over HTTP.
    pub audit: Arc<dyn AuditRepository>,
    /// The **account-side** append-only audit store (Conspect, ADR-0053 §4):
    /// every account action (lease install, remote action requested/cancelled/
    /// executed, salvo fire, consent/relay change, …) is recorded here as an
    /// immutable, timestamped, actor-attributed entry. Separate from the
    /// change-audit log so engine/config and account/licensing trails are not
    /// conflated. Control-plane-only; cannot back-pressure the engine (inv #10).
    pub account_audit: Arc<dyn AccountAuditRepository>,
    /// The **pending remote-actions queue** (Conspect, the brief §10/§11): the
    /// backend for the SPA pending-action strip. Fed **locally now** (operator
    /// restart/reboot/salvo) and **portal-fed later** (over the O1-blocked
    /// heartbeat/relay transport). Local cancel always wins. Control-plane-only;
    /// cannot back-pressure the engine (inv #10).
    pub pending_actions: Arc<dyn PendingActionRepository>,
    /// The config/layout revision store (immutable revisions + diff + rollback).
    pub config_versions: Arc<dyn ConfigVersionStore>,
    /// The `Idempotency-Key` deduplication store.
    pub idempotency: Arc<IdempotencyStore>,
    /// The command-outcome correlation registry: pairs each accepted command's
    /// [`OperationId`](crate::command::OperationId) with the realtime outcome
    /// event it will produce, so the outcome's envelope echoes the op id as
    /// `corr` (ADR-W008). Bounded, control-plane-only, drop-oldest — it can
    /// never back-pressure the engine (invariant #10).
    pub corr: Arc<crate::realtime::CorrRegistry>,
    /// The clock used to stamp alarm acknowledgements **and** audit entries.
    pub ack_clock: AckClock,
    /// The live-preview provider (program + per-input JPEG stills). The default
    /// ([`NoPreview`](crate::preview::NoPreview)) yields no frames; the binary
    /// swaps in an engine-backed provider. Isolation-safe (invariant #10).
    pub preview: crate::preview::SharedPreview,
    /// The WHEP focus transport seam (sub-second WebRTC focus per scope). The
    /// default ([`NoWhep`](crate::preview::NoWhep)) refuses every focus with an
    /// honest `503 fallback`; the binary swaps in a transport-backed provider
    /// (str0m / sidecar) behind a further gate. Isolation-safe (invariant #10):
    /// a focus session is a best-effort preview consumer that can never
    /// back-pressure the engine.
    pub whep: crate::preview::SharedWhep,
    /// Whether authentication is **disabled** (every request runs as a local
    /// admin). Off by default — the control plane requires a verified API key.
    /// An operator turns this on **explicitly** (config/env) for a trusted/local
    /// deployment; the binary logs a loud warning when it does. When `true`, the
    /// `Principal` extractor and the realtime `resolve_principal` short-circuit to
    /// [`Principal::local_admin`](crate::auth::Principal::local_admin) without a
    /// token, so the whole API + WS/SSE are open.
    pub auth_disabled: bool,
    /// The complete configuration document loaded at startup (serialized
    /// verbatim), used as the baseline for `GET /api/v1/config/export` so
    /// sections the resource stores do not carry (control, placement, audio,
    /// salvos, tally profiles, walls, routing) are never destroyed by an
    /// export round-trip. `None` for store-only deployments (tests).
    pub base_document: Option<Arc<serde_json::Value>>,
    /// The layout id `GET /api/v1/config/export` composes canvas/layout/cells
    /// from (set by seeding; `None` falls back to the first layout carrying a
    /// `canvas`).
    pub working_layout_id: Option<String>,
    /// The running session's **pinned canvas** snapshot (set by seeding,
    /// immutable thereafter — ADR-W019 / ADR-R004). The apply-layout Class-1
    /// gate compares stored layouts against this; when [`None`] (no seeded
    /// snapshot) the gate **fails closed** for document-carrying applies.
    pub running_canvas: Option<multiview_config::LayoutCanvas>,
    /// The local entitlement plane (Conspect, ADR-0050): the verified-lease store
    /// the `GET /api/v1/licence` resource renders and the optional pinned key the
    /// `POST /api/v1/licence/lease` install path verifies against. Control-plane
    /// only; it can never back-pressure the engine or take a program off air
    /// (invariant #1/#10). The default is empty + unpinned.
    pub licence: LicenceState,
}

/// The default [`AckClock`]: system time as nanoseconds since the Unix epoch.
#[must_use]
fn system_ack_clock() -> MediaTime {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    MediaTime::from_nanos(nanos)
}

impl AppState {
    /// Assemble an [`AppState`] from its parts, with an in-memory alarm store and
    /// the system acknowledgement clock.
    #[must_use]
    pub fn new(
        engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
        commands: CommandSender,
        repository: Arc<dyn Repository>,
        api_keys: Arc<ApiKeyStore>,
    ) -> Self {
        Self {
            engine,
            commands,
            repository,
            base_document: None,
            working_layout_id: None,
            running_canvas: None,
            sources: Arc::new(InMemorySourceStore::new()),
            outputs: Arc::new(InMemoryOutputStore::new()),
            overlays: Arc::new(InMemoryOverlayStore::new()),
            probes: Arc::new(InMemoryProbeStore::new()),
            devices: Arc::new(InMemoryDeviceStore::new()),
            sync_groups: Arc::new(InMemorySyncGroupStore::new()),
            device_status: Arc::new(DeviceStatusRegistry::new()),
            discovery: Arc::new(DiscoveryInventory::default()),
            discovery_browser: Arc::new(NullBrowser),
            discovery_scan_gate: Arc::new(ScanGate::new()),
            discovery_config: Arc::new(multiview_config::DiscoveryConfig::default()),
            device_drivers: Arc::new(DeviceDriverRegistry::new()),
            device_pollers: Arc::new(DevicePollerRegistry::new()),
            audio_routing: Arc::new(AudioRoutingStore::new()),
            alarms: Arc::new(InMemoryAlarmStore::new()),
            warnings: Arc::new(InMemoryWarningStore::new()),
            salvos: Arc::new(InMemorySalvoStore::new()),
            tally: Arc::new(TallyMirror::new()),
            tally_overrides: Arc::new(OverrideRegistry::new()),
            tally_profiles: Arc::new(InMemoryProfileStore::new()),
            nmos: Arc::new(NmosRegistry::new()),
            routes: Arc::new(RouteTable::new()),
            api_keys,
            jwt: None,
            jwt_api_name: "multiview".to_owned(),
            audit: Arc::new(InMemoryAuditLog::new()),
            account_audit: Arc::new(InMemoryAccountAudit::new()),
            pending_actions: Arc::new(InMemoryPendingActions::new()),
            config_versions: Arc::new(InMemoryConfigVersionStore::new()),
            idempotency: Arc::new(IdempotencyStore::new()),
            // Bound the in-flight correlations: a generous ceiling for pending
            // command outcomes (drop-oldest beyond it, invariant #10). A backlog
            // this deep means outcomes are not being consumed; the oldest
            // correlation is dropped and its outcome simply rides uncorrelated.
            corr: Arc::new(crate::realtime::CorrRegistry::new(256)),
            ack_clock: Arc::new(system_ack_clock),
            preview: crate::preview::no_preview(),
            whep: crate::preview::no_whep(),
            // Secure default: authentication is REQUIRED. An operator opts out
            // explicitly via `with_auth_disabled` (config/env), never silently.
            auth_disabled: false,
            // Empty + unpinned by default: the licence resource reports
            // unlicensed data and the install path refuses until the binary
            // pins an issuer key + wires a store (CONSPECT-10). Never off air.
            licence: LicenceState::default(),
        }
    }

    /// Wire the local entitlement plane (Conspect): the verified-lease store the
    /// licence resource renders and the pinned issuer key the install path
    /// verifies against. The binary calls this with the host store + pinned key;
    /// tests inject a clock-controlled store. Control-plane only (invariant
    /// #1/#10).
    #[must_use]
    pub fn with_licence(mut self, licence: LicenceState) -> Self {
        self.licence = licence;
        self
    }

    /// Replace the live-preview provider (the binary wires an engine-backed one;
    /// the default yields no frames).
    #[must_use]
    pub fn with_preview(mut self, preview: crate::preview::SharedPreview) -> Self {
        self.preview = preview;
        self
    }

    /// Replace the WHEP focus transport provider (the binary wires a
    /// transport-backed one; the default refuses every focus with a `fallback`).
    #[must_use]
    pub fn with_whep(mut self, whep: crate::preview::SharedWhep) -> Self {
        self.whep = whep;
        self
    }

    /// Enable `OAuth2`/JWT authentication as an alternative to API keys, validating
    /// tokens with `validator` and mapping the grant for `api_name` to a role.
    #[must_use]
    pub fn with_jwt(
        mut self,
        validator: Arc<crate::jwt::JwtValidator>,
        api_name: impl Into<String>,
    ) -> Self {
        self.jwt = Some(validator);
        self.jwt_api_name = api_name.into();
        self
    }

    /// **Disable** authentication (every request runs as a local admin). This is
    /// an explicit, opt-in trusted-network mode; the secure default keeps auth on.
    /// The binary calls this only when the operator set it via config/env, and
    /// logs a loud warning when it does.
    #[must_use]
    pub fn with_auth_disabled(mut self, disabled: bool) -> Self {
        self.auth_disabled = disabled;
        self
    }

    /// Whether a verified credential is required to reach privileged routes
    /// (`true` in the default secure mode; `false` when auth is disabled). Surfaced
    /// unauthenticated via `GET /api/v1/auth/status` so the SPA can decide whether
    /// to prompt for a key.
    #[must_use]
    pub fn auth_required(&self) -> bool {
        !self.auth_disabled
    }

    /// Replace the audit log (e.g. to share one store with a test or a
    /// persistent backend).
    #[must_use]
    pub fn with_audit_log(mut self, audit: Arc<dyn AuditRepository>) -> Self {
        self.audit = audit;
        self
    }

    /// Replace the config-version store (e.g. to share one with a test).
    #[must_use]
    pub fn with_config_versions(mut self, config_versions: Arc<dyn ConfigVersionStore>) -> Self {
        self.config_versions = config_versions;
        self
    }

    /// Record a successful mutation in the audit log, stamped with the current
    /// acknowledgement clock. Convenience used by the mutating handlers so the
    /// who/what/when is captured in one call after the mutation succeeds.
    pub fn audit(
        &self,
        actor: &str,
        action: crate::audit::AuditAction,
        object_kind: &str,
        object_id: &str,
        detail: Option<serde_json::Value>,
    ) {
        self.audit.record(
            actor,
            action,
            object_kind,
            object_id,
            self.ack_now(),
            detail,
        );
    }

    /// Replace the account-side append-only audit store (e.g. to share one with a
    /// test, or to use a persistent backend).
    #[must_use]
    pub fn with_account_audit(mut self, account_audit: Arc<dyn AccountAuditRepository>) -> Self {
        self.account_audit = account_audit;
        self
    }

    /// Replace the pending remote-actions queue (e.g. to share one with a test).
    #[must_use]
    pub fn with_pending_actions(
        mut self,
        pending_actions: Arc<dyn PendingActionRepository>,
    ) -> Self {
        self.pending_actions = pending_actions;
        self
    }

    /// Record an account-side action in the append-only account audit store,
    /// stamped with the current acknowledgement clock. Convenience the account
    /// handlers + the lease-install seam call so the who/what/when is captured in
    /// one place. The store's assigned `seq` is not surfaced here (callers do not
    /// need it); query the trail to observe it.
    pub fn audit_account(
        &self,
        actor: &str,
        kind: crate::account_audit::AccountAuditKind,
        detail: Option<serde_json::Value>,
    ) {
        let _seq = self
            .account_audit
            .record(actor, kind, self.ack_now(), detail);
    }

    /// Replace the alarm store (e.g. to share one store with an ingest task or
    /// to use the SQLite-backed implementation).
    #[must_use]
    pub fn with_alarm_store(mut self, alarms: Arc<dyn AlarmRepository>) -> Self {
        self.alarms = alarms;
        self
    }

    /// Replace the health-warning store (e.g. to share one store with the
    /// `warning_ingest` task so engine-emitted warnings surface over
    /// `GET /api/v1/health`).
    #[must_use]
    pub fn with_warning_store(mut self, warnings: Arc<dyn WarningRepository>) -> Self {
        self.warnings = warnings;
        self
    }

    /// Replace the salvo store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_salvo_store(mut self, salvos: Arc<dyn SalvoRepository>) -> Self {
        self.salvos = salvos;
        self
    }

    /// Replace the sources store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_sources_store(mut self, sources: Arc<dyn ResourceRepository>) -> Self {
        self.sources = sources;
        self
    }

    /// Replace the outputs store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_outputs_store(mut self, outputs: Arc<dyn ResourceRepository>) -> Self {
        self.outputs = outputs;
        self
    }

    /// Replace the overlays store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_overlays_store(mut self, overlays: Arc<dyn ResourceRepository>) -> Self {
        self.overlays = overlays;
        self
    }

    /// Replace the probes store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_probes_store(mut self, probes: Arc<dyn ResourceRepository>) -> Self {
        self.probes = probes;
        self
    }

    /// Replace the devices store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_devices_store(mut self, devices: Arc<dyn ResourceRepository>) -> Self {
        self.devices = devices;
        self
    }

    /// Replace the sync-groups store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_sync_groups_store(mut self, sync_groups: Arc<dyn ResourceRepository>) -> Self {
        self.sync_groups = sync_groups;
        self
    }

    /// Replace the device status registry (e.g. to share one with a driver
    /// poller / broadcaster).
    #[must_use]
    pub fn with_device_status(mut self, device_status: Arc<DeviceStatusRegistry>) -> Self {
        self.device_status = device_status;
        self
    }

    /// Replace the mDNS browse seam (DEV-A5). The binary installs the real
    /// `mdns-sd`-backed browser (behind the `discovery` feature); tests inject a
    /// `StaticBrowser`. The browser is the only socket-touching part of
    /// discovery; the scan task runs it off the engine path (invariant #10).
    #[must_use]
    pub fn with_discovery_browser(mut self, browser: Arc<dyn DiscoveryBrowser>) -> Self {
        self.discovery_browser = browser;
        self
    }

    /// Replace the untrusted discovery inventory (e.g. to share one with a test).
    #[must_use]
    pub fn with_discovery_inventory(mut self, discovery: Arc<DiscoveryInventory>) -> Self {
        self.discovery = discovery;
        self
    }

    /// Set the `[discovery]` browse configuration from the loaded config: the
    /// operator-configured zowietek-control service type and any extra DNS-SD
    /// types to browse. The binary threads `MultiviewConfig::discovery` here;
    /// the default is the empty section (built-in Cast + NDI types only).
    #[must_use]
    pub fn with_discovery_config(mut self, config: multiview_config::DiscoveryConfig) -> Self {
        self.discovery_config = Arc::new(config);
        self
    }

    /// Replace the device **driver** registry (e.g. to share one with the
    /// `zowietek` driver actors so their enumerated facets reach the
    /// source-candidate / output-target routes — ADR-M009, DEV-A4).
    #[must_use]
    pub fn with_device_drivers(mut self, device_drivers: Arc<DeviceDriverRegistry>) -> Self {
        self.device_drivers = device_drivers;
        self
    }

    /// Replace the runtime device **poller** registry (DEV-A4): the binary
    /// installs one carrying the reqwest-backed [`DevicePollerFactory`](crate::devices::DevicePollerFactory)
    /// (feature `zowietek`) so adopting a `zowietek` device spawns a live
    /// supervised poller; tests inject a scripted factory.
    #[must_use]
    pub fn with_device_pollers(mut self, device_pollers: Arc<DevicePollerRegistry>) -> Self {
        self.device_pollers = device_pollers;
        self
    }

    /// The control-plane wiring a spawned poller actor needs (the broadcaster it
    /// publishes through and the driver registry it enumerates facets into),
    /// assembled from this state. The broadcaster's status registry is this
    /// state's [`device_status`](AppState::device_status), so a poller's
    /// published status reaches `GET /devices/{id}/status`.
    #[must_use]
    pub fn poller_wiring(&self) -> crate::devices::PollerWiring {
        crate::devices::PollerWiring {
            broadcaster: crate::devices::DeviceBroadcaster::new(
                Arc::clone(&self.engine),
                Arc::clone(&self.device_status),
            ),
            drivers: Arc::clone(&self.device_drivers),
        }
    }

    /// Boot-seed: start a supervised poller for every config-declared device
    /// (DEV-A4), so a `multiview run` that loads a config with `[[devices]]`
    /// brings each managed device online (login → probe → enumerate facets →
    /// poll) without an operator re-adopt. A no-op for devices the poller
    /// factory does not manage (the default build's no-op factory spawns
    /// nothing). Called once at bind time, off the engine hot loop (invariant
    /// #10). Returns the number of pollers spawned.
    #[allow(clippy::must_use_candidate)] // count is informational at the call site.
    pub fn seed_device_pollers(&self, devices: &[multiview_config::Device]) -> usize {
        let wiring = self.poller_wiring();
        devices
            .iter()
            .filter(|device| self.device_pollers.start(device, &wiring))
            .count()
    }

    /// Replace the audio-routing singleton store (e.g. to share one seeded
    /// store with a test).
    #[must_use]
    pub fn with_audio_routing(mut self, audio_routing: Arc<AudioRoutingStore>) -> Self {
        self.audio_routing = audio_routing;
        self
    }

    /// Install resource stores seeded from a loaded config ([`seed_resources`]),
    /// replacing the empty default sources/outputs/overlays stores **and** the
    /// layout repository in one call.
    ///
    /// The binary uses this so the web UI resource pages reflect the running
    /// config's sources/outputs/overlays/layout. Read-mostly control-plane state;
    /// installed once at bind time, off the engine hot loop (invariant #10).
    #[must_use]
    pub fn with_seeded_resources(mut self, seeded: SeededResources) -> Self {
        self.sources = seeded.sources;
        self.outputs = seeded.outputs;
        self.overlays = seeded.overlays;
        self.probes = seeded.probes;
        self.devices = seeded.devices;
        self.sync_groups = seeded.sync_groups;
        self.device_status = seeded.device_status;
        self.audio_routing = seeded.audio;
        self.repository = seeded.layouts;
        self.working_layout_id = Some(seeded.working_layout_id);
        self.running_canvas = Some(seeded.running_canvas);
        self
    }

    /// Install the running session's pinned-canvas snapshot (ADR-W019): the
    /// immutable geometry + cadence the apply-layout Class-1 gate compares
    /// stored layouts against. Set by [`AppState::with_seeded_resources`] in
    /// the binary; exposed separately for store-only deployments and tests.
    #[must_use]
    pub fn with_running_canvas(mut self, canvas: multiview_config::LayoutCanvas) -> Self {
        self.running_canvas = Some(canvas);
        self
    }

    /// Install the loaded configuration document as the export baseline
    /// (`GET /api/v1/config/export` overlays the live stores onto it, so
    /// authored sections the stores do not carry survive the round-trip).
    #[must_use]
    pub fn with_base_document(mut self, document: serde_json::Value) -> Self {
        self.base_document = Some(Arc::new(document));
        self
    }

    /// Designate the layout id the export composes canvas/layout/cells from.
    #[must_use]
    pub fn with_working_layout_id(mut self, id: impl Into<String>) -> Self {
        self.working_layout_id = Some(id.into());
        self
    }

    /// Replace the resolved-tally mirror (e.g. to share one with the tally
    /// ingest task or a test).
    #[must_use]
    pub fn with_tally_mirror(mut self, tally: Arc<TallyMirror>) -> Self {
        self.tally = tally;
        self
    }

    /// Replace the tally-profile store.
    #[must_use]
    pub fn with_tally_profiles(mut self, profiles: Arc<dyn TallyProfileRepository>) -> Self {
        self.tally_profiles = profiles;
        self
    }

    /// Replace the NMOS registry (e.g. to share one seeded registry with a test
    /// or the NMOS registration task).
    #[must_use]
    pub fn with_nmos(mut self, nmos: Arc<NmosRegistry>) -> Self {
        self.nmos = nmos;
        self
    }

    /// Replace the router crosspoint mirror (e.g. to share one with a
    /// route-follow ingest task or a test).
    #[must_use]
    pub fn with_routes(mut self, routes: Arc<RouteTable>) -> Self {
        self.routes = routes;
        self
    }

    /// Replace the acknowledgement clock (used by tests for determinism).
    #[must_use]
    pub fn with_ack_clock(mut self, ack_clock: AckClock) -> Self {
        self.ack_clock = ack_clock;
        self
    }

    /// The current acknowledgement timestamp on the media timeline.
    #[must_use]
    pub fn ack_now(&self) -> MediaTime {
        (self.ack_clock)()
    }

    /// Attempt `OAuth2`/JWT authentication of an `Authorization` header value.
    ///
    /// Returns:
    /// * `None` — JWT auth is not configured, or the header is absent/not a
    ///   `Bearer` token (so the caller keeps the native API-key error).
    /// * `Some(Ok(principal))` — the JWT validated (signature + issuer/audience/
    ///   expiry, `alg=none` refused) and its grant mapped to a
    ///   [`Principal`](crate::Principal).
    /// * `Some(Err(_))` — a `Bearer` token was present and a validator is
    ///   configured, but validation failed; the caller surfaces this rejection.
    ///
    /// The validation time is read from the acknowledgement clock (Unix seconds),
    /// off the engine.
    #[must_use]
    pub fn authenticate_jwt(
        &self,
        header_value: Option<&str>,
    ) -> Option<Result<crate::auth::Principal, crate::error::ControlError>> {
        let validator = self.jwt.as_ref()?;
        let value = header_value?;
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))?
            .trim();
        let now_secs = self.ack_now().as_nanos().div_euclid(1_000_000_000);
        Some(self.map_jwt(validator, token, now_secs))
    }

    /// Validate `token` and map its claims to a [`Principal`](crate::Principal).
    fn map_jwt(
        &self,
        validator: &crate::jwt::JwtValidator,
        token: &str,
        now_secs: i64,
    ) -> Result<crate::auth::Principal, crate::error::ControlError> {
        let claims = validator
            .validate(token, now_secs)
            .map_err(|_| crate::error::ControlError::Unauthenticated)?;
        // The subject is the audit/authz identity; the NMOS grant for the
        // configured API maps to a role. A token granting no usable access is
        // forbidden (authenticated but unauthorized).
        let role = claims.role_for(&self.jwt_api_name).map_err(|_| {
            crate::error::ControlError::Forbidden(format!(
                "JWT grants no access to API {:?}",
                self.jwt_api_name
            ))
        })?;
        Ok(crate::auth::Principal {
            key_id: claims.sub,
            role,
            // JWT principals are not object/output scoped here; per-object and
            // per-output BOLA guards still run and pass (unscoped). A deployment
            // mapping JWT claims to scopes would populate these.
            scoped_object_ids: None,
            scoped_output_ids: None,
        })
    }
}

#[cfg(test)]
mod seed_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use multiview_config::{MultiviewConfig, Output, Overlay, Probe, Source};

    use super::seed_resources;

    /// A config carrying 3 sources, 2 outputs, 1 overlay, and 2 probes (plus
    /// the canvas/layout/cells the parser requires).
    const SEED_DOC: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "cam_a"
display_name = "Camera A"
kind = "rtsp"
url = "rtsp://x/a"
[[sources]]
id = "cam_b"
kind = "hls"
url = "https://x/b.m3u8"
[[sources]]
id = "cam_c"
kind = "ndi"
name = "STUDIO (CAM C)"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "cam_b"
[[overlays]]
id = "clock_1"
kind = "clock"
target = "canvas"
[[probes]]
id = "black_a"
cell = "cell_a"
kind = "black"
luma_threshold = 16
[[probes]]
id = "silence_b"
cell = "cell_b"
kind = "silence"
level_dbfs = -60.0
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
[[outputs]]
kind = "ll_hls"
path = "/srv/hls"
codec = "h264"
"##;

    /// A minimal config with NO sources/outputs/overlays (still parses: canvas +
    /// grid layout only).
    const EMPTY_DOC: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
"##;

    #[test]
    fn seeds_one_resource_per_config_source() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        let listed = seeded.sources.list().expect("list sources");
        let ids: Vec<&str> = listed.iter().map(|v| v.resource.id.as_str()).collect();
        // id-sorted, exactly the three config source ids.
        assert_eq!(ids, vec!["cam_a", "cam_b", "cam_c"]);

        // The display name is mirrored when present, else the id.
        let cam_a = seeded.sources.get("cam_a").expect("cam_a present");
        assert_eq!(cam_a.resource.name, "Camera A");
        let cam_b = seeded.sources.get("cam_b").expect("cam_b present");
        assert_eq!(cam_b.resource.name, "cam_b");
    }

    #[test]
    fn mirror_roundtrips_source_body() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        for want in &config.sources {
            let stored = seeded.sources.get(&want.id).expect("source present");
            let got: Source =
                serde_json::from_value(stored.resource.body.clone()).expect("body is a Source");
            assert_eq!(
                &got, want,
                "seeded body must round-trip to the config source"
            );
        }
    }

    #[test]
    fn seeds_outputs_and_overlays_with_roundtrip_bodies() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        let outputs = seeded.outputs.list().expect("list outputs");
        assert_eq!(outputs.len(), 2, "two config outputs seeded");
        // Each stored output body round-trips to the typed config Output, in order.
        for (versioned, want) in outputs.iter().zip(config.outputs.iter()) {
            let got: Output =
                serde_json::from_value(versioned.resource.body.clone()).expect("body is an Output");
            assert_eq!(&got, want);
        }

        let overlays = seeded.overlays.list().expect("list overlays");
        let overlay_ids: Vec<&str> = overlays.iter().map(|v| v.resource.id.as_str()).collect();
        assert_eq!(overlay_ids, vec!["clock_1"]);
        let stored = seeded.overlays.get("clock_1").expect("overlay present");
        let got: Overlay =
            serde_json::from_value(stored.resource.body.clone()).expect("body is an Overlay");
        assert_eq!(got, config.overlays[0]);
    }

    #[test]
    fn seeds_probes_with_roundtrip_bodies() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        let probes = seeded.probes.list().expect("list probes");
        let ids: Vec<&str> = probes.iter().map(|v| v.resource.id.as_str()).collect();
        assert_eq!(ids, vec!["black_a", "silence_b"], "id-sorted config probes");
        for want in &config.probes {
            let stored = seeded.probes.get(&want.id).expect("probe present");
            let got: Probe =
                serde_json::from_value(stored.resource.body.clone()).expect("body is a Probe");
            assert_eq!(
                &got, want,
                "seeded body must round-trip to the config probe"
            );
        }
    }

    #[test]
    fn empty_config_yields_empty_stores() {
        let config = MultiviewConfig::load_from_toml(EMPTY_DOC).expect("parse empty config");
        let seeded = seed_resources(&config).expect("seed resources");

        assert!(seeded.sources.list().expect("list").is_empty());
        assert!(seeded.outputs.list().expect("list").is_empty());
        assert!(seeded.overlays.list().expect("list").is_empty());
        assert!(seeded.probes.list().expect("list").is_empty());
        let (audio, _) = seeded.audio.snapshot();
        assert!(
            audio.is_none(),
            "no [audio] block seeds no routing document"
        );
    }

    #[test]
    fn seeds_the_audio_routing_singleton_from_the_audio_block() {
        // SEED_DOC + an [audio] block: the singleton store mirrors it so the
        // Audio page is non-empty under a live `multiview run`.
        let doc = format!(
            "{SEED_DOC}\n[audio]\nsample_rate_hz = 48000\n\n[[audio.routes]]\n\
             input_id = \"cam_a\"\ntarget_track = \"cam-a-clean\"\n\
             include_in_program_bus = true\ngain_db = -3.0\n\n\
             [audio.routes.channels]\nkind = \"stereo\"\n"
        );
        let config = MultiviewConfig::load_from_toml(&doc).expect("parse audio config");
        let seeded = seed_resources(&config).expect("seed resources");

        let (audio, _) = seeded.audio.snapshot();
        let routing = audio.expect("the [audio] block is seeded");
        assert_eq!(routing.sample_rate_hz, 48_000);
        assert_eq!(routing.routes.len(), 1);
        assert_eq!(routing.routes[0].input_id, "cam_a");
        assert_eq!(
            routing.routes[0].target_track.as_deref(),
            Some("cam-a-clean")
        );
    }

    #[test]
    fn seeds_a_layout_resource_from_canvas_and_cells() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        // The single working layout is mirrored so the web UI layout page is
        // non-empty under a live run; its body carries the two authored cells.
        let layouts = seeded.layouts.list_layouts().expect("list layouts");
        assert_eq!(layouts.len(), 1, "one working layout seeded");
        let body = &layouts[0].layout.body;
        let cells = body
            .get("cells")
            .and_then(|c| c.as_array())
            .expect("layout body carries a cells array");
        assert_eq!(
            cells.len(),
            2,
            "both authored cells mirrored into the layout"
        );
    }
}
