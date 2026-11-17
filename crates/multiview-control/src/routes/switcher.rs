//! First-slice switcher HTTP surface. No route advertises a 202 unless a real
//! dual-bus engine handle is installed; operation completion is engine-tick-owned.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::switcher::{FrameCount, SourceRef, TransitionDescriptor};
use multiview_engine::switcher_control::SwitcherSnapshot;
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
    /// `cut`, `auto`, `set_transition`, `keyer_cut`, `keyer_auto`, `keyer_tie`,
    /// `keyer_set`, `arm`; the action is part of the same validated request.
    pub action: String,
    /// AUTO duration at the output's exact cadence; no floating milliseconds.
    /// For `keyer_auto`, an optional rate override in 1..=300 frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_frames: Option<u32>,
    /// Keyer fields (ADR-0113 §8, K4/K3): `keyer` names the keyer (object id
    /// `<program>/<keyer>`, or `<me>/<keyer>` with `family = "usk"`); `on`,
    /// `tie` and the `keyer_set` definition fields; `arm` takes `background`
    /// and `keys` (upstream keyer ids). Fill and key name sources only
    /// (`in:<n>` is 422).
    #[serde(flatten)]
    pub keyer: super::switcher_keyers::KeyerFields,
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
    /// Output canvas width in pixels (the `fly` coordinate space).
    pub canvas_width: u32,
    /// Output canvas height in pixels.
    pub canvas_height: u32,
    /// Exact output cadence numerator: `tick` advances `fps_num / fps_den`
    /// times a second (clients extrapolate a keyer ramp from it).
    pub fps_num: i64,
    /// Exact output cadence denominator.
    pub fps_den: i64,
    /// Downstream keyer rows the caller may read (keyer object id), with
    /// unreadable source ids `"redacted"`; changes only on state changes.
    pub keyers: Vec<super::switcher_keyers::KeyerRow>,
    /// Engine tally resolved to source ids; unreadable ids are dropped.
    pub tally: super::switcher_keyers::TallyView,
    /// The next-transition set (`arm`, K3): the background flag and the
    /// armed upstream keyer ids the caller may read.
    pub armed: super::switcher_keyers::ArmedView,
    /// The operator's selected transition, as the engine applied it
    /// (ADR-0112 §4); an AUTO's own rate never replaces it. Same shape as
    /// the request's `transition`.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = crate::openapi_schemas::TransitionDescriptorDoc)
    )]
    pub transition: TransitionDescriptor,
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
    // ADR-0113 §10: the keyer object id and every named fill/key source,
    // before any existence answer.
    super::switcher_keyers::authorize(principal, &req.me, &req.program, &req.keyer)?;
    Ok(())
}

fn validate_me(req: &SwitcherRequest) -> ControlResult<()> {
    if req.me != "me1" || req.program != "main" {
        return Err(ControlError::Validation(
            "first-slice runtime admits only me1 in main".into(),
        ));
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
    validate_me(req)?;
    if !req.keyer.is_empty() {
        return Err(ControlError::Validation(
            "keyer fields belong to the keyer verbs".into(),
        ));
    }
    if req.output.is_some() {
        return Err(ControlError::Validation(
            "output binding is not attached to the switcher frame provider; route an output with \
             POST /api/v1/routing/output/take"
                .into(),
        ));
    }
    if kind != "set_transition" && req.transition.is_some() {
        return Err(ControlError::Validation(
            "only set_transition carries a transition; AUTO uses the selected one".into(),
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
        "set_transition"
            if nothing && req.duration_frames.is_none() && req.transition.is_some() =>
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
        member_revisions: Vec::new(),
    }))
}

/// A CUT/AUTO authorizes the Preview of the snapshot it read, and carries it
/// as its [`SelectionGuard`] (#307 review A1): a select or re-patch queued
/// before it, or a body edit, makes the engine refuse it rather than take an
/// unauthorized source. When that Preview is `layout:<id>` the caller needs
/// Read on the layout and on every source its registered body binds (one
/// generic 403). `None` for any other verb, or before the first snapshot.
///
/// A key-only take (K3: the arm set excludes the background) moves neither
/// bus, so it neither authorizes nor guards Preview (review R3-KEYONLY-1).
/// The decision reads the same `snapshot` whose `arm_version` the take is
/// bound to, so an arm change that adds the background makes the engine
/// refuse the take (`arm-changed`) instead of taking an unguarded Preview.
fn authorize_take_of_layout(
    state: &AppState,
    principal: &Principal,
    snapshot: Option<&SwitcherSnapshot>,
    req: &SwitcherRequest,
) -> ControlResult<Option<SelectionGuard>> {
    if !matches!(req.action.as_str(), "cut" | "auto") {
        return Ok(None);
    }
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    if !snapshot.next.background {
        return Ok(None);
    }
    let layout_revision = match snapshot.preview.as_str().strip_prefix("layout:") {
        Some(id) => Some(authorize_registered_layout(state, principal, id)?),
        None => None,
    };
    Ok(Some(SelectionGuard {
        preview: Some(snapshot.preview.clone()),
        target: None,
        layout_revision,
        member_revisions: Vec::new(),
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
        // Review MEDIA-1: only a take that moves the background puts PVW on
        // PGM; a key-only take (K3) changes no bus.
        "cut" | "auto" if snapshot.next.background && snapshot.preview == snapshot.program => Err(
            ControlError::Validation(multiview_engine::switcher_control::REFUSE_SAME_SOURCE.into()),
        ),
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
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/switcher/{me}", tag = "switcher", params(("me" = String, Path)), responses((status = 200, description = "Engine-owned state. `keyers[]` lists only the downstream keyer rows whose object id (`<program>/<keyer>`) the caller may read, with unreadable fill/key source ids replaced by `\"redacted\"`; `tally` drops unreadable ids (ADR-0113 §8).", body = SwitcherStateResponse), (status = 403, body = crate::problem::Problem), (status = 409, body = crate::problem::Problem))))]
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
    let provider = handle.provider_status().ok_or_else(|| {
        ControlError::Conflict("switcher has not published a real output format".into())
    })?;
    let defs = handle.keyer_definitions();
    Ok((
        StatusCode::OK,
        Json(SwitcherStateResponse {
            tick: handle.applied_tick(),
            program_source: snapshot.program.as_str().to_owned(),
            preview_source: snapshot.preview.as_str().to_owned(),
            in_flight: snapshot.in_flight,
            canvas_width: provider.width,
            canvas_height: provider.height,
            fps_num: provider.fps_num,
            fps_den: provider.fps_den,
            keyers: super::switcher_keyers::rows(&principal, defs.as_deref(), &snapshot),
            tally: super::switcher_keyers::tally(&principal, defs.as_deref(), &snapshot),
            armed: super::switcher_keyers::armed(&principal, defs.as_deref(), &snapshot),
            transition: snapshot.transition.clone(),
        }),
    )
        .into_response())
}

/// `OpenAPI` shape of a `POST /switcher/plan` `200`: a background-verb
/// [`SwitcherPlan`], or a `KeyerPlan` for a keyer verb (#389 review H5).
#[cfg(feature = "openapi")]
pub struct SwitcherPlanResponseDoc;

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for SwitcherPlanResponseDoc {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{OneOfBuilder, Ref};
        OneOfBuilder::new()
            .item(Ref::from_schema_name("SwitcherPlan"))
            .item(Ref::from_schema_name("KeyerPlan"))
            .description(Some(
                "A `SwitcherPlan` for a background verb, or a `KeyerPlan` for `keyer_cut`, `keyer_auto`, `keyer_tie` and `keyer_set`.",
            ))
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for SwitcherPlanResponseDoc {}

/// The [`SwitcherRequest`] body (P3R2-B). Delegates to axum's [`Json`], so a
/// missing content type, a body limit, a syntax error or a data error keeps
/// the exact status and shape it had; only an error located inside
/// `transition` (a number JSON cannot carry, such as `1e400` or a literal
/// `NaN`) becomes the typed RFC 9457 422 `invalid-transition` that
/// [`parse_transition`] gives every other malformed descriptor.
pub(crate) struct SwitcherBody(SwitcherRequest);

impl<S: Send + Sync> FromRequest<S> for SwitcherBody {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();
        let bytes = Bytes::from_request(Request::from_parts(parts.clone(), body), state)
            .await
            .map_err(IntoResponse::into_response)?;
        let rebuilt = Request::from_parts(parts, Body::from(bytes.clone()));
        match Json::<SwitcherRequest>::from_request(rebuilt, state).await {
            Ok(Json(req)) => Ok(Self(req)),
            Err(rejection) => Err(transition_error(&bytes).map_or_else(
                || rejection.into_response(),
                |detail| typed("invalid-transition", detail),
            )),
        }
    }
}

/// The error detail when `bytes` fail to parse *inside* `transition`; `None`
/// for any other failure (including a body truncated there: that is a
/// malformed body, not a malformed descriptor).
fn transition_error(bytes: &[u8]) -> Option<String> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let err = serde_path_to_error::deserialize::<_, SwitcherRequest>(&mut de).err()?;
    let path = err.path().to_string();
    let inside =
        path == "transition" || path.starts_with("transition.") || path.starts_with("transition[");
    (inside && !err.inner().is_eof()).then(|| format!("{path}: {}", err.inner()))
}

/// Dry-run command admission, including BOLA on every referenced object.
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/switcher/plan", tag = "switcher", request_body = SwitcherRequest, responses((status = 200, description = "`SwitcherPlan` for a background verb; `KeyerPlan` for `keyer_cut`/`keyer_auto`/`keyer_tie`/`keyer_set`/`arm` (class, keyer object id, on-air, the admission cost — scoped callers see only their own delta — and the refusal the command would answer: `capacity`, `on-air-protected`, `clean-unavailable-with-dsk`, `keyer-ramping`, `keyer-in-transition` (a USK `keyer_cut` during a transition), `in-flight` (`arm` during a transition), `arm-hidden-keys` (`arm` while the armed set holds a key the caller may not see; generic, never naming it)).", body = SwitcherPlanResponseDoc), (status = 403, description = "Not authorized for the M/E, the Program scope, the keyer object id (`<program>/<id>` for a DSK, `<me>/<id>` for a USK; for `arm` every armed key) or a named fill/key source (for `arm` the fill and key of every armed key) (checked before any existence answer); a keyer verb also needs the command role (Operator), so a Viewer or ReadOnly key cannot plan one.", body = crate::problem::Problem), (status = 404, description = "`unknown-keyer` (a keyer the caller may read is not defined).", body = crate::problem::Problem), (status = 409, body = crate::problem::Problem), (status = 422, description = "Invalid verb fields; `keyer-source-not-a-source` (a fill or key naming a mixer input `in:<n>`); `keyer-rate`.", body = crate::problem::Problem))))]
pub(crate) async fn plan_switcher(
    State(state): State<AppState>,
    principal: Principal,
    SwitcherBody(req): SwitcherBody,
) -> ControlResult<Response> {
    // A keyer plan rehearses a command (it reports admission cost, the
    // on-air state and the refusal the command would answer), so it needs
    // the command's role, before any object check or existence answer
    // (#389 review H1).
    let keyer_verb = super::switcher_keyers::is_keyer_verb(&req.action);
    principal.role.require(if keyer_verb {
        Action::Write
    } else {
        Action::Read
    })?;
    let mut req = req;
    normalize_layout_source(&mut req);
    authorize_refs(&principal, &req)?;
    if keyer_verb {
        validate_me(&req)?;
        let handle = engine(&state)?;
        return super::switcher_keyers::plan(&state, &principal, handle, &req);
    }
    validate_source_configured(&state, &req)?;
    validate_first_slice(&req, &req.action)?;
    if let Err(response) = parse_transition(&req) {
        return Ok(*response);
    }
    let handle = engine(&state)?;
    if matches!(req.action.as_str(), "cut" | "auto") {
        // Review AUTHZ-1: the take toggles the armed keys.
        super::switcher_keyers::authorize_armed(&principal, &req.me, handle)?;
    }
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
    // One snapshot for the take's Preview authorization and its armed-set
    // binding (review R3-KEYONLY-1).
    let take_snapshot = handle.snapshot();
    authorize_take_of_layout(&state, &principal, take_snapshot.as_deref(), &req)?;
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
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/switcher/commands/{kind}", tag = "switcher", params(("kind" = String, Path, description = "The verb; must equal `action`."), ("Idempotency-Key" = String, Header, description = "Required; a replay answers the first 202.")), request_body = SwitcherRequest, responses((status = 202, description = "Queued; the outcome arrives as `switcher.operation` at the engine tick. Keyer verbs also emit `switcher.keyer` on a committed state change.", body = SwitcherAccepted), (status = 403, description = "Not authorized for the M/E, the Program scope, a referenced object, the keyer object id or a named fill/key source, or (a `layout` select, or a CUT/AUTO whose Preview is `layout:<id>`) every source the stored layout binds; one generic refusal; the key is not burned.", body = crate::problem::Problem), (status = 404, description = "`unknown-keyer`.", body = crate::problem::Problem), (status = 409, description = "`capacity` (keyer budget, generic for scoped callers; or a fifth stored layout as an M/E source, or a source it shows cannot be leased), `layout-changed` (the stored layout was edited or deleted while this request was in flight; retry), `on-air-protected` (a fill/key change of a keyer on air without `on_air = \"allow\"`), `clean-unavailable-with-dsk` (the first DSK while an output carries Clean), `keyer-ramping`, `keyer-in-transition`, `in-flight` (`arm` during a transition), `arm-hidden-keys` (`arm` while the armed set holds a key the caller may not see: it replaces the whole set, so it would silently disarm it; generic, never naming it), `keyers-unavailable`, `source-unavailable` (a selected source that cannot be leased for the bus, or a `media_player` source whose player has no channel; the detail says why); nothing is enqueued and every lease and key taken is released.", body = crate::problem::Problem), (status = 422, description = "Invalid verb or references (exactly one of `source`/`input`/`layout` on a select; none on cut/auto), `not-on-an-input`, `unknown-slot`, `unknown-source` (no such stored layout), `layout-not-admissible` (another canvas, a translucent cell, or a nested layout/bus cell).", body = crate::problem::Problem), (status = 503, description = "`engine-busy`: the 64-slot mailbox is full, a stored layout could not be read now, or the output clock did not adopt a newly registered layout within 1 s (key and leases released); or `idempotency-capacity`.", body = crate::problem::Problem))))]
#[allow(clippy::too_many_lines)]
// reason: one linear admission sequence (authz → resolve input → buses → key →
// lease → enqueue); the input resolution added the lines, not new branches.
pub(crate) async fn submit_switcher(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(kind): Path<String>,
    SwitcherBody(req): SwitcherBody,
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
    if super::switcher_keyers::is_keyer_verb(&kind) {
        validate_me(&req)?;
        let handle = engine(&state)?;
        let scene_version = handle
            .scene_version()
            .ok_or_else(|| ControlError::Conflict("switcher has no active scene".into()))?;
        return super::switcher_keyers::submit(
            &state,
            &principal,
            handle,
            &idem,
            &req,
            scene_version,
        );
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
    let target = match resolve_target(handle, &req) {
        Ok(target) => target,
        Err(response) => return Ok(*response),
    };
    let target_guard = authorize_target(&state, &principal, &req, target.as_ref())?;
    let verb = verb(&req, &kind, target.as_ref(), transition)?;
    // P3R2-C: authorization of every EXPLICIT request field and static
    // validation are done; an EXISTING reservation is a replay and answers
    // its original 202 before the transient admission checks (buses, an
    // active take, the scene), which describe the engine now rather than the
    // request. `peek` never reserves; a fresh key still passes every check
    // below before `reserve`. The key is scoped per principal and path, so
    // this never answers another principal's operation.
    if let Some(op) = idem
        .0
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .and_then(|key| state.idempotency.peek(key))
    {
        return Ok(accepted(&op, kind));
    }
    // Review AUTHZ-1: a NEW take toggles the armed keys; it is authorized
    // on them and bound to the arm set and definitions it was authorized
    // for. Review round 2 IDEMP-1: the armed set is engine state, not part
    // of the request, so a replay (above) is not re-authorized against a set
    // another operator armed since; the original op already carries the
    // versions it was bound to.
    //
    // Review R3-IDEMP-1: the current Preview of a CUT/AUTO is engine state
    // too, so its layout authorization and guard also run only for a NEW
    // take, after the replay above, from the same snapshot the armed-set
    // binding reads (a key-only take guards no Preview: R3-KEYONLY-1).
    let take_snapshot = handle.snapshot();
    let take_guard = authorize_take_of_layout(&state, &principal, take_snapshot.as_deref(), &req)?;
    let mut expect = target_guard.or(take_guard);
    let bound = if matches!(kind.as_str(), "cut" | "auto") {
        super::switcher_keyers::authorize_armed_at(
            &principal,
            &req.me,
            take_snapshot.as_deref(),
            handle,
        )?
    } else {
        None
    };
    let verb = match bound {
        Some((arm_version, defs_version)) => {
            SwitcherVerb::Bound(Box::new(multiview_engine::switcher_control::BoundVerb {
                verb,
                arm_version: Some(arm_version),
                defs_version: Some(defs_version),
            }))
        }
        None => verb,
    };
    validate_buses(handle, &req, target.as_ref())?;
    let scene_version = handle
        .scene_version()
        .ok_or_else(|| ControlError::Conflict("switcher has no active scene".into()))?;
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
                        // A select that can never show a picture is refused
                        // with its reason (the source was authorized above).
                        if let Some(reason) = demand.select_refusal(source) {
                            state.idempotency.release(Some(key), &op);
                            return Ok(crate::problem::Problem::new(
                                409,
                                "source-unavailable",
                                "Selection refused",
                            )
                            .with_detail(reason)
                            .into_response());
                        }
                        let Some(lease) = demand.switcher_preview_lease(source, label) else {
                            state.idempotency.release(Some(key), &op);
                            return Ok(crate::problem::Problem::new(
                                409,
                                "source-unavailable",
                                "Selection refused",
                            )
                            .with_detail(format!(
                                "source {source:?} cannot be leased for the bus: the running \
                                 engine has not registered it yet (retry shortly) or the lease \
                                 limit is reached"
                            ))
                            .into_response());
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
            // Audit the accepted request (who selected what); the engine's
            // outcome follows on the realtime stream. A replay is not audited
            // again. The source id was authorized above.
            state.audit(
                &principal.key_id,
                crate::audit::AuditAction::Command,
                "switcher",
                &req.me,
                Some(serde_json::json!({
                    "command": kind,
                    "operation_id": op.to_string(),
                    "source": target.as_ref().map(|t| t.source.clone()),
                })),
            );
            op
        }
    };
    Ok(accepted(&op, kind))
}

/// The `202` for an accepted (or replayed) operation.
fn accepted(op: &crate::command::OperationId, kind: String) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(SwitcherAccepted {
            operation_id: op.to_string(),
            kind,
        }),
    )
        .into_response()
}
