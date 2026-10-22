//! The salvo operator surface under `/api/v1/salvos`.
//!
//! A **salvo** is a named, atomically-applied recall (layout + source rebinds +
//! forced tally + UMD text) the operator stages with **arm** and fires with
//! **take** (broadcast-multiviewer brief §8). This module exposes:
//!
//! * `GET /api/v1/salvos` — list salvo definitions (role: read).
//! * `GET /api/v1/salvos/{id}` — fetch one (role: read; `ETag`).
//! * `PUT /api/v1/salvos/{id}` — create-or-replace a definition (role: write;
//!   `If-Match` on replace → `412`). The body is validated by
//!   [`multiview_config::Salvo::validate`] before it is stored.
//! * `DELETE /api/v1/salvos/{id}` — delete a definition (role: administer;
//!   `If-Match`).
//! * `POST /api/v1/salvos/{id}/arm` — arm (stage) the salvo (role: write;
//!   `Idempotency-Key`; `202 Accepted` + operation id).
//! * `POST /api/v1/salvos/{id}/take` — take (atomically apply) the salvo (role:
//!   write; `Idempotency-Key`; `202`).
//! * `POST /api/v1/salvos/{id}/cancel` — cancel a previously-armed salvo (role:
//!   write; `Idempotency-Key`; `202`).
//!
//! The arm/take/cancel actions submit to the engine command bus and return `202`
//! immediately; their outcome arrives on the realtime stream as a
//! `salvo.armed` / `salvo.taken` / `salvo.cancelled` event (ADR-W008). The bus
//! submit is **non-blocking** — a full bus sheds to `503`, never blocking the
//! engine (invariant #10). Errors are RFC 9457 problem documents.
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::Salvo;
use serde::Deserialize;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::Command;
use crate::concurrency::{IdempotencyKey, IfMatch};
use crate::error::ControlResult;
use crate::routes::submit_accepted;
use crate::salvo_store::{VersionedSalvo, SALVO_KIND};
use crate::state::AppState;

/// Optional `?head=<id>` query selecting the output head an arm/take/cancel
/// targets (multi-head walls). Absent means the default/only head.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct HeadQuery {
    /// The output head this recall targets, if scoped to one head.
    #[serde(default)]
    pub head: Option<String>,
}

/// Attach the salvo's `ETag` to a successful response carrying the definition.
fn salvo_response(status: StatusCode, versioned: &VersionedSalvo) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.salvo.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/salvos` — list salvo definitions (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/salvos",
        tag = "salvos",
        responses(
            (status = 200, description = "All salvo definitions, id-sorted.", body = [crate::openapi_schemas::SalvoDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_salvos(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Salvo>>> {
    principal.role.require(Action::Read)?;
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): a salvo is gated by
    // its OWN id (`get_salvo` 403s an out-of-scope id), so the list must drop
    // rows outside the allowlist — exactly as `list_devices` does. A salvo
    // carries no embedded device_ref, so the row filter alone is complete; it is
    // a no-op for an unscoped principal.
    let salvos = state
        .salvos
        .list()?
        .into_iter()
        .map(|v| v.salvo)
        .filter(|salvo| crate::auth::authorize_object(&principal, &salvo.id).is_ok())
        .collect();
    Ok(Json(salvos))
}

/// `GET /api/v1/salvos/{id}` — fetch one salvo (role: read; per-object authz).
pub(crate) async fn get_salvo(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.salvos.get(&id)?;
    Ok(salvo_response(StatusCode::OK, &versioned))
}

/// `PUT /api/v1/salvos/{id}` — create-or-replace a salvo definition.
///
/// On create returns `201`; on replace enforces `If-Match` (→ `412`) and returns
/// `200`. The submitted body is validated by [`Salvo::validate`] first; the path
/// `id` is authoritative (the body's `id` is overwritten to match it).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/salvos/{id}",
        tag = "salvos",
        params(("id" = String, Path, description = "Salvo id.")),
        request_body = crate::openapi_schemas::SalvoDoc,
        responses(
            (status = 200, description = "The replaced salvo.", body = crate::openapi_schemas::SalvoDoc),
            (status = 201, description = "The created salvo.", body = crate::openapi_schemas::SalvoDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The salvo failed validation.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn put_salvo(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(mut salvo): Json<Salvo>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // The path id is authoritative; align the body and re-validate.
    salvo.id.clone_from(&id);
    salvo
        .validate()
        .map_err(|e| crate::error::ControlError::Validation(e.to_string()))?;

    match state.salvos.get(&id) {
        Ok(current) => {
            // Replace: enforce the optimistic-concurrency precondition first.
            if_match.require(SALVO_KIND, &id, current.version)?;
            let versioned = state.salvos.update(&id, salvo)?;
            // ADR-W024 round 6: a salvo definition is runtime-mutable running
            // state composed into active.toml; audit (the ONE persist choke
            // point) so the debounced persister captures the edit. The store
            // edit is always-commit (no engine command), so it IS adopted.
            state.audit(
                &principal.key_id,
                AuditAction::Update,
                SALVO_KIND,
                &id,
                Some(serde_json::to_value(&versioned.salvo).unwrap_or(serde_json::Value::Null)),
            );
            Ok(salvo_response(StatusCode::OK, &versioned))
        }
        Err(crate::error::ControlError::NotFound { .. }) => {
            let versioned = state.salvos.create(salvo)?;
            state.audit(
                &principal.key_id,
                AuditAction::Create,
                SALVO_KIND,
                &id,
                Some(serde_json::to_value(&versioned.salvo).unwrap_or(serde_json::Value::Null)),
            );
            Ok(salvo_response(StatusCode::CREATED, &versioned))
        }
        Err(other) => Err(other),
    }
}

/// `DELETE /api/v1/salvos/{id}` — delete a salvo (role: administer; If-Match).
pub(crate) async fn delete_salvo(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.salvos.get(&id)?;
    if_match.require(SALVO_KIND, &id, current.version)?;
    state.salvos.delete(&id)?;
    // ADR-W024 round 6: removing a runtime-mutable salvo definition changes the
    // composed running state — audit so the persister rewrites active.toml.
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        SALVO_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Confirm a salvo exists (so arm/take/cancel of an unknown id is a clean `404`
/// rather than an opaque engine no-op), then submit the engine command.
///
/// Enforces three authorization dimensions before the command is built:
/// the action gate (write), per-object authz on the salvo id (BOLA), and —
/// when the request addresses a specific output head — per-output authz on that
/// head ([`crate::auth::authorize_output`], per-output BOLA / OWASP API1). An
/// output-scoped principal addressing a head outside its allowlist is denied
/// here, at the HTTP boundary, before any command reaches the engine.
fn submit_for_existing_salvo(
    state: &AppState,
    principal: &Principal,
    id: &str,
    head: Option<String>,
    idem: &IdempotencyKey,
    build: impl FnOnce(crate::command::OperationId, Option<String>) -> Command,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(principal, id)?;
    // Per-output BOLA: an output-scoped principal may only address heads inside
    // its allowlist. Checked before reserving an idempotency key or touching the
    // engine bus, so a cross-output recall enqueues nothing.
    if let Some(head) = head.as_deref() {
        crate::auth::authorize_output(principal, head)?;
    }
    // Fail fast on an unknown salvo before reserving an idempotency key.
    let _ = state.salvos.get(id)?;
    submit_accepted(state, idem, |op| build(op, head))
}

/// `POST /api/v1/salvos/{id}/arm` — arm (stage) the salvo (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/salvos/{id}/arm",
        tag = "salvos",
        params(("id" = String, Path, description = "Salvo id to arm."), HeadQuery),
        responses(
            (status = 202, description = "Arm accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to arm.", body = crate::problem::Problem),
            (status = 404, description = "No salvo with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn arm_salvo(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    Query(head): Query<HeadQuery>,
) -> ControlResult<Response> {
    submit_for_existing_salvo(&state, &principal, &id, head.head, &idem, |op, head| {
        Command::ArmSalvo {
            op,
            salvo: id.clone(),
            head,
        }
    })
}

/// `POST /api/v1/salvos/{id}/take` — take (apply) the salvo (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/salvos/{id}/take",
        tag = "salvos",
        params(("id" = String, Path, description = "Salvo id to take."), HeadQuery),
        responses(
            (status = 202, description = "Take accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to take.", body = crate::problem::Problem),
            (status = 404, description = "No salvo with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn take_salvo(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    Query(head): Query<HeadQuery>,
) -> ControlResult<Response> {
    submit_for_existing_salvo(&state, &principal, &id, head.head, &idem, |op, head| {
        Command::TakeSalvo {
            op,
            salvo: Some(id.clone()),
            head,
        }
    })
}

/// `POST /api/v1/salvos/{id}/cancel` — cancel an armed salvo (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/salvos/{id}/cancel",
        tag = "salvos",
        params(("id" = String, Path, description = "Salvo id to cancel."), HeadQuery),
        responses(
            (status = 202, description = "Cancel accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to cancel.", body = crate::problem::Problem),
            (status = 404, description = "No salvo with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn cancel_salvo(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    Query(head): Query<HeadQuery>,
) -> ControlResult<Response> {
    submit_for_existing_salvo(&state, &principal, &id, head.head, &idem, |op, head| {
        Command::CancelSalvo {
            op,
            salvo: Some(id.clone()),
            head,
        }
    })
}
