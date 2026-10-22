//! The outputs resource surface under `/api/v1/outputs`.
//!
//! An **output** is a managed sink/server (`multiview_config::Output`):
//! RTSP/LL-HLS/HLS/NDI/RTMP/SRT. This module mirrors the layouts handlers
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
use crate::resource_store::{Resource, ResourceInput, VersionedResource, OUTPUT_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply_restart, TypedCollection};

/// Attach the resource's `ETag` to a successful response carrying an output.
fn output_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/outputs` — list all outputs (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/outputs",
        tag = "outputs",
        responses(
            (status = 200, description = "All outputs, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_outputs(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): an output is gated by
    // its OWN id (`get_output` 403s an out-of-scope id), so the list must drop
    // rows outside the allowlist — exactly as `list_devices` does. THEN redact an
    // embedded out-of-scope `device_ref` on a surviving in-scope row. Both are
    // no-ops for an unscoped principal.
    let outputs = state
        .outputs
        .list()?
        .into_iter()
        .filter(|v| crate::auth::authorize_object(&principal, &v.resource.id).is_ok())
        .map(|v| {
            let mut resource = v.resource;
            crate::routes::redact_out_of_scope_device_refs(&principal, &mut resource);
            crate::support_bundle::redact_inline_secrets_for_read(&principal, &mut resource.body);
            resource
        })
        .collect();
    Ok(Json(outputs))
}

/// `GET /api/v1/outputs/{id}` — fetch one output (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/outputs/{id}",
        tag = "outputs",
        params(("id" = String, Path, description = "Output id.")),
        responses(
            (status = 200, description = "The output (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this output.", body = crate::problem::Problem),
            (status = 404, description = "No output with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_output(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let mut versioned = state.outputs.get(&id)?;
    // Redact an embedded out-of-scope `device_ref` (BOLA visibility,
    // ADR-W005/ADR-W025): the output itself is in scope, but its device-projection
    // link must not leak a device id the principal could not `GET`. No-op when
    // unscoped.
    crate::routes::redact_out_of_scope_device_refs(&principal, &mut versioned.resource);
    crate::support_bundle::redact_inline_secrets_for_read(&principal, &mut versioned.resource.body);
    Ok(output_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/outputs/{id}` — create an output (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/outputs/{id}",
        tag = "outputs",
        params(("id" = String, Path, description = "Output id.")),
        request_body = crate::openapi_schemas::OutputResourceInputDoc,
        responses(
            (status = 201, description = "The created output (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid output document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_output(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Outputs, &id, &input.body)?,
    };
    let versioned = state.outputs.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        OUTPUT_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(output_response(
        StatusCode::CREATED,
        &versioned,
    )))
}

/// `PUT /api/v1/outputs/{id}` — replace an output (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/outputs/{id}",
        tag = "outputs",
        params(("id" = String, Path, description = "Output id.")),
        request_body = crate::openapi_schemas::OutputResourceInputDoc,
        responses(
            (status = 200, description = "The replaced output (new ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No output with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_output(
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
    let current = state.outputs.get(&id)?;
    if_match.require(OUTPUT_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Outputs, &id, &input.body)?,
    };
    let versioned = state.outputs.update(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        OUTPUT_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(output_response(
        StatusCode::OK,
        &versioned,
    )))
}

/// `DELETE /api/v1/outputs/{id}` — delete an output (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/outputs/{id}",
        tag = "outputs",
        params(("id" = String, Path, description = "Output id.")),
        responses(
            (status = 204, description = "The output was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No output with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_output(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.outputs.get(&id)?;
    if_match.require(OUTPUT_KIND, &id, current.version)?;
    state.outputs.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        OUTPUT_KIND,
        &id,
        None,
    );
    Ok(with_apply_restart(StatusCode::NO_CONTENT.into_response()))
}
