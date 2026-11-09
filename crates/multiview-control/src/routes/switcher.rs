//! First-slice switcher HTTP surface. No route advertises a 202 unless a real
//! dual-bus engine handle is installed; operation completion is engine-tick-owned.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_engine::switcher_control::{SwitcherCommand, SwitcherControl, SwitcherVerb};
use multiview_events::AuthzScope;
use serde::{Deserialize, Serialize};

use crate::auth::{authorize_object, authorize_output, authorize_scope, Action, Principal};
use crate::concurrency::{IdempotencyKey, Reservation};
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// One source-select, take or output-binding request. References are checked
/// independently; absent references never authorize implicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SwitcherRequest {
    /// M/E identity (`me1` for the first slice).
    pub me: String,
    /// Program identity (`main` for the first slice).
    pub program: String,
    /// Referenced source for `select_program`/`select_preview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Referenced stored layout, when selecting an authored scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Referenced output to bind; all output reads/writes require output BOLA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Action to classify on `/plan`, one of `select_program`, `select_preview`,
    /// `cut`, `auto`; the action is part of the same validated request.
    pub action: String,
    /// AUTO duration at the output's exact cadence; no floating milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_frames: Option<u32>,
}

/// A dry-run result, including the scene generation the take must recheck.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SwitcherPlan {
    /// First-slice admission classification.
    pub class: &'static str,
    /// The exact immutable scene generation the caller may submit against.
    pub scene_version: u64,
    /// Plan is valid only when a real engine bus is attached.
    pub can_take: bool,
}

/// A 202 means queued, not yet on-air; operation completion arrives on a
/// correlated engine outcome at its actual tick.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SwitcherAccepted {
    /// Correlation id, echoed by the real tick driver's outcome.
    pub operation_id: String,
    /// Verb name, never `applied`.
    pub kind: String,
}

/// Engine-owned bus selection and current tick, never guessed from request.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SwitcherStateResponse {
    /// Applied output tick.
    pub tick: u64,
    /// Actual PGM source id.
    pub program_source: String,
    /// Actual PVW source id.
    pub preview_source: String,
    /// Active AUTO status.
    pub in_flight: bool,
}

fn engine(state: &AppState) -> ControlResult<&SwitcherControl> {
    let handle = state.switcher.as_ref().ok_or_else(|| {
        ControlError::Conflict(
            "switcher unavailable: no real Program/Preview output bus is attached".into(),
        )
    })?;
    if !handle.is_ready() {
        return Err(ControlError::Conflict(
            "switcher unavailable: no real paired Program/Preview frame has been published".into(),
        ));
    }
    Ok(handle)
}

/// Authorize the M/E plus every referenced resource independently (BOLA).
///
/// # Errors
/// Denies any missing object/output/program grant before changing the engine.
pub fn authorize_refs(principal: &Principal, req: &SwitcherRequest) -> ControlResult<()> {
    authorize_object(principal, &req.me)?;
    authorize_scope(principal, AuthzScope::Program(&req.program))?;
    if let Some(source) = &req.source {
        authorize_object(principal, source)?;
    }
    if let Some(layout) = &req.layout {
        authorize_object(principal, layout)?;
    }
    if let Some(output) = &req.output {
        authorize_output(principal, output)?;
    }
    Ok(())
}

fn validate_first_slice(req: &SwitcherRequest, kind: &str) -> ControlResult<()> {
    if req.me != "me1" || req.program != "main" {
        return Err(ControlError::Validation(
            "first-slice runtime admits only me1 in main".into(),
        ));
    }
    if req.layout.is_some() || req.output.is_some() {
        return Err(ControlError::Validation("layout selection and output binding are not yet attached to the switcher frame provider".into()));
    }
    match kind {
        "select_program" | "select_preview"
            if req.source.as_ref().is_some_and(|id| !id.is_empty())
                && req.duration_frames.is_none() =>
        {
            Ok(())
        }
        "cut" if req.source.is_none() && req.duration_frames.is_none() => Ok(()),
        "auto"
            if req.source.is_none() && req.duration_frames.is_some_and(|duration| duration > 0) =>
        {
            Ok(())
        }
        _ => Err(ControlError::Validation(
            "invalid switcher verb or references".into(),
        )),
    }
}

fn verb(req: &SwitcherRequest, kind: &str) -> ControlResult<SwitcherVerb> {
    match kind {
        "select_program" => Ok(SwitcherVerb::SelectProgram(SourceRef::new(
            req.source
                .clone()
                .ok_or_else(|| ControlError::Validation("source required".into()))?,
        ))),
        "select_preview" => Ok(SwitcherVerb::SelectPreview(SourceRef::new(
            req.source
                .clone()
                .ok_or_else(|| ControlError::Validation("source required".into()))?,
        ))),
        "cut" => Ok(SwitcherVerb::Cut),
        "auto" => Ok(SwitcherVerb::Auto(
            FrameCount::try_from(
                req.duration_frames
                    .ok_or_else(|| ControlError::Validation("duration_frames required".into()))?,
            )
            .map_err(|e| ControlError::Validation(e.to_string()))?,
        )),
        _ => Err(ControlError::NotFound {
            kind: "switcher-verb",
            id: kind.into(),
        }),
    }
}

/// Read authoritative state; no runtime means an explicit 409, never fake PGM.
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/switcher/{me}", tag = "switcher", params(("me" = String, Path)), responses((status = 200, body = SwitcherStateResponse), (status = 403, body = crate::problem::Problem), (status = 409, body = crate::problem::Problem))))]
pub(crate) async fn get_switcher(
    State(state): State<AppState>,
    principal: Principal,
    Path(me): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    authorize_object(&principal, &me)?;
    authorize_scope(&principal, AuthzScope::Program("main"))?;
    if me != "me1" {
        return Err(ControlError::NotFound {
            kind: "mix_effects",
            id: me,
        });
    }
    let handle = engine(&state)?;
    let snapshot = handle.snapshot().ok_or_else(|| {
        ControlError::Conflict("switcher has not emitted a real output tick".into())
    })?;
    Ok((
        StatusCode::OK,
        Json(SwitcherStateResponse {
            tick: handle.applied_tick(),
            program_source: snapshot.program.as_str().to_owned(),
            preview_source: snapshot.preview.as_str().to_owned(),
            in_flight: snapshot.in_flight,
        }),
    )
        .into_response())
}

/// Dry-run command admission, including BOLA on every referenced object.
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/switcher/plan", tag = "switcher", request_body = SwitcherRequest, responses((status = 200, body = SwitcherPlan), (status = 403, body = crate::problem::Problem), (status = 409, body = crate::problem::Problem), (status = 422, body = crate::problem::Problem))))]
pub(crate) async fn plan_switcher(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<SwitcherRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    authorize_refs(&principal, &req)?;
    validate_first_slice(&req, &req.action)?;
    let handle = engine(&state)?;
    if req.action == "auto" && !handle.supports_auto() {
        return Err(ControlError::Validation(
            "AUTO requires a same-epoch preview compositor and admitted transition budget".into(),
        ));
    }
    let version = handle
        .scene_version()
        .ok_or_else(|| ControlError::Conflict("switcher has no active scene".into()))?;
    Ok((
        StatusCode::OK,
        Json(SwitcherPlan {
            class: "class1",
            scene_version: version,
            can_take: true,
        }),
    )
        .into_response())
}

/// Enqueue a checked verb; a 202 never asserts frame application.
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/switcher/commands/{kind}", tag = "switcher", params(("kind" = String, Path)), request_body = SwitcherRequest, responses((status = 202, body = SwitcherAccepted), (status = 403, body = crate::problem::Problem), (status = 409, body = crate::problem::Problem), (status = 422, body = crate::problem::Problem), (status = 503, body = crate::problem::Problem))))]
pub(crate) async fn submit_switcher(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(kind): Path<String>,
    Json(req): Json<SwitcherRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    authorize_refs(&principal, &req)?;
    if req.action != kind {
        return Err(ControlError::Validation(
            "switcher action disagrees with command path".into(),
        ));
    }
    validate_first_slice(&req, &kind)?;
    let handle = engine(&state)?;
    if req.action == "auto" && !handle.supports_auto() {
        return Err(ControlError::Validation(
            "AUTO requires a same-epoch preview compositor and admitted transition budget".into(),
        ));
    }
    let scene_version = handle
        .scene_version()
        .ok_or_else(|| ControlError::Conflict("switcher has no active scene".into()))?;
    let verb = verb(&req, &kind)?;
    // The request's lease is acquired only on the Fresh reservation arm below;
    // a replay never changes PVW demand. Keep the old lease through admission.
    let demand = state.source_demand.clone();
    let Some(key) = idem.0.as_deref().filter(|key| !key.trim().is_empty()) else {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    };
    let operation = state.idempotency.reserve(Some(key));
    let op = match operation {
        Reservation::Replay(op) => op,
        Reservation::Fresh(op) => {
            let lease = if req.action == "select_preview" {
                match (demand.as_ref(), req.source.as_ref()) {
                    (Some(demand), Some(source)) => {
                        let Some(lease) = demand.preview_lease(source, "switcher:me1:pvw") else {
                            state.idempotency.release(Some(key), &op);
                            return Err(ControlError::Conflict(
                                "preview source lease unavailable (unknown source or lease limit)"
                                    .into(),
                            ));
                        };
                        Some((
                            source.clone(),
                            crate::source_demand::SwitcherPreviewLease::new(lease),
                        ))
                    }
                    (None, _) => None,
                    (_, None) => {
                        state.idempotency.release(Some(key), &op);
                        return Err(ControlError::Validation("preview source required".into()));
                    }
                }
            } else {
                None
            };
            let command = SwitcherCommand {
                operation_id: op.to_string(),
                scene_version,
                verb,
            };
            let admitted = if let Some((target, lease)) = lease {
                state
                    .switcher_preview_lease
                    .admit(op.to_string(), target, lease, handle, || {
                        handle.try_send(command).map_err(|_| ())?;
                        Ok(handle.composed_count())
                    })
                    .is_ok()
            } else {
                handle.try_send(command).is_ok()
            };
            if !admitted {
                state.idempotency.release(Some(key), &op);
                return Err(ControlError::EngineBusy);
            }
            op
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(SwitcherAccepted {
            operation_id: op.to_string(),
            kind,
        }),
    )
        .into_response())
}
