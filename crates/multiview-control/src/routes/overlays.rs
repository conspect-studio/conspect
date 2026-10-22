//! The overlays resource surface under `/api/v1/overlays`.
//!
//! An **overlay** is a managed overlay layer (`multiview_config::Overlay`):
//! clock, tally border, label, etc. This module mirrors the layouts handlers
//! ([`crate::routes`]) over the [`ResourceRepository`](crate::resource_store::ResourceRepository)
//! trait, with `ETag`/`If-Match` optimistic concurrency on every mutation
//! (ADR-W006), RBAC via [`Principal`], and an audit record after each successful
//! write. The stored `body` is the config-as-code document, **validated against
//! `multiview_config::Overlay` at this boundary** (ADR-W015), and every accepted
//! mutation declares its apply semantics via `X-Multiview-Apply` (ADR-W022):
//! when the binary injected an overlay live-apply capability, the mutation is
//! enqueued for the engine's frame-boundary drain and the header is `live` iff
//! the running renderer visibly draws the document. Errors are RFC 9457
//! problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::resource_store::{Resource, ResourceInput, VersionedResource, OVERLAY_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply, ApplyMode, TypedCollection};

/// Parse a stored (already ADR-W015-validated) overlay body back into the
/// canonical config type, or `None` for a legacy/foreign document that does
/// not parse (then no live apply is attempted — restart semantics).
fn parse_stored_overlay(body: &serde_json::Value) -> Option<multiview_config::Overlay> {
    serde_json::from_value(body.clone()).ok()
}

/// Apply a stored overlay upsert to the **running** engine where a live
/// overlay seam exists (ADR-W022, invariant #11), returning the apply
/// semantics the response must declare.
///
/// With a capability injected, the document **always** rides the bounded bus
/// (the engine's working-set mirror stays coherent; the drain warns for kinds
/// it cannot render — never silently, never lying). The header is `live` only
/// when the submit was accepted **and** the running picture visibly follows
/// the change: the renderer draws the **new** document, or it drew the
/// **previous** one (an edit that replaces a rendered face with a
/// non-rendering body makes that face vanish at the next frame — itself a
/// live-visible change, ADR-W022 §4). `previous` is the stored document the
/// upsert replaces (`None` on create). A shed submit (full/closed bus —
/// inv #10) or a mutation rendering neither before nor after degrades
/// honestly to `restart`.
fn live_apply_upsert(
    state: &AppState,
    overlay: Option<multiview_config::Overlay>,
    previous: Option<&multiview_config::Overlay>,
) -> ApplyMode {
    let Some(capability) = state.live_apply.overlays.as_ref() else {
        return ApplyMode::Restart;
    };
    let Some(overlay) = overlay else {
        return ApplyMode::Restart;
    };
    let renders =
        capability.renders(&overlay) || previous.is_some_and(|prev| capability.renders(prev));
    let submitted = state
        .commands
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(overlay.clone()),
        })
        .is_ok();
    // MAJOR-B round 4: the engine ADOPTED this overlay iff its UpsertOverlay
    // landed on the bus — only then does the adopted snapshot gain it (a shed
    // keeps the store but the engine did not receive it). The `renders` flag
    // only decides the `X-Multiview-Apply` header (a non-rendering overlay the
    // engine holds is still adopted config). The caller holds the
    // config-mutation lock, so this is ordered against the persister.
    if submitted {
        if let Some(model) = state.boot_model.as_ref() {
            model.adopt_overlay(overlay);
        }
    }
    if renders && submitted {
        ApplyMode::Live
    } else {
        ApplyMode::Restart
    }
}

/// Enqueue a live removal of overlay `id` for the running engine (ADR-W022),
/// returning the apply semantics the DELETE response must declare: `live` iff
/// the **previous** stored document was one the running renderer drew (its
/// face disappears at the next frame boundary) and the submit was accepted.
fn live_apply_remove(
    state: &AppState,
    previous: Option<&multiview_config::Overlay>,
    id: &str,
) -> ApplyMode {
    let Some(capability) = state.live_apply.overlays.as_ref() else {
        return ApplyMode::Restart;
    };
    let renders = previous.is_some_and(|overlay| capability.renders(overlay));
    let submitted = state
        .commands
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: id.to_owned(),
        })
        .is_ok();
    // MAJOR-B round 4: when the RemoveOverlay LANDS the engine drops the
    // overlay, so drop it from the adopted snapshot. A shed removal keeps the
    // overlay on screen, so the snapshot keeps it (active.toml is never told the
    // overlay is gone while the engine still renders it).
    if submitted {
        if let Some(model) = state.boot_model.as_ref() {
            model.unadopt_overlay(id);
        }
    }
    if renders && submitted {
        ApplyMode::Live
    } else {
        ApplyMode::Restart
    }
}

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
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): an overlay is gated by
    // its OWN id (`get_overlay` 403s an out-of-scope id), so the list must drop
    // rows outside the allowlist — exactly as `list_devices` does. An overlay
    // carries no embedded device_ref, so the row filter alone is complete; it is
    // a no-op for an unscoped principal.
    let overlays = state
        .overlays
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .filter(|resource| crate::auth::authorize_object(&principal, &resource.id).is_ok())
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
            (status = 201, description = "The created overlay (ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` when the running engine's renderer draws the document (e.g. an analog-face clock on an overlay-rendering build) and it was applied at a frame boundary; `restart` otherwise — non-rendering kinds are stored losslessly and mirrored to the engine with a warning, never lied about (ADR-W022).", body = crate::resource_store::Resource),
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
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // store-write → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    let versioned = state.overlays.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        OVERLAY_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    let mode = live_apply_upsert(&state, parse_stored_overlay(&versioned.resource.body), None);
    Ok(with_apply(
        mode,
        overlay_response(StatusCode::CREATED, &versioned),
    ))
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
            (status = 200, description = "The replaced overlay (new ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` when the edit was applied at a frame boundary and the running picture visibly follows it — the renderer draws the new document, or it drew the previous one (editing a rendered face away makes it vanish, itself a live change); `restart` otherwise (ADR-W022).", body = crate::resource_store::Resource),
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
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // precondition → store-write → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    // Preconditions are evaluated before request content (RFC 9110 §13.2.2):
    // a stale `If-Match` (or a missing resource) is reported even when the
    // submitted body is itself invalid.
    let current = state.overlays.get(&id)?;
    if_match.require(OVERLAY_KIND, &id, current.version)?;
    // The document this edit replaces: if the running renderer drew it, the
    // edit is live-visible even when the NEW body renders nothing (the old
    // face vanishes at the next frame boundary) — ADR-W022 §4.
    let previous = parse_stored_overlay(&current.resource.body);
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
    let mode = live_apply_upsert(
        &state,
        parse_stored_overlay(&versioned.resource.body),
        previous.as_ref(),
    );
    Ok(with_apply(
        mode,
        overlay_response(StatusCode::OK, &versioned),
    ))
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
            (status = 204, description = "The overlay was deleted. X-Multiview-Apply: `live` when the running renderer drew the document (its face disappears at the next frame boundary), `restart` otherwise (ADR-W022)."),
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
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // precondition → store-delete → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    let current = state.overlays.get(&id)?;
    if_match.require(OVERLAY_KIND, &id, current.version)?;
    let previous = parse_stored_overlay(&current.resource.body);
    state.overlays.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        OVERLAY_KIND,
        &id,
        None,
    );
    let mode = live_apply_remove(&state, previous.as_ref(), &id);
    Ok(with_apply(mode, StatusCode::NO_CONTENT.into_response()))
}
