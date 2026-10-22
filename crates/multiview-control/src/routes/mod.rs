//! The HTTP handlers and router assembly under `/api/v1`.
//!
//! One resource is wired end to end here — **layouts** — over the
//! [`Repository`](crate::repository::Repository) trait, with `ETag`/`If-Match`
//! optimistic concurrency on every mutation (ADR-W006). The operational
//! commands (`start`/`stop`/`swap`) submit to the engine command bus and return
//! `202 Accepted` + an operation id; their outcome arrives later on the realtime
//! stream (ADR-W008). Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::concurrency::{IdempotencyKey, IfMatch, Reservation};
use crate::error::{ControlError, ControlResult};
use crate::problem::Problem;
use crate::repository::{Layout, LayoutInput, VersionedLayout, LAYOUT_KIND};
use crate::state::AppState;

pub mod account;
pub mod alarms;
pub mod audio;
pub mod audit;
pub mod cast_sessions;
pub mod config;
pub mod devices;
pub mod discovery;
pub mod health;
pub mod inputs;
pub mod licence;
pub mod logs;
pub mod media_players;
pub mod mesh;
pub mod outputs;
pub mod overlays;
pub mod preview;
pub mod probes;
pub mod routing;
pub mod salvos;
pub mod sources;
pub mod support;
pub mod sync_groups;
pub mod system;
pub mod tally;
pub mod telemetry;
pub mod whep_output;
pub mod whip;

/// A `202 Accepted` body returned for an asynchronously-applied command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AcceptedBody {
    /// The operation id correlating this command's eventual outcome on the
    /// realtime stream.
    pub operation_id: String,
    /// The command kind (e.g. `start`).
    pub kind: String,
    /// For `apply-layout` (ADR-W019): the per-cell property classes the live
    /// apply genuinely applies at the next frame boundary (e.g. `geometry`,
    /// `bindings`, `z_order`, `opacity`, `on_loss`). Absent on other commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_live: Option<Vec<String>>,
    /// For `apply-layout` (ADR-W019): the property classes that are **carried**
    /// in the stored document (persisted, exported) but not yet rendered by the
    /// compositor (e.g. `border`, `qos`, `fit`). Absent on other commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_only: Option<Vec<String>>,
}

/// The per-cell property classes a stored-layout live apply **genuinely
/// applies** at the frame boundary (ADR-W019): the solved geometry (grid/rect),
/// source bindings, z-order, per-cell opacity, and the per-cell `on_loss`
/// failover slate the compositor drive composites for down tiles.
const APPLY_LIVE_CLASSES: &[&str] = &["geometry", "bindings", "z_order", "opacity", "on_loss"];

/// The property classes a stored layout **carries** (persisted, exported,
/// mirrored into the working config) but the compositor does not yet render —
/// honestly reported on the `202` so the operator knows what changed on screen
/// (ADR-W019; the canvas axes are pinned for the session, ADR-R004).
const APPLY_CARRIED_CLASSES: &[&str] = &[
    "fit",
    "align",
    "border",
    "qos",
    "corner_radius",
    "scaler",
    "visible",
    "static_friendly",
    "label",
    "rotation",
    "canvas_pixel_format",
    "canvas_background",
    "canvas_color",
];

/// The body of a `POST /commands/swap` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SwapRequest {
    /// The tile/cell id whose source binding changes.
    pub tile: String,
    /// The new source/input id to bind.
    pub source: String,
}

/// The body of a `POST /commands/apply-layout` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApplyLayoutRequest {
    /// The layout id to make active on the running multiview.
    pub layout: String,
}

/// Attach the resource's `ETag` to a successful response carrying a layout.
fn layout_response(status: StatusCode, versioned: &VersionedLayout) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.layout.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/layouts` — list all layouts (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/layouts",
        tag = "layouts",
        responses(
            (status = 200, description = "All layouts, id-sorted.", body = [Layout]),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = Problem),
        ),
    )
)]
async fn list_layouts(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Layout>>> {
    principal.role.require(Action::Read)?;
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): a layout is gated by
    // its OWN id (`get_layout` 403s an out-of-scope id), so the list must drop
    // rows outside the allowlist — exactly as `list_devices` does. A layout
    // carries no embedded device_ref, so the row filter alone is complete; it is
    // a no-op for an unscoped principal.
    let layouts = state
        .repository
        .list_layouts()?
        .into_iter()
        .map(|v| v.layout)
        .filter(|layout| crate::auth::authorize_object(&principal, &layout.id).is_ok())
        .collect();
    Ok(Json(layouts))
}

/// `GET /api/v1/layouts/{id}` — fetch one layout (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        responses(
            (status = 200, description = "The layout (ETag in the response header).", body = Layout),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to read this layout.", body = Problem),
            (status = 404, description = "No layout with that id.", body = Problem),
        ),
    )
)]
async fn get_layout(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.repository.get_layout(&id)?;
    Ok(layout_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/layouts/{id}` — create a layout (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        request_body = LayoutInput,
        responses(
            (status = 201, description = "The created layout (ETag in the response header).", body = Layout),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to write.", body = Problem),
        ),
    )
)]
async fn create_layout(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<LayoutInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.repository.create_layout(&id, input)?;
    // Audit only after the mutation succeeded (who/what/when).
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        LAYOUT_KIND,
        &id,
        Some(versioned.layout.body.clone()),
    );
    Ok(layout_response(StatusCode::CREATED, &versioned))
}

/// `PUT /api/v1/layouts/{id}` — replace a layout (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        request_body = LayoutInput,
        responses(
            (status = 200, description = "The replaced layout (new ETag in the response header).", body = Layout),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to write.", body = Problem),
            (status = 404, description = "No layout with that id.", body = Problem),
            (status = 412, description = "If-Match precondition failed.", body = Problem),
        ),
    )
)]
async fn update_layout(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<LayoutInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Enforce the optimistic-concurrency precondition against the live version
    // before mutating.
    let current = state.repository.get_layout(&id)?;
    if_match.require(LAYOUT_KIND, &id, current.version)?;
    let versioned = state.repository.update_layout(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        LAYOUT_KIND,
        &id,
        Some(versioned.layout.body.clone()),
    );
    Ok(layout_response(StatusCode::OK, &versioned))
}

/// `DELETE /api/v1/layouts/{id}` — delete a layout (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        responses(
            (status = 204, description = "The layout was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to administer.", body = Problem),
            (status = 404, description = "No layout with that id.", body = Problem),
            (status = 412, description = "If-Match precondition failed.", body = Problem),
        ),
    )
)]
async fn delete_layout(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.repository.get_layout(&id)?;
    if_match.require(LAYOUT_KIND, &id, current.version)?;
    state.repository.delete_layout(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        LAYOUT_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Submit a command, honoring the `Idempotency-Key` and returning `202`.
///
/// Shared by every operational-command handler (start/stop/swap and the salvo +
/// tally-override surfaces): it reserves the idempotency key, builds the command
/// with the minted [`OperationId`], and `try_submit`s it **non-blocking** so a
/// full bus sheds to `503` (invariant #10) rather than ever blocking the engine.
pub(crate) fn submit_accepted(
    state: &AppState,
    idem: &IdempotencyKey,
    build: impl FnOnce(OperationId) -> Command,
) -> ControlResult<Response> {
    let body = submit_accepted_body(state, idem, |op| Ok(build(op)))?;
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// [`submit_accepted`]'s core, returning the `202` body instead of a built
/// response so a handler can decorate it (e.g. `apply-layout`'s
/// `applied_live`/`carried_only` classes) before serializing.
///
/// The `build` closure runs **inside the fresh-reservation arm** and may fail
/// (e.g. `apply-layout`'s resolve+solve, ADR-W019): a replayed
/// `Idempotency-Key` therefore answers from the reservation **without**
/// re-running `build`, and a `build` refusal **releases** the reservation (the
/// command never reached the engine) so a corrected retry with the same key
/// can actually submit — exactly the shed-on-full rule.
pub(crate) fn submit_accepted_body(
    state: &AppState,
    idem: &IdempotencyKey,
    build: impl FnOnce(OperationId) -> ControlResult<Command>,
) -> ControlResult<AcceptedBody> {
    match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Replay(op) => {
            // A retried request with the same key: return the original id
            // without re-enqueuing the command (and without re-running
            // `build` — the original command already reached the engine, so
            // the honest answer to "did it land?" is this 202 even if the
            // referenced resource has since changed or been deleted).
            Ok(AcceptedBody {
                operation_id: op.to_string(),
                kind: "replay".to_owned(),
                applied_live: None,
                carried_only: None,
            })
        }
        Reservation::Fresh(op) => {
            let command = match build(op.clone()) {
                Ok(command) => command,
                Err(refusal) => {
                    // The command was never built (a pre-submit refusal, e.g.
                    // a 422): release the reservation so a corrected retry
                    // with the same key re-reserves and actually submits.
                    state.idempotency.release(idem.0.as_deref(), &op);
                    return Err(refusal);
                }
            };
            let kind = command.kind().to_owned();
            // The command-outcome correlation key (if any) — computed from the
            // command before it is submitted (and moved). Commands with a single,
            // unambiguous outcome event (start/stop, named salvo arm/take/cancel)
            // are keyed; others yield no key and ride the stream uncorrelated.
            let corr_key = crate::realtime::CorrKey::for_command(&command);
            // Non-blocking submit: a full bus sheds load (503) rather than
            // blocking the engine (invariant #10). If the submit is shed, the
            // command never reached the engine, so we MUST release the
            // idempotency reservation we just took — otherwise a client retry
            // with the same key would hit `Reservation::Replay` and receive a
            // false `202 Accepted` (kind:"replay") for a command that was never
            // enqueued. Releasing lets the retry re-reserve and actually submit.
            if let Err(_shed) = state.commands.try_submit(command) {
                state.idempotency.release(idem.0.as_deref(), &op);
                return Err(ControlError::EngineBusy);
            }
            // The command is enqueued: record its outcome correlation so the
            // realtime projection echoes this op id as `corr` on the matching
            // outcome event (ADR-W008). Recorded only on the success path, so a
            // shed command leaves no orphan correlation. The engine drains on its
            // own tick cadence (never synchronously here), and the realtime
            // projection runs on separate per-client tasks, so the correlation is
            // recorded before any outcome can be delivered. The registry is
            // bounded drop-oldest and never on the engine hot loop (invariant #10).
            if let Some(key) = corr_key {
                state.corr.record(key, op.clone());
            }
            Ok(AcceptedBody {
                operation_id: op.to_string(),
                kind,
                applied_live: None,
                carried_only: None,
            })
        }
    }
}

/// Redact embedded **managed-device id** references from a resource `body` that a
/// scoped principal is not authorized to see (BOLA visibility, ADR-W005/ADR-W025).
///
/// Some config objects carry a managed device id in a *field* rather than being a
/// device collection: a device-projected source/output via `device_ref`
/// (ADR-M009), and a sync group's `members[].device` (ADR-M010). The resource
/// itself is gated by its own id, so an in-scope principal still sees the row —
/// but an **out-of-scope device id embedded in it would leak the device's
/// existence**, exactly the enumeration a single-device `GET` would `403`. So for
/// an object-scoped principal this drops any such reference whose device is
/// outside the allowlist:
///
/// * top-level `body.device_ref` (source/output) — the key is removed.
/// * each `body.members[].device` (sync group) — the key is removed from that
///   member object (the member entry and its `offset_ms` stay; only the hidden
///   device id is dropped).
///
/// An **unscoped** principal (`scoped_object_ids: None`, the default
/// admin/operator/viewer) is a no-op — every reference is shown unchanged. This
/// is a pure read-side projection on the response body; it never mutates the
/// stored resource.
pub(crate) fn redact_out_of_scope_device_refs(
    principal: &Principal,
    resource: &mut crate::resource_store::Resource,
) {
    redact_device_refs_in_body(principal, &mut resource.body);
}

/// Redact embedded out-of-scope managed-device id references **in a body
/// [`serde_json::Value`]** for a scoped principal (BOLA visibility,
/// ADR-W005/ADR-W025) — the shape-level core of
/// [`redact_out_of_scope_device_refs`].
///
/// Operates directly on a JSON body so it serves both a wrapped
/// [`Resource`](crate::resource_store::Resource) (the live source/output/
/// sync-group reads) and a bare body value (an **audit `detail`**, which carries
/// the full resource body on an Update). Drops, when the referenced device is
/// outside the allowlist:
///
/// * top-level `device_ref` (source/output) — the key is removed.
/// * each `members[].device` (sync group) — the key is removed from that member.
///
/// An **unscoped** principal is a no-op. A `body` that is not a JSON object (or
/// carries neither key) is left unchanged.
pub(crate) fn redact_device_refs_in_body(principal: &Principal, body: &mut serde_json::Value) {
    // Unscoped principals see everything: nothing to redact (fast path).
    if !principal.is_scoped() {
        return;
    }
    let out_of_scope =
        |device_id: &str| crate::auth::authorize_object(principal, device_id).is_err();

    // Source/output device-projection link.
    if let Some(serde_json::Value::String(device_id)) = body.get("device_ref") {
        if out_of_scope(device_id) {
            if let Some(map) = body.as_object_mut() {
                map.remove("device_ref");
            }
        }
    }

    // Sync-group member device ids.
    if let Some(serde_json::Value::Array(members)) = body.get_mut("members") {
        for member in members.iter_mut() {
            let leaks = matches!(
                member.get("device"),
                Some(serde_json::Value::String(device_id)) if out_of_scope(device_id)
            );
            if leaks {
                if let Some(member_map) = member.as_object_mut() {
                    member_map.remove("device");
                }
            }
        }
    }
}

/// Deny a request that exposes or mutates a **whole-system** artifact for a
/// principal scoped on ANY authorization axis (BOLA wholesale-enumeration
/// defense, ADR-W005 / ADR-W025 / ADR-W026).
///
/// Some surfaces span the entire desired-state: the config export and the
/// support bundle embed *every* device id, `device_ref`, and sync-group member
/// id in one document, and the config mutations revert-to-start / promote
/// rewrite the *entire* running / boot document across all objects. Per-field
/// redaction cannot apply (a scope-filtered config is not a valid runnable
/// document), and a scoped operator has no business rewriting state for objects
/// outside its allowlist — so a principal restricted on the object, output, OR
/// discovery-domain axis is denied outright. Only a principal that can see the
/// whole system ([`Principal::is_global`]) is allowed; an unscoped
/// admin/operator/viewer is a no-op.
///
/// Keyed off the unified `is_global` predicate rather than a frozen per-axis
/// check, so a newly-added scope axis is covered automatically.
///
/// # Errors
///
/// [`ControlError::Forbidden`] if the principal is scoped on any axis.
pub(crate) fn require_unscoped_for_whole_system(principal: &Principal) -> Result<(), ControlError> {
    if !principal.is_global() {
        return Err(ControlError::Forbidden(format!(
            "principal {:?} is scoped (object, output, or discovery-domain) and may not \
             access a whole-system artifact (it would disclose or mutate resources outside \
             its allowlist)",
            principal.key_id
        )));
    }
    Ok(())
}

/// The exact operator/portal copy the Conspect startup gate (S1) refuses a NEW
/// program-output start with at the `block-new-instance` rung (ADR-0050 §5/§6.2).
/// **Verbatim** — the cli's `BLOCK_NEW_INSTANCE_REASON` and the portal show the
/// same words; the trailing clause is the never-off-air promise.
const BLOCK_NEW_INSTANCE_REASON: &str =
    "Lease expired — new engine instances won't start; running ones untouched";

/// `POST /api/v1/commands/start` — start program output (role: write; 202).
///
/// Gated by the Conspect startup gate (S1, ADR-0050 §5): when the entitlement
/// ladder is at the `block-new-instance` rung, a NEW start is refused with a
/// `409 lease_expired` RFC-9457 problem carrying [`BLOCK_NEW_INSTANCE_REASON`].
/// A **running** program is never touched — `stop` and every operational command
/// stay reachable; this blocks only a *new* start (the never-off-air promise).
/// The gate is a lock-free read of the entitlement store off the engine hot loop.
async fn cmd_start(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // S1 startup gate: refuse a NEW start at the block-new-instance rung (the
    // lock fires only on positive evidence of a lapsed lease — an unlicensed /
    // compliant machine starts normally, fail-toward-leniency).
    let blocked = state
        .licence
        .store
        .status()
        .is_some_and(|status| status.blocks_new_instances);
    if blocked {
        return Ok(Problem::new(
            StatusCode::CONFLICT.as_u16(),
            "lease_expired",
            "Lease expired (new starts blocked)",
        )
        .with_detail(BLOCK_NEW_INSTANCE_REASON)
        .with_instance("/settings/licence")
        .into_response());
    }
    submit_accepted(&state, &idem, |op| Command::Start { op })
}

/// `POST /api/v1/commands/stop` — stop program output (role: write; 202).
async fn cmd_stop(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    submit_accepted(&state, &idem, |op| Command::Stop { op })
}

/// `POST /api/v1/commands/swap` — swap a tile's source (role: write; 202).
async fn cmd_swap(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<SwapRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &req.tile)?;
    // Per-object authz on the SOURCE too (BOLA, SEC-09): a swap binds the source
    // input onto the tile, so a scoped operator may swap in only sources within
    // its object allowlist (a missing/out-of-scope id fails closed).
    crate::auth::authorize_object(&principal, &req.source)?;
    let tile = req.tile.clone();
    let source = req.source.clone();
    let response = submit_accepted(&state, &idem, |op| Command::SwapSource {
        op,
        tile: req.tile,
        source: req.source,
    })?;
    // Audit the accepted command (the engine reports its outcome separately on
    // the realtime stream; what we audit here is the operator's request).
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "tile",
        &tile,
        Some(serde_json::json!({ "command": "swap", "source": source })),
    );
    Ok(response)
}

/// `POST /api/v1/commands/apply-layout` — apply a **stored** layout to the
/// running multiview, live, at the next frame boundary (role: write;
/// per-object authz; 202) — ADR-W019, invariant #11 Class-1.
///
/// The stored layout body is resolved from the layouts repository and solved
/// **here, at request time** (off the engine hot path): an unknown id, a body
/// that does not parse as a `{canvas, layout, cells}` document, one that does
/// not solve (bad grid / geometry), a pinned-canvas mismatch (a Class-2
/// change — ADR-R004), or an unknown running canvas (no seeded snapshot — the
/// gate fails closed) is an honest `422` **before** any `202`. On `202` the
/// command carries the solved layout, so the engine's frame-boundary drain
/// only swaps (O(cells), no I/O — invariants #1/#10); the `202` body's
/// `applied_live`/`carried_only` arrays state which per-cell property classes
/// genuinely apply on screen. Idempotency reserves **before** resolution: a
/// replayed `Idempotency-Key` returns the original operation id (kind
/// `replay`, undecorated) without re-resolving, and a refused resolve
/// releases the key. The submit mirrors [`cmd_swap`] (shed-on-full) and never
/// blocks the engine (invariant #10); the outcome rides the realtime stream
/// as a `job.progress` event (ADR-W008).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/commands/apply-layout",
        tag = "commands",
        request_body = ApplyLayoutRequest,
        responses(
            (status = 202, description = "Apply-layout accepted: the stored layout was resolved + solved and will swap in at the next frame boundary; `applied_live`/`carried_only` state the property classes. Outcome on the realtime stream (`job.progress`).", body = AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to apply a layout.", body = Problem),
            (status = 422, description = "The layout id does not exist, its stored body does not parse/solve, its canvas differs from the running session's pinned canvas (Class-2), or the running canvas is unknown (no seeded snapshot — the gate fails closed).", body = Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = Problem),
        ),
    )
)]
pub(crate) async fn cmd_apply_layout(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<ApplyLayoutRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &req.layout)?;

    let layout = req.layout.clone();
    // ADR-W024 MAJOR-B round 5: hold the config-mutation lock across
    // resolve→submit→adopt so the persister (same lock) can never compose
    // mid-apply, and the layout snapshot advances atomically with the engine
    // submit. The adopted canvas/layout/cells come from the working-layout body
    // captured here; they are adopted into the snapshot ONLY when the
    // `ApplyLayout` lands (a shed leaves the snapshot at the prior adopted
    // layout, so a shed apply-layout never leaks into `active.toml`).
    let _mutation = state.lock_config_mutation().await;
    let adopted_body: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);
    // Resolution runs INSIDE the fresh-reservation arm (pinned semantics,
    // ADR-W019): a replayed Idempotency-Key answers from the reservation
    // without re-resolving, and a refused resolve releases the key.
    let mut body = submit_accepted_body(&state, &idem, |op| {
        // Resolve + solve the STORED layout at request time: repository read +
        // pure solve, both fine here and forbidden on the render thread.
        // Every failure is a 422 problem BEFORE any 202 — a 202 is a promise.
        let versioned = state
            .repository
            .get_layout(&req.layout)
            .map_err(|e| match e {
                ControlError::NotFound { .. } => ControlError::Validation(format!(
                    "layout {:?} does not exist in the layouts library",
                    req.layout
                )),
                other => other,
            })?;
        // The ONE resolve machinery (parse + solve + Class-1 pinned-canvas
        // gate), shared with the config-file watcher (ADR-W019/W020). A `None`
        // running canvas fails closed there.
        let resolved = crate::command::resolve_layout_document(
            &req.layout,
            &versioned.layout.body,
            state.running_canvas.as_ref(),
        )?;
        // Capture the authored body so a LANDED apply can adopt its
        // canvas/layout/cells into the snapshot (the resolved form is the
        // engine's solved layout, not the authored config sections).
        if let Ok(mut slot) = adopted_body.lock() {
            *slot = Some(versioned.layout.body.clone());
        }
        Ok(Command::ApplyLayout {
            op,
            layout: req.layout,
            document: Some(Box::new(resolved)),
        })
    })?;
    // A FRESH landed apply (not a replay) → adopt the layout into the snapshot
    // (MAJOR-B round 5). A shed never reaches here (`submit_accepted_body`
    // returns `Err(EngineBusy)`), so a shed apply-layout leaves the snapshot at
    // the prior adopted layout.
    if body.kind != "replay" {
        if let Some(model) = state.boot_model.as_ref() {
            let captured = adopted_body.lock().ok().and_then(|mut s| s.take());
            if let Some(layout_body) = captured {
                adopt_layout_from_body(model, &layout_body);
            }
        }
    }
    // State honestly which per-cell property classes land on screen at the
    // frame boundary vs are carried-but-not-yet-rendered (ADR-W019). A replay
    // body stays undecorated: it answers "did the original land?", it does not
    // re-promise an apply.
    if body.kind != "replay" {
        body.applied_live = Some(APPLY_LIVE_CLASSES.iter().map(|s| (*s).to_owned()).collect());
        body.carried_only = Some(
            APPLY_CARRIED_CLASSES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        );
    }
    // Audit the accepted command (the engine reports its outcome separately on
    // the realtime stream; what we audit here is the operator's request).
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "layout",
        &layout,
        Some(serde_json::json!({ "command": "apply_layout" })),
    );
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// Adopt a landed `ApplyLayout`'s canvas/layout/cells into the boot-model
/// snapshot (ADR-W024 MAJOR-B round 5): the working-layout `body` is the
/// authored `{canvas, layout, cells}` shape. Each section is deserialized into
/// its typed form; a section that fails to deserialize (in practice impossible —
/// the body already validated on the store write) is skipped, leaving the
/// snapshot's prior adopted value rather than persisting an invalid layout.
/// Call ONLY while holding the config-mutation lock.
fn adopt_layout_from_body(model: &crate::boot_model::BootModel, body: &serde_json::Value) {
    let canvas = body
        .get("canvas")
        .and_then(|v| serde_json::from_value::<multiview_config::Canvas>(v.clone()).ok());
    let layout = body
        .get("layout")
        .and_then(|v| serde_json::from_value::<multiview_config::Layout>(v.clone()).ok());
    let cells = body
        .get("cells")
        .and_then(|v| serde_json::from_value::<Vec<multiview_config::Cell>>(v.clone()).ok());
    if let (Some(canvas), Some(layout), Some(cells)) = (canvas, layout, cells) {
        model.adopt_layout(canvas, layout, cells);
    }
}

impl axum::extract::FromRequestParts<AppState> for Principal {
    type Rejection = ControlError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Auth disabled (explicit trusted-network mode): every request is a local
        // admin, no credential required.
        if state.auth_disabled {
            return Ok(Principal::local_admin());
        }
        let header = header_value(&parts.headers, header::AUTHORIZATION);
        // Primary: native API key. If that fails and a JWT validator is
        // configured, fall back to OAuth2/JWT (the alternative authn path).
        match state.api_keys.verify_authorization(header.as_deref()) {
            Ok(principal) => Ok(principal),
            Err(api_key_err) => state
                .authenticate_jwt(header.as_deref())
                .ok_or(api_key_err)?,
        }
    }
}

/// Extract a header value as an owned string, if present and valid UTF-8.
fn header_value(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Build the versioned-document resource CRUD routes (layouts plus the
/// config-as-code stores: sources, outputs, overlays, probes) and the
/// read-only input stream inventory. Split from [`api_router`] so each stays
/// a readable size.
fn resource_router() -> Router<AppState> {
    Router::new()
        .route("/layouts", get(list_layouts))
        .route(
            "/layouts/{id}",
            get(get_layout)
                .post(create_layout)
                .put(update_layout)
                .delete(delete_layout),
        )
        // Sources resource CRUD (managed inputs), mirroring layouts.
        .route("/sources", get(sources::list_sources))
        .route(
            "/sources/{id}",
            get(sources::get_source)
                .post(sources::create_source)
                .put(sources::update_source)
                .delete(sources::delete_source),
        )
        // Read-only input elementary-stream inventory (RT-3): every stream an
        // input offers, projected from the off-engine cached snapshot (inv #10).
        .route("/inputs/{id}/streams", get(inputs::get_input_streams))
        // Outputs resource CRUD (managed sinks/servers), mirroring layouts.
        .route("/outputs", get(outputs::list_outputs))
        .route(
            "/outputs/{id}",
            get(outputs::get_output)
                .post(outputs::create_output)
                .put(outputs::update_output)
                .delete(outputs::delete_output),
        )
        // The audio-routing singleton document (program-bus + discrete tracks).
        .route(
            "/audio-routing",
            get(audio::get_audio_routing).put(audio::put_audio_routing),
        )
        // Overlays resource CRUD (managed overlay layers), mirroring layouts.
        .route("/overlays", get(overlays::list_overlays))
        .route(
            "/overlays/{id}",
            get(overlays::get_overlay)
                .post(overlays::create_overlay)
                .put(overlays::update_overlay)
                .delete(overlays::delete_overlay),
        )
        // Probes resource CRUD (per-cell fail-state detection), mirroring
        // sources.
        .route("/probes", get(probes::list_probes))
        .route(
            "/probes/{id}",
            get(probes::get_probe)
                .post(probes::create_probe)
                .put(probes::update_probe)
                .delete(probes::delete_probe),
        )
        .merge(device_router())
}

/// Build the managed-devices surface (ADR-M008/M009/M011/W017): the device
/// registry CRUD + status + bare-verb actions + stream-binding projections,
/// ephemeral Cast sessions, mDNS discovery, and presentation-sync groups.
/// Split from [`resource_router`] so each stays a readable size.
fn device_router() -> Router<AppState> {
    // Managed-devices CRUD (ADR-M008): the config-as-code device registry,
    // plus the read-only runtime status snapshot, the bare-verb actions
    // (ADR-W017), and the declared stream-binding projections (ADR-M009).
    Router::new()
        .route("/devices", get(devices::list_devices))
        .route(
            "/devices/{id}",
            get(devices::get_device)
                .post(devices::create_device)
                .put(devices::update_device)
                .delete(devices::delete_device),
        )
        .route("/devices/{id}/status", get(devices::get_device_status))
        .route("/devices/{id}/probe", post(devices::probe_device))
        .route("/devices/{id}/set-mode", post(devices::set_mode))
        .route("/devices/{id}/reboot", post(devices::reboot_device))
        .route("/devices/{id}/identify", post(devices::identify_device))
        .route(
            "/devices/{id}/test-pattern",
            post(devices::test_pattern),
        )
        .route(
            "/devices/{id}/source-candidates",
            get(devices::source_candidates),
        )
        .route(
            "/devices/{id}/output-targets",
            get(devices::output_targets),
        )
        // Ephemeral Cast sessions (DEV-D2, ADR-M011): start/list/stop an
        // ad-hoc cast of a served HLS rendition, save-as-device promotion,
        // and the receiver-namespace volume verb. Runtime-only — never part
        // of the devices store, never exported.
        .route(
            "/cast/sessions",
            get(cast_sessions::list_cast_sessions).post(cast_sessions::start_cast_session),
        )
        .route(
            "/cast/sessions/{id}",
            get(cast_sessions::get_cast_session).delete(cast_sessions::stop_cast_session),
        )
        .route(
            "/cast/sessions/{id}/save",
            post(cast_sessions::save_cast_session),
        )
        .route(
            "/cast/sessions/{id}/volume",
            post(cast_sessions::set_cast_volume),
        )
        // mDNS device discovery (ADR-M008 §6 / ADR-0041): kick a time-bounded
        // browse (202 + device.discovered events) and read the untrusted
        // inventory. Discovery never creates a device — confirm-adopt is the
        // separate POST /devices/{id} referencing a discovered address.
        .route(
            "/discovery/devices/scan",
            post(discovery::scan_devices),
        )
        .route("/discovery/devices", get(discovery::list_discovered))
        // Presentation-sync-groups CRUD (ADR-M008/M010) + the measure action.
        .route("/sync-groups", get(sync_groups::list_sync_groups))
        .route(
            "/sync-groups/{id}",
            get(sync_groups::get_sync_group)
                .post(sync_groups::create_sync_group)
                .put(sync_groups::update_sync_group)
                .delete(sync_groups::delete_sync_group),
        )
        .route(
            "/sync-groups/{id}/measure",
            post(sync_groups::measure_sync_group),
        )
}

/// Build the Conspect **account / licensing / telemetry** sub-router: the local
/// entitlement plane, the local mesh, the telemetry pipe (consent + schema), the
/// diagnostics snapshot, the account-side audit + pending-actions, and the local
/// support surface. Split out of [`api_router`] so each stays under the
/// `too_many_lines` lint, mirroring [`resource_router`]; all control-plane only,
/// off the engine hot loop (invariant #10).
///
/// **Two-pipe separation (ADR-0052 §1):** the licensing heartbeat-status lives
/// under `/licensing/`; the telemetry consent + schema live under `/telemetry/`.
/// They are deliberately distinct paths and are **never** co-mingled.
fn conspect_router() -> Router<AppState> {
    Router::new()
        // Local licence (Conspect, CONSPECT-1 / ADR-0050): the computed licence
        // resource (enforcement is DATA → always 200), the lease install path
        // (verify + install a presented signed binding), and the salted CBOR
        // challenge export. All-local: no licence-server calls; never off air.
        .route("/licence", get(licence::get_licence))
        .route("/licence/lease", post(licence::install_lease))
        .route("/licence/challenge", get(licence::get_challenge))
        // The read-only heartbeat-status surface (Conspect Hook 4, ADR-0050 §3):
        // the honest local heartbeat status (transport + last/next contact +
        // payload fields). The spec mandates NO mutating endpoint — `get` only.
        // This is the LICENSING pipe — under `/licensing/`, never `/telemetry/`.
        .route(
            "/licensing/heartbeat-status",
            get(licence::get_heartbeat_status),
        )
        // Local mesh (Conspect, CONSPECT-3a / ADR-0051): the always-on discovery
        // + relay status (GET, never an off switch — the spec's locked row), the
        // relay opt-in toggle (a real persisted PUT), and the untrusted
        // discovered-peer inventory (GET). Control-plane only; no engine handle
        // (invariant #10). There is intentionally NO route that disables discovery.
        .route("/mesh/status", get(mesh::get_status))
        .route("/mesh/relay", axum::routing::put(mesh::set_relay))
        .route("/mesh/peers", get(mesh::list_peers))
        // Telemetry pipe (Conspect, ADR-0052): the opt-in daily-pipe consent
        // (GET + PUT, off by default, last-writer-wins) and the published schema
        // (sent + never-sent). Deliberately under `/telemetry/`, never co-mingled
        // with the licensing heartbeat under `/licensing/` (two-pipe separation).
        // Consent gates NO local route. Control-plane only (invariant #10).
        .route(
            "/telemetry/consent",
            get(telemetry::get_consent).put(telemetry::set_consent),
        )
        .route("/telemetry/schema", get(telemetry::get_schema))
        // Diagnostics snapshot (Conspect, spec §4.2 / ADR-0053): the one-button
        // support bundle (logs + engine state, never media) — POST assembles →
        // 202 {snapshot_id}; GET reads it back by id. Composed by the shared #111
        // context-pack composer from the consent-independent local retention
        // buffer + redacted config. Control-plane only (inv #10).
        .route("/diagnostics/snapshot", post(telemetry::request_snapshot))
        .route("/diagnostics/{id}", get(telemetry::get_snapshot))
        // Account-side append-only audit (Conspect §10/§11): cursor-paginated.
        .route("/account/audit", get(account::list_account_audit))
        // Pending remote-actions strip + local cancel (local always wins).
        .route("/actions/pending", get(account::list_pending_actions))
        .route("/actions/{id}/cancel", post(account::cancel_action))
        // Local support surface (Conspect §10/§11): tier-derived entitlement
        // routing, the local ticket store (CS-xxxx, machine-context auto-attach,
        // reply/close), the previewable redacted media-free context-pack
        // composer, and local approve/deny of inbound egress data requests.
        .route("/support/entitlement", get(support::get_entitlement))
        .route(
            "/support/tickets",
            get(support::list_tickets).post(support::raise_ticket),
        )
        .route("/support/tickets/{id}", get(support::get_ticket))
        .route("/support/tickets/{id}/reply", post(support::reply_ticket))
        .route("/support/tickets/{id}/close", post(support::close_ticket))
        .route("/support/bundle", post(support::compose))
        .route("/support/bundle/{id}", get(support::get_bundle))
        .route(
            "/support/data-request/{id}/approve",
            post(support::approve_data_request),
        )
        .route(
            "/support/data-request/{id}/deny",
            post(support::deny_data_request),
        )
}

/// Build the `/api/v1` resource + command routes (without the realtime or docs
/// routes, which are wired by [`crate::router()`]).
pub fn api_router() -> Router<AppState> {
    Router::new()
        .merge(resource_router())
        .merge(conspect_router())
        .route("/commands/start", post(cmd_start))
        .route("/commands/stop", post(cmd_stop))
        .route("/commands/swap", post(cmd_swap))
        .route("/commands/apply-layout", post(cmd_apply_layout))
        .route("/alarms", get(alarms::list_alarms))
        .route("/alarms/{id}/ack", post(alarms::ack_alarm))
        // Read-only structured log tail (ADR-0060): recent buffered records from
        // the bounded drop-oldest ring, filterable by resource/kind/level/since.
        .route("/logs", get(logs::list_logs))
        // Read-only health warnings (SA-0 / ADR-0035): active capability mismatches
        // (e.g. GPU present but compositing fell back to CPU) with remediation.
        .route("/health", get(health::list_health))
        // Read-only system capability + licence surface (ADR-W030): which
        // codec/compositor backends are available, the compositor class, the
        // effective build-profile licence, and the NDI attribution. System-global
        // (no per-object axis) — a viewer may read it.
        .route("/system/capabilities", get(system::system_capabilities))
        // Salvo operator surface: CRUD + arm/take/cancel.
        .route("/salvos", get(salvos::list_salvos))
        .route(
            "/salvos/{id}",
            get(salvos::get_salvo)
                .put(salvos::put_salvo)
                .delete(salvos::delete_salvo),
        )
        .route("/salvos/{id}/arm", post(salvos::arm_salvo))
        .route("/salvos/{id}/take", post(salvos::take_salvo))
        .route("/salvos/{id}/cancel", post(salvos::cancel_salvo))
        // Salvo parity (Conspect §11): fire a named salvo through the bus → 202.
        .route("/salvos/{id}/fire", post(account::fire_salvo))
        // Media-player transport surface (ADR-0057/ADR-0097): read + the
        // load/cue/play/pause/stop/seek verbs + the vamp-exit arm/take/cancel
        // triad, each 202 + op id onto the bounded command bus.
        .route("/media/players", get(media_players::list_players))
        .route("/media/players/{id}", get(media_players::get_player))
        .route("/media/players/{id}/load", post(media_players::load_player))
        .route("/media/players/{id}/cue", post(media_players::cue_player))
        .route("/media/players/{id}/play", post(media_players::play_player))
        .route("/media/players/{id}/pause", post(media_players::pause_player))
        .route("/media/players/{id}/stop", post(media_players::stop_player))
        .route("/media/players/{id}/seek", post(media_players::seek_player))
        .route("/media/players/{id}/exit/arm", post(media_players::arm_exit))
        .route("/media/players/{id}/exit/take", post(media_players::take_exit))
        .route(
            "/media/players/{id}/exit/cancel",
            post(media_players::cancel_exit),
        )
        // Per-stream crosspoint routing (RT-11): classify (plan) + apply (take).
        .route("/routing/plan", post(routing::plan_route))
        .route("/routing/{kind}/take", post(routing::take_route))
        // Tally operator surface: read resolved state, profiles, manual override.
        .route("/tally", get(tally::list_tally))
        .route(
            "/tally/override",
            axum::routing::put(tally::set_override).delete(tally::clear_override),
        )
        .route("/tally/profiles", get(tally::list_profiles))
        .route(
            "/tally/profiles/{id}",
            get(tally::get_profile)
                .put(tally::put_profile)
                .delete(tally::delete_profile),
        )
        // Live preview snapshots (program + per-input JPEG stills). These are
        // the JPEG-ladder fallback, NOT WebRTC media signalling — they stay
        // outside the media-signalling CORS scope (ADR-0048 §9). The WebRTC
        // media-signalling routes (capabilities + WHIP + WHEP-serve +
        // preview-WHEP) live in `signalling_router`, which `crate::router` merges
        // **with its CORS layer** so the allow-list applies only to them.
        .route("/preview/program.jpg", get(preview::program_jpeg))
        .route("/preview/inputs", get(preview::list_input_ids))
        .route("/preview/inputs/{id}", get(preview::input_jpeg))
        // Read-only change audit log.
        .route("/audit", get(audit::list_audit))
        // (The Conspect account / licensing / telemetry / support routes are
        // merged from `conspect_router()` above.)
        // Config-as-code export: the live stores rendered as multiview.toml.
        .route("/config/export", get(config::export_config))
        // The config-file watch status (ADR-W020): read-only, honest
        // "not watched" default. A static segment, so it never collides with
        // the `{target}` capture below (axum prefers static matches).
        .route("/config/watch-status", get(config::watch_status))
        // The Boot/Loaded/Running config model (ADR-W024). Static segments, so
        // they never collide with the `{target}` capture below: the divergence
        // indicator (read), revert-to-start (Running := Loaded, live), and
        // promote (write Running to the boot file via the ADR-W020 seam).
        .route("/config/boot-model", get(config::boot_model_status))
        .route(
            "/config/revert-to-start",
            post(config::revert_to_start),
        )
        .route("/config/promote", post(config::promote_to_boot))
        // Config versioning: history + commit, single revision, diff, rollback.
        .route(
            "/config/{target}",
            get(config::list_history).put(config::commit_revision),
        )
        .route("/config/{target}/rev/{revision}", get(config::get_revision))
        .route("/config/{target}/diff", get(config::diff_revisions))
        .route(
            "/config/{target}/rollback",
            post(config::rollback_revision),
        )
}

/// Build the **WebRTC media-signalling** routes (without their CORS layer, which
/// [`crate::router`] applies). These are the routes ADR-0048 §9 scopes
/// `webrtc.cors_allow_origins` to: preview transport capabilities, the
/// preview-WHEP focus routes, WHIP ingest, and WHEP-serve output — the surface a
/// browser served from a web origin negotiates against. Kept separate from
/// [`api_router`] so the CORS allow-list applies **only** here.
pub fn signalling_router() -> Router<AppState> {
    Router::new()
        // Preview transport capabilities (WHEP vs the JPEG ladder, ADR-P006).
        .route("/preview/capabilities", get(preview::capabilities))
        // WHEP focus sessions (sub-second WebRTC preview) per scope: SDP offer
        // in -> 201 + answer SDP + Location; DELETE the resource to release.
        .route("/preview/program/whep", post(preview::program_whep_open))
        .route(
            "/preview/program/whep/{session_id}",
            axum::routing::delete(preview::program_whep_close),
        )
        .route("/preview/inputs/{id}/whep", post(preview::input_whep_open))
        .route(
            "/preview/inputs/{id}/whep/{session_id}",
            axum::routing::delete(preview::input_whep_close),
        )
        .route("/preview/outputs/{id}/whep", post(preview::output_whep_open))
        .route(
            "/preview/outputs/{id}/whep/{session_id}",
            axum::routing::delete(preview::output_whep_close),
        )
        // WHIP ingest (RFC 9725): a WebRTC contribution publisher POSTs an SDP
        // offer to the source-derived endpoint -> 201 + answer SDP + Location;
        // DELETE the session resource to tear it down. PATCH is 405 (vanilla
        // ICE, no trickle/restart); OPTIONS is the CORS preflight (ADR-T014 §2).
        .route(
            "/whip/{source_id}",
            post(whip::whip_publish).options(whip::whip_options),
        )
        .route(
            "/whip/{source_id}/sessions/{session_id}",
            axum::routing::delete(whip::whip_delete).patch(whip::whip_patch),
        )
        // WHEP-serve output viewers (ADR-0049 §5.1): a browser POSTs an SDP offer
        // to the output-derived endpoint -> 201 + answer SDP + Location, then
        // receives the real encoded program over SRTP; DELETE the session resource
        // to release the viewer slot. PATCH is 405 (vanilla ICE); OPTIONS is the
        // CORS preflight. A real-output surface distinct from the preview focus
        // WHEP routes above.
        .route(
            "/whep/{output_id}",
            post(whep_output::whep_view).options(whep_output::whep_options),
        )
        .route(
            "/whep/{output_id}/sessions/{session_id}",
            axum::routing::delete(whep_output::whep_delete).patch(whep_output::whep_patch),
        )
}
