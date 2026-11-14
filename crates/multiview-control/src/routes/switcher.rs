//! First-slice switcher HTTP surface. No route advertises a 202 unless a real
//! dual-bus engine handle is installed; operation completion is engine-tick-owned.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_engine::switcher_control::{
    SelectionGuard, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
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
    /// A stored layout as the selected source (#307), for
    /// `select_program`/`select_preview`: exactly one of
    /// `source`/`input`/`layout`. Needs Read on the layout AND on every source
    /// its cells bind (one generic 403); it must exist, solve on the running
    /// canvas, have opaque cells and bind no other layout or bus (422
    /// `unknown-source` / `layout-not-admissible`). With numbered inputs it
    /// must be patched to an input (`not-on-an-input`). The engine source id
    /// is `layout:<id>`; at most 4 layouts are M/E sources at once (409
    /// `capacity`). Refused on `cut`/`auto`. A `source` of `layout:<id>` means
    /// the same.
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

/// How long a select naming a newly registered layout waits for the output
/// clock to adopt its store (#307); on timeout 503 `engine-busy`, key released.
const ME_LAYOUT_ADOPT_WAIT: std::time::Duration = std::time::Duration::from_millis(1_000);

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

/// `source: "layout:<id>"` is the same request as `layout: "<id>"` (#307):
/// normalized before authorization, so the layout id (not the engine key) is
/// what is authorized and `validate_source_configured` never sees it.
fn normalize_layout_source(req: &mut SwitcherRequest) {
    if let Some(id) = req
        .source
        .as_deref()
        .and_then(|s| s.strip_prefix("layout:"))
    {
        if req.layout.is_none() {
            req.layout = Some(id.to_owned());
            req.source = None;
        }
    }
}

fn validate_first_slice(req: &SwitcherRequest, kind: &str) -> ControlResult<()> {
    if req.me != "me1" || req.program != "main" {
        return Err(ControlError::Validation(
            "first-slice runtime admits only me1 in main".into(),
        ));
    }
    if req.output.is_some() {
        return Err(ControlError::Validation(
            "output binding is not attached to the switcher frame provider; route an output with \
             POST /api/v1/routing/output/take"
                .into(),
        ));
    }
    let named = usize::from(req.source.as_ref().is_some_and(|id| !id.is_empty()))
        + usize::from(req.input.is_some())
        + usize::from(req.layout.as_ref().is_some_and(|id| !id.is_empty()));
    let nothing = req.source.is_none() && req.input.is_none() && req.layout.is_none();
    match kind {
        "select_program" | "select_preview" if named == 1 && req.duration_frames.is_none() => {
            Ok(())
        }
        "cut" if nothing && req.duration_frames.is_none() => Ok(()),
        "auto" if nothing && req.duration_frames.is_some_and(|duration| duration > 0) => Ok(()),
        _ => Err(ControlError::Validation(
            "invalid switcher verb or references".into(),
        )),
    }
}

fn verb(req: &SwitcherRequest, kind: &str, target: Option<&Target>) -> ControlResult<SwitcherVerb> {
    let target = || {
        target
            .map(|t| t.engine_ref.clone())
            .ok_or_else(|| ControlError::Validation("source required".into()))
    };
    match kind {
        "select_program" => Ok(SwitcherVerb::SelectProgram(target()?)),
        "select_preview" => Ok(SwitcherVerb::SelectPreview(target()?)),
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

/// What a select names: the engine ref (`in:<n>` with numbered inputs, else
/// the source id) and the SOURCE behind it, which takes the bus lease (a
/// `layout:<id>` source takes none: the M/E layout registry leases its
/// bound sources).
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
    // A layout select names the engine source `layout:<id>` (#307).
    let named = req
        .layout
        .as_deref()
        .map(crate::me_layouts::source_key)
        .or_else(|| req.source.clone());
    let Some(table) = handle.inputs() else {
        if req.input.is_some() {
            return Err(Box::new(typed(
                "unknown-slot",
                "this M/E declares no numbered inputs; select a source",
            )));
        }
        return Ok(named.map(|source| Target {
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
        let Some(source) = named else {
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

/// Read on stored layout `id` and on every source its REGISTERED body binds
/// (what the engine composes), returning that body's revision for the
/// command's [`SelectionGuard`] (#307 review A1): the engine refuses the
/// command if it holds another body by then. A layout that is not
/// registered any more is 409 `layout-changed` (fail closed).
fn authorize_registered_layout(
    state: &AppState,
    principal: &Principal,
    id: &str,
) -> ControlResult<u64> {
    authorize_object(principal, id)?;
    let body = state
        .me_layouts
        .as_ref()
        .and_then(|registry| registry.registered_body(id))
        .ok_or_else(|| ControlError::LayoutChanged(id.to_owned()))?;
    super::output_routing::authorize_layout_sources(principal, &body.0.layout)?;
    Ok(body.0.revision)
}

/// The input-resolved source a select puts on a bus must still be readable:
/// `authorize_refs` checked a named `source`; an `input` names one indirectly.
/// A `layout:<id>` source (#307) needs Read on the layout and on every source
/// its registered body binds (one generic 403). Returns the guard the
/// command carries: the resolved source authorized, and for a layout reached
/// through an input its registered body revision (a named `layout` select
/// fills the revision after its registration).
fn authorize_target(
    state: &AppState,
    principal: &Principal,
    req: &SwitcherRequest,
    target: Option<&Target>,
) -> ControlResult<Option<SelectionGuard>> {
    let Some(target) = target else {
        return Ok(None);
    };
    let layout_revision = match target.source.strip_prefix("layout:") {
        // A named `layout` was authorized (id + closure) by `resolve_layout_select`.
        Some(id) if req.layout.as_deref() == Some(id) => None,
        Some(id) => Some(authorize_registered_layout(state, principal, id)?),
        None => {
            authorize_object(principal, &target.source)?;
            None
        }
    };
    Ok(Some(SelectionGuard {
        preview: None,
        target: Some(SourceRef::new(&target.source)),
        layout_revision,
    }))
}

/// A CUT/AUTO authorizes the Preview of the snapshot it read, and carries it
/// as its [`SelectionGuard`] (#307 review A1): a select or re-patch queued
/// before it, or a body edit, makes the engine refuse it rather than take an
/// unauthorized source. When that Preview is `layout:<id>` the caller needs
/// Read on the layout and on every source its registered body binds (one
/// generic 403). `None` for any other verb, or before the first snapshot.
fn authorize_take_of_layout(
    state: &AppState,
    principal: &Principal,
    handle: &SwitcherControl,
    req: &SwitcherRequest,
) -> ControlResult<Option<SelectionGuard>> {
    if !matches!(req.action.as_str(), "cut" | "auto") {
        return Ok(None);
    }
    let Some(snapshot) = handle.snapshot() else {
        return Ok(None);
    };
    let layout_revision = match snapshot.preview.as_str().strip_prefix("layout:") {
        Some(id) => Some(authorize_registered_layout(state, principal, id)?),
        None => None,
    };
    Ok(Some(SelectionGuard {
        preview: Some(snapshot.preview.clone()),
        target: None,
        layout_revision,
    }))
}

/// A `layout` select (#307): read the stored layout once, authorize its
/// source closure, and resolve it as an M/E source. `Ok(Err(..))` is the
/// typed 422 response. `None` for any other request.
async fn resolve_layout_select(
    state: &AppState,
    principal: &Principal,
    req: &SwitcherRequest,
) -> ControlResult<Option<Result<crate::output_routing::LayoutBody, Response>>> {
    let Some(id) = req.layout.as_deref() else {
        return Ok(None);
    };
    let read = super::output_routing::read_layout(state, id).await?;
    Ok(Some(
        super::output_routing::resolve_me_layout(state, principal, id, Some(&read))?
            .map_err(super::output_routing::me_refusal),
    ))
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
    let mut req = req;
    normalize_layout_source(&mut req);
    authorize_refs(&principal, &req)?;
    validate_source_configured(&state, &req)?;
    validate_first_slice(&req, &req.action)?;
    let handle = engine(&state)?;
    if req.action == "auto" && !handle.supports_auto() {
        return Err(ControlError::Validation(
            "AUTO requires a same-epoch preview compositor and admitted transition budget".into(),
        ));
    }
    // A plan resolves a layout select exactly like a submit, but never
    // registers it.
    if let Some(Err(response)) = resolve_layout_select(&state, &principal, &req).await? {
        return Ok(response);
    }
    authorize_take_of_layout(&state, &principal, handle, &req)?;
    let target = match resolve_target(handle, &req) {
        Ok(target) => target,
        Err(response) => return Ok(*response),
    };
    authorize_target(&state, &principal, &req, target.as_ref())?;
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
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/switcher/commands/{kind}", tag = "switcher", params(("kind" = String, Path)), request_body = SwitcherRequest, responses((status = 202, body = SwitcherAccepted), (status = 403, description = "Not authorized for the M/E, Program scope, a referenced object, or (a `layout` select, or a CUT/AUTO whose Preview is `layout:<id>`) every source the stored layout binds; one generic refusal.", body = crate::problem::Problem), (status = 409, description = "Switcher unavailable; `capacity` (a fifth stored layout as an M/E source, or a source it shows cannot be leased; key released); `layout-changed` (the stored layout was edited or deleted while this request was in flight; key released, retry); no source lease.", body = crate::problem::Problem), (status = 422, description = "Invalid verb or references (exactly one of `source`/`input`/`layout` on a select; none on cut/auto), `not-on-an-input`, `unknown-slot`, `unknown-source` (no such stored layout), `layout-not-admissible` (another canvas, a translucent cell, or a nested layout/bus cell).", body = crate::problem::Problem), (status = 503, description = "`engine-busy`: the mailbox is full, a stored layout could not be read now, or the output clock did not adopt a newly registered layout within 1 s (key released).", body = crate::problem::Problem))))]
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
    let mut req = req;
    normalize_layout_source(&mut req);
    authorize_refs(&principal, &req)?;
    validate_source_configured(&state, &req)?;
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
    let layout_body = match resolve_layout_select(&state, &principal, &req).await? {
        Some(Ok(body)) => Some(body),
        Some(Err(response)) => return Ok(response),
        None => None,
    };
    let registry = match (&layout_body, state.me_layouts.as_ref()) {
        (Some(_), None) => {
            return Err(ControlError::Conflict(
                "stored layouts as M/E sources are not available in this run".into(),
            ))
        }
        (_, registry) => registry,
    };
    let take_guard = authorize_take_of_layout(&state, &principal, handle, &req)?;
    let target = match resolve_target(handle, &req) {
        Ok(target) => target,
        Err(response) => return Ok(*response),
    };
    let mut expect = authorize_target(&state, &principal, &req, target.as_ref())?.or(take_guard);
    validate_buses(handle, &req, target.as_ref())?;
    let scene_version = handle
        .scene_version()
        .ok_or_else(|| ControlError::Conflict("switcher has no active scene".into()))?;
    let verb = verb(&req, &kind, target.as_ref())?;
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
            // A layout select registers the layout as an M/E source first
            // (the registry leases its bound sources; no per-select lease).
            if let (Some(body), Some(registry)) = (layout_body, registry) {
                // The engine must compose exactly the body authorized here;
                // a newer registered one makes it refuse `selection-changed`.
                let authorized = body.0.revision;
                if let Err(error) =
                    registry.ensure(body, demand.as_ref(), std::time::Instant::now())
                {
                    state.idempotency.release(Some(key), &op);
                    return Ok(crate::problem::Problem::new(
                        409,
                        error.kind(),
                        "Selection refused",
                    )
                    .with_detail(error.detail())
                    .into_response());
                }
                if let Some(guard) = expect.as_mut() {
                    guard.layout_revision = Some(authorized);
                }
                // The command is drained after the frame hook adopted the
                // store (bounded wait, off the clock), so PVW shows it at once.
                if !registry.adopted_within(ME_LAYOUT_ADOPT_WAIT).await {
                    state.idempotency.release(Some(key), &op);
                    return Err(ControlError::EngineBusy);
                }
            }
            let layout_target = target
                .as_ref()
                .is_some_and(|t| t.source.starts_with("layout:"));
            let program_bus = req.action == "select_program";
            let lease = if layout_target {
                None
            } else if req.action == "select_preview" || program_bus {
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
                expect: expect.take(),
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
