//! The **telemetry** pipe's local REST surface under `/api/v1/telemetry` and the
//! **diagnostics-snapshot** surface under `/api/v1/diagnostics` (Conspect,
//! ADR-0052 §2/§3/§4, ADR-0053, the brief §7.1/§10, spec §4.2/§11).
//!
//! Five endpoints:
//!
//! * `GET /api/v1/telemetry/consent` — the consent record `{enabled, changed_at,
//!   actor}`. **Off by default** (opt-in, incl. the free tier). Role: read.
//! * `PUT /api/v1/telemetry/consent` `{enabled}` — set consent, recorded on the
//!   machine under **last-writer-wins** by timestamp (a machine-UI write is the
//!   `local` actor, stamped from the acknowledgement clock). Writes a
//!   [`AccountAuditKind::ConsentChange`](crate::account_audit::AccountAuditKind::ConsentChange)
//!   entry to the account-side append-only audit store (#106). Role: write.
//! * `GET /api/v1/telemetry/schema` — the published daily-pipe schema: a version,
//!   the exhaustive list of categories **sent** when consented, and the exhaustive
//!   list of categories **never** sent (media, stream URLs, hostnames, layouts,
//!   anything typed). Role: read.
//! * `POST /api/v1/diagnostics/snapshot` → `202 {snapshot_id}` — assemble the §4.2
//!   one-button support bundle (logs + engine state, **never** media) from the
//!   consent-independent local metrics retention buffer (ADR-0053). Role: write.
//! * `GET /api/v1/diagnostics/{id}` — the assembled bundle when ready. Role: read.
//!
//! # Two-pipe separation (ADR-0052 §1) — pinned here
//!
//! This is the **telemetry** pipe. It must **never** be co-mingled with the
//! licensing **heartbeat** in naming, grouping, transport, or copy: the
//! heartbeat-status surface lives under `/api/v1/licensing/`
//! ([`crate::routes::licence::get_heartbeat_status`]); this consent + schema +
//! diagnostics surface lives under `/api/v1/telemetry/` and `/api/v1/diagnostics/`.
//! Co-mingling them would make "opt out of telemetry" look like "lose your
//! licence", which is a hard operator directive against. The separation is pinned
//! by the route tests.
//!
//! # Consent gates nothing locally (ADR-0052)
//!
//! No handler in this module — or anywhere in the control plane — consults the
//! consent record to gate a local route. The record governs **only** the (future,
//! O1-gated) outbound daily telemetry pipe. Staying off costs none of the local
//! UI/API. Pinned by `consent_gates_no_local_route`.
//!
//! # Isolation (invariant #10)
//!
//! Every handler reads/writes control-plane state (the consent `Mutex`, the
//! snapshot store, the retention buffer) off the engine hot loop, never holding a
//! lock the engine holds and never awaiting a client.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use multiview_core::time::MediaTime;

use crate::account_audit::AccountAuditKind;
use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::problem::Problem;
use crate::state::AppState;
use crate::support_bundle::{
    compose_bundle, Bundle, BundleInclude, BundleRequest, BundleWindow, ConfigSources,
};
use crate::telemetry_consent::{ConsentActor, SnapshotStatus};

/// The version of the published telemetry schema. Bumped when the daily-pipe
/// payload categories change; stable across the machine and the portal.
pub const TELEMETRY_SCHEMA_VERSION: &str = "1";

/// The exhaustive list of categories the daily telemetry pipe **sends** when
/// consented (ADR-0052 §1/§4, brief §7/§8): aggregated/anonymised counts and
/// histograms only — never raw identifiers. Stable slugs pinned by test.
const TELEMETRY_SENT_CATEGORIES: &[&str] = &[
    // The schema version of the payload itself.
    "schema_version",
    // OS + CPU architecture (e.g. "linux/x86_64").
    "os_arch",
    // The salted, non-reversible machine-fingerprint digest (never a raw serial).
    "anonymous_digest",
    // The mix of ingest/egress protocols + codecs in use, as counts.
    "protocol_codec_mix",
    // How many tiles/cells the multiview composites, as counts.
    "tile_counts",
    // Performance percentiles (CPU/GPU utilisation, output cadence health).
    "performance_percentiles",
    // Error *classes* (categorised counts), never error strings or stream context.
    "error_classes",
];

/// The exhaustive list of categories the telemetry pipe **NEVER** sends — the
/// hard privacy guarantee across both pipes and the mesh (ADR-0052 §4, brief §8).
/// **Load-bearing**: pinned by test so a "tidy" cannot silently leak a category
/// into the daily pipe. None of these ever leaves the machine via telemetry.
const TELEMETRY_NEVER_SENT_CATEGORIES: &[&str] = &[
    // Decoded/encoded media or any frame data.
    "media",
    // Source/output stream URLs.
    "stream_urls",
    // Machine/network hostnames.
    "hostnames",
    // Layout/template documents (the operator's composition).
    "layouts",
    // Anything the operator typed (labels, names, captions, notes).
    "typed_content",
    // Raw identifiers: serials, MAC addresses, file paths with user data,
    // any direct hardware identifier.
    "raw_identifiers",
];

/// The `GET /api/v1/telemetry/consent` body: the recorded consent document
/// (ADR-0052 §2). `enabled` is the outbound-daily-pipe consent (off by default);
/// `changed_at` is the RFC 3339 instant it was last written (absent when never
/// written); `actor` is who wrote it (`local` | `portal`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct ConsentResource {
    /// Whether the outbound daily telemetry pipe is consented (default `false`).
    pub enabled: bool,
    /// When the record was last written (RFC 3339); `None` when never written
    /// (the opt-in default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
    /// Who wrote the record: `local` (the machine UI/API) or `portal`.
    pub actor: ConsentActor,
}

/// The `PUT /api/v1/telemetry/consent` request body: the new consent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ConsentSetRequest {
    /// Whether to consent to the outbound daily telemetry pipe.
    pub enabled: bool,
}

/// The `GET /api/v1/telemetry/schema` body: the published daily-pipe schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct TelemetrySchema {
    /// The schema version (stable across machine + portal).
    pub version: String,
    /// The exhaustive list of categories sent when consented.
    pub sent: Vec<String>,
    /// The exhaustive list of categories that are NEVER sent.
    pub never_sent: Vec<String>,
}

/// The `202 Accepted` body of `POST /api/v1/diagnostics/snapshot`: the id to read
/// the assembled bundle back with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SnapshotAccepted {
    /// The id of the assembled diagnostics snapshot.
    pub snapshot_id: String,
}

/// The assembled diagnostics snapshot bundle (`GET /api/v1/diagnostics/{id}`).
///
/// The `diagnostics` section is the **shared** [`crate::support_bundle::Bundle`]
/// the #111 context-pack composer produces — utilisation percentiles + shed/
/// reconnect counts, incident markers, and the redacted config — so this snapshot
/// and the support-ticket context-pack draw their diagnostics from one source of
/// truth (the consent-independent retention store + the config redactor). Logs +
/// engine state, **never** media, raw identifiers, or secrets (brief §8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct DiagnosticsSnapshot {
    /// The snapshot id.
    pub snapshot_id: String,
    /// The lifecycle status (`ready` when readable).
    pub status: SnapshotStatus,
    /// The Unix second the snapshot was assembled.
    pub assembled_at_unix_seconds: u64,
    /// The redacted diagnostics bundle (the shared #111 composer output): logs +
    /// engine state, never media.
    pub diagnostics: Bundle,
}

/// Render a [`crate::telemetry_consent::ConsentRecord`] as the wire resource.
fn consent_resource(record: crate::telemetry_consent::ConsentRecord) -> ConsentResource {
    // `changed_at` is the media-timeline instant the record was last written.
    // `None` when never written (instant zero = the opt-in default), else the
    // RFC 3339 rendering of the Unix-nanosecond instant.
    let changed_at = if record.changed_at.as_nanos() == 0 {
        None
    } else {
        chrono::DateTime::from_timestamp_nanos(record.changed_at.as_nanos())
            .to_rfc3339()
            .into()
    };
    ConsentResource {
        enabled: record.enabled,
        changed_at,
        actor: record.actor,
    }
}

/// `GET /api/v1/telemetry/consent` — the recorded telemetry-consent document
/// (role: read).
///
/// Always `200`: a data report. Off by default (opt-in, incl. the free tier).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/telemetry/consent",
        tag = "telemetry",
        responses(
            (status = 200, description = "The recorded telemetry-consent document (off by default).", body = ConsentResource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_consent(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<ConsentResource>> {
    principal.role.require(Action::Read)?;
    Ok(Json(consent_resource(state.consent.record())))
}

/// `PUT /api/v1/telemetry/consent` — set telemetry consent (role: write).
///
/// Recorded on the machine under last-writer-wins by timestamp: a machine-UI
/// write is the `local` actor, stamped from the acknowledgement clock. A
/// successful change writes a
/// [`AccountAuditKind::ConsentChange`](crate::account_audit::AccountAuditKind::ConsentChange)
/// entry to the account-side append-only audit store (#106) and the change-audit
/// log. The detail carries only the new boolean — never a raw identifier (data
/// minimisation, brief §8).
///
/// Returns the updated consent resource. Because the local clock is monotonic the
/// machine-UI write always advances the timestamp, so it always applies; the LWW
/// guard exists for the portal mirror's later, out-of-order writes (pinned by the
/// `ConsentState` unit + integration LWW tests).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/telemetry/consent",
        tag = "telemetry",
        request_body = ConsentSetRequest,
        responses(
            (status = 200, description = "Consent updated; the new consent document.", body = ConsentResource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to write.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn set_consent(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ConsentSetRequest>,
) -> ControlResult<Json<ConsentResource>> {
    principal.role.require(Action::Write)?;
    // A machine-UI edit is the LOCAL actor, stamped from the ack clock (monotonic;
    // off the engine). LWW applies the write (a later local write always wins over
    // any earlier portal mirror).
    let now = state.ack_now();
    state.consent.apply(req.enabled, now, ConsentActor::Local);
    // The change-audit log (the engine/config change trail): the object id is the
    // coarse `telemetry` toggle, the detail the new state.
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        "telemetry.consent",
        "consent",
        Some(serde_json::json!({ "enabled": req.enabled })),
    );
    // The account-side evidence trail (#106 / ADR-0053 §4): an immutable,
    // timestamped, actor-attributed ConsentChange. Detail carries only the new
    // boolean — never a raw identifier (brief §8).
    state.audit_account(
        &principal.key_id,
        AccountAuditKind::ConsentChange,
        Some(serde_json::json!({ "enabled": req.enabled })),
    );
    Ok(Json(consent_resource(state.consent.record())))
}

/// `GET /api/v1/telemetry/schema` — the published daily-pipe telemetry schema
/// (role: read).
///
/// A static, versioned report of exactly what the daily pipe sends when consented
/// and — load-bearing — what it NEVER sends. Served from the versioned consts so
/// the never-sent guarantee is a single source of truth pinned by test.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/telemetry/schema",
        tag = "telemetry",
        responses(
            (status = 200, description = "The published daily-pipe telemetry schema (sent + never-sent).", body = TelemetrySchema),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_schema(
    State(_state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<TelemetrySchema>> {
    principal.role.require(Action::Read)?;
    Ok(Json(TelemetrySchema {
        version: TELEMETRY_SCHEMA_VERSION.to_owned(),
        sent: TELEMETRY_SENT_CATEGORIES
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        never_sent: TELEMETRY_NEVER_SENT_CATEGORIES
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    }))
}

/// `POST /api/v1/diagnostics/snapshot` — assemble the §4.2 one-button support
/// bundle (role: write).
///
/// Returns `202 {snapshot_id}`; the assembled bundle is then readable at
/// `GET /api/v1/diagnostics/{id}`. The bundle is logs + engine state (the local
/// retention buffer), **never** media (ADR-0053, brief §8). Assembly is
/// synchronous (the buffer is in-memory) but the contract is `202 + id` so a
/// future asynchronous/large-bundle composer is a non-breaking change. A
/// `ContextPackExport`-adjacent action is **not** recorded here: the bundle stays
/// on the machine (no egress) until an explicit operator-approved data request,
/// which is the audited egress gate, not this assemble step.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/diagnostics/snapshot",
        tag = "telemetry",
        responses(
            (status = 202, description = "Snapshot assembled; read it back by id.", body = SnapshotAccepted),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to write.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn request_snapshot(
    State(state): State<AppState>,
    principal: Principal,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    // Per-object visibility (BOLA, ADR-W005/ADR-W025): the snapshot ALWAYS
    // composes the whole config (every device id / `device_ref` / sync-group
    // member, redacted only for secrets/URLs, not ids), so — like the config
    // export + support bundle — an OBJECT-scoped principal must not compose one
    // (it would disclose every object outside its allowlist wholesale). Unscoped
    // principals are unaffected.
    if let Err(err) = crate::routes::require_unscoped_for_whole_system(&principal) {
        return err.into_response();
    }
    // The §4.2 bundle: logs + engine state from the consent-independent local
    // retention buffer + the REDACTED config — composed by the shared #111
    // context-pack composer so this snapshot and the support-ticket context-pack
    // share one diagnostics source of truth. Never media (the composer is
    // media-free by construction). All sections over the last-24h window. Composed
    // off the engine hot loop (invariant #10).
    let now = state.ack_now();
    let now_secs = unix_seconds(now);
    let request = BundleRequest {
        window: BundleWindow::LastDay,
        include: vec![
            BundleInclude::Diagnostics,
            BundleInclude::Config,
            BundleInclude::Incidents,
        ],
    };
    let config = ConfigSources {
        sources: state.sources.as_ref(),
        outputs: state.outputs.as_ref(),
        overlays: state.overlays.as_ref(),
        probes: state.probes.as_ref(),
        devices: state.devices.as_ref(),
    };
    let snapshot_id = uuid::Uuid::new_v4().to_string();
    let diagnostics = compose_bundle(&request, &state.retention, &config, now_secs, now, || {
        snapshot_id.clone()
    });
    let bundle = DiagnosticsSnapshot {
        snapshot_id: snapshot_id.clone(),
        status: SnapshotStatus::Ready,
        assembled_at_unix_seconds: now_secs,
        diagnostics,
    };
    match serde_json::to_value(&bundle) {
        Ok(value) => state.diagnostics_snapshots.put(snapshot_id.clone(), value),
        Err(err) => {
            return Problem::new(
                500,
                "snapshot_serialize_failed",
                format!("could not serialize the diagnostics snapshot: {err}"),
            )
            .into_response();
        }
    }
    (StatusCode::ACCEPTED, Json(SnapshotAccepted { snapshot_id })).into_response()
}

/// `GET /api/v1/diagnostics/{id}` — the assembled diagnostics snapshot (role:
/// read).
///
/// `200` with the bundle when the id is known; `404` (RFC 9457 problem) when not.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/diagnostics/{id}",
        tag = "telemetry",
        params(("id" = String, Path, description = "The diagnostics snapshot id.")),
        responses(
            (status = 200, description = "The assembled diagnostics snapshot.", body = DiagnosticsSnapshot),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
            (status = 404, description = "No snapshot with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_snapshot(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    // Per-object visibility (BOLA, ADR-W005/ADR-W025): the snapshot embeds the
    // whole config (every device id / ref / member); an object-scoped principal
    // must not read it — defence in depth alongside the request guard. Unscoped
    // principals are unaffected.
    if let Err(err) = crate::routes::require_unscoped_for_whole_system(&principal) {
        return err.into_response();
    }
    match state.diagnostics_snapshots.get(&id) {
        Some(value) => (StatusCode::OK, Json(value)).into_response(),
        None => Problem::new(
            404,
            "snapshot_unknown",
            "No diagnostics snapshot with that id.",
        )
        .into_response(),
    }
}

/// Convert a media-timeline instant (Unix nanoseconds) to Unix **seconds** for
/// the retention-buffer window queries, clamping a negative instant to zero.
fn unix_seconds(at: MediaTime) -> u64 {
    let nanos = at.as_nanos();
    if nanos <= 0 {
        0
    } else {
        u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX)
    }
}
