//! The patch panel, mixer-input slice (ADR-0110, `docs/architecture/output-routing.md`
//! §12.8, #341): `GET /api/v1/patch`, `POST /api/v1/patch/plan`,
//! `POST /api/v1/patch/take`.
//!
//! Order of checks (plan and take), so nothing leaks and nothing is burned:
//! role → crosspoint count → per crosspoint destination authorization
//! (`authorize_object(me)` + Program scope; an output slot: the output axis)
//! and source authorization (`authorize_object(source)`) — all **before**
//! existence and before the `Idempotency-Key` is looked at. Then shape
//! (plane, destination kind, source kind, duplicates), existence against the
//! engine's live input table, the on-air guard, and only for a take the key
//! (a replay answers its first 202), the rate limits (429), the reservation,
//! the on-demand leases and the bounded `try_send` (full → 503, key and
//! leases released). The outcome rides the existing `switcher.operation`
//! event; an applied patch is persisted by [`crate::patch`].
//!
//! Mixer inputs take `plane = "video"` crosspoints with a configured source,
//! a stored layout (`{"layout": id}`, #307: the engine source `layout:<id>`,
//! admitted like a layout select and registered as an M/E source at take
//! time; Read on it and on every source it binds) or `"none"` (slice A). Output slots (#342 B2): a request carries EITHER
//! mixer crosspoints OR the crosspoints of ONE output — its video slot 0
//! (`{source}` or `{bus}`) and optionally audio slot 0 `"follow"` (which
//! rides the video take). An output take goes through exactly the machinery
//! of `POST /routing/output/take` (that route is the slot-0 alias): the take
//! store's replay, the per-output and per-principal limits, the bounded
//! `try_send`, the `route.output` outcome and persistence on `applied`.
//! Atomic multi-destination sets arrive with patch salvos (#308).

use std::time::Instant;

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::switcher::SourceRef;
use multiview_engine::switcher_control::{
    PatchSet, SwitcherCommand, SwitcherControl, SwitcherSnapshot,
};
use multiview_engine::switcher_inputs::{InputsReadback, MAX_PATCH_MEMBERS};
use multiview_events::AuthzScope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{authorize_object, authorize_scope, Action, Principal};
use crate::concurrency::{IdempotencyKey, Reservation};
use crate::error::{ControlError, ControlResult};
use crate::problem::Problem;
use crate::state::AppState;

/// The refusal of a request mixing destinations (two outputs, or mixer +
/// output).
pub(crate) const MULTI_DESTINATION: &str =
    "multi-destination patch sets arrive with patch salvos (#308); one request patches mixer \
     inputs or the slots of one output";

/// How long a take naming a newly registered layout waits for the output
/// clock to adopt its store (#307). Ten ticks at 25 fps; on timeout the take
/// is 503 `engine-busy` with its key released.
const ME_LAYOUT_ADOPT_WAIT: std::time::Duration = std::time::Duration::from_millis(1_000);

/// The first slice's one M/E and its program.
const ME: &str = "me1";
const PROGRAM: &str = "main";

/// One crosspoint destination: a mixer input (`{"mixer","input"}`) or, in a
/// later slice, an output slot (`{"output","slot"}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PatchDestination {
    /// M/E id (`me1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixer: Option<String>,
    /// Mixer input number, 1..=64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u16>,
    /// Output API id (an output slot destination, with `slot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Output slot index: video and audio slot 0 in this release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

/// One crosspoint `{ plane, destination, source }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PatchCrosspoint {
    /// `video` (this slice), `audio` or `subtitle`.
    pub plane: String,
    /// Where the source is patched.
    pub destination: PatchDestination,
    /// A mixer input: `{"source": id}`, `{"layout": id}` (a stored layout as
    /// an M/E source, #307: Read on it AND on every source its cells bind;
    /// it must solve on the running canvas with opaque cells and bind no other
    /// layout or bus) or `"none"`; `{"bus": ..}` is refused. An output video slot 0:
    /// `{"source": id}`, `{"layout": id}` (a stored layout; Read on it AND on
    /// every source its cells bind) or `{"bus": ..}`; audio slot 0: `"follow"`.
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub source: Value,
}

/// `on_air`: whether a crosspoint may re-patch the input on Program.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OnAir {
    /// Refuse (409 `on-air-protected`) — the default.
    #[default]
    Protect,
    /// Apply it as a cut on Program.
    Allow,
}

/// Plan/take body: 1..=64 crosspoints applied atomically at one tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    /// The crosspoints.
    pub crosspoints: Vec<PatchCrosspoint>,
    /// On-air guard for the input on Program.
    #[serde(default)]
    pub on_air: OnAir,
}

/// A typed refusal (`type` suffix + detail).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PatchRefusal {
    /// RFC 9457 `type` suffix, e.g. `on-air-protected`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Human-readable reason.
    pub detail: String,
}

/// The cost of an output-slot crosspoint (§12.4, §12.10).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SlotCost {
    /// `0` (P1: an encoded bus already carries it) or `1`.
    pub new_encoders: u8,
    /// `1` when a P3 needs a convert whose (picture, format) feed is not
    /// live yet, else `0`.
    pub converts: u8,
    /// Live-opened encoder sessions in use (whole-system principals only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions_in_use: Option<usize>,
    /// The session limit (whole-system principals only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions_max: Option<usize>,
    /// Whether the take fits the encoder budget now.
    pub fits: bool,
}

/// One planned crosspoint.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PlannedCrosspoint {
    /// The destination as requested.
    pub destination: PatchDestination,
    /// The path this crosspoint takes: `mixer-input`, `P1`, `P2`, `P3`
    /// (§12.4), or `follow` (an audio slot riding the video take).
    pub path: &'static str,
    /// Whether the destination input is on Program now (mixer inputs).
    pub on_air: bool,
    /// The cost of an output video slot (absent for mixer inputs and audio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<SlotCost>,
    /// `null` when the crosspoint can be taken.
    pub refusal: Option<PatchRefusal>,
}

/// `POST /patch/plan` answer.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PatchPlan {
    /// Change class (a mixer re-patch is Class-1, hot).
    pub class: &'static str,
    /// Per-crosspoint plan, request order.
    pub crosspoints: Vec<PlannedCrosspoint>,
    /// `true` when every crosspoint can be taken.
    pub can_take: bool,
}

/// `POST /patch/take` 202 answer; the outcome arrives as `switcher.operation`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PatchAccepted {
    /// Correlates with `switcher.operation.operation_id`.
    pub operation_id: String,
    /// Always `accepted`; never `applied`.
    pub status: &'static str,
}

pub(crate) fn problem(status: u16, kind: &str, detail: impl Into<String>) -> Response {
    Problem::new(status, kind, "Patch refused")
        .with_detail(detail)
        .into_response()
}

fn too_many(retry: std::time::Duration) -> Response {
    let secs = retry
        .as_secs()
        .saturating_add(u64::from(retry.subsec_nanos() > 0))
        .max(1);
    let mut response = Problem::new(429, "too-many-requests", "Too many takes")
        .with_detail("too many patch takes for this destination or principal; retry shortly")
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The source id named by a crosspoint, if it is `{"source": id}`.
fn source_id(source: &Value) -> Option<&str> {
    source.get("source").and_then(Value::as_str)
}

/// Authorize both ends of every crosspoint (BOLA), before any existence check.
fn authorize(
    principal: &Principal,
    req: &PatchRequest,
    layout: Option<&super::output_routing::LayoutRead>,
) -> ControlResult<()> {
    for xp in &req.crosspoints {
        let destination = &xp.destination;
        if let Some(mixer) = &destination.mixer {
            authorize_object(principal, mixer)?;
            authorize_scope(principal, AuthzScope::Program(PROGRAM))?;
        }
        if let Some(output) = &destination.output {
            super::output_routing::authorize_output_end(principal, output)?;
        }
        if let Some(object) = source_id(&xp.source) {
            authorize_object(principal, object)?;
        }
        // A layout: its id AND its whole current source closure, before any
        // output or feed existence answer (#342 review T-A1).
        if let Some(id) = xp.source.get("layout").and_then(Value::as_str) {
            super::output_routing::authorize_layout_feed(principal, id, layout)?;
        }
        if xp.source.get("bus").is_some() {
            authorize_object(principal, ME)?;
            authorize_scope(principal, AuthzScope::Program(PROGRAM))?;
        }
    }
    Ok(())
}

/// The request's ONE stored-layout read (#342 review T-I1): the layout a
/// crosspoint names (an output slot's, or a mixer input's, #307), read off the async worker under the
/// layout-read permit BEFORE any authorization answer, and only once every
/// destination passed its own authorization, so an unauthorized caller
/// never costs a read. The body feeds the closure authorization (T-A1) and
/// the later resolve.
///
/// # Errors
/// 403 for a destination; `EngineBusy` without a read permit; repository
/// errors.
async fn read_patch_layout(
    state: &AppState,
    principal: &Principal,
    req: &PatchRequest,
) -> ControlResult<Option<super::output_routing::LayoutRead>> {
    let Some(id) = req
        .crosspoints
        .iter()
        .find_map(|xp| xp.source.get("layout").and_then(Value::as_str))
    else {
        return Ok(None);
    };
    authorize(principal, req, None)?;
    authorize_object(principal, id)?;
    Ok(Some(super::output_routing::read_layout(state, id).await?))
}

/// A validated mixer crosspoint. `source` is the engine source id
/// (`layout:<id>` for a stored layout).
struct Member {
    input: u16,
    source: Option<String>,
    /// The stored layout id, for a `{"layout": id}` crosspoint.
    layout: Option<String>,
}

/// Shape checks: this slice's plane/destination/source kinds, no duplicates.
fn shape(req: &PatchRequest) -> Result<Vec<Member>, Box<Response>> {
    let mut members = Vec::with_capacity(req.crosspoints.len());
    for xp in &req.crosspoints {
        if xp.plane != "video" {
            return Err(Box::new(problem(
                422,
                "slot-medium-mismatch",
                format!(
                    "plane {:?} is not patchable in this release; mixer inputs take video \
                     (audio and subtitle slots arrive in a later slice)",
                    xp.plane
                ),
            )));
        }
        let destination = &xp.destination;
        if destination.output.is_some() || destination.slot.is_some() {
            return Err(Box::new(problem(422, "unknown-slot", MULTI_DESTINATION)));
        }
        let (Some(_), Some(input)) = (&destination.mixer, destination.input) else {
            return Err(Box::new(problem(
                422,
                "unknown-slot",
                "a mixer destination needs both `mixer` and `input`",
            )));
        };
        let layout = xp
            .source
            .get("layout")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let source =
            match &xp.source {
                Value::String(none) if none == "none" => None,
                Value::Object(map) if map.len() == 1 && source_id(&xp.source).is_some() => {
                    source_id(&xp.source).map(str::to_owned)
                }
                Value::Object(map) if map.len() == 1 && layout.is_some() => {
                    layout.map(crate::me_layouts::source_key)
                }
                _ => return Err(Box::new(problem(
                    422,
                    "unknown-source",
                    "a mixer input takes {\"source\": id}, {\"layout\": id} or \"none\"; buses \
                     as mixer inputs arrive in a later slice",
                ))),
            };
        if members.iter().any(|m: &Member| m.input == input) {
            return Err(Box::new(problem(
                422,
                "unknown-slot",
                format!("mixer input {input} appears twice in one patch"),
            )));
        }
        members.push(Member {
            input,
            source,
            layout: layout.map(str::to_owned),
        });
    }
    Ok(members)
}

fn configured_sources(state: &AppState) -> ControlResult<Vec<String>> {
    Ok(crate::boot_model::accepted_resources(state, "sources")?
        .map_or_else(|| state.sources.list(), Ok)?
        .into_iter()
        .map(|v| v.resource.id)
        .collect())
}

/// Live engine handle with numbered inputs, or the typed refusal.
fn engine_with_inputs(
    state: &AppState,
) -> Result<(&SwitcherControl, std::sync::Arc<InputsReadback>), Box<Response>> {
    let Some(handle) = state.switcher.as_ref() else {
        return Err(Box::new(problem(
            409,
            "switcher-unavailable",
            "no real Program/Preview switcher is attached to this run",
        )));
    };
    if !handle.is_ready() {
        return Err(Box::new(problem(
            409,
            "switcher-unavailable",
            "no real paired Program/Preview frame has been published",
        )));
    }
    let Some(inputs) = handle.inputs() else {
        return Err(Box::new(problem(
            422,
            "unknown-slot",
            "this M/E declares no numbered mixer inputs",
        )));
    };
    Ok((handle, inputs))
}

/// Everything after authorization and shape: existence and the on-air guard.
struct Checked {
    members: Vec<Member>,
    /// Resolved stored layouts the members patch (#307), registered at take.
    bodies: Vec<crate::output_routing::LayoutBody>,
    plan: PatchPlan,
    first_refusal: Option<PatchRefusal>,
}

fn check(
    state: &AppState,
    principal: &Principal,
    req: &PatchRequest,
    inputs: &InputsReadback,
    snapshot: Option<&SwitcherSnapshot>,
    layout: Option<&super::output_routing::LayoutRead>,
) -> ControlResult<Result<Checked, Response>> {
    let members = match shape(req) {
        Ok(members) => members,
        Err(response) => return Ok(Err(*response)),
    };
    let sources = configured_sources(state)?;
    let program_input = snapshot.and_then(|s| s.program_input);
    let preview_input = snapshot.and_then(|s| s.preview_input);
    let in_flight = snapshot.is_some_and(|s| s.in_flight);
    let mut planned = Vec::with_capacity(members.len());
    let mut bodies = Vec::new();
    let mut first_refusal = None;
    for (xp, member) in req.crosspoints.iter().zip(&members) {
        if xp.destination.mixer.as_deref() != Some(ME) {
            return Ok(Err(problem(
                422,
                "unknown-slot",
                format!("unknown mixer {:?}", xp.destination.mixer),
            )));
        }
        let Some(live) = inputs.inputs.iter().find(|i| i.number == member.input) else {
            return Ok(Err(problem(
                422,
                "unknown-slot",
                format!("mixer input {} is not declared", member.input),
            )));
        };
        if let Some(id) = &member.layout {
            // A stored layout (#307): admissible as an M/E source, resolved
            // from the request's one read (closure authorized already).
            match super::output_routing::resolve_me_layout(state, principal, id, layout)? {
                Ok(body) => bodies.push(body),
                Err((status, kind, detail)) => return Ok(Err(problem(status, kind, detail))),
            }
        } else if let Some(source) = &member.source {
            if !sources.iter().any(|id| id == source) {
                return Ok(Err(problem(
                    422,
                    "unknown-source",
                    "source is not in the running configuration",
                )));
            }
        }
        let on_program = program_input == Some(member.input);
        let on_bus = on_program || preview_input == Some(member.input);
        let refusal = if on_bus && in_flight {
            Some(multiview_engine::switcher_control::REFUSE_PATCH_DURING_AUTO)
        } else if on_bus && member.source.is_none() {
            Some("on-air-protected: the input is on Program or Preview and cannot be unpatched")
        } else if on_program
            && req.on_air == OnAir::Protect
            && member.source.as_deref() != live.source.as_ref().map(SourceRef::as_str)
        {
            Some(multiview_engine::switcher_control::REFUSE_ON_AIR_PROTECTED)
        } else {
            None
        }
        .map(|detail| PatchRefusal {
            kind: crate::patch::refusal_kind(detail).to_owned(),
            detail: detail.to_owned(),
        });
        if first_refusal.is_none() {
            first_refusal.clone_from(&refusal);
        }
        planned.push(PlannedCrosspoint {
            destination: xp.destination.clone(),
            path: "mixer-input",
            on_air: on_program,
            cost: None,
            refusal,
        });
    }
    Ok(Ok(Checked {
        members,
        bodies,
        plan: PatchPlan {
            class: "class1",
            can_take: first_refusal.is_none(),
            crosspoints: planned,
        },
        first_refusal,
    }))
}

fn validate_count(req: &PatchRequest) -> ControlResult<()> {
    if req.crosspoints.is_empty() || req.crosspoints.len() > MAX_PATCH_MEMBERS {
        return Err(ControlError::Validation(format!(
            "a patch carries 1..={MAX_PATCH_MEMBERS} crosspoints"
        )));
    }
    // The request reads ONE stored layout (its closure is authorized from
    // that read), so every `{layout}` crosspoint must name the same one.
    let mut layouts = req
        .crosspoints
        .iter()
        .filter_map(|xp| xp.source.get("layout").and_then(Value::as_str));
    if let Some(first) = layouts.next() {
        if layouts.any(|other| other != first) {
            return Err(ControlError::Validation(
                "one patch request names at most one stored layout".into(),
            ));
        }
    }
    Ok(())
}

/// The live patch (mixer plane), built from the engine's readback.
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/patch", tag = "patch",
    responses(
        (status = 200, description = "`{ mixer: [ { me, inputs: [ { number, label, video: {source}|{redacted:true}|null, on_program, on_preview } ], inputs_version, tick } ], outputs: [ { output, format: {width,height,fit,…}|null, video: [ { slot: 0, live, source: {source}|{bus}|{layout}|{redacted:true}|null, path: P1|P2|P3|null, pending: {source, held_ticks}|null, degraded } ], audio: [ { slot, track, language, source } ] } ] }`. Video slot 0 comes from the engine's live readback (`live: true`); `path` is P1 whenever an encoded bus (Program at the output's format, or a live key of its own) carries it. With no readback (no running engine, routing unavailable, or the output not in this run) `live` is false and `source` and `path` are null: the live state is unknown, never reported as Program. Audio slot 0's `source` is the output's accepted audio feed (`\"follow\"` by default, else `\"program\"`, `\"none\"`, `{source}` or `{tracks}`), projected and redacted exactly as `GET /routing`; audio slots i > 0 are `{track}`, fixed by config. A mixer the caller cannot read (M/E object + Program scope) is omitted, an output it does not pass the output axis for is omitted, and a feed it cannot read (a source, or a track whose route input it cannot read) is `{\"redacted\": true}`.", body = Object),
        (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        (status = 403, description = "Role lacks Read.", body = crate::problem::Problem),
        (status = 503, description = "engine-busy: every layout-read slot is in use (a layout on air on a visible output); retry", body = crate::problem::Problem),
    )))]
pub(crate) async fn get_patch(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let mut mixer = Vec::new();
    let readable = authorize_object(&principal, ME).is_ok()
        && authorize_scope(&principal, AuthzScope::Program(PROGRAM)).is_ok();
    if let (true, Some(handle)) = (readable, state.switcher.as_ref()) {
        if let Some(table) = handle.inputs() {
            let snapshot = handle.snapshot();
            let program_input = snapshot.as_ref().and_then(|s| s.program_input);
            let preview_input = snapshot.as_ref().and_then(|s| s.preview_input);
            let inputs: Vec<Value> = table
                .inputs
                .iter()
                .map(|input| {
                    let video = input.source.as_ref().map(|source| {
                        if authorize_object(&principal, source.as_str()).is_ok() {
                            json!({ "source": source.as_str() })
                        } else {
                            json!({ "redacted": true })
                        }
                    });
                    json!({
                        "number": input.number,
                        "label": input.label,
                        "video": video,
                        "on_program": program_input == Some(input.number),
                        "on_preview": preview_input == Some(input.number),
                    })
                })
                .collect();
            mixer.push(json!({
                "me": ME,
                "inputs": inputs,
                "inputs_version": snapshot.as_ref().map_or(table.version, |s| s.inputs_version.max(table.version)),
                "tick": handle.applied_tick(),
            }));
        }
    }
    let outputs = super::patch_output::outputs_view(&state, &principal).await?;
    Ok((
        StatusCode::OK,
        Json(json!({ "mixer": mixer, "outputs": outputs })),
    )
        .into_response())
}

/// Plan 1..64 mixer crosspoints (path, on-air flag, refusals) or the slots of
/// one output (§12.4 path P1/P2/P3 and cost); no change.
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/patch/plan", tag = "patch", request_body = PatchRequest,
    responses(
        (status = 200, body = PatchPlan),
        (status = 403, description = "Not authorized for a destination (M/E object + Program scope; an output: object + output axis) or a source.", body = crate::problem::Problem),
        (status = 404, description = "Unknown output.", body = crate::problem::Problem),
        (status = 409, description = "`switcher-unavailable` or `routing-unavailable`.", body = crate::problem::Problem),
        (status = 422, description = "`slot-medium-mismatch` (an audio slot other than slot 0 follow, a subtitle slot), `unknown-slot` (video slot >= 1, two outputs, or mixer + output: multi-destination sets arrive with patch salvos #308), `unknown-source` (a bus on a mixer input, an unknown source or stored layout; an unknown feed), `layout-not-admissible` (a mixer input's `{layout}`: not on the running canvas, a translucent cell, or a cell binding another layout or a bus — nested layouts and buses are not M/E sources), or not 1..=64 crosspoints, or two different stored layouts in one request. A layout on an output slot is planned like the alias: `layout-not-admissible` and `aux-capacity` are refusals in the plan. A plan never registers a mixer input's layout.", body = crate::problem::Problem),
        (status = 503, description = "`engine-busy`: an output slot's `{layout}` could not be read now (every layout-read slot is in use); retry.", body = crate::problem::Problem),
    )))]
pub(crate) async fn plan_patch(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<PatchRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    validate_count(&req)?;
    let layout = read_patch_layout(&state, &principal, &req).await?;
    authorize(&principal, &req, layout.as_ref())?;
    if super::patch_output::has_output(&req) {
        let patch = match super::patch_output::shape(&req) {
            Ok(patch) => patch,
            Err(response) => return Ok(*response),
        };
        return Ok(
            match super::patch_output::plan(&state, &principal, &req, &patch, layout.as_ref())? {
                Ok(plan) => (StatusCode::OK, Json(plan)).into_response(),
                Err(response) => response,
            },
        );
    }
    let (handle, inputs) = match engine_with_inputs(&state) {
        Ok(found) => found,
        Err(response) => return Ok(*response),
    };
    let snapshot = handle.snapshot();
    match check(
        &state,
        &principal,
        &req,
        &inputs,
        snapshot.as_deref(),
        layout.as_ref(),
    )? {
        Ok(checked) => Ok((StatusCode::OK, Json(checked.plan)).into_response()),
        Err(response) => Ok(response),
    }
}

/// Take 1..64 mixer crosspoints atomically at one engine tick, or one
/// output's video slot 0 (+ audio slot 0 follow) through the same machinery
/// as `POST /routing/output/take` (the slot-0 alias; shared idempotency
/// store and limits; the outcome rides `route.output`).
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/patch/take", tag = "patch", request_body = PatchRequest,
    params(("Idempotency-Key" = String, Header, description = "Required; a replay answers the first 202.")),
    responses(
        (status = 202, body = PatchAccepted),
        (status = 403, description = "Not authorized for a destination (M/E object + Program scope; an output: object AND output axis) or a source; a replayed key is re-authorized.", body = crate::problem::Problem),
        (status = 404, description = "Unknown output.", body = crate::problem::Problem),
        (status = 409, description = "`conflict` (an output take's Idempotency-Key first used for another output or feed; nothing enqueued), `on-air-protected` (the input on Program under `protect`, or the Program/Preview input during an AUTO), `switcher-unavailable`, `routing-unavailable`, `switcher-required`, `output-kind-unroutable`, `capacity` (also: a mixer input's `{layout}` would be the fifth stored layout registered as an M/E source, or a source it shows cannot be leased; the key is released), `layout-changed` (a mixer input's `{layout}` was edited or deleted while this request was in flight; the key is released, retry), `aux-capacity` (no free layout canvas), or no source lease.", body = crate::problem::Problem),
        (status = 422, description = "`slot-medium-mismatch`, `unknown-slot`, `unknown-source`, `unknown-feed` / `layout-not-admissible` (an output slot's or a mixer input's layout), missing Idempotency-Key, or not 1..=64 crosspoints.", body = crate::problem::Problem),
        (status = 429, description = "Over 2 takes/s per destination or 10/s per principal (one request = one take); see Retry-After.", body = crate::problem::Problem),
        (status = 503, description = "`engine-busy` (the key and leases are released; or a `{layout}` could not be read now: every layout-read slot is in use; or the output clock did not adopt a newly registered mixer-input layout within 1 s) or `idempotency-capacity`.", body = crate::problem::Problem),
    )))]
#[allow(clippy::too_many_lines)]
// reason: one linear admission sequence whose order (authz → shape → existence
// → key → limits → reserve → lease → enqueue) is the security contract.
pub(crate) async fn take_patch(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<PatchRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    validate_count(&req)?;
    let layout = read_patch_layout(&state, &principal, &req).await?;
    authorize(&principal, &req, layout.as_ref())?;
    if super::patch_output::has_output(&req) {
        let patch = match super::patch_output::shape(&req) {
            Ok(patch) => patch,
            Err(response) => return Ok(*response),
        };
        return Ok(
            match super::patch_output::take(&state, &principal, &idem, &patch, layout.as_ref())? {
                Ok(op) => accepted(op),
                Err(response) => response,
            },
        );
    }
    let (handle, inputs) = match engine_with_inputs(&state) {
        Ok(found) => found,
        Err(response) => return Ok(*response),
    };
    let snapshot = handle.snapshot();
    let checked = match check(
        &state,
        &principal,
        &req,
        &inputs,
        snapshot.as_deref(),
        layout.as_ref(),
    )? {
        Ok(checked) => checked,
        Err(response) => return Ok(response),
    };
    if let Some(refusal) = checked.first_refusal {
        return Ok(problem(409, &refusal.kind, refusal.detail));
    }
    let Some(key) = idem.0.as_deref().filter(|key| !key.trim().is_empty()) else {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    };
    if let Some(op) = state.idempotency.peek(key) {
        return Ok(accepted(op.to_string()));
    }
    let destinations: Vec<String> = checked
        .members
        .iter()
        .map(|m| format!("{ME}.in{}", m.input))
        .collect();
    if let Err(retry) =
        state
            .patch_limiter
            .admit_set(&destinations, &principal.key_id, Instant::now())
    {
        return Ok(too_many(retry));
    }
    let op = match state.idempotency.reserve(Some(key)) {
        Reservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        Reservation::Replay(op) => return Ok(accepted(op.to_string())),
        Reservation::Fresh(op) => op,
    };
    let Some(scene_version) = handle.scene_version() else {
        state.idempotency.release(Some(key), &op);
        return Err(ControlError::Conflict(
            "switcher has no active scene".into(),
        ));
    };
    // A stored layout patched to an input is registered as an M/E source
    // (#307) before the enqueue; the engine refuses a patch to a store it
    // has not adopted, so the take waits (bounded, off the clock) for the
    // frame hook to adopt it.
    if !checked.bodies.is_empty() {
        let Some(registry) = state.me_layouts.as_ref() else {
            state.idempotency.release(Some(key), &op);
            return Err(ControlError::Conflict(
                "stored layouts as M/E sources are not available in this run".into(),
            ));
        };
        for body in checked.bodies {
            if let Err(error) = registry.ensure(body, state.source_demand.as_ref(), Instant::now())
            {
                state.idempotency.release(Some(key), &op);
                return Ok(problem(409, error.kind(), error.detail()));
            }
        }
        if !registry.adopted_within(ME_LAYOUT_ADOPT_WAIT).await {
            state.idempotency.release(Some(key), &op);
            return Err(ControlError::EngineBusy);
        }
    }
    // On-demand leases for a newly patched source landing on PGM or PVW, so
    // an on-demand source starts before the engine needs its fresh frame.
    let program_input = snapshot.as_ref().and_then(|s| s.program_input);
    let preview_input = snapshot.as_ref().and_then(|s| s.preview_input);
    let lease_for = |bus: Option<u16>, label: &str| {
        // A layout member takes no source lease: the registry holds leases
        // on every source it binds.
        let member = checked
            .members
            .iter()
            .find(|m| Some(m.input) == bus && m.source.is_some() && m.layout.is_none())?;
        let source = member.source.clone()?;
        Some((source, label.to_owned()))
    };
    let wanted = [
        lease_for(program_input, "switcher:me1:pgm:patch"),
        lease_for(preview_input, "switcher:me1:pvw:patch"),
    ];
    let mut leases = [None, None];
    if let Some(demand) = state.source_demand.as_ref() {
        for (slot, want) in leases.iter_mut().zip(&wanted) {
            if let Some((source, label)) = want {
                let Some(lease) = demand.switcher_preview_lease(source, label) else {
                    state.idempotency.release(Some(key), &op);
                    return Ok(problem(
                        409,
                        "capacity",
                        "source lease unavailable (unknown source or lease limit)",
                    ));
                };
                *slot = Some((
                    source.clone(),
                    crate::source_demand::SwitcherPreviewLease::new(lease),
                ));
            }
        }
    }
    let command = SwitcherCommand {
        operation_id: op.to_string(),
        scene_version,
        verb: multiview_engine::switcher_control::SwitcherVerb::Patch(PatchSet {
            members: checked
                .members
                .iter()
                .map(|m| (m.input, m.source.as_deref().map(SourceRef::new)))
                .collect(),
            allow_on_air: req.on_air == OnAir::Allow,
        }),
    };
    let [program_lease, preview_lease] = leases;
    let owner = &state.switcher_preview_lease;
    let enqueue = || {
        handle.try_send(command).map_err(|_| ())?;
        Ok(handle.composed_count())
    };
    // The first lease owns the enqueue (reserve-then-send under the owner's
    // lock); a dropped lease value is released at once (refusal path).
    let admitted = match (program_lease, preview_lease) {
        (Some((target, lease)), preview) => {
            let sent = owner
                .admit_program(op.to_string(), target, lease, handle, enqueue)
                .is_ok();
            if sent {
                if let Some((target, lease)) = preview {
                    // Resolved by the off-clock sweep against the engine's PVW.
                    let _ = owner.admit(format!("{op}:pvw"), target, lease, handle, || {
                        Ok(handle.composed_count())
                    });
                }
            }
            sent
        }
        (None, Some((target, lease))) => owner
            .admit(op.to_string(), target, lease, handle, enqueue)
            .is_ok(),
        (None, None) => enqueue().is_ok(),
    };
    if !admitted {
        state.idempotency.release(Some(key), &op);
        return Err(ControlError::EngineBusy);
    }
    Ok(accepted(op.to_string()))
}

/// The patch routes, merged into the API router.
pub(crate) fn router() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/patch", get(get_patch))
        .route("/patch/plan", post(plan_patch))
        .route("/patch/take", post(take_patch))
}

fn accepted(operation_id: String) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(PatchAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}
