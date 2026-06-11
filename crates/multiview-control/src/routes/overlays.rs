//! The overlays resource surface under `/api/v1/overlays`.
//!
//! An **overlay** is a managed overlay layer (`multiview_config::Overlay`):
//! clock, tally border, label, etc. This module mirrors the layouts handlers
//! ([`crate::routes`]) over the [`ResourceRepository`](crate::resource_store::ResourceRepository)
//! trait, with `ETag`/`If-Match` optimistic concurrency on every mutation
//! (ADR-W006), RBAC via [`Principal`], and an audit record after each successful
//! write. The stored `body` is the opaque config-as-code document; engine-side
//! validation against `multiview-config` happens before it is applied. Errors
//! are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::resource_store::{Resource, ResourceInput, VersionedResource, OVERLAY_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply_restart, TypedCollection};

/// Attach the resource's `ETag` to a successful response carrying an overlay.
fn overlay_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/overlays` — list all overlays (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/overlays",
        tag = "overlays",
        responses(
            (status = 200, description = "All overlays, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_overlays(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    let overlays = state
        .overlays
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .collect();
    Ok(Json(overlays))
}

/// `GET /api/v1/overlays/{id}` — fetch one overlay (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/overlays/{id}",
        tag = "overlays",
        params(("id" = String, Path, description = "Overlay id.")),
        responses(
            (status = 200, description = "The overlay (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this overlay.", body = crate::problem::Problem),
            (status = 404, description = "No overlay with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_overlay(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.overlays.get(&id)?;
    Ok(overlay_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/overlays/{id}` — create an overlay (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/overlays/{id}",
        tag = "overlays",
        params(("id" = String, Path, description = "Overlay id.")),
        request_body = crate::openapi_schemas::OverlayResourceInputDoc,
        responses(
            (status = 201, description = "The created overlay (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid overlay document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_overlay(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Overlays, &id, &input.body)?,
    };
    let versioned = state.overlays.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        OVERLAY_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(overlay_response(
        StatusCode::CREATED,
        &versioned,
    )))
}

/// `PUT /api/v1/overlays/{id}` — replace an overlay (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/overlays/{id}",
        tag = "overlays",
        params(("id" = String, Path, description = "Overlay id.")),
        request_body = crate::openapi_schemas::OverlayResourceInputDoc,
        responses(
            (status = 200, description = "The replaced overlay (new ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No overlay with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_overlay(
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
    let current = state.overlays.get(&id)?;
    if_match.require(OVERLAY_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Overlays, &id, &input.body)?,
    };
    let versioned = state.overlays.update(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        OVERLAY_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(overlay_response(
        StatusCode::OK,
        &versioned,
    )))
}

/// `DELETE /api/v1/overlays/{id}` — delete an overlay (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/overlays/{id}",
        tag = "overlays",
        params(("id" = String, Path, description = "Overlay id.")),
        responses(
            (status = 204, description = "The overlay was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No overlay with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_overlay(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.overlays.get(&id)?;
    if_match.require(OVERLAY_KIND, &id, current.version)?;
    state.overlays.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        OVERLAY_KIND,
        &id,
        None,
    );
    Ok(with_apply_restart(StatusCode::NO_CONTENT.into_response()))
}
