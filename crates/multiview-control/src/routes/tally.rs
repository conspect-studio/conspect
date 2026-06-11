//! The tally operator surface under `/api/v1/tally`.
//!
//! Three concerns (broadcast-multiviewer brief §2):
//!
//! * **Read resolved tally.** `GET /api/v1/tally` lists every tile/element's
//!   current resolved lamp state from the control plane's
//!   [`TallyMirror`](crate::tally_state::TallyMirror), which the engine feeds
//!   lossily (lagged-skip) over the event stream. Role: read.
//! * **Configure tally profiles.** `GET/PUT/DELETE /api/v1/tally/profiles[/{id}]`
//!   manage the config-as-code [`multiview_config::TallyProfile`] (the bit↔colour
//!   and index↔cell binding for an external tally bus), with `ETag`/`If-Match`.
//!   Read role to list/get; write to put; administer to delete.
//! * **Manual override.** `PUT /api/v1/tally/override` forces a target's lamp to
//!   a fixed colour (or `DELETE` clears it), submitting a `SetTallyOverride`
//!   command to the engine (`202` + operation id; outcome on the realtime
//!   stream) and recording the request in the control-plane override registry.
//!   Role: write.
//!
//! The override submit is **non-blocking**: a full command bus sheds to `503`,
//! never blocking the engine (invariant #10). Errors are RFC 9457 problem
//! documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::TallyProfile;
use multiview_core::tally::TallyColor;
use multiview_events::TallyTarget;
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::command::Command;
use crate::concurrency::{IdempotencyKey, IfMatch};
use crate::error::{ControlError, ControlResult};
use crate::routes::submit_accepted;
use crate::state::AppState;
use crate::tally_state::{TallyEntry, VersionedProfile, TALLY_PROFILE_KIND};

/// The body of a `PUT /api/v1/tally/override` request: force `target`'s lamp to
/// `color`. A `color` of [`None`] is rejected here (use `DELETE` to clear); the
/// engine command carries `Some(color)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideRequest {
    /// The tally target the override applies to.
    pub target: TallyTarget,
    /// The lamp colour to force.
    pub color: TallyColor,
}

/// The body of a `DELETE /api/v1/tally/override` request: clear any override on
/// `target`, returning it to arbitration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearOverrideRequest {
    /// The tally target whose override is cleared.
    pub target: TallyTarget,
}

/// Attach a profile's `ETag` to a successful response carrying the profile.
fn profile_response(status: StatusCode, versioned: &VersionedProfile) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.profile.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/tally` — list resolved tally state (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/tally",
        tag = "tally",
        responses(
            (status = 200, description = "Resolved tally state per target.", body = [crate::openapi_schemas::TallyEntryDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_tally(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<TallyEntry>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.tally.list()))
}

/// `GET /api/v1/tally/profiles` — list tally profiles (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/tally/profiles",
        tag = "tally",
        responses(
            (status = 200, description = "All tally profiles, id-sorted.", body = [crate::openapi_schemas::TallyProfileDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_profiles(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<TallyProfile>>> {
    principal.role.require(Action::Read)?;
    let profiles = state
        .tally_profiles
        .list()?
        .into_iter()
        .map(|v| v.profile)
        .collect();
    Ok(Json(profiles))
}

/// `GET /api/v1/tally/profiles/{id}` — fetch one tally profile (role: read).
pub(crate) async fn get_profile(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let versioned = state.tally_profiles.get(&id)?;
    Ok(profile_response(StatusCode::OK, &versioned))
}

/// `PUT /api/v1/tally/profiles/{id}` — create-or-replace a tally profile.
///
/// On create returns `201`; on replace enforces `If-Match` (→ `412`) and returns
/// `200`. The body is validated by [`TallyProfile::validate`] first; the path
/// `id` is authoritative.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/tally/profiles/{id}",
        tag = "tally",
        params(("id" = String, Path, description = "Tally profile id.")),
        request_body = crate::openapi_schemas::TallyProfileDoc,
        responses(
            (status = 200, description = "The replaced profile.", body = crate::openapi_schemas::TallyProfileDoc),
            (status = 201, description = "The created profile.", body = crate::openapi_schemas::TallyProfileDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The profile failed validation.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn put_profile(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(mut profile): Json<TallyProfile>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    profile.id.clone_from(&id);
    profile
        .validate()
        .map_err(|e| ControlError::Validation(e.to_string()))?;

    // On a replace, require a matching If-Match against the live version.
    match state.tally_profiles.get(&id) {
        Ok(current) => {
            if_match.require(TALLY_PROFILE_KIND, &id, current.version)?;
            let versioned = state.tally_profiles.put(profile)?;
            Ok(profile_response(StatusCode::OK, &versioned))
        }
        Err(ControlError::NotFound { .. }) => {
            let versioned = state.tally_profiles.put(profile)?;
            Ok(profile_response(StatusCode::CREATED, &versioned))
        }
        Err(other) => Err(other),
    }
}

/// `DELETE /api/v1/tally/profiles/{id}` — delete a profile (role: administer).
pub(crate) async fn delete_profile(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Administer)?;
    let current = state.tally_profiles.get(&id)?;
    if_match.require(TALLY_PROFILE_KIND, &id, current.version)?;
    state.tally_profiles.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/v1/tally/override` — force a target's lamp (role: write; 202).
///
/// Submits a `SetTallyOverride` command to the engine and records the request in
/// the control-plane override registry. The engine *applies* the override; this
/// endpoint reports `202` (the resolved state arrives later on the realtime
/// stream as a `tally.state` event).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/tally/override",
        tag = "tally",
        request_body = crate::openapi_schemas::OverrideRequestDoc,
        responses(
            (status = 202, description = "Override accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn set_override(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<OverrideRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    let target = req.target.clone();
    let color = req.color;
    let response = submit_accepted(&state, &idem, |op| Command::SetTallyOverride {
        op,
        target: req.target,
        color: Some(req.color),
    })?;
    // Only record the override locally once the command was actually enqueued
    // (submit_accepted returns Err — mapped to 503 — when the bus shed it).
    state.tally_overrides.set(&target, color);
    Ok(response)
}

/// `DELETE /api/v1/tally/override` — clear a target's override (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/tally/override",
        tag = "tally",
        request_body = crate::openapi_schemas::ClearOverrideRequestDoc,
        responses(
            (status = 202, description = "Clear accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn clear_override(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<ClearOverrideRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    let target = req.target.clone();
    let response = submit_accepted(&state, &idem, |op| Command::SetTallyOverride {
        op,
        target: req.target,
        color: None,
    })?;
    state.tally_overrides.clear(&target);
    Ok(response)
}
