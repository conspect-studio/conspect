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
use crate::typed_resources::{
    validated_body, with_apply, with_apply_restart, ApplyMode, TypedCollection,
};

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
            (status = 200, description = "The replaced output (new ETag in the response header). `x-multiview-apply: live` when the change is ONLY the output's `format` and/or `slots.video[0].bitrate_kbps` on a running HLS output (TS segments): that output is reset live; with no `x-multiview-operation-id` when nothing changed (an identical PUT) or those settings are effectively unchanged (an absent bitrate = 4000 kbit/s, a canvas-size default-fit format = absent: nothing to reset) (about 1 s; Program and other outputs are not affected; `x-multiview-apply-note` says so, `x-multiview-operation-id` correlates the `route.output` outcome `reset:applied` or `reset:refused:<reason>`). Otherwise `x-multiview-apply: restart` (a codec, audio-slot or other change, or any other kind, SRT/RIST included: follow-up #380).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write (a live reset also needs the output axis).", body = crate::problem::Problem),
            (status = 404, description = "No output with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 429, description = "A live reset over 2/s for this output or 10/s for the principal (one reset = one take); nothing committed. See Retry-After.", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: a live reset could not be handed to the engine; the previous output is restored (nothing changed). Retry.", body = crate::problem::Problem),
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
    // #342 B3: a change of ONLY the picture format and/or video slot 0's
    // bitrate. Compared EFFECTIVELY (review M-4): equal effective settings
    // are a no-op (committed, applied, no reset); a real change on a running
    // HLS output (TS segments, review M-1) is reset live; anything else is
    // restart-pending.
    let change = classify_change(
        &current.resource.body,
        &input.body,
        state.running_canvas.as_ref(),
    );
    let live_reset = change == FormatChange::Reset
        && state.output_routing.is_some()
        && input.body.get("kind").and_then(serde_json::Value::as_str) == Some("hls");
    // Review X2 — the order, so the store and the engine never silently
    // disagree: validate → authz (both axes) → rate limit (charged) →
    // COMMIT → submit. A submit refused as `restart-required` keeps the
    // commit and answers restart (the stored change applies at the next
    // restart, today's contract) and refunds the rate-limit slot. A BUSY
    // engine re-commits the previous body (the store is back to what the
    // engine runs), refunds the slot and answers 503 `engine-busy`; if that
    // rollback itself fails the answer is an explicit `CommitIndeterminate`,
    // never a silent mismatch. Commit-then-submit (not submit-then-commit)
    // because a reset in flight has no cancel: a failed commit after a
    // submit would leave the engine applying a body the store never took.
    let mut charged = None;
    if live_reset {
        super::output_routing::authorize_output_end(&principal, &id)?;
        if let Some(control) = state.output_routing.as_ref() {
            let now = std::time::Instant::now();
            if let Err(retry) = control.limiter.admit(&id, &principal.key_id, now) {
                return Ok(super::output_routing::too_many_pub(retry));
            }
            charged = Some(now);
        }
    }
    let previous = ResourceInput {
        name: current.resource.name.clone(),
        body: current.resource.body.clone(),
    };
    commit_output(&state, &id, input).await?;
    let versioned = accepted_output(&state, &id).map_err(|error| {
        crate::error::ControlError::CommitIndeterminate(format!(
            "output {id:?} was committed, but its accepted representation could not be reread; GET before retrying: {error}"
        ))
    })?;
    let refund = |state: &AppState| {
        if let (Some(control), Some(at)) = (state.output_routing.as_ref(), charged) {
            control.limiter.refund(&id, &principal.key_id, at);
        }
    };
    if live_reset {
        match start_live_reset(&state, &principal, &versioned) {
            Ok(response) => {
                audit_update(&state, &principal, &id, &versioned);
                return Ok(response);
            }
            Err(crate::output_routing::ReformatSubmitError::Busy) => {
                refund(&state);
                commit_output(&state, &id, previous).await.map_err(|error| {
                    crate::error::ControlError::CommitIndeterminate(format!(
                        "output {id:?} was committed but the engine was busy and the rollback failed; GET before retrying: {error}"
                    ))
                })?;
                return Err(crate::error::ControlError::EngineBusy);
            }
            Err(crate::output_routing::ReformatSubmitError::RestartRequired) => refund(&state),
        }
    }
    audit_update(&state, &principal, &id, &versioned);
    if change == FormatChange::NoOp {
        // The effective settings are unchanged: nothing to reset or restart.
        return Ok(with_apply(
            ApplyMode::Live,
            output_response(StatusCode::OK, &versioned),
        ));
    }
    Ok(with_apply_restart(output_response(
        StatusCode::OK,
        &versioned,
    )))
}

/// Commit an output body to the boot model and project it to the store.
async fn commit_output(state: &AppState, id: &str, input: ResourceInput) -> ControlResult<()> {
    crate::boot_model::commit_resource(
        state,
        "outputs",
        id,
        Some(input.body.clone()),
        Some(input.name.clone()),
    )
    .await?;
    if let Err(error) = state.outputs.update(id, input) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(output = %id, %error, "accepted output committed; store projection failed");
    }
    Ok(())
}

fn audit_update(state: &AppState, principal: &Principal, id: &str, versioned: &VersionedResource) {
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        OUTPUT_KIND,
        id,
        Some(versioned.resource.body.clone()),
    );
}

/// The header carrying a live reset's operation id (its outcome rides
/// `route.output` as `reset:applied` / `reset:refused:<reason>`).
const RESET_OPERATION_HEADER: header::HeaderName =
    header::HeaderName::from_static("x-multiview-operation-id");
/// The header carrying the operator note of a live reset.
const RESET_NOTE_HEADER: header::HeaderName =
    header::HeaderName::from_static("x-multiview-apply-note");

/// What a PUT changes about an output's live-resettable settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatChange {
    /// Something other than `format` / video slot 0's bitrate changed:
    /// today's restart-pending answer.
    Other,
    /// Nothing changed (an identical PUT, review R2-M1), or only `format` /
    /// slot 0's bitrate changed with EQUAL effective values (review M-4): no
    /// reset, no restart.
    NoOp,
    /// Only `format` / slot 0's bitrate changed, effectively: a reset.
    Reset,
}

/// Classify `current` → `next` (#342 B3, review M-4). The raw documents may
/// differ only in `format` and `slots.video[0].bitrate_kbps`; those are then
/// compared EFFECTIVELY: an absent bitrate is the run default
/// ([`crate::output_routing::DEFAULT_VIDEO_KBPS`]) and an explicit format
/// equal to the canvas size with the default fit (and no other field) is the
/// same as an absent one — exactly how the engine reads them.
fn classify_change(
    current: &serde_json::Value,
    next: &serde_json::Value,
    canvas: Option<&multiview_config::LayoutCanvas>,
) -> FormatChange {
    let strip = |v: &serde_json::Value| -> serde_json::Value {
        let mut v = v.clone();
        if let Some(map) = v.as_object_mut() {
            map.remove("format");
            if let Some(slots) = map
                .get_mut("slots")
                .and_then(serde_json::Value::as_object_mut)
            {
                if let Some(slot0) = slots
                    .get_mut("video")
                    .and_then(serde_json::Value::as_array_mut)
                    .and_then(|video| video.first_mut())
                    .and_then(serde_json::Value::as_object_mut)
                {
                    slot0.remove("bitrate_kbps");
                }
                // An emptied slot / slot list / slots table is "absent".
                if let Some(video) = slots
                    .get_mut("video")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    video.retain(|slot| slot.as_object().is_none_or(|o| !o.is_empty()));
                    if video.is_empty() {
                        slots.remove("video");
                    }
                }
                if slots.is_empty() {
                    map.remove("slots");
                }
            }
        }
        v
    };
    if current == next {
        return FormatChange::NoOp;
    }
    if strip(current) != strip(next) {
        return FormatChange::Other;
    }
    let effective = |v: &serde_json::Value| -> serde_json::Value {
        let kbps = v
            .pointer("/slots/video/0/bitrate_kbps")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(u64::from(crate::output_routing::DEFAULT_VIDEO_KBPS));
        let format = serde_json::from_value::<multiview_config::OutputFormat>(
            v.get("format").cloned().unwrap_or(serde_json::Value::Null),
        )
        .ok();
        let format = format.and_then(|f| {
            let canvas_default = canvas.is_some_and(|c| {
                f == multiview_config::OutputFormat::new(
                    c.width,
                    c.height,
                    multiview_config::OutputFit::default(),
                )
            });
            (!canvas_default).then(|| serde_json::to_value(f).unwrap_or_default())
        });
        serde_json::json!({ "kbps": kbps, "format": format })
    };
    if effective(current) == effective(next) {
        FormatChange::NoOp
    } else {
        FormatChange::Reset
    }
}

/// Hand a committed format/bitrate change to the engine as a live reset of
/// that output: the `apply: live` response, or why it was not started
/// (the caller rolls back on `Busy`, answers restart on `RestartRequired`).
fn start_live_reset(
    state: &AppState,
    principal: &Principal,
    versioned: &VersionedResource,
) -> Result<Response, crate::output_routing::ReformatSubmitError> {
    use crate::output_routing::ReformatSubmitError;
    let control = state
        .output_routing
        .as_ref()
        .ok_or(ReformatSubmitError::RestartRequired)?;
    let config: multiview_config::Output = serde_json::from_value(versioned.resource.body.clone())
        .map_err(|_| ReformatSubmitError::RestartRequired)?;
    let operation_id = crate::command::OperationId::new().to_string();
    control
        .routing
        .try_reformat(crate::output_routing::OutputReformatRequest {
            operation_id: operation_id.clone(),
            output: config.id(),
            config,
            principal: principal.key_id.clone(),
        })?;
    let mut response = with_apply(ApplyMode::Live, output_response(StatusCode::OK, versioned));
    if let Ok(value) = header::HeaderValue::from_str(&operation_id) {
        response.headers_mut().insert(RESET_OPERATION_HEADER, value);
    }
    // A static, header-safe note (review X3: never the raw resource id).
    response.headers_mut().insert(
        RESET_NOTE_HEADER,
        header::HeaderValue::from_static(
            "resets this output (about 1 s); Program and other outputs are not affected",
        ),
    );
    Ok(response)
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

/// `GET /api/v1/outputs/{id}/link` — the live link state of a push output
/// (role: read; per-object authz).
///
/// Projects `outputs.<id>.link` out of the cached off-engine snapshot (inv
/// #10: never touches the engine). `404` when the output has no live push link
/// (not a push output, or the program is not running).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/outputs/{id}/link",
        tag = "outputs",
        params(("id" = String, Path, description = "Output id.")),
        responses(
            (status = 200, description = "The output's live push-link state.", body = crate::openapi_schemas::OutputLinkDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this output.", body = crate::problem::Problem),
            (status = 404, description = "No live push link for this output.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_output_link(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let not_found = || crate::error::ControlError::NotFound {
        kind: OUTPUT_KIND,
        id: id.clone(),
    };
    let versioned = accepted_output(&state, &id)?;
    let snapshot = state.engine.state.latest().ok_or_else(not_found)?;
    let runtime_id = runtime_output_id(&versioned.resource.body, &id);
    let mut link = output_link_from_snapshot(&snapshot, &runtime_id).ok_or_else(not_found)?;
    redact_link_for_read(&mut link);
    Ok((StatusCode::OK, Json(link)).into_response())
}

/// The id the running pipeline knows this output by. An output with no authored
/// id is `output-N` here but keyed by its derived label at run time.
fn runtime_output_id(body: &serde_json::Value, resource_id: &str) -> String {
    serde_json::from_value::<multiview_config::Output>(body.clone())
        .map_or_else(|_| resource_id.to_owned(), |output| output.id())
}

/// Defence in depth for a read-role response (review SRT-03): whatever the
/// pipeline published, `url` and every URL-shaped `peers` entry leave here in
/// the shared display-safe form, so no stream key, passphrase, stream id or
/// RIST secret reaches a viewer. Idempotent on already-redacted values.
fn redact_link_for_read(link: &mut serde_json::Value) {
    use multiview_core::redact::{redact_peer, redact_transport_url};
    if let Some(url) = link.get_mut("url") {
        *url = url
            .as_str()
            .map_or(serde_json::Value::Null, |u| redact_transport_url(u).into());
    }
    if let Some(peers) = link
        .get_mut("peers")
        .and_then(serde_json::Value::as_array_mut)
    {
        for peer in peers.iter_mut() {
            *peer = peer
                .as_str()
                .map_or(serde_json::Value::Null, |p| redact_peer(p).into());
        }
    }
}

/// `outputs.<id>.link` from the conflated snapshot, if present.
fn output_link_from_snapshot(
    snapshot: &crate::state::EngineStateSnapshot,
    id: &str,
) -> Option<serde_json::Value> {
    snapshot
        .get("outputs")
        .and_then(|outputs| outputs.get(id))
        .and_then(|output| output.get("link"))
        .filter(|link| link.is_object())
        .cloned()
}

#[cfg(test)]
mod link_tests {
    #![allow(clippy::unwrap_used)]
    use super::{output_link_from_snapshot, runtime_output_id};
    use serde_json::json;

    #[test]
    fn resource_ids_resolve_to_the_runtime_key() {
        let idless = json!({"kind": "srt", "url": "srt://[::]:9000", "codec": "h264"});
        assert_eq!(
            runtime_output_id(&idless, "output-0"),
            "srt srt://[::]:9000"
        );
        let authored =
            json!({"kind": "srt", "id": "main", "url": "srt://[::]:9000", "codec": "h264"});
        assert_eq!(runtime_output_id(&authored, "main"), "main");
        assert_eq!(runtime_output_id(&json!({"kind": "?"}), "x"), "x");
    }

    #[test]
    fn projects_the_link_or_none() {
        let snap = json!({
            "v": 1,
            "outputs": { "srt-out": { "link": { "state": "listening", "clients": 0 } } }
        });
        let link = output_link_from_snapshot(&snap, "srt-out").unwrap();
        assert_eq!(link["state"], "listening");
        assert!(output_link_from_snapshot(&snap, "other").is_none());
        assert!(output_link_from_snapshot(&json!({"v": 1}), "srt-out").is_none());
    }
}
