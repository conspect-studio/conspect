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
/// * A **synthetic** kind (`bars`/`solid`/`clock`) is enqueued as
///   [`Command::UpsertSource`]; the engine drain registers it at a frame
///   boundary → `live`.
/// * A kind change **off** synthetic (`previous` synthetic, new kind not)
///   enqueues [`Command::RemoveSource`] — the running generator stops (a stale
///   picture pretending to be the new URL would be dishonest) — but the new
///   document itself still applies on `restart`.
/// * Network/decoded kinds, a full/closed bus (no engine draining), or an
///   unparseable stored doc → `restart`, honestly.
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
    if source.kind.is_synthetic() {
        let submitted = state
            .commands
            .try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source: Box::new(source),
            })
            .is_ok();
        return if submitted {
            ApplyMode::Live
        } else {
            ApplyMode::Restart
        };
    }
    if previous.is_some_and(|prev| prev.kind.is_synthetic()) {
        // Synthetic -> decoded kind change: stop the running generator now;
        // the stored decoded source applies on restart. A shed submit is
        // surfaced (never silent): the stale generator keeps rendering until
        // restart, and the operator should know why.
        if let Err(err) = state.commands.try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: source.id.clone(),
        }) {
            tracing::warn!(
                source = %source.id,
                error = %err,
                "kind-change RemoveSource shed: the running generator keeps \
                 rendering the old synthetic picture until restart"
            );
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
    if submitted {
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
    let sources = state
        .sources
        .list()?
        .into_iter()
        .map(|v| v.resource)
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
    let versioned = state.sources.get(&id)?;
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
            (status = 201, description = "The created source (ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` for synthetic kinds (bars/solid/clock) applied to the running engine at a frame boundary, `restart` otherwise (ADR-W018).", body = crate::resource_store::Resource),
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
            (status = 200, description = "The replaced source (new ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` for synthetic kinds applied to the running engine, `restart` otherwise (ADR-W018).", body = crate::resource_store::Resource),
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
