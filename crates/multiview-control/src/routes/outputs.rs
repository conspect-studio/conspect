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
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::IfMatch;
use crate::error::{ControlError, ControlResult};
use crate::resource_store::{Resource, ResourceInput, VersionedResource, OUTPUT_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply_restart, TypedCollection};

/// Read accepted intent rather than a lagging, fallible SQLite projection.
fn accepted_outputs(state: &AppState) -> ControlResult<Vec<VersionedResource>> {
    crate::boot_model::accepted_resources(state, "outputs")?
        .map_or_else(|| state.outputs.list(), Ok)
}

fn accepted_output(state: &AppState, id: &str) -> ControlResult<VersionedResource> {
    accepted_outputs(state)?
        .into_iter()
        .find(|entry| entry.resource.id == id)
        .ok_or_else(|| crate::error::ControlError::NotFound {
            kind: OUTPUT_KIND,
            id: id.into(),
        })
}

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
    let outputs = crate::boot_model::accepted_resources(&state, "outputs")?
        .map_or_else(|| state.outputs.list(), Ok)?
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
    let mut versioned = accepted_output(&state, &id)?;
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
    let _mutation = state.lock_config_mutation().await;
    if accepted_outputs(&state)?
        .iter()
        .any(|entry| entry.resource.id == id)
    {
        return Err(crate::error::ControlError::Validation(format!(
            "output {id:?} already exists"
        )));
    }
    crate::boot_model::commit_resource(
        &state,
        "outputs",
        &id,
        Some(input.body.clone()),
        Some(input.name.clone()),
    )
    .await?;
    if let Err(error) = state.outputs.create(&id, input) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(output = %id, %error, "accepted output committed; store projection failed");
    }
    let versioned = accepted_output(&state, &id).map_err(|error| {
        crate::error::ControlError::CommitIndeterminate(format!(
            "output {id:?} was committed, but its accepted representation could not be reread; GET before retrying: {error}"
        ))
    })?;
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
    let _mutation = state.lock_config_mutation().await;
    let current = accepted_output(&state, &id)?;
    if_match.require(OUTPUT_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Outputs, &id, &input.body)?,
    };
    crate::boot_model::commit_resource(
        &state,
        "outputs",
        &id,
        Some(input.body.clone()),
        Some(input.name.clone()),
    )
    .await?;
    if let Err(error) = state.outputs.update(&id, input) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(output = %id, %error, "accepted output committed; store projection failed");
    }
    let versioned = accepted_output(&state, &id).map_err(|error| {
        crate::error::ControlError::CommitIndeterminate(format!(
            "output {id:?} was committed, but its accepted representation could not be reread; GET before retrying: {error}"
        ))
    })?;
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
    let _mutation = state.lock_config_mutation().await;
    let current = accepted_output(&state, &id)?;
    if_match.require(OUTPUT_KIND, &id, current.version)?;
    crate::boot_model::commit_resource(&state, "outputs", &id, None, None).await?;
    if let Err(error) = state.outputs.delete(&id) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(output = %id, %error, "accepted output deletion committed; store projection failed");
    }
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        OUTPUT_KIND,
        &id,
        None,
    );
    Ok(with_apply_restart(StatusCode::NO_CONTENT.into_response()))
}

/// `GET /api/v1/outputs/{id}/playback` — derive this output's viewer
/// playback URL(s) (ADR-0101: "a link to copy the actual URL to paste into a
/// tool like VLC"). Role: read; per-object authz (BOLA) identical to
/// `get_output`.
///
/// Never returns a secret: an SRT/RIST pre-shared key is never read here (it
/// is a `secret_ref`, resolved only at the engine's run path) — an encrypted
/// listener instead carries an operator-facing note that a passphrase is
/// required out-of-band.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/outputs/{id}/playback",
        tag = "outputs",
        params(("id" = String, Path, description = "Output id.")),
        responses(
            (status = 200, description = "This output's viewer playback URL(s).", body = crate::openapi_schemas::OutputPlaybackResponseDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this output.", body = crate::problem::Problem),
            (status = 404, description = "No output with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_output_playback(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ControlResult<Json<OutputPlaybackResponse>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = accepted_output(&state, &id)?;
    let output: multiview_config::Output = serde_json::from_value(versioned.resource.body.clone())
        .map_err(|error| {
            ControlError::Validation(format!(
                "output {id:?}'s stored body is not a valid output document: {error}"
            ))
        })?;

    // Resolve the public host: `[control] public_host` wins over the request
    // `Host` header (nginx passes `Host` through unchanged), which wins over
    // "no public host known" (an honest note, never a guessed URL).
    let configured_host = state.public_host.as_deref();
    let header_host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(multiview_config::host_header_hostname);
    let public_host: Option<String> = configured_host.map(ToOwned::to_owned).or(header_host);
    let public_host_is_ip_literal = public_host
        .as_deref()
        .is_some_and(multiview_config::is_ip_literal);

    // Resolve the scheme for an absolute viewer URL (ADR-0101 scheme
    // derivation): the request's own `X-Forwarded-Proto` header (the
    // production nginx front end sets this), else the `[control]
    // public_scheme` override, else "https" when `public_host` is
    // explicitly CONFIGURED (not merely the Host-header fallback), else
    // plain "http" for an un-proxied direct request.
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok());
    let scheme = multiview_config::resolve_public_scheme(
        forwarded_proto,
        state.public_scheme.as_deref(),
        configured_host.is_some(),
    );

    // The HLS/LL-HLS mount route: resolved across EVERY accepted HLS/LL-HLS
    // output (not just this one), in declaration order, so a mount-name
    // collision resolves identically to the actual `/hls/{…}` router
    // (`multiview-cli::control::hls_mounts` delegates to the SAME
    // `resolve_hls_mounts` function).
    let all_outputs = accepted_outputs(&state)?;
    let hls_pairs: Vec<(String, String)> = all_outputs
        .iter()
        .filter_map(|entry| {
            let parsed: multiview_config::Output =
                serde_json::from_value(entry.resource.body.clone()).ok()?;
            match parsed {
                multiview_config::Output::Hls { path, .. }
                | multiview_config::Output::LlHls { path, .. } => {
                    Some((entry.resource.id.clone(), path))
                }
                _ => None,
            }
        })
        .collect();
    let hls_refs: Vec<(&str, &str)> = hls_pairs
        .iter()
        .map(|(oid, path)| (oid.as_str(), path.as_str()))
        .collect();
    let resolved_hls = multiview_config::resolve_hls_mounts(&hls_refs);
    let this_mount = resolved_hls.get(&id);

    let ctx = multiview_config::PlaybackContext {
        public_host: public_host.as_deref(),
        public_host_is_ip_literal,
        hls_route: this_mount.map(|m| m.route.as_str()),
        hls_playlist: this_mount.and_then(|m| m.playlist.as_deref()),
        scheme,
    };
    let urls = output
        .playback_urls(&ctx)
        .into_iter()
        .map(|u| crate::openapi_schemas::PlaybackUrlDoc {
            label: u.label,
            url: u.url,
            notes: u.notes,
            vlc_command: u.vlc_command,
        })
        .collect();
    Ok(Json(OutputPlaybackResponse { urls }))
}

/// `GET /api/v1/outputs/{id}/playback` response body (mirrors
/// [`crate::openapi_schemas::OutputPlaybackResponseDoc`] field-for-field — the
/// `OpenAPI` doc type exists only to describe the schema for a foreign
/// `multiview_config` type with no `ToSchema` derive, identical to the alarm
/// pattern this module's sibling routes use).
#[derive(serde::Serialize)]
pub(crate) struct OutputPlaybackResponse {
    urls: Vec<crate::openapi_schemas::PlaybackUrlDoc>,
}
