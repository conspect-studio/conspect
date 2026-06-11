//! The probes resource surface under `/api/v1/probes`.
//!
//! A **probe** is a per-cell fail-state detector (`multiview_config::Probe`):
//! black / freeze / silence / loudness-violation, each with a detection zone,
//! threshold, dwell windows, and the X.733 severity it asserts. This module
//! mirrors the sources handlers ([`crate::routes::sources`]) over the
//! [`ResourceRepository`](crate::resource_store::ResourceRepository) trait,
//! with `ETag`/`If-Match` optimistic concurrency on every mutation (ADR-W006),
//! RBAC via [`Principal`], and an audit record after each successful write. The
//! stored `body` is the config-as-code document, **validated against
//! `multiview_config::Probe` at this boundary** (ADR-W015): an invalid document
//! is rejected with `422 /problems/validation` naming the field path, and every
//! accepted mutation declares its apply semantics via `X-Multiview-Apply`.
//! Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::resource_store::{Resource, ResourceInput, VersionedResource, PROBE_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply_restart, TypedCollection};

/// Attach the resource's `ETag` to a successful response carrying a probe.
fn probe_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/probes` — list all probes (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/probes",
        tag = "probes",
        responses(
            (status = 200, description = "All probes, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_probes(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    let probes = state
        .probes
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .collect();
    Ok(Json(probes))
}

/// `GET /api/v1/probes/{id}` — fetch one probe (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/probes/{id}",
        tag = "probes",
        params(("id" = String, Path, description = "Probe id.")),
        responses(
            (status = 200, description = "The probe (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this probe.", body = crate::problem::Problem),
            (status = 404, description = "No probe with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_probe(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.probes.get(&id)?;
    Ok(probe_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/probes/{id}` — create a probe (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/probes/{id}",
        tag = "probes",
        params(("id" = String, Path, description = "Probe id.")),
        request_body = crate::openapi_schemas::ProbeResourceInputDoc,
        responses(
            (status = 201, description = "The created probe (ETag in the response header; X-Multiview-Apply declares how it takes effect).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid probe document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_probe(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Probes, &id, &input.body)?,
    };
    let versioned = state.probes.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        PROBE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(probe_response(
        StatusCode::CREATED,
        &versioned,
    )))
}

/// `PUT /api/v1/probes/{id}` — replace a probe (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/probes/{id}",
        tag = "probes",
        params(("id" = String, Path, description = "Probe id.")),
        request_body = crate::openapi_schemas::ProbeResourceInputDoc,
        responses(
            (status = 200, description = "The replaced probe (new ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No probe with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid probe document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_probe(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Preconditions are evaluated before request content (RFC 9110 §13.2.2):
    // a stale `If-Match` (or a missing resource) is reported even when the
    // submitted body is itself invalid.
    let current = state.probes.get(&id)?;
    if_match.require(PROBE_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Probes, &id, &input.body)?,
    };
    let versioned = state.probes.update(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        PROBE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(probe_response(
        StatusCode::OK,
        &versioned,
    )))
}

/// `DELETE /api/v1/probes/{id}` — delete a probe (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/probes/{id}",
        tag = "probes",
        params(("id" = String, Path, description = "Probe id.")),
        responses(
            (status = 204, description = "The probe was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No probe with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_probe(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.probes.get(&id)?;
    if_match.require(PROBE_KIND, &id, current.version)?;
    state.probes.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        PROBE_KIND,
        &id,
        None,
    );
    Ok(with_apply_restart(StatusCode::NO_CONTENT.into_response()))
}
