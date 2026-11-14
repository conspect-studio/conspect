//! First-slice switcher HTTP surface. No route advertises a 202 unless a real
//! dual-bus engine handle is installed; operation completion is engine-tick-owned.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::switcher::{FrameCount, SourceRef, TransitionDescriptor};
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
    /// Numbered mixer input for `select_program`/`select_preview` (ADR-0110
    /// §12.8); exactly one of `source`/`input`. Requires declared inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u16>,
    /// Referenced stored layout, when selecting an authored scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Referenced output to bind; all output reads/writes require output BOLA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Action to classify on `/plan`, one of `select_program`, `select_preview`,
    /// `cut`, `auto`, `set_transition`; the action is part of the same
    /// validated request.
    pub action: String,
    /// AUTO duration at the output's exact cadence; no floating milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_frames: Option<u32>,
    /// The transition `set_transition` selects for the next AUTO (ADR-0112
    /// §4), required by it and refused on every other action. Tagged on
    /// `kind`: `{"kind":"cut"}`; `{"kind":"mix","rate":N}`;
    /// `{"kind":"dip","rate":N,"color":"#rrggbb","switch_point":{"num":1,"den":2}}`
    /// (rate >= 2, switch point strictly inside (0, 1)); `{"kind":"wipe",
    /// "pattern":"linear_edge"|"vertical"|"diagonal"|"box"|"circle","rate":N,
    /// "reverse":bool,"softness":0..1,"border_width":0..1,"border_color":"#rrggbb",
    /// "position_xy":[0..1,0..1]}`. `rate` is 1..=1000 frames; AUTO replaces it
    /// with its own `duration_frames`. Refused with `unsupported-transition`:
    /// `stinger`, DVE kinds, a dip through a source, `flip_flop` and
    /// `border_fill_source`; with `invalid-transition`: anything that does not
    /// parse or is out of range.
    ///
    /// Parsed from raw JSON so a malformed descriptor is a typed 422; the
    /// documented shape is [`crate::openapi_schemas::TransitionDescriptorDoc`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = Option<crate::openapi_schemas::TransitionDescriptorDoc>)
    )]
    pub transition: Option<serde_json::Value>,
}

/// Longest transition rate the control API admits, in output frames (the
/// desk's AUTO bound: 40 s at 25 fps).
pub const MAX_TRANSITION_FRAMES: u32 = 1000;

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
    /// The operator's selected transition, as the engine applied it
    /// (ADR-0112 §4); an AUTO's own rate never replaces it. Same shape as
    /// the request's `transition`.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = crate::openapi_schemas::TransitionDescriptorDoc)
    )]
    pub transition: TransitionDescriptor,
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
    // A transition may name a source (a dip source, a border fill, a stinger
    // player). Each is authorized before the descriptor is parsed or refused
    // as unrendered, so a denied id never learns whether it exists.
    if let Some(transition) = req.transition.as_ref().and_then(|t| t.as_object()) {
        for field in ["dip_source", "border_fill_source", "player"] {
            if let Some(id) = transition.get(field).and_then(serde_json::Value::as_str) {
                authorize_object(principal, id)?;
            }
        }
    }
    Ok(())
}

/// Parse and check `set_transition`'s descriptor against the engine's own
/// rules (ADR-0112): typed 422 `invalid-transition` for a shape or number
/// the machine refuses, `unsupported-transition` for a kind or option the
/// Program drive does not render. The engine re-checks at the tick.
fn parse_transition(req: &SwitcherRequest) -> Result<Option<TransitionDescriptor>, Box<Response>> {
    let Some(value) = req.transition.as_ref() else {
        return Ok(None);
    };
    let invalid = |detail: String| Box::new(typed("invalid-transition", detail));
    let descriptor: TransitionDescriptor =
        serde_json::from_value(value.clone()).map_err(|e| invalid(e.to_string()))?;
    if !descriptor.is_rendered() {
        return Err(Box::new(typed(
            "unsupported-transition",
            multiview_engine::switcher_control::REFUSE_UNSUPPORTED_TRANSITION,
        )));
    }
    if !descriptor.is_valid() {
        return Err(invalid(
            "a dip needs rate >= 2 and a switch point strictly inside (0, 1); wipe softness, \
             border_width and position_xy are finite and in [0, 1]"
                .into(),
        ));
    }
    if descriptor
        .rate()
        .is_some_and(|rate| rate.get() > MAX_TRANSITION_FRAMES)
    {
        return Err(invalid(format!(
            "rate must be at most {MAX_TRANSITION_FRAMES} frames"
        )));
    }
    Ok(Some(descriptor))
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
    if kind != "set_transition" && req.transition.is_some() {
        return Err(ControlError::Validation(
            "only set_transition carries a transition; AUTO uses the selected one".into(),
        ));
    }
    match kind {
        "select_program" | "select_preview"
            if (req.source.as_ref().is_some_and(|id| !id.is_empty()) != req.input.is_some())
                && req.duration_frames.is_none() =>
        {
            Ok(())
        }
        "cut" if req.source.is_none() && req.input.is_none() && req.duration_frames.is_none() => {
            Ok(())
        }
        "auto"
            if req.source.is_none()
                && req.input.is_none()
                && req.duration_frames.is_some_and(|duration| duration > 0) =>
        {
            Ok(())
        }
        "set_transition"
            if req.source.is_none()
                && req.input.is_none()
                && req.duration_frames.is_none()
                && req.transition.is_some() =>
        {
            Ok(())
        }
        _ => Err(ControlError::Validation(
            "invalid switcher verb or references".into(),
        )),
    }
}

fn verb(
    req: &SwitcherRequest,
    kind: &str,
    target: Option<&Target>,
    transition: Option<TransitionDescriptor>,
) -> ControlResult<SwitcherVerb> {
    let target = || {
        target
            .map(|t| t.engine_ref.clone())
            .ok_or_else(|| ControlError::Validation("source required".into()))
    };
    match kind {
        "select_program" => Ok(SwitcherVerb::SelectProgram(target()?)),
        "select_preview" => Ok(SwitcherVerb::SelectPreview(target()?)),
        "cut" => Ok(SwitcherVerb::Cut),
        "set_transition" => transition
            .map(SwitcherVerb::SetTransition)
            .ok_or_else(|| ControlError::Validation("transition required".into())),
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

/// What a select names: the engine ref (`in:<n>` with numbered inputs, else
/// the source id) and the SOURCE behind it, which takes the bus lease.
struct Target {
    engine_ref: SourceRef,
    source: String,
}

/// A typed 422 (`unknown-slot`, `not-on-an-input`), RFC 9457.
fn typed(kind: &str, detail: impl Into<String>) -> Response {
    crate::problem::Problem::new(422, kind, "Selection refused")
        .with_detail(detail)
        .into_response()
}

/// Resolve a select's `source`/`input` against the engine's live input table
/// (ADR-0110 §12.8): with inputs declared, `input` must be declared and
/// patched (`unknown-slot`) and a bare `source` selects the lowest input it is
/// patched to (`not-on-an-input`); without inputs, `input` is refused.
fn resolve_target(
    handle: &SwitcherControl,
    req: &SwitcherRequest,
) -> Result<Option<Target>, Box<Response>> {
    if !matches!(req.action.as_str(), "select_program" | "select_preview") {
        return Ok(None);
    }
    let Some(table) = handle.inputs() else {
        if req.input.is_some() {
            return Err(Box::new(typed(
                "unknown-slot",
                "this M/E declares no numbered inputs; select a source",
            )));
        }
        return Ok(req.source.clone().map(|source| Target {
            engine_ref: SourceRef::new(&source),
            source,
        }));
    };
    let (number, source) = if let Some(number) = req.input {
        let source = table
            .inputs
            .iter()
            .find(|i| i.number == number)
            .and_then(|i| i.source.as_ref())
            .ok_or_else(|| {
                Box::new(typed(
                    "unknown-slot",
                    format!("mixer input {number} is not declared or not patched"),
                ))
            })?;
        (number, source.as_str().to_owned())
    } else {
        let Some(source) = req.source.clone() else {
            return Ok(None);
        };
        let number = table
            .inputs
            .iter()
            .find(|i| i.source.as_ref().is_some_and(|s| s.as_str() == source))
            .map(|i| i.number)
            .ok_or_else(|| {
                Box::new(typed(
                    "not-on-an-input",
                    format!("source {source:?} is not patched to a mixer input"),
                ))
            })?;
        (number, source)
    };
    Ok(Some(Target {
        engine_ref: SourceRef::input(number),
        source,
    }))
}

/// The input-resolved source a select puts on a bus must still be readable:
/// `authorize_refs` checked a named `source`; an `input` names one indirectly.
fn authorize_target(principal: &Principal, target: Option<&Target>) -> ControlResult<()> {
    match target {
        Some(target) => authorize_object(principal, &target.source),
        None => Ok(()),
    }
}

/// ADR-0104 §1: a referenced source must be a source of the running
/// configuration (the accepted document's sources, the same list
/// `GET /api/v1/sources` serves). Authorization of the id itself is
/// `authorize_refs`, which runs first; this check never widens it.
fn validate_source_configured(state: &AppState, req: &SwitcherRequest) -> ControlResult<()> {
    let Some(source) = req.source.as_deref() else {
        return Ok(());
    };
    let known = crate::boot_model::accepted_resources(state, "sources")?
        .map_or_else(|| state.sources.list(), Ok)?
        .iter()
        .any(|v| v.resource.id == source);
    if known {
        Ok(())
    } else {
        Err(ControlError::Validation(
            "source is not in the running configuration".into(),
        ))
    }
}

/// #247: a take or a PVW selection that cannot change the picture is refused
/// up front with a typed reason. The engine re-checks at the tick.
fn validate_buses(
    handle: &SwitcherControl,
    req: &SwitcherRequest,
    target: Option<&Target>,
) -> ControlResult<()> {
    let Some(snapshot) = handle.snapshot() else {
        return Ok(());
    };
    let program = snapshot.program.as_str();
    let selected = target.map(|t| t.source.as_str());
    match req.action.as_str() {
        "select_preview" if selected == Some(program) => Err(ControlError::Validation(
            multiview_engine::switcher_control::REFUSE_SAME_SOURCE.into(),
        )),
        "select_program" if selected == Some(program) => Err(ControlError::Validation(
            "source is already on program".into(),
        )),
        "cut" | "auto" if snapshot.preview == snapshot.program => Err(ControlError::Validation(
            multiview_engine::switcher_control::REFUSE_SAME_SOURCE.into(),
        )),
        "auto" if snapshot.in_flight => Err(ControlError::Validation(
            "cannot start a second AUTO during an active take".into(),
        )),
        // The flight's descriptor is frozen (ADR-0112 §1); the engine
        // re-checks at the tick.
        "set_transition" if snapshot.in_flight => Err(ControlError::Validation(
            "cannot change the transition during an active take".into(),
        )),
        _ => Ok(()),
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
            transition: snapshot.transition.clone(),
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
    validate_source_configured(&state, &req)?;
    validate_first_slice(&req, &req.action)?;
    if let Err(response) = parse_transition(&req) {
        return Ok(*response);
    }
    let handle = engine(&state)?;
    if req.action == "auto" && !handle.supports_auto() {
        return Err(ControlError::Validation(
            "AUTO requires a same-epoch preview compositor and admitted transition budget".into(),
        ));
    }
    let target = match resolve_target(handle, &req) {
        Ok(target) => target,
        Err(response) => return Ok(*response),
    };
    authorize_target(&principal, target.as_ref())?;
    validate_buses(handle, &req, target.as_ref())?;
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
#[allow(clippy::too_many_lines)]
// reason: one linear admission sequence (authz → resolve input → buses → key →
// lease → enqueue); the input resolution added the lines, not new branches.
pub(crate) async fn submit_switcher(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(kind): Path<String>,
    Json(req): Json<SwitcherRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    authorize_refs(&principal, &req)?;
    validate_source_configured(&state, &req)?;
    if req.action != kind {
        return Err(ControlError::Validation(
            "switcher action disagrees with command path".into(),
        ));
    }
    validate_first_slice(&req, &kind)?;
    let transition = match parse_transition(&req) {
        Ok(transition) => transition,
        Err(response) => return Ok(*response),
    };
    let handle = engine(&state)?;
    if req.action == "auto" && !handle.supports_auto() {
        return Err(ControlError::Validation(
            "AUTO requires a same-epoch preview compositor and admitted transition budget".into(),
        ));
    }
    let target = match resolve_target(handle, &req) {
        Ok(target) => target,
        Err(response) => return Ok(*response),
    };
    authorize_target(&principal, target.as_ref())?;
    validate_buses(handle, &req, target.as_ref())?;
    let scene_version = handle
        .scene_version()
        .ok_or_else(|| ControlError::Conflict("switcher has no active scene".into()))?;
    let verb = verb(&req, &kind, target.as_ref(), transition)?;
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
        Reservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        Reservation::Replay(op) => op,
        Reservation::Fresh(op) => {
            let program_bus = req.action == "select_program";
            let lease = if req.action == "select_preview" || program_bus {
                // The lease is on the RESOLVED source (the input's patch).
                match (demand.as_ref(), target.as_ref().map(|t| &t.source)) {
                    (Some(demand), Some(source)) => {
                        let label = if program_bus {
                            "switcher:me1:pgm"
                        } else {
                            "switcher:me1:pvw"
                        };
                        let Some(lease) = demand.switcher_preview_lease(source, label) else {
                            state.idempotency.release(Some(key), &op);
                            return Err(ControlError::Conflict(
                                "source lease unavailable (unknown source or lease limit)".into(),
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
                let enqueue = || {
                    handle.try_send(command).map_err(|_| ())?;
                    Ok(handle.composed_count())
                };
                let owner = &state.switcher_preview_lease;
                if program_bus {
                    owner.admit_program(op.to_string(), target, lease, handle, enqueue)
                } else {
                    owner.admit(op.to_string(), target, lease, handle, enqueue)
                }
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
