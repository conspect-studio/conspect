//! Atomic multi-destination patch sets (ADR-0110 decision 8,
//! output-routing §12.8 "Atomic at one tick", #308): the path of a
//! `POST /patch/plan|take` whose crosspoints touch two or more destinations
//! (several outputs, or mixer inputs plus outputs), and of
//! `POST /salvos/{id}/take` with stored patch crosspoints.
//!
//! The security contract is the order of checks: role → count → the
//! bounded layout reads (one per distinct layout, at most
//! [`super::patch::MAX_SET_LAYOUTS`]) → authorize EVERY member's both ends
//! → shape → the key's replay/conflict (as the slot-0 alias) → existence →
//! plan gates per member → the limits → the set's encoder delta → reserve
//! → enqueue. Any 403 or 422 changes nothing and burns no key.
//!
//! Enqueue: with a mixer half the switcher's `PatchArmed` is sent FIRST
//! (bounded `try_send`), then the router's `try_take_set`; if the second
//! fails the shared arm is refused so the switcher drops its half, and the
//! key is released (503). The outcome is aggregated into `patch.take`
//! ([`crate::patch_set`]).

use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::VideoFeed;
use multiview_core::switcher::SourceRef;
use multiview_engine::patch_set::{SetArm, SetRefusal};
use multiview_engine::switcher_control::{PatchSet, SwitcherCommand, SwitcherVerb};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::output_routing::{
    audio_gate, aux_capacity_text, capacity_text, output_of, refusal_problem, resolve_feed,
    routing_unavailable, unroutable_kind, LayoutRead,
};
use super::patch::{
    check, engine_with_inputs, problem, shape, too_many, Member, OnAir, PatchCrosspoint, PatchPlan,
    PatchRequest, ME,
};
use super::patch_output::OutputPatch;
use crate::auth::Principal;
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::output_routing::{
    LayoutBody, OutputTakeRequest, OutputTakeSet, PlanRefusal, RoutedAudio, RoutedFeed,
    TakeSubmitError,
};
use crate::patch_set::{SetMembers, SetReservation};
use crate::state::AppState;

/// The aggregate cost of a set (`POST /patch/plan`, #308): its NEW encoder
/// delta, deduplicated per identical engine encoder identity (feed at the
/// output's format, encoder settings, live reset settings), against the
/// caller's per-principal cap and the global sessions.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetCost {
    /// New encoder sessions the whole set opens.
    pub new_encoders: usize,
    /// Whether that delta fits the caller's cap and the global sessions now.
    pub fits: bool,
}

fn is_mixer(xp: &PatchCrosspoint) -> bool {
    xp.destination.mixer.is_some() || xp.destination.input.is_some()
}

fn is_output(xp: &PatchCrosspoint) -> bool {
    xp.destination.output.is_some() || xp.destination.slot.is_some()
}

/// Whether a request is an atomic set: two or more distinct outputs, or
/// mixer inputs together with an output. One mixer patch or one output keep
/// their exact single-destination paths and bodies.
pub(crate) fn is_set(req: &PatchRequest) -> bool {
    let mut outputs: Vec<&str> = Vec::new();
    let mut mixer = false;
    for xp in &req.crosspoints {
        if is_mixer(xp) {
            mixer = true;
        }
        if let Some(output) = xp.destination.output.as_deref() {
            if !outputs.contains(&output) {
                outputs.push(output);
            }
        }
    }
    outputs.len() >= 2 || (mixer && !outputs.is_empty())
}

/// The set store key of a request: its own namespace (`patch-set`,
/// `salvo-take:<id>`) ahead of the scoped `Idempotency-Key` (principal,
/// method, path, raw key), so a set key never replays a single take.
///
/// # Errors
/// A missing `Idempotency-Key` (422).
pub(crate) fn set_key(idem: &IdempotencyKey, namespace: &str) -> ControlResult<String> {
    let scoped = idem
        .0
        .as_deref()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| ControlError::Validation("Idempotency-Key is required".into()))?;
    Ok(format!("{}:{namespace}:{scoped}", namespace.len()))
}

/// The digest of a set's canonical body: the crosspoints sorted (order is
/// not meaning) plus the on-air guard. A key replays only this body.
fn digest(req: &PatchRequest) -> String {
    let mut members: Vec<String> = req
        .crosspoints
        .iter()
        .map(|xp| {
            serde_json::json!({
                "plane": xp.plane,
                "destination": xp.destination,
                "source": xp.source,
            })
            .to_string()
        })
        .collect();
    members.sort_unstable();
    let mut hasher = Sha256::new();
    for member in &members {
        hasher.update(member.len().to_le_bytes());
        hasher.update(member.as_bytes());
    }
    hasher.update(if req.on_air == OnAir::Allow {
        b"allow".as_slice()
    } else {
        b"protect".as_slice()
    });
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// A set split into its halves, shape-checked.
struct Shaped {
    /// The mixer crosspoints as their own request (`None`: no mixer half).
    mixer: Option<PatchRequest>,
    /// Per output, in first-seen order: its sub-request and shape.
    outputs: Vec<(PatchRequest, OutputPatch)>,
}

/// Shape every member: a destination is a mixer input or an output slot;
/// the mixer half has the mixer path's shape (video, `{source}`/`"none"`,
/// no input twice); each output the single-output shape (video slot 0 once,
/// audio slot 0 `follow` at most once). Pure: no existence answer.
fn shape_set(req: &PatchRequest) -> Result<Shaped, Box<Response>> {
    let mut mixer = Vec::new();
    let mut outputs: Vec<(String, Vec<PatchCrosspoint>)> = Vec::new();
    for xp in &req.crosspoints {
        match (is_mixer(xp), is_output(xp)) {
            (true, true) | (false, false) => {
                return Err(Box::new(problem(
                    422,
                    "unknown-slot",
                    "a destination is a mixer input or an output slot, not both",
                )));
            }
            (true, false) => mixer.push(xp.clone()),
            (false, true) => {
                let Some(output) = xp.destination.output.clone() else {
                    return Err(Box::new(problem(
                        422,
                        "unknown-slot",
                        "an output slot destination needs `output`",
                    )));
                };
                match outputs.iter_mut().find(|(o, _)| *o == output) {
                    Some((_, members)) => members.push(xp.clone()),
                    None => outputs.push((output, vec![xp.clone()])),
                }
            }
        }
    }
    let mixer = (!mixer.is_empty()).then_some(PatchRequest {
        crosspoints: mixer,
        on_air: req.on_air,
    });
    if let Some(mixer) = &mixer {
        shape(mixer)?;
    }
    let mut shaped = Vec::with_capacity(outputs.len());
    for (_, crosspoints) in outputs {
        let sub = PatchRequest {
            crosspoints,
            on_air: req.on_air,
        };
        let patch = super::patch_output::shape(&sub)?;
        shaped.push((sub, patch));
    }
    Ok(Shaped {
        mixer,
        outputs: shaped,
    })
}

/// The layout read for a video feed (`None` for any other feed).
fn layout_for<'a>(video: &VideoFeed, layouts: &'a [LayoutRead]) -> Option<&'a LayoutRead> {
    match video {
        VideoFeed::Layout(id) => layouts.iter().find(|l| l.id == *id),
        _ => None,
    }
}

/// The dedupe key of a member's new encoder: the engine's complete
/// encoder-sharing identity (feed at the output's format, the output's
/// encoder settings and live reset settings, via
/// `OutputRouting::encoder_identity`). Members with equal keys share one
/// encoder; a member whose identity is unknown is its own (`member`, its
/// config output id, which is unique in a set), never shared.
fn encoder_key(state: &AppState, config_id: &str, feed: &RoutedFeed) -> String {
    state
        .output_routing
        .as_ref()
        .and_then(|c| c.routing.encoder_identity(config_id, feed))
        .map_or_else(
            || format!("member:{config_id}"),
            |id| format!("encoder:{id}"),
        )
}

/// Whether a set's `delta` new encoders fit `principal`'s cap (when the
/// engine reports it) and the global sessions `(in_use, max)`.
fn delta_fits(
    state: &AppState,
    principal: &Principal,
    delta: usize,
    global: Option<(usize, usize)>,
) -> bool {
    if delta == 0 {
        return true;
    }
    let mine = state
        .output_routing
        .as_ref()
        .and_then(|c| c.routing.principal_sessions(&principal.key_id))
        .is_none_or(|p| p.held.saturating_add(delta) <= p.max);
    let all = global.is_none_or(|(in_use, max)| in_use.saturating_add(delta) <= max);
    mine && all
}

/// `POST /patch/plan` for a set (after role, count, layout reads and
/// authorization of every member): every member planned in request order,
/// plus the set's aggregate encoder cost; `can_take` only when every member
/// can and the delta fits.
pub(crate) fn plan_set(
    state: &AppState,
    principal: &Principal,
    req: &PatchRequest,
    layouts: &[LayoutRead],
) -> ControlResult<Response> {
    let shaped = match shape_set(req) {
        Ok(shaped) => shaped,
        Err(response) => return Ok(*response),
    };
    let mut planned: Vec<(PatchCrosspoint, super::patch::PlannedCrosspoint)> = Vec::new();
    let mut can_take = true;
    if let Some(mixer) = &shaped.mixer {
        let (handle, inputs) = match engine_with_inputs(state) {
            Ok(found) => found,
            Err(response) => return Ok(*response),
        };
        let snapshot = handle.snapshot();
        let checked = match check(state, mixer, &inputs, snapshot.as_deref())? {
            Ok(checked) => checked,
            Err(response) => return Ok(response),
        };
        can_take &= checked.plan.can_take;
        planned.extend(
            mixer
                .crosspoints
                .iter()
                .cloned()
                .zip(checked.plan.crosspoints),
        );
    }
    let mut encoders: Vec<String> = Vec::new();
    let mut global = None;
    for (sub, patch) in &shaped.outputs {
        let layout = layout_for(&patch.video, layouts);
        let plan = match super::patch_output::plan(state, principal, sub, patch, layout)? {
            Ok(plan) => plan,
            Err(response) => return Ok(response),
        };
        can_take &= plan.can_take;
        for (xp, member) in sub.crosspoints.iter().zip(&plan.crosspoints) {
            if let Some(cost) = member.cost.as_ref().filter(|c| c.new_encoders == 1) {
                let feed = RoutedFeed::of(&patch.video);
                let output = output_of(state, &patch.output)?;
                if let (Some(feed), Some(output)) = (feed, output) {
                    let key = encoder_key(state, &output.id(), &feed);
                    if !encoders.contains(&key) {
                        encoders.push(key);
                    }
                }
                if let (Some(in_use), Some(max)) = (cost.sessions_in_use, cost.sessions_max) {
                    global = Some((in_use, max));
                }
            }
            planned.push((xp.clone(), member.clone()));
        }
        if global.is_none() {
            global = live_sessions(state);
        }
    }
    let delta = encoders.len();
    let fits = delta_fits(state, principal, delta, global);
    can_take &= fits;
    // Request order.
    let mut crosspoints = Vec::with_capacity(req.crosspoints.len());
    for xp in &req.crosspoints {
        if let Some(i) = planned.iter().position(|(p, _)| same_xp(p, xp)) {
            crosspoints.push(planned.swap_remove(i).1);
        }
    }
    Ok((
        StatusCode::OK,
        Json(SetPlan {
            plan: PatchPlan {
                class: "class1",
                crosspoints,
                can_take,
            },
            set: SetCost {
                new_encoders: delta,
                fits,
            },
        }),
    )
        .into_response())
}

fn same_xp(a: &PatchCrosspoint, b: &PatchCrosspoint) -> bool {
    a.plane == b.plane && a.destination == b.destination && a.source == b.source
}

/// The plan body of a set: the [`PatchPlan`] plus its aggregate cost.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetPlan {
    /// The per-member plan (same fields as a single-destination plan).
    #[serde(flatten)]
    pub plan: PatchPlan,
    /// The whole set's encoder delta and fit (sets only).
    pub set: SetCost,
}

/// The global live sessions from the routing readback.
fn live_sessions(state: &AppState) -> Option<(usize, usize)> {
    let live = state.output_routing.as_ref()?.routing.snapshot()?;
    Some((live.sessions_in_use, live.sessions_max))
}

/// One output member after existence and its plan gate.
struct ReadyOutput {
    api_id: String,
    config_id: String,
    feed: RoutedFeed,
    layout: Option<LayoutBody>,
    audio_follow: bool,
    new_encoder: Option<String>,
}

/// Existence and the plan gate of one output member (§12.8): the output,
/// the feed (a layout resolved and its closure authorized), the audio gate
/// and the engine's plan (`capacity` for one member over the sessions).
fn ready_output(
    state: &AppState,
    principal: &Principal,
    patch: &OutputPatch,
    layouts: &[LayoutRead],
    global: &mut Option<(usize, usize)>,
) -> ControlResult<Result<ReadyOutput, Response>> {
    let Some(control) = state.output_routing.as_ref() else {
        return Ok(Err(routing_unavailable()));
    };
    let Some(output) = output_of(state, &patch.output)? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: patch.output.clone(),
        });
    };
    let config_id = output.id();
    let (feed, layout) = match resolve_feed(
        state,
        principal,
        &patch.video,
        layout_for(&patch.video, layouts),
    )? {
        Ok(found) => found,
        Err((status, kind, detail)) => {
            return Ok(Err(refusal_problem(status, kind, detail)));
        }
    };
    if patch.audio_follow {
        if let Some(refused) = audio_gate(control, &config_id, Some(&feed), &RoutedAudio::Follow) {
            return Ok(Err(refused));
        }
    }
    let cost = match control.routing.plan(&config_id, &feed) {
        Ok(cost) if cost.fits() => cost,
        Ok(cost) => {
            return Ok(Err(refusal_problem(
                409,
                "capacity",
                capacity_text(principal, cost.sessions_in_use, cost.sessions_max),
            )))
        }
        Err(PlanRefusal::SwitcherRequired) => {
            return Ok(Err(refusal_problem(
                409,
                "switcher-required",
                "Preview and Clean need a switcher (M/E) in this run".to_owned(),
            )))
        }
        Err(PlanRefusal::OutputKindUnroutable) => return Ok(Err(unroutable_kind())),
        Err(PlanRefusal::AudioUnsupported) => {
            return Ok(Err(refusal_problem(
                409,
                "audio-unsupported",
                "This output cannot carry a routed audio feed".to_owned(),
            )))
        }
        Err(PlanRefusal::AuxCapacity { active, max }) => {
            return Ok(Err(refusal_problem(
                409,
                "aux-capacity",
                aux_capacity_text(principal, active, max),
            )))
        }
    };
    *global = Some((cost.sessions_in_use, cost.sessions_max));
    Ok(Ok(ReadyOutput {
        api_id: patch.output.clone(),
        new_encoder: cost
            .needs_encoder
            .then(|| encoder_key(state, &config_id, &feed)),
        config_id,
        feed,
        layout,
        audio_follow: patch.audio_follow,
    }))
}

/// The mixer half after existence and the on-air gate.
struct ReadyMixer {
    members: Vec<Member>,
    scene_version: u64,
    snapshot: Option<Arc<multiview_engine::switcher_control::SwitcherSnapshot>>,
}

/// Take a set (after role, count, layout reads and the authorization of
/// every member): shape, existence, the plan gates, the key, the limits,
/// the encoder delta, the reservation and the enqueue. `Ok(Ok(op))` is the
/// operation to answer 202 with (a replay answers its first operation).
///
/// # Errors
/// Repository errors, `NotFound` for an unknown output, 409 `conflict` for
/// a key first used for another body, `idempotency-capacity`, `engine-busy`.
#[allow(clippy::too_many_lines)]
// reason: one linear admission sequence whose order (shape → existence →
// gates → key → limits → delta → reserve → enqueue) is the security contract.
pub(crate) fn take_set(
    state: &AppState,
    principal: &Principal,
    key: &str,
    req: &PatchRequest,
    layouts: &[LayoutRead],
) -> ControlResult<Result<String, Response>> {
    let shaped = match shape_set(req) {
        Ok(shaped) => shaped,
        Err(response) => return Ok(Err(*response)),
    };
    // The key, right after the authorization of every member under the
    // caller's CURRENT grants and the shape (as the slot-0 alias): a replay
    // of this very body answers its first operation (never re-planned: the
    // set may hold the very encoders a fresh plan would count); a different
    // body is 409 `conflict`. Nothing is enqueued either way.
    let digest = digest(req);
    match state.patch_sets.peek(key, &digest) {
        Some(Ok(op)) => return Ok(Ok(op.to_string())),
        Some(Err(())) => return Err(super::output_routing::key_reused()),
        None => {}
    }
    // Existence and plan gates: the mixer half, then every output member.
    let mut mixer = None;
    if let Some(mixer_req) = &shaped.mixer {
        let (handle, inputs) = match engine_with_inputs(state) {
            Ok(found) => found,
            Err(response) => return Ok(Err(*response)),
        };
        let snapshot = handle.snapshot();
        let checked = match check(state, mixer_req, &inputs, snapshot.as_deref())? {
            Ok(checked) => checked,
            Err(response) => return Ok(Err(response)),
        };
        if let Some(refusal) = checked.first_refusal {
            return Ok(Err(problem(409, &refusal.kind, refusal.detail)));
        }
        let Some(scene_version) = handle.scene_version() else {
            return Err(ControlError::Conflict(
                "switcher has no active scene".into(),
            ));
        };
        mixer = Some(ReadyMixer {
            members: checked.members,
            scene_version,
            snapshot,
        });
    }
    let mut outputs = Vec::with_capacity(shaped.outputs.len());
    let mut global = None;
    for (_, patch) in &shaped.outputs {
        match ready_output(state, principal, patch, layouts, &mut global)? {
            Ok(ready) => outputs.push(ready),
            Err(response) => return Ok(Err(response)),
        }
    }
    // One take for the principal, one per destination (§12.8 rule 3).
    //
    // B3: a set's output members are charged against the SAME limiter a
    // single-output take uses (`control.limiter`, `take_output_slot0`'s
    // `output_routing.rs`), not the mixer's `patch_limiter`: alternating
    // single-output takes with two-output sets on one output must not
    // double that output's or the principal's per-second budget. Mixer
    // members keep `patch_limiter`, exactly as a plain mixer patch
    // (`take_patch`). The two limiters are checked together before either
    // records (`check_set` on both, then `record_set` on both): a refusal
    // on either consumes nothing on the other (docs/architecture/
    // output-routing.md §12.8).
    let mixer_destinations: Vec<String> = mixer
        .as_ref()
        .map(|m| {
            m.members
                .iter()
                .map(|m| format!("{ME}.in{}", m.input))
                .collect()
        })
        .unwrap_or_default();
    let output_destinations: Vec<String> = outputs.iter().map(|o| o.api_id.clone()).collect();
    // The principal is charged once per set, but on every limiter whose
    // destinations it touches: a mixed set must not escape either the
    // mixer-patch principal budget or the output-take principal budget by
    // routing only part of its destinations through each limiter.
    let charge_mixer_principal = mixer.is_some();
    let charge_output_principal = !outputs.is_empty();
    let now = Instant::now();
    let control_limiter = state.output_routing.as_ref().map(|c| &c.limiter);
    if let Err(retry) = state.patch_limiter.check_set(
        &mixer_destinations,
        charge_mixer_principal.then_some(principal.key_id.as_str()),
        now,
    ) {
        return Ok(Err(too_many(retry)));
    }
    if !output_destinations.is_empty() {
        let Some(limiter) = control_limiter else {
            return Ok(Err(routing_unavailable()));
        };
        if let Err(retry) = limiter.check_set(
            &output_destinations,
            charge_output_principal.then_some(principal.key_id.as_str()),
            now,
        ) {
            return Ok(Err(too_many(retry)));
        }
    }
    // Both limiters passed their check; record on both now (never partially:
    // a check failure above returned before either limiter recorded).
    state.patch_limiter.record_set(
        &mixer_destinations,
        charge_mixer_principal.then_some(principal.key_id.as_str()),
        now,
    );
    if !output_destinations.is_empty() {
        // Existence of `control_limiter` was already proven above when
        // `output_destinations` is non-empty.
        if let Some(limiter) = control_limiter {
            limiter.record_set(
                &output_destinations,
                charge_output_principal.then_some(principal.key_id.as_str()),
                now,
            );
        }
    }
    // The set's NEW encoder delta (members with one encoder identity share
    // one) against the principal's cap and the global sessions (rule 1).
    let mut encoders: Vec<&String> = Vec::new();
    for key in outputs.iter().filter_map(|o| o.new_encoder.as_ref()) {
        if !encoders.contains(&key) {
            encoders.push(key);
        }
    }
    if global.is_none() {
        global = live_sessions(state);
    }
    if !delta_fits(state, principal, encoders.len(), global) {
        let (in_use, max) = global.unwrap_or((0, 0));
        return Ok(Err(refusal_problem(
            409,
            "capacity",
            if principal.is_global() {
                format!(
                    "This set needs {} new encoders; the encoder budget does not fit them \
                     ({in_use} of {max} sessions in use, or the caller's own cap)",
                    encoders.len()
                )
            } else {
                capacity_text(principal, in_use, max)
            },
        )));
    }
    // The key record and the pending slot in one atomic reservation,
    // registered before the enqueue so an outcome never beats it; every
    // failure below releases both.
    let op = match state.patch_sets.reserve(
        key,
        &digest,
        &principal.key_id,
        &SetMembers {
            outputs: outputs.iter().map(|o| o.api_id.clone()).collect(),
            mixer: mixer.is_some(),
        },
        Instant::now(),
    ) {
        SetReservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        SetReservation::Conflict => return Err(super::output_routing::key_reused()),
        SetReservation::Replay(op) => return Ok(Ok(op.to_string())),
        SetReservation::Fresh(op) => op,
    };
    let arm = (mixer.is_some() && !outputs.is_empty()).then(|| Arc::new(SetArm::new()));
    // The mixer half first: armed (with outputs) or a plain atomic patch.
    if let Some(mixer) = &mixer {
        let Some(handle) = state.switcher.as_ref() else {
            state.patch_sets.release(key, &op);
            return Ok(Err(problem(
                409,
                "switcher-unavailable",
                "no real Program/Preview switcher is attached to this run",
            )));
        };
        let leases = match super::patch::mixer_leases(
            state.source_demand.as_ref(),
            &mixer.members,
            mixer.snapshot.as_deref(),
        ) {
            Ok(leases) => leases,
            Err(response) => {
                state.patch_sets.release(key, &op);
                return Ok(Err(*response));
            }
        };
        let set = PatchSet {
            members: mixer
                .members
                .iter()
                .map(|m| (m.input, m.source.as_deref().map(SourceRef::new)))
                .collect(),
            allow_on_air: req.on_air == OnAir::Allow,
        };
        let verb = match &arm {
            Some(arm) => SwitcherVerb::PatchArmed {
                set,
                arm: Arc::clone(arm),
            },
            None => SwitcherVerb::Patch(set),
        };
        let command = SwitcherCommand {
            operation_id: op.to_string(),
            scene_version: mixer.scene_version,
            verb,
        };
        if !super::patch::admit_with_leases(state, handle, op.as_str(), leases, command) {
            state.patch_sets.release(key, &op);
            return Err(ControlError::EngineBusy);
        }
    }
    if !outputs.is_empty() {
        let Some(control) = state.output_routing.as_ref() else {
            if let Some(arm) = &arm {
                let _ = arm.refuse(SetRefusal::Busy);
            }
            state.patch_sets.release(key, &op);
            return Ok(Err(routing_unavailable()));
        };
        let members = outputs
            .iter()
            .map(|o| OutputTakeRequest {
                operation_id: op.to_string(),
                output: o.config_id.clone(),
                video: Some(o.feed.clone()),
                audio: o.audio_follow.then_some(RoutedAudio::Follow),
                layout: o.layout.clone(),
                principal: principal.key_id.clone(),
            })
            .collect();
        let set = OutputTakeSet {
            operation_id: op.to_string(),
            principal: principal.key_id.clone(),
            members,
            arm: arm.clone(),
        };
        if let Err(error) = control.routing.try_take_set(set) {
            // The switcher's armed half is dropped by the refusal (it
            // publishes its own refused outcome, which releases its leases);
            // nothing of the set changes.
            if let Some(arm) = &arm {
                let _ = arm.refuse(SetRefusal::Busy);
            }
            state.patch_sets.release(key, &op);
            return Ok(Err(match error {
                TakeSubmitError::Busy => ControlError::EngineBusy.into_response(),
                TakeSubmitError::Unroutable => unroutable_kind(),
            }));
        }
    }
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "patch",
        op.as_str(),
        Some(serde_json::json!({
            "command": "patch_take_set",
            "operation_id": op.to_string(),
            "mixer_inputs": mixer.as_ref().map_or(0, |m| m.members.len()),
            "outputs": outputs.iter().map(|o| Value::String(o.api_id.clone())).collect::<Vec<_>>(),
            "feeds": outputs.iter().map(|o| Value::String(o.feed.bus_name())).collect::<Vec<_>>(),
        })),
    );
    Ok(Ok(op.to_string()))
}

/// A stored patch salvo's crosspoints as a patch request.
#[must_use]
pub(crate) fn salvo_request(salvo: &multiview_config::Salvo) -> PatchRequest {
    PatchRequest {
        crosspoints: salvo
            .patch
            .iter()
            .map(|xp| PatchCrosspoint {
                plane: xp.plane.clone(),
                destination: super::patch::PatchDestination {
                    mixer: xp.destination.mixer.clone(),
                    input: xp.destination.input,
                    output: xp.destination.output.clone(),
                    slot: xp.destination.slot,
                },
                source: xp.source.to_json(),
            })
            .collect(),
        on_air: match salvo.on_air {
            multiview_config::PatchOnAir::Allow => OnAir::Allow,
            multiview_config::PatchOnAir::Protect => OnAir::Protect,
        },
    }
}

/// Store-time checks of a patch salvo (`PUT /salvos/{id}`, #308), after
/// every member's both ends were authorized: the shape of the set path,
/// every output id exists, every `{layout}` exists. Mixer inputs and
/// sources are checked by the document validation.
///
/// # Errors
/// 422 for a bad shape or an unknown output or layout.
pub(crate) fn validate_stored(
    state: &AppState,
    req: &PatchRequest,
    layouts: &[LayoutRead],
) -> ControlResult<()> {
    let refuse = |detail: String| Err(ControlError::Validation(format!("salvo patch: {detail}")));
    let Ok(shaped) = shape_set(req) else {
        return refuse(
            "a crosspoint has an unsupported shape (see POST /patch/plan for the typed refusal)"
                .into(),
        );
    };
    for (_, patch) in &shaped.outputs {
        if output_of(state, &patch.output)?.is_none() {
            return refuse(format!("unknown output {:?}", patch.output));
        }
        if let VideoFeed::Layout(id) = &patch.video {
            if layout_for(&patch.video, layouts)
                .and_then(|l| l.body.as_ref())
                .is_none()
            {
                return refuse(format!("unknown layout {id:?}"));
            }
        }
    }
    Ok(())
}
