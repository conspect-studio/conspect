//! The sources resource surface under `/api/v1/sources`.
//!
//! A **source** is a managed input (`multiview_config::Source`): RTSP/HLS/SRT/
//! RTMP/NDI/file/test. This module mirrors the layouts handlers
//! ([`crate::routes`]) over the [`ResourceRepository`](crate::resource_store::ResourceRepository)
//! trait, with `ETag`/`If-Match` optimistic concurrency on every mutation
//! (ADR-W006), RBAC via [`Principal`], and an audit record after each successful
//! write. The stored `body` is the config-as-code document, **validated against
//! `multiview_config::Source` at this boundary** (ADR-W015): an invalid
//! document is rejected with `422 /problems/validation` naming the field path,
//! and every accepted mutation declares its apply semantics via
//! `X-Multiview-Apply`. Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::resource_store::{Resource, ResourceInput, VersionedResource, SOURCE_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply, ApplyMode, TypedCollection};

/// Parse a stored (already ADR-W015-validated) source body back into the
/// canonical config type, or `None` for a legacy/foreign document that does
/// not parse (then no live apply is attempted — restart semantics).
fn parse_stored_source(body: &serde_json::Value) -> Option<multiview_config::Source> {
    serde_json::from_value(body.clone()).ok()
}

/// Apply a stored source mutation to the **running** engine where the engine
/// can take it live (ADR-W018, invariant #11), returning the apply semantics
/// the response must declare.
///
/// * A kind the run declared **live-appliable**
///   ([`AppState::live_sources`](crate::LiveSourceCapability) — synthetic kinds
///   on every run; network/file kinds when the run wired a real ingest spawner
///   into its live-source hub) is enqueued as [`Command::UpsertSource`]; the
///   engine drain registers it at a frame boundary → `live`.
/// * A kind change from a live-appliable kind to one the run **cannot** apply
///   live (e.g. `rtsp` → `ndi`) enqueues [`Command::RemoveSource`] — the
///   running producer stops (a stale picture pretending to be the new feed
///   would be dishonest) — but the new document itself applies on `restart`.
/// * Non-live kinds (`ndi`/`youtube`/`aes67`, or network kinds on a run with
///   no decoder), a full/closed bus (no engine draining), or an unparseable
///   stored doc → `restart`, honestly.
///
/// Submission is `try_submit` (bounded, non-blocking, ADR-W008): control can
/// never block on the engine (invariant #10); a shed submit degrades the
/// header to `restart` (the stored doc remains the durable truth).
fn live_apply_upsert(
    state: &AppState,
    source: Option<multiview_config::Source>,
    previous: Option<&multiview_config::Source>,
) -> ApplyMode {
    let Some(source) = source else {
        return ApplyMode::Restart;
    };
    if state.live_sources.is_live(&source.kind) {
        let submitted = state
            .commands
            .try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source: Box::new(source.clone()),
            })
            .is_ok();
        // MAJOR-B round 4: only a LANDED command is adopted, so only then does
        // the adopted snapshot (what `active.toml` is rendered from) gain this
        // source. A shed keeps the store (ADR-W018 restart) but is NOT adopted,
        // so it stays out of `active.toml` until a restart. The caller holds
        // the config-mutation lock, so this is ordered against the persister.
        if submitted {
            if let Some(model) = state.boot_model.as_ref() {
                model.adopt_source(source);
            }
            return ApplyMode::Live;
        }
        return ApplyMode::Restart;
    }
    if previous.is_some_and(|prev| state.live_sources.is_live(&prev.kind)) {
        // Synthetic -> decoded kind change: stop the running generator now;
        // the stored decoded source applies on restart. A shed submit is
        // surfaced (never silent): the stale generator keeps rendering until
        // restart, and the operator should know why.
        let removed = state.commands.try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: source.id.clone(),
        });
        if let Err(err) = &removed {
            tracing::warn!(
                source = %source.id,
                error = %err,
                "kind-change RemoveSource shed: the running generator keeps \
                 rendering the old synthetic picture until restart"
            );
        }
        // MAJOR-B round 4: when the stop LANDS the engine no longer runs the
        // old synthetic source, so drop it from the adopted snapshot. The new
        // decoded source is restart-only — NOT adopted — so it never enters the
        // snapshot here.
        if removed.is_ok() {
            if let Some(model) = state.boot_model.as_ref() {
                model.unadopt_source(&source.id);
            }
        }
    }
    ApplyMode::Restart
}

/// Enqueue a live removal of `id` for the running engine, returning the apply
/// semantics the DELETE response must declare (`live` iff enqueued).
fn live_apply_remove(state: &AppState, id: &str) -> ApplyMode {
    let submitted = state
        .commands
        .try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: id.to_owned(),
        })
        .is_ok();
    // MAJOR-B round 4: only when the removal LANDS does the engine stop running
    // the source, so only then is it dropped from the adopted snapshot. A shed
    // removal keeps the source running, so the snapshot keeps it (active.toml is
    // never told the source is gone while the engine still runs it).
    if submitted {
        if let Some(model) = state.boot_model.as_ref() {
            model.unadopt_source(id);
        }
        ApplyMode::Live
    } else {
        ApplyMode::Restart
    }
}

/// Attach the resource's `ETag` to a successful response carrying a source.
fn source_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/sources` — list all sources (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources",
        tag = "sources",
        responses(
            (status = 200, description = "All sources, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_sources(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): a source is gated by
    // its OWN id (`get_source` 403s an out-of-scope id), so the list must drop
    // rows outside the allowlist — exactly as `list_devices` does. THEN redact an
    // embedded out-of-scope `device_ref` on a surviving in-scope row (its
    // device-projection link may point to a device the principal cannot `GET`).
    // Both are no-ops for an unscoped principal.
    let sources = state
        .sources
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
    Ok(Json(sources))
}

/// `GET /api/v1/sources/{id}` — fetch one source (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 200, description = "The source (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this source.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let mut versioned = state.sources.get(&id)?;
    // Redact an embedded out-of-scope `device_ref` (BOLA visibility,
    // ADR-W005/ADR-W025): the source itself is in scope (authorized above), but
    // its device-projection link must not leak a device id the principal could
    // not `GET`. No-op when unscoped.
    crate::routes::redact_out_of_scope_device_refs(&principal, &mut versioned.resource);
    crate::support_bundle::redact_inline_secrets_for_read(&principal, &mut versioned.resource.body);
    Ok(source_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/sources/{id}` — create a source (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        request_body = crate::openapi_schemas::SourceResourceInputDoc,
        responses(
            (status = 201, description = "The created source (ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` when the running engine applies it at a frame boundary — synthetic kinds (bars/solid/clock) on every run, network/file kinds (rtsp/hls/ts/srt/rtmp/rist/file) on a full-engine run — `restart` otherwise (ndi/youtube/aes67, or a run without the decoder; ADR-W018).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid source document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Sources, &id, &input.body)?,
    };
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // store-write → submit → adopt-snapshot sequence, so the persister (which
    // takes the same lock) can never compose mid-mutation and the adopted
    // snapshot advances atomically with the engine submit.
    let _mutation = state.lock_config_mutation().await;
    let versioned = state.sources.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        SOURCE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    let mode = live_apply_upsert(&state, parse_stored_source(&versioned.resource.body), None);
    Ok(with_apply(
        mode,
        source_response(StatusCode::CREATED, &versioned),
    ))
}

/// `PUT /api/v1/sources/{id}` — replace a source (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        request_body = crate::openapi_schemas::SourceResourceInputDoc,
        responses(
            (status = 200, description = "The replaced source (new ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` when the running engine applies it at a frame boundary (synthetic kinds on every run; network/file kinds on a full-engine run — an edit swaps the producer behind the same tile store), `restart` otherwise (ADR-W018).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid source document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // precondition → store-write → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    // Preconditions are evaluated before request content (RFC 9110 §13.2.2):
    // a stale `If-Match` (or a missing resource) is reported even when the
    // submitted body is itself invalid.
    let current = state.sources.get(&id)?;
    if_match.require(SOURCE_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Sources, &id, &input.body)?,
    };
    let previous = parse_stored_source(&current.resource.body);
    let versioned = state.sources.update(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        SOURCE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    let mode = live_apply_upsert(
        &state,
        parse_stored_source(&versioned.resource.body),
        previous.as_ref(),
    );
    Ok(with_apply(
        mode,
        source_response(StatusCode::OK, &versioned),
    ))
}

/// `DELETE /api/v1/sources/{id}` — delete a source (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 204, description = "The source was deleted. X-Multiview-Apply: `live` when the running engine unregisters it at a frame boundary (bound tiles ride their failover slate), `restart` when no engine is draining (ADR-W018)."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // precondition → store-delete → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    let current = state.sources.get(&id)?;
    if_match.require(SOURCE_KIND, &id, current.version)?;
    state.sources.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        SOURCE_KIND,
        &id,
        None,
    );
    let mode = live_apply_remove(&state, &id);
    Ok(with_apply(mode, StatusCode::NO_CONTENT.into_response()))
}
