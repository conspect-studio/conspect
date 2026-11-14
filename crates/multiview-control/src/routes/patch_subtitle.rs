//! Subtitle slots in the patch panel (slice F #345 part F2, ADR-0110 §12.3,
//! §12.8): the `subtitle` plane of `POST /patch/plan` and `POST /patch/take`,
//! and the `subtitle` list of each output in `GET /api/v1/patch`.
//!
//! A subtitle request carries ONLY subtitle crosspoints of ONE output, each
//! slot at most once, every source `{"source": id}` or `"none"`. The patch
//! route has already checked the role, the count and both ends (the output
//! end on both axes via `authorize_output_end`, Read on every named source)
//! BEFORE anything here answers, so no 404/422 is an oracle (#342 review
//! T-A1 ordering).
//!
//! A take shares the slot-0 take machinery's state: the take-scoped
//! idempotency store under the same key namespace (a key is bound to the
//! whole take: output + slot→source map; another body is 409 `conflict`),
//! and the per-output 2/s and per-principal 10/s limiter (one request = one
//! take). The engine re-points the output's route table wait-free (an RCU
//! swap, no channel); the off-clock sampler reads it at its next tick, so
//! the re-patch lands on one tick (the first sampled tick after the
//! reported one; the consumer may shed ticks, so no tick is promised). The
//! swap and its queued outcome share one serialization with a bounded,
//! single-consumer persistence queue: a full queue refuses the take before
//! any route changes; the consumer persists the latest slot sources (in
//! swap order) under the config-mutation lock, then publishes the outcomes
//! on `route.output` (`subtitle:applied`, `subtitle:refused:not-in-run`).

use std::time::Instant;

use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use super::output_routing::{routing_unavailable, slot0_key_of};
use super::patch::{problem, PatchPlan, PatchRefusal, PatchRequest, PlannedCrosspoint};
use crate::auth::Principal;
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::output_routing::{PendingTake, SubtitleAdmission, SubtitleTakeRequest, TakeReservation};
use crate::state::AppState;

/// The plan note of a source that declares no `captions`: not a refusal,
/// the slot then carries an empty track.
pub(crate) const NOTE_NO_CAPTIONS: &str = "source-has-no-captions";

const ONE_OUTPUT: &str = "a subtitle patch carries only the subtitle crosspoints of one output; \
     multi-destination patch sets arrive with patch salvos (#308)";

/// One validated subtitle request: the output API id and its slot → source
/// map (sorted by slot; `None` = unpatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubtitlePatch {
    /// The output API id.
    pub output: String,
    /// Slot → new source, sorted by slot, each slot once.
    pub slots: Vec<(usize, Option<String>)>,
}

/// Whether any crosspoint is on the `subtitle` plane.
pub(crate) fn has_subtitle(req: &PatchRequest) -> bool {
    req.crosspoints.iter().any(|xp| xp.plane == "subtitle")
}

fn refuse(kind: &str, detail: impl Into<String>) -> Response {
    problem(422, kind, detail)
}

/// Shape of a subtitle request (purely syntactic; runs after authorization).
pub(crate) fn shape(req: &PatchRequest) -> Result<SubtitlePatch, Box<Response>> {
    let mut output: Option<&str> = None;
    let mut slots: Vec<(usize, Option<String>)> = Vec::with_capacity(req.crosspoints.len());
    for xp in &req.crosspoints {
        let d = &xp.destination;
        if xp.plane != "subtitle" || d.mixer.is_some() || d.input.is_some() {
            return Err(Box::new(refuse("unknown-slot", ONE_OUTPUT)));
        }
        let Some(this) = d.output.as_deref() else {
            return Err(Box::new(refuse(
                "unknown-slot",
                "a subtitle destination is {\"output\", \"slot\"}",
            )));
        };
        if output.is_some_and(|o| o != this) {
            return Err(Box::new(refuse("unknown-slot", ONE_OUTPUT)));
        }
        output = Some(this);
        let slot = usize::try_from(d.slot.unwrap_or(0)).unwrap_or(usize::MAX);
        if slots.iter().any(|(s, _)| *s == slot) {
            return Err(Box::new(refuse(
                "unknown-slot",
                format!("subtitle slot {slot} appears twice in one patch"),
            )));
        }
        let source = match &xp.source {
            Value::String(none) if none == "none" => None,
            Value::Object(map) if map.len() == 1 => {
                if let Some(id) = map.get("source").and_then(Value::as_str) {
                    Some(id.to_owned())
                } else if map.contains_key("layout") || map.contains_key("bus") {
                    return Err(Box::new(refuse(
                        "slot-medium-mismatch",
                        "a subtitle slot carries a source's captions: use {\"source\": id} or \
                         \"none\" (a layout or a bus has no subtitle essence)",
                    )));
                } else {
                    return Err(Box::new(refuse(
                        "unknown-source",
                        "a subtitle slot takes {\"source\": id} or \"none\"",
                    )));
                }
            }
            _ => {
                return Err(Box::new(refuse(
                    "unknown-source",
                    "a subtitle slot takes {\"source\": id} or \"none\"",
                )))
            }
        };
        slots.push((slot, source));
    }
    let Some(output) = output else {
        return Err(Box::new(refuse("unknown-slot", "no subtitle crosspoint")));
    };
    slots.sort_by_key(|(slot, _)| *slot);
    Ok(SubtitlePatch {
        output: output.to_owned(),
        slots,
    })
}

/// The accepted output behind an API id and the accepted sources.
fn lookup(
    state: &AppState,
    api_id: &str,
) -> ControlResult<Option<(multiview_config::Output, multiview_config::MultiviewConfig)>> {
    let resources = crate::boot_model::accepted_resources(state, "outputs")?
        .map_or_else(|| state.outputs.list(), Ok)?;
    let Some(versioned) = resources.into_iter().find(|v| v.resource.id == api_id) else {
        return Ok(None);
    };
    let output: multiview_config::Output = serde_json::from_value(versioned.resource.body)
        .map_err(|e| ControlError::Repository(format!("output {api_id:?} body: {e}")))?;
    let config: multiview_config::MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))?;
    Ok(Some((output, config)))
}

/// Per slot: an unknown source (`Some(refusal)`) or the captions note.
struct Checked {
    config_id: String,
    /// Per request slot, in `patch.slots` order.
    per_slot: Vec<(Option<PatchRefusal>, Option<&'static str>)>,
}

/// Existence and capability (after authorization and shape). `Err` is the
/// response (404 unknown output, 422 slot refusals).
fn check(state: &AppState, patch: &SubtitlePatch) -> ControlResult<Result<Checked, Response>> {
    let Some((output, config)) = lookup(state, &patch.output)? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: patch.output.clone(),
        });
    };
    if output.slot_capability().subtitle_max == 0 {
        return Ok(Err(problem(
            422,
            "slot-medium-mismatch",
            "this output's medium carries no subtitle slots",
        )));
    }
    let declared = output.slots().map_or(0, |s| s.subtitle.len());
    if let Some((slot, _)) = patch.slots.iter().find(|(slot, _)| *slot >= declared) {
        return Ok(Err(problem(
            422,
            "unknown-slot",
            format!(
                "subtitle slot {slot} does not exist: this output declares {declared} subtitle \
                 slot(s) (outputs.slots.subtitle)"
            ),
        )));
    }
    let per_slot = patch
        .slots
        .iter()
        .map(|(_, source)| {
            let Some(id) = source else {
                return (None, None);
            };
            match config.sources.iter().find(|s| s.id == *id) {
                None => (
                    Some(PatchRefusal {
                        kind: "unknown-source".to_owned(),
                        detail: "the source does not exist".to_owned(),
                    }),
                    None,
                ),
                Some(source) => {
                    let captions = !matches!(
                        source.captions,
                        None | Some(multiview_config::schema::CaptionSelector::Off)
                    );
                    (None, (!captions).then_some(NOTE_NO_CAPTIONS))
                }
            }
        })
        .collect();
    Ok(Ok(Checked {
        config_id: output.id(),
        per_slot,
    }))
}

/// Plan a subtitle patch: no change. Unknown sources are plan refusals; a
/// source without captions is a note.
pub(crate) fn plan(
    state: &AppState,
    req: &PatchRequest,
    patch: &SubtitlePatch,
) -> ControlResult<Result<PatchPlan, Response>> {
    if state.output_routing.is_none() {
        return Ok(Err(routing_unavailable()));
    }
    let checked = match check(state, patch)? {
        Ok(checked) => checked,
        Err(response) => return Ok(Err(response)),
    };
    let mut crosspoints = Vec::with_capacity(req.crosspoints.len());
    for xp in &req.crosspoints {
        let slot = usize::try_from(xp.destination.slot.unwrap_or(0)).unwrap_or(usize::MAX);
        let index = patch.slots.iter().position(|(s, _)| *s == slot);
        let (refusal, note) = index
            .and_then(|i| checked.per_slot.get(i).cloned())
            .unwrap_or((None, None));
        crosspoints.push(PlannedCrosspoint {
            destination: xp.destination.clone(),
            path: "subtitle",
            on_air: false,
            cost: None,
            refusal,
            note,
        });
    }
    let can_take = crosspoints.iter().all(|c| c.refusal.is_none());
    Ok(Ok(PatchPlan {
        class: "class1",
        can_take,
        crosspoints,
    }))
}

/// Take a subtitle patch. `Ok(op)` is the operation id to answer `202
/// {operation_id, status:"accepted"}` with (a replay answers its own first
/// operation).
///
/// # Errors
/// A missing key (422), an unknown output (404), a key reused for another
/// take (409), `idempotency-capacity`, repository errors.
pub(crate) fn take(
    state: &AppState,
    principal: &Principal,
    idem: &IdempotencyKey,
    patch: &SubtitlePatch,
) -> ControlResult<Result<String, Response>> {
    if idem.0.as_deref().is_none_or(|k| k.trim().is_empty()) {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    }
    let Some(control) = state.output_routing.clone() else {
        return Ok(Err(routing_unavailable()));
    };
    // The slot-0 take store's namespace: one key, one take, across every
    // output-take route (#342 B2). The caller re-authorized both ends under
    // its current grants, so a replay of a revoked caller is already 403.
    let key = slot0_key_of(idem)?;
    let pending = PendingTake {
        api_id: patch.output.clone(),
        feed: None,
        audio: None,
        subtitles: Some(patch.slots.clone()),
    };
    if let Some((replay, recorded)) = control.replay_with_take(&key) {
        if recorded != pending {
            return Err(key_reused());
        }
        return Ok(Ok(replay.operation_id.to_string()));
    }
    let checked = match check(state, patch)? {
        Ok(checked) => checked,
        Err(response) => return Ok(Err(response)),
    };
    if checked.per_slot.iter().any(|(r, _)| r.is_some()) {
        // The video take's mapping of an unknown feed.
        return Ok(Err(problem(
            422,
            "unknown-feed",
            "the source does not exist",
        )));
    }
    // Limits key on existing outputs only (one request = one take).
    let admitted_at = Instant::now();
    if let Err(retry) = control
        .limiter
        .admit(&patch.output, &principal.key_id, admitted_at)
    {
        return Ok(Err(super::output_routing::too_many_pub(retry)));
    }
    let op = match control.reserve_take(&key, &principal.key_id, pending) {
        TakeReservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        TakeReservation::Conflict => return Err(key_reused()),
        TakeReservation::Replay(replay) => return Ok(Ok(replay.operation_id.to_string())),
        TakeReservation::Fresh(op) => op,
    };
    let request = SubtitleTakeRequest {
        operation_id: op.to_string(),
        output: checked.config_id.clone(),
        slots: patch.slots.clone(),
        principal: principal.key_id.clone(),
    };
    // Persistence capacity is reserved, the engine swapped and the outcome
    // queued under ONE serialization, so the persisted order is the swap
    // order and a full queue refuses before any route changes (review
    // COR-1, AUZ-1/2, INV-3/4).
    control.ensure_subtitle_worker(state);
    let outcome = match control.take_subtitles(request, &patch.output) {
        SubtitleAdmission::Queued(outcome) => outcome,
        SubtitleAdmission::Busy => {
            control.release_take(&key, &op);
            control
                .limiter
                .refund(&patch.output, &principal.key_id, admitted_at);
            return Ok(Err(ControlError::EngineBusy.into_response()));
        }
        SubtitleAdmission::Full => {
            control.release_take(&key, &op);
            control
                .limiter
                .refund(&patch.output, &principal.key_id, admitted_at);
            return Ok(Err(problem(
                503,
                "engine-busy",
                "subtitle re-patches are waiting to be persisted; nothing was changed, retry",
            )));
        }
    };
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "output",
        &patch.output,
        Some(json!({
            "command": "route_output_subtitles",
            "slots": patch.slots,
            "operation_id": op.to_string(),
            "outcome": outcome.outcome,
        })),
    );
    Ok(Ok(op.to_string()))
}

fn key_reused() -> ControlError {
    ControlError::Conflict("Idempotency-Key reused with a different request".into())
}

/// The `subtitle` list of an output in `GET /patch`: from the engine's live
/// route table when the run samples it (`live: true`), else from the
/// accepted config (`live: false`). A source the caller cannot read is
/// `{redacted: true}`. Decided per slot: a slot the run's table lacks is
/// from config.
pub(crate) fn output_view(
    state: &AppState,
    principal: &Principal,
    output: &multiview_config::Output,
) -> Vec<Value> {
    let Some(slots) = output.slots().map(|s| s.subtitle.as_slice()) else {
        return Vec::new();
    };
    if slots.is_empty() {
        return Vec::new();
    }
    let live = state
        .output_routing
        .as_ref()
        .and_then(|c| c.routing.subtitle_routes(&output.id()));
    slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            // A slot beyond the run's (shorter) table is not live: its
            // configured source, `live: false` (review COR-2).
            let entry = live.as_ref().and_then(|table| table.get(i));
            let is_live = entry.is_some();
            let source = match entry {
                Some(live_source) => live_source.clone(),
                None => slot.source.clone(),
            };
            let source = source.map_or(Value::Null, |id| {
                if crate::auth::authorize_object(principal, &id).is_ok() {
                    json!({ "source": id })
                } else {
                    json!({ "redacted": true })
                }
            });
            json!({
                "slot": i,
                "format": slot.format.as_str(),
                "language": slot.language,
                "name": slot.name,
                "live": is_live,
                "source": source,
            })
        })
        .collect()
}
