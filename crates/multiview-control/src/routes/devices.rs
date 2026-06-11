//! The managed-devices resource surface under `/api/v1/devices` (ADR-M008 /
//! ADR-M009 / ADR-W017).
//!
//! A **managed device** is operator-adopted hardware (encoder/decoder
//! appliances, display nodes, cast targets) stored as config-as-code
//! (`multiview_config::Device`) over the generic versioned
//! [`ResourceRepository`](crate::resource_store::ResourceRepository), with
//! `ETag`/`If-Match` optimistic concurrency on every mutation (ADR-W006), RBAC
//! via [`Principal`], typed-body validation against `multiview_config::Device`
//! (ADR-W015), and an audit record after each successful write. This module
//! exposes:
//!
//! * `GET /api/v1/devices` — list (role: read).
//! * `GET /api/v1/devices/{id}` — fetch one (role: read; `ETag`).
//! * `POST /api/v1/devices/{id}` — adopt/create (role: write; `422` on an
//!   invalid `Device` document; seeds the runtime status in `ADOPTING`).
//! * `PUT /api/v1/devices/{id}` — replace (role: write; `If-Match` → `412`).
//! * `DELETE /api/v1/devices/{id}` — drop (role: administer; `If-Match`;
//!   `409 /problems/conflict` while a Source/Output still references it via
//!   `device_ref`, ADR-M009).
//! * `GET /api/v1/devices/{id}/status` — the read-only latest-wins runtime
//!   snapshot from the [`DeviceStatusRegistry`](crate::devices::DeviceStatusRegistry)
//!   (never persisted/exported).
//! * `POST /api/v1/devices/{id}/{probe|set-mode|reboot|identify|test-pattern}` —
//!   the bare-verb device actions (ADR-W017). `probe` is synchronous (`200`);
//!   `set-mode`/`reboot` are long-running (`202` + operation id; `set-mode`
//!   declares its DEV-class impact before apply); `identify`/`test-pattern` are
//!   fire-and-forget (`204`).
//! * `GET /api/v1/devices/{id}/source-candidates` and `/output-targets` — the
//!   declared stream-binding projections (ADR-M009): honestly empty until a
//!   driver enumerates, never fabricated live telemetry.
//!
//! Device I/O runs on the per-device **driver poller** (DEV-A4, ADR-M009): a
//! `zowietek` device adopted here spawns a supervised control-plane poller that
//! logs in, probes, enumerates the three facets (so `source-candidates` /
//! `output-targets` return real data at runtime), polls status, and drives the
//! device lifecycle — all isolated from the engine (invariant #10). `set-mode`
//! dispatches its convergence to that poller; `reboot` mints an operation id and
//! `202`s (its live transport lands with the management facet). When no live
//! poller is running (the default build's no-op factory, or a non-`zowietek`
//! device), the projection routes stay honestly empty and the long-running
//! actions still `202`. Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::OperationId;
use crate::concurrency::{IdempotencyKey, IfMatch, Reservation};
use crate::devices::projection::{OutputTarget, SourceCandidate};
use crate::error::{ControlError, ControlResult};
use crate::resource_store::{Resource, ResourceInput, VersionedResource, DEVICE_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply_restart, TypedCollection};

/// Attach the device resource's `ETag` to a successful response.
fn device_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/devices` — list all devices, id-sorted (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        // Explicit `operation_id` so the managed-device list does not collide
        // with the NMOS Node API's `list_devices` (`/x-nmos/.../devices`), which
        // derives the same operationId from its handler name — a duplicate
        // operationId breaks the generated TypeScript client.
        operation_id = "list_managed_devices",
        path = "/api/v1/devices",
        tag = "devices",
        responses(
            (status = 200, description = "All devices, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_devices(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    let devices = state
        .devices
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .collect();
    Ok(Json(devices))
}

/// `GET /api/v1/devices/{id}` — fetch one device (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/{id}",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        responses(
            (status = 200, description = "The device (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this device.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_device(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.devices.get(&id)?;
    Ok(device_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/devices/{id}` — adopt/create a device (role: write).
///
/// Validates the body against `multiview_config::Device` (`422` on an invalid
/// document), seeds the runtime status registry in `ADOPTING` so
/// `GET /devices/{id}/status` answers immediately, and **starts the device's
/// supervised driver poller** (DEV-A4) — which performs the first probe and
/// drives the device to `ONLINE`/`AUTH_FAILED`/`UNREACHABLE`. The poller is a
/// no-op for devices the factory does not manage (the default build / a
/// non-`zowietek` driver).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        request_body = crate::openapi_schemas::DeviceResourceInputDoc,
        responses(
            (status = 201, description = "The adopted device (ETag in the response header; X-Multiview-Apply declares how it takes effect).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid device document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_device(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Devices, &id, &input.body)?,
    };
    // A fresh create clears any delete tombstone for this id BEFORE the store
    // insert (the ordering that keeps the registry's stop/start race-free: a
    // delete that races THIS create tombstones only after observing the insert
    // below, so this clear can never erase that delete's tombstone — see
    // `DevicePollerRegistry::clear_tombstone`).
    state.device_pollers.clear_tombstone(&id);
    let versioned = state.devices.create(&id, input)?;
    // Seed the runtime status in ADOPTING (idempotent), so the read-only status
    // snapshot answers before any driver probe — the conflated status lane's
    // backing store (invariant #10: control-plane-only, never the engine).
    state.device_status.ensure(&id);
    // Spawn the device's supervised driver poller (DEV-A4): a `zowietek` device
    // gets a live poller that logs in → probes → enumerates the three facets →
    // polls status → drives the lifecycle, so the projection routes return real
    // data and `set-mode` dispatches convergence. A no-op in the default build
    // (the no-op factory spawns nothing — no live transport) and for non-driver
    // devices; control-plane-only, off the engine hot loop (invariant #10).
    start_device_poller(&state, &versioned);
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        DEVICE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(device_response(
        StatusCode::CREATED,
        &versioned,
    )))
}

/// `PUT /api/v1/devices/{id}` — replace a device (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/devices/{id}",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        request_body = crate::openapi_schemas::DeviceResourceInputDoc,
        responses(
            (status = 200, description = "The replaced device (new ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid device document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_device(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Preconditions before content (RFC 9110 §13.2.2): a stale `If-Match` (or a
    // missing resource) is reported even when the submitted body is invalid.
    let current = state.devices.get(&id)?;
    if_match.require(DEVICE_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Devices, &id, &input.body)?,
    };
    let versioned = state.devices.update(&id, input)?;
    // Restart the supervised poller on an edit (DEV-A4): the address/credential/
    // desired-mode may have changed, so the registry replaces the running task
    // (the old one is aborted) and the fresh poller re-adopts + re-converges. A
    // no-op in the default build / for non-driver devices (invariant #10).
    start_device_poller(&state, &versioned);
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        DEVICE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(device_response(
        StatusCode::OK,
        &versioned,
    )))
}

/// `DELETE /api/v1/devices/{id}` — drop a device (role: administer; If-Match).
///
/// Refused `409 /problems/conflict` while any Source or Output still references
/// this device via `device_ref` (ADR-M009): the problem detail names the
/// blocking resource so the operator knows what to unbind. The delete is never
/// partially applied — a refused delete leaves the device intact.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/devices/{id}",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        responses(
            (status = 204, description = "The device was dropped."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
            (status = 409, description = "A Source/Output still references this device via device_ref.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_device(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.devices.get(&id)?;
    if_match.require(DEVICE_KIND, &id, current.version)?;
    // Refuse the delete while a Source/Output binds this device (ADR-M009): a
    // dangling `device_ref` would orphan the bound stream. Reads the resource
    // stores' opaque JSON bodies — control-plane-only, off the engine hot loop.
    if let Some(blocker) = first_device_ref_binding(&state, &id)? {
        return Err(ControlError::Conflict(format!(
            "device {id:?} cannot be deleted while {} {:?} references it via device_ref; \
             unbind it first",
            blocker.kind, blocker.resource_id
        )));
    }
    state.devices.delete(&id)?;
    // Stop the supervised driver poller (DEV-A4): the device is gone, so its
    // poller must not keep probing — the registry tombstones the id (a racing
    // adopt's late `start` is rejected, never a ghost poller), aborts the task,
    // and awaits its termination, so the forgets below cannot be raced by a
    // final in-flight publish. Then drop the runtime status and any
    // driver-enumerated facets — control-plane-only cleanup, off the engine hot
    // loop (invariant #10).
    state.device_pollers.stop(&id).await;
    state.device_status.forget(&id);
    state.device_drivers.forget(&id);
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        DEVICE_KIND,
        &id,
        None,
    );
    Ok(with_apply_restart(StatusCode::NO_CONTENT.into_response()))
}

/// One resource that binds a device via `device_ref` (the blocking reference a
/// `DELETE` reports).
struct DeviceRefBinding {
    /// The blocking resource's collection name (`source` / `output`).
    kind: &'static str,
    /// The blocking resource's id.
    resource_id: String,
}

/// The first Source or Output whose stored body carries `device_ref == device_id`,
/// or [`None`] when nothing binds the device (ADR-M009 binding check).
///
/// Scans the opaque JSON bodies of the sources then the outputs stores in
/// id-sorted order so the reported blocker is deterministic. Reading these
/// control-plane stores never touches the engine (invariant #10).
fn first_device_ref_binding(
    state: &AppState,
    device_id: &str,
) -> ControlResult<Option<DeviceRefBinding>> {
    for (store, kind) in [
        (&state.sources, crate::resource_store::SOURCE_KIND),
        (&state.outputs, crate::resource_store::OUTPUT_KIND),
    ] {
        for versioned in store.list()? {
            if versioned
                .resource
                .body
                .get("device_ref")
                .and_then(serde_json::Value::as_str)
                == Some(device_id)
            {
                return Ok(Some(DeviceRefBinding {
                    kind,
                    resource_id: versioned.resource.id,
                }));
            }
        }
    }
    Ok(None)
}

/// Start (or restart) the supervised driver poller for a just-adopted/edited
/// device (DEV-A4): parse the stored body back into a
/// [`multiview_config::Device`] and hand it to the runtime
/// [`DevicePollerRegistry`](crate::devices::DevicePollerRegistry), which spawns a
/// poller iff its factory manages this device (a `zowietek` device with a live
/// transport). A no-op otherwise — the default build's no-op factory spawns
/// nothing, so the projection routes stay honestly empty exactly as before.
///
/// A body that does not parse back to a `Device` is logged and skipped (it was
/// validated on create, so this is defensive); never a panic, never a `500`.
/// Off the engine hot loop, control-plane-only (invariant #10).
fn start_device_poller(state: &AppState, versioned: &VersionedResource) {
    match serde_json::from_value::<multiview_config::Device>(versioned.resource.body.clone()) {
        Ok(device) => {
            let _spawned = state.device_pollers.start(&device, &state.poller_wiring());
        }
        Err(e) => tracing::warn!(
            device = %versioned.resource.id,
            error = %e,
            "device body did not parse back to a Device; no poller spawned"
        ),
    }
}

/// `GET /api/v1/devices/{id}/status` — the read-only runtime status snapshot.
///
/// Reads the latest-wins [`DeviceStatusRegistry`](crate::devices::DeviceStatusRegistry)
/// (never persisted/exported). A freshly-adopted device with no driver probe yet
/// sits in `ADOPTING`. `404` when the device is not adopted.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/{id}/status",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        responses(
            (status = 200, description = "The device's latest-wins runtime status (never persisted).", body = crate::openapi_schemas::DeviceStatusDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this device.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id is adopted.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_device_status(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<multiview_events::DeviceStatus>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let status = state
        .device_status
        .snapshot(&id)
        .ok_or_else(|| ControlError::NotFound {
            kind: DEVICE_KIND,
            id: id.clone(),
        })?;
    Ok(Json(status))
}

/// Confirm a device exists (so an action/projection on an unknown id is a clean
/// `404`), returning its current versioned resource.
fn require_device(state: &AppState, id: &str) -> ControlResult<VersionedResource> {
    state.devices.get(id)
}

/// `POST /api/v1/devices/{id}/probe` — re-probe the device now (role: write).
///
/// A synchronous management verb (ADR-W017): it confirms the device exists and
/// acknowledges the probe request (`200`). The device's supervised driver poller
/// (DEV-A4) is already probing on its own ≤1 Hz cadence and re-probing on
/// reconnect, so the latest status is read via `GET /devices/{id}/status`; this
/// verb is the operator's explicit "I looked" acknowledgement.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}/probe",
        tag = "devices",
        params(("id" = String, Path, description = "Device id to probe.")),
        responses(
            (status = 200, description = "Probe acknowledged."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to probe.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn probe_device(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    require_device(&state, &id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        DEVICE_KIND,
        &id,
        Some(serde_json::json!({ "action": "probe" })),
    );
    Ok(StatusCode::OK)
}

/// The `POST /api/v1/devices/{id}/set-mode` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetModeRequest {
    /// The desired converged work mode (driver vocabulary, e.g. `encoder`).
    pub mode: String,
}

/// The `202 Accepted` body for `set-mode`: the operation id, the **declared**
/// pre-apply impact class, and the human-readable impact statement (ADR-M009 —
/// the API surfaces the impact BEFORE applying).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetModeAccepted {
    /// The operation id correlating this mode change's eventual outcome on the
    /// realtime stream (`device.mode`).
    pub operation_id: String,
    /// The declared impact class: a device-side (`dev`) impact — the device
    /// restarts its pipeline; Multiview program output is never interrupted.
    pub impact: String,
    /// The human-readable impact statement declared before apply.
    pub detail: String,
}

/// `POST /api/v1/devices/{id}/set-mode` — converge the device to a mode
/// (role: write; `202` + operation id + declared DEV-class impact).
///
/// The device-side impact is **declared in the body before apply** (ADR-M009):
/// the device restarts its pipeline; bound sources ride the tile ladder to
/// `NO_SIGNAL` during the switch; no Multiview output is interrupted. The route
/// mints the operation id, `202`s, and **dispatches** the convergence to the
/// device's running driver poller (DEV-A4), which records the requested mode as
/// its desired mode, runs `plan_mode_convergence` → `converge_mode`
/// (close-before-open), and publishes the `device.mode` outcome on the realtime
/// stream; a failed apply is re-converged on the poller's next adopt/reconnect
/// pass. When no live poller is running (the default build / a device with no
/// spawned driver), the `202` still declares the impact but nothing applies the
/// change: the device's configured `desired_mode` is what a later-spawned
/// poller converges onto when its adopt/reconnect reaches `ONLINE`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}/set-mode",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        request_body = SetModeRequest,
        responses(
            (status = 202, description = "Mode change accepted (impact declared); outcome on the realtime stream.", body = SetModeAccepted),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to change the mode.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn set_mode(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    Json(request): Json<SetModeRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    require_device(&state, &id)?;
    let op = reserve_operation(&state, &idem);
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        DEVICE_KIND,
        &id,
        Some(serde_json::json!({ "action": "set-mode", "mode": request.mode })),
    );
    // Dispatch the convergence to the device's running driver/poller (DEV-A4):
    // the actor records the requested mode as its desired mode, runs
    // `plan_mode_convergence` → `converge_mode` (close-before-open), and
    // publishes the DEV-class `device.mode` outcome on the realtime stream (a
    // failed apply re-converges on its next adopt/reconnect pass).
    // Non-blocking `try_send` — the route never awaits the actor (invariant #10).
    // When no poller is running (the default build's no-op factory, a device
    // whose driver was not spawned, or its control channel is momentarily full),
    // the 202 still declares the impact but nothing applies this request; the
    // configured `desired_mode` is what a later-spawned poller converges onto.
    let dispatched = state.device_pollers.dispatch(
        &id,
        crate::devices::PollerControl::SetMode {
            mode: request.mode.clone(),
        },
    );
    if !dispatched {
        tracing::debug!(
            device = %id,
            mode = %request.mode,
            "set-mode: no running poller to dispatch to (no live driver on this build/device); \
             the 202 declares the impact but nothing applies this request — the configured \
             desired_mode converges when a poller next adopts"
        );
    }
    // The declared DEV-class impact is the SAME statement the driver's
    // `ModeConvergence::Switch` plan declares (both derive it from
    // `mode_impact_detail`), so the API surfaces exactly what the driver will do
    // — ADR-M009: the impact is declared BEFORE apply.
    let body = SetModeAccepted {
        operation_id: op.to_string(),
        impact: "dev".to_owned(),
        detail: crate::devices::broadcaster::mode_impact_detail(&id, &request.mode),
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// `POST /api/v1/devices/{id}/reboot` — reboot the device (role: write; `202`).
///
/// A long-running management verb (ADR-W017): the operation id is minted and
/// `202`'d; the outcome arrives on the realtime stream once the driver lands.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}/reboot",
        tag = "devices",
        params(("id" = String, Path, description = "Device id to reboot.")),
        responses(
            (status = 202, description = "Reboot accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to reboot.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn reboot_device(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    require_device(&state, &id)?;
    let op = reserve_operation(&state, &idem);
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        DEVICE_KIND,
        &id,
        Some(serde_json::json!({ "action": "reboot" })),
    );
    let body = crate::routes::AcceptedBody {
        operation_id: op.to_string(),
        kind: "reboot".to_owned(),
        applied_live: None,
        carried_only: None,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// `POST /api/v1/devices/{id}/identify` — flash the device's identify indicator
/// (role: write; fire-and-forget `204`).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}/identify",
        tag = "devices",
        params(("id" = String, Path, description = "Device id to identify.")),
        responses(
            (status = 204, description = "Identify acknowledged."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to identify.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn identify_device(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    fire_and_forget(&state, &principal, &id, "identify")
}

/// `POST /api/v1/devices/{id}/test-pattern` — display a test pattern on the
/// device (role: write; fire-and-forget `204`).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}/test-pattern",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        responses(
            (status = 204, description = "Test-pattern acknowledged."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn test_pattern(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    fire_and_forget(&state, &principal, &id, "test-pattern")
}

/// Acknowledge a fire-and-forget device verb (`identify` / `test-pattern`):
/// require write + per-object authz, confirm the device exists, audit the
/// action, and return `204`.
fn fire_and_forget(
    state: &AppState,
    principal: &Principal,
    id: &str,
    action: &str,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(principal, id)?;
    require_device(state, id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        DEVICE_KIND,
        id,
        Some(serde_json::json!({ "action": action })),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Mint (or replay, by `Idempotency-Key`) an operation id for a long-running
/// device action.
///
/// Device actions run on the control-plane driver poller (DEV-A4), not the
/// engine command bus, so this only mints the operation id the `202` returns;
/// the driver publishes the matching `device.mode`/outcome event on the realtime
/// stream as the convergence runs. A retried `Idempotency-Key` returns the
/// original id.
fn reserve_operation(state: &AppState, idem: &IdempotencyKey) -> OperationId {
    match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Fresh(op) | Reservation::Replay(op) => op,
    }
}

/// `GET /api/v1/devices/{id}/source-candidates` — the source-binding projection
/// (ADR-M009 facet (a)).
///
/// Returns the device's running driver's enumerated candidates (DEV-A4: a
/// `zowietek` device's served RTSP mounts). Honestly empty until a driver has
/// enumerated — and on a build/device with no live driver — never fabricated
/// live telemetry.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/{id}/source-candidates",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        responses(
            (status = 200, description = "The declared source candidates (empty until a driver enumerates).", body = [crate::devices::projection::SourceCandidate]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this device.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn source_candidates(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<Vec<SourceCandidate>>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    require_device(&state, &id)?;
    // The driver registry holds whatever a live driver (DEV-A4 `zowietek`)
    // enumerated for this device; it is honestly empty until a driver has
    // enumerated, never fabricated live telemetry (ADR-M009).
    Ok(Json(state.device_drivers.source_candidates(&id)))
}

/// `GET /api/v1/devices/{id}/output-targets` — the output-binding projection
/// (ADR-M009 facet (b)).
///
/// Returns the device's running driver's enumerated targets (DEV-A4: a
/// decoder-mode `zowietek` box's decode-table slots). Honestly empty until a
/// driver has enumerated — and on a build/device with no live driver — never
/// fabricated live telemetry.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/{id}/output-targets",
        tag = "devices",
        params(("id" = String, Path, description = "Device id.")),
        responses(
            (status = 200, description = "The declared output targets (empty until a driver enumerates).", body = [crate::devices::projection::OutputTarget]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this device.", body = crate::problem::Problem),
            (status = 404, description = "No device with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn output_targets(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<Vec<OutputTarget>>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    require_device(&state, &id)?;
    // The driver registry holds whatever a live driver (DEV-A4 `zowietek`)
    // enumerated for this device; it is honestly empty until a driver has
    // enumerated, never fabricated live telemetry (ADR-M009).
    Ok(Json(state.device_drivers.output_targets(&id)))
}
