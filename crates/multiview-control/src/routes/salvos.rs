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
//! * `POST /api/v1/salvos/{id}/plan` — plan a patch salvo's crosspoints
//!   (role: write; the `POST /patch/plan` answer; no change).
//!
//! A salvo may carry **patch** crosspoints (ADR-0110 decision 8, #308):
//! `PUT` authorizes every member's both ends at store time (a principal can
//! never store a salvo it could not take), and `take` runs them through the
//! atomic set path of `POST /patch/take` (its own `salvo-take:<id>` key
//! namespace; outcome `patch.take`) and then submits the legacy recall
//! exactly as before when the salvo has one. A salvo with only legacy
//! fields keeps its path byte-for-byte.
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
    let salvos = crate::boot_model::accepted_salvos(&state)?
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
    let versioned = crate::boot_model::accepted_salvo(&state, &id)?;
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
            (status = 403, description = "Not authorized to write, or (a patch salvo, #308) not authorized for a patch member's destination or source: every member's both ends are authorized at store time.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The salvo failed validation (a patch salvo: more than 64 crosspoints, a destination twice, an unknown source, mixer input, output or layout, or an unsupported crosspoint shape).", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: a patch member's `{layout}` could not be read now; retry.", body = crate::problem::Problem),
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
    // #308: every patch member's both ends, before anything is stored, so a
    // principal cannot store a salvo it could not take; then the shape and
    // the outputs/layouts it names (the document validation checks sources
    // and mixer inputs).
    if !salvo.patch.is_empty() {
        let req = crate::routes::patch_set::salvo_request(&salvo);
        let layouts =
            match crate::routes::patch::read_patch_layouts(&state, &principal, &req).await? {
                Ok(layouts) => layouts,
                Err(response) => return Ok(response),
            };
        crate::routes::patch::authorize(&principal, &req, &layouts)?;
        crate::routes::patch_set::validate_stored(&state, &req, &layouts)?;
    }

    // Review H1: serialize with every other durable config mutation and
    // commit the desired envelope before the store projection changes.
    let _mutation = state.lock_config_mutation().await;
    let durable = crate::boot_model::config_value(&salvo)?;
    match crate::boot_model::accepted_salvo(&state, &id) {
        Ok(current) => {
            // Replace: enforce the optimistic-concurrency precondition first.
            if_match.require(SALVO_KIND, &id, current.version)?;
            crate::boot_model::commit_resource(&state, "salvos", &id, Some(durable), None).await?;
            let versioned =
                crate::boot_model::project_salvo(&state, &id, || state.salvos.update(&id, salvo))?;
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
            crate::boot_model::commit_resource(&state, "salvos", &id, Some(durable), None).await?;
            let versioned =
                crate::boot_model::project_salvo(&state, &id, || state.salvos.create(salvo))?;
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
    // Review H1: serialize with every other durable config mutation and
    // commit the desired envelope before the store projection changes.
    let _mutation = state.lock_config_mutation().await;
    let current = crate::boot_model::accepted_salvo(&state, &id)?;
    if_match.require(SALVO_KIND, &id, current.version)?;
    crate::boot_model::commit_resource(&state, "salvos", &id, None, None).await?;
    crate::boot_model::project_salvo_delete(&state, &id)?;
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
    recalls: bool,
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
    let salvo = crate::boot_model::accepted_salvo(state, id)?;
    if recalls {
        refuse_cell_recall_under_switcher(state, &salvo.salvo)?;
    }
    submit_accepted(state, idem, |op| build(op, head))
}

/// #307 review M1: with a switcher M/E declared, a salvo that recalls a
/// layout or cell sources would change neither Program (the M/E bus) nor the
/// stored layout outputs show — refused (`switcher-owns-program`) instead of
/// accepted as a silent no-op. A salvo of only tally/UMD recalls still fires.
/// Called after authorization and existence, before any reservation.
///
/// # Errors
/// [`ControlError::SwitcherOwnsCells`] for such a salvo under a switcher.
pub(crate) fn refuse_cell_recall_under_switcher(
    state: &AppState,
    salvo: &Salvo,
) -> ControlResult<()> {
    if (salvo.layout.is_some() || !salvo.sources.is_empty())
        && crate::routes::switcher_declared(state)?
    {
        return Err(crate::error::ControlError::SwitcherOwnsCells(
            "a salvo that recalls a layout or cell sources",
        ));
    }
    Ok(())
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
    submit_for_existing_salvo(
        &state,
        &principal,
        &id,
        head.head,
        &idem,
        false,
        |op, head| Command::ArmSalvo {
            op,
            salvo: id.clone(),
            head,
        },
    )
}

/// `OpenAPI` shape of a `POST /salvos/{id}/take` `202`: `AcceptedBody` for
/// a salvo with only legacy recalls, `PatchAccepted` for a patch salvo
/// (#308). Two distinct Rust types; only the document is a `oneOf`.
#[cfg(feature = "openapi")]
pub struct SalvoTakeAcceptedDoc;

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for SalvoTakeAcceptedDoc {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{OneOfBuilder, Ref};
        OneOfBuilder::new()
            .item(Ref::from_schema_name("AcceptedBody"))
            .item(Ref::from_schema_name("PatchAccepted"))
            .description(Some(
                "`AcceptedBody` for a salvo with only legacy recalls; `PatchAccepted` for a patch salvo (#308).",
            ))
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for SalvoTakeAcceptedDoc {}

/// `POST /api/v1/salvos/{id}/take` — take (apply) the salvo (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/salvos/{id}/take",
        tag = "salvos",
        params(("id" = String, Path, description = "Salvo id to take."), HeadQuery),
        responses(
            (status = 202, description = "Take accepted; outcome on the realtime stream. A salvo with only legacy recalls answers `AcceptedBody` (outcome `salvo.taken`). A patch salvo (#308) answers `{operation_id, status: \"accepted\", recall_operation_id?}`: its crosspoints are taken atomically through the `POST /patch/take` set path (outcome `patch.take`, for one destination too) and a legacy recall, when present, is submitted too (`recall_operation_id`).", body = SalvoTakeAcceptedDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to take (a patch salvo: every member's both ends are re-authorized under the caller's current grants).", body = crate::problem::Problem),
            (status = 404, description = "No salvo with that id.", body = crate::problem::Problem),
            (status = 409, description = "A patch salvo: as `POST /patch/take` (`conflict`, `capacity`, `on-air-protected`, ...).", body = crate::problem::Problem),
            (status = 429, description = "A patch salvo: as `POST /patch/take`.", body = crate::problem::Problem),
            (status = 422, description = "`switcher-owns-program`: a switcher M/E is declared and the salvo recalls a layout or cell sources (Program is the M/E bus; edit the stored layout with `PUT /api/v1/layouts/{id}` or use a switcher select). Refused before the Idempotency-Key is reserved. A salvo of only tally/UMD recalls is unaffected.", body = crate::problem::Problem),
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
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    if let Some(head) = head.head.as_deref() {
        crate::auth::authorize_output(&principal, head)?;
    }
    let salvo = crate::boot_model::accepted_salvo(&state, &id)?.salvo;
    if salvo.patch.is_empty() {
        // A salvo with only legacy recalls: today's path, byte-for-byte.
        return submit_for_existing_salvo(
            &state,
            &principal,
            &id,
            head.head,
            &idem,
            true,
            |op, head| Command::TakeSalvo {
                op,
                salvo: Some(id.clone()),
                head,
            },
        );
    }
    take_patch_salvo(&state, &principal, &idem, &id, head.head, &salvo).await
}

/// Whether a salvo recalls anything besides its patch.
fn has_recall(salvo: &Salvo) -> bool {
    salvo.layout.is_some()
        || !salvo.sources.is_empty()
        || !salvo.tally.is_empty()
        || !salvo.umd.is_empty()
}

/// Take a patch salvo (#308): the stored crosspoints through the atomic set
/// path of `POST /patch/take` (every member re-authorized under the caller's
/// CURRENT grants, the key in its own `salvo-take:<id>` namespace), then the
/// legacy recall exactly as today when the salvo has one. A retry with the
/// same key replays the set and re-submits only what did not land.
async fn take_patch_salvo(
    state: &AppState,
    principal: &Principal,
    idem: &IdempotencyKey,
    id: &str,
    head: Option<String>,
    salvo: &Salvo,
) -> ControlResult<Response> {
    let recall = has_recall(salvo);
    if recall {
        refuse_cell_recall_under_switcher(state, salvo)?;
    }
    let req = crate::routes::patch_set::salvo_request(salvo);
    crate::routes::patch::validate_count(&req)?;
    let layouts = match crate::routes::patch::read_patch_layouts(state, principal, &req).await? {
        Ok(layouts) => layouts,
        Err(response) => return Ok(response),
    };
    crate::routes::patch::authorize(principal, &req, &layouts)?;
    let key = crate::routes::patch_set::set_key(idem, &format!("salvo-take:{id}"))?;
    let op = match crate::routes::patch_set::take_set(state, principal, &key, &req, &layouts)? {
        Ok(op) => op,
        Err(response) => return Ok(response),
    };
    let recall_operation_id = if recall {
        let body = crate::routes::submit_accepted_body(state, idem, |op| {
            Ok(Command::TakeSalvo {
                op,
                salvo: Some(id.to_owned()),
                head,
            })
        })?;
        Some(body.operation_id)
    } else {
        None
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(crate::routes::patch::PatchAccepted {
            operation_id: op,
            status: "accepted",
            recall_operation_id,
        }),
    )
        .into_response())
}

/// `POST /api/v1/salvos/{id}/plan` — plan a patch salvo's crosspoints
/// (#308): the `POST /patch/plan` answer for the stored crosspoints and
/// `on_air`, authorized like the take; no change. `arm` keeps its legacy
/// meaning.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/salvos/{id}/plan",
        tag = "salvos",
        params(("id" = String, Path, description = "Salvo id to plan.")),
        responses(
            (status = 200, description = "The patch plan of the salvo's crosspoints (as `POST /patch/plan`): a `PatchPlan`, or a `SetPlan` with `set: {new_encoders, fits}` for two or more destinations.", body = crate::routes::patch::PatchPlanResponseDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized for the salvo or for a patch member's destination or source.", body = crate::problem::Problem),
            (status = 404, description = "No salvo with that id.", body = crate::problem::Problem),
            (status = 409, description = "As `POST /patch/plan`.", body = crate::problem::Problem),
            (status = 422, description = "The salvo has no patch crosspoints, or as `POST /patch/plan`.", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: a `{layout}` could not be read now.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn plan_salvo(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let salvo = crate::boot_model::accepted_salvo(&state, &id)?.salvo;
    if salvo.patch.is_empty() {
        return Err(crate::error::ControlError::Validation(
            "this salvo has no patch crosspoints to plan".into(),
        ));
    }
    let req = crate::routes::patch_set::salvo_request(&salvo);
    crate::routes::patch::plan_request(&state, &principal, &req).await
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
    submit_for_existing_salvo(
        &state,
        &principal,
        &id,
        head.head,
        &idem,
        false,
        |op, head| Command::CancelSalvo {
            op,
            salvo: Some(id.clone()),
            head,
        },
    )
}
