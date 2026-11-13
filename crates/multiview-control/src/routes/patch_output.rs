//! Output slots in the patch panel (ADR-0110, output-routing §12.4, §12.8,
//! #342 B2): the `outputs` section of `GET /api/v1/patch`, and the output
//! crosspoints of `POST /patch/plan` and `POST /patch/take`.
//!
//! A request patches the slots of ONE output: its video slot 0 (`{source}`
//! or `{bus}`) and optionally audio slot 0 with `"follow"`, which rides the
//! video take. Other slots are fixed by config in this slice. The take goes
//! through [`super::output_routing::take_output_slot0`], the same machinery
//! as `POST /routing/output/take` (that route is the slot-0 alias).

use axum::response::Response;
use multiview_config::{OutputBus, VideoFeed};
use serde_json::{json, Value};

use super::output_routing::{
    capacity_text, config_id_of, resolve_feed, routing_unavailable, take_output_slot0, SlotTake,
};
use super::patch::{
    problem, PatchCrosspoint, PatchPlan, PatchRefusal, PatchRequest, PlannedCrosspoint, SlotCost,
};
use crate::auth::{authorize_output, authorize_scope, Principal};
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::output_routing::PlanRefusal;
use crate::state::AppState;
use multiview_events::AuthzScope;

/// Whether `principal` passes the output axis for `output` (the patch
/// route's destination check: `ObjectAndOutput` + `authorize_output`).
pub(crate) fn may_patch_output(principal: &Principal, output: &str) -> bool {
    authorize_scope(
        principal,
        AuthzScope::ObjectAndOutput {
            object: output,
            output,
        },
    )
    .is_ok()
        && authorize_output(principal, output).is_ok()
}

/// One validated output-slot request: the output and its video feed. Audio
/// slot 0 `"follow"` (when named) rides the video take exactly as the alias's
/// `audio: follow` does: the output keeps following its video.
pub(crate) struct OutputPatch {
    /// The output API id.
    pub output: String,
    /// Video slot 0's feed.
    pub video: VideoFeed,
}

/// Whether any crosspoint addresses an output slot.
pub(crate) fn has_output(req: &PatchRequest) -> bool {
    req.crosspoints
        .iter()
        .any(|xp| xp.destination.output.is_some() || xp.destination.slot.is_some())
}

fn video_feed(xp: &PatchCrosspoint) -> Result<VideoFeed, Box<Response>> {
    let refuse = |detail: &str| Box::new(problem(422, "unknown-source", detail.to_owned()));
    let Value::Object(map) = &xp.source else {
        return Err(refuse(
            "an output video slot takes {\"source\": id} or {\"bus\": \"program\"|\"preview\"|\"clean\"}",
        ));
    };
    if map.len() != 1 {
        return Err(refuse("a crosspoint source names exactly one feed"));
    }
    if let Some(id) = map.get("source").and_then(Value::as_str) {
        return Ok(VideoFeed::Source(id.to_owned()));
    }
    match map.get("bus").and_then(Value::as_str) {
        Some("program") => Ok(VideoFeed::Bus(OutputBus::Program)),
        Some("preview") => Ok(VideoFeed::Bus(OutputBus::Preview)),
        Some("clean") => Ok(VideoFeed::Bus(OutputBus::Clean)),
        Some(_) => Err(refuse("unknown bus; use program, preview or clean")),
        None if map.contains_key("layout") => Err(refuse(
            "layouts as output feeds arrive with layout pictures (#306)",
        )),
        None => Err(refuse(
            "an output video slot takes {\"source\": id} or {\"bus\": ...}",
        )),
    }
}

/// Shape of an output request: one output, video slot 0 (required) and
/// optionally audio slot 0 `follow`; nothing else.
pub(crate) fn shape(req: &PatchRequest) -> Result<OutputPatch, Box<Response>> {
    let mut output: Option<&str> = None;
    let mut video: Option<VideoFeed> = None;
    for xp in &req.crosspoints {
        let d = &xp.destination;
        let Some(this) = d.output.as_deref() else {
            // A mixer crosspoint beside an output one.
            return Err(Box::new(problem(
                422,
                "unknown-slot",
                super::patch::MULTI_DESTINATION,
            )));
        };
        if d.mixer.is_some() || d.input.is_some() {
            return Err(Box::new(problem(
                422,
                "unknown-slot",
                "a destination is a mixer input or an output slot, not both",
            )));
        }
        if output.is_some_and(|o| o != this) {
            return Err(Box::new(problem(
                422,
                "unknown-slot",
                super::patch::MULTI_DESTINATION,
            )));
        }
        output = Some(this);
        let slot = d.slot.unwrap_or(0);
        match xp.plane.as_str() {
            "video" => {
                if slot != 0 {
                    return Err(Box::new(problem(
                        422,
                        "unknown-slot",
                        format!("video slot {slot} does not exist: every output has one video slot (0) in this release"),
                    )));
                }
                if video.is_some() {
                    return Err(Box::new(problem(
                        422,
                        "unknown-slot",
                        "video slot 0 appears twice in one patch",
                    )));
                }
                video = Some(video_feed(xp)?);
            }
            "audio" => {
                if slot != 0 || xp.source != Value::String("follow".to_owned()) {
                    return Err(Box::new(problem(
                        422,
                        "slot-medium-mismatch",
                        "audio slots are fixed by the output's configuration in this release; \
                         only audio slot 0 with \"follow\" (it rides the video take) is patchable",
                    )));
                }
            }
            "subtitle" => {
                return Err(Box::new(problem(
                    422,
                    "slot-medium-mismatch",
                    "subtitle slots arrive with subtitle routing (#345)",
                )));
            }
            other => {
                return Err(Box::new(problem(
                    422,
                    "slot-medium-mismatch",
                    format!("unknown plane {other:?}; use video, audio or subtitle"),
                )));
            }
        }
    }
    let (Some(output), Some(video)) = (output, video) else {
        return Err(Box::new(problem(
            422,
            "unknown-slot",
            "an output patch needs its video slot 0 (audio slot 0 follow rides it)",
        )));
    };
    Ok(OutputPatch {
        output: output.to_owned(),
        video,
    })
}

fn refusal(kind: &str, detail: impl Into<String>) -> Option<PatchRefusal> {
    Some(PatchRefusal {
        kind: kind.to_owned(),
        detail: detail.into(),
    })
}

/// Plan an output patch (after role, count, authorization and shape).
pub(crate) fn plan(
    state: &AppState,
    principal: &Principal,
    req: &PatchRequest,
    patch: &OutputPatch,
) -> ControlResult<Result<PatchPlan, Response>> {
    let Some(control) = state.output_routing.as_ref() else {
        return Ok(Err(routing_unavailable()));
    };
    let Some(config_id) = config_id_of(state, &patch.output)? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: patch.output.clone(),
        });
    };
    let mut path = "P1";
    let mut cost = None;
    let mut refused = None;
    match resolve_feed(state, &patch.video)? {
        Err(_) => {
            refused = refusal(
                "unknown-source",
                "the feed does not exist or cannot be routed",
            );
        }
        Ok(feed) => match control.routing.plan_slot(&config_id, &feed) {
            Ok(planned) => {
                path = planned.path.as_str();
                let fits = planned.cost.fits();
                let global = principal.is_global();
                cost = Some(SlotCost {
                    new_encoders: u8::from(planned.cost.needs_encoder),
                    converts: planned.converts,
                    sessions_in_use: global.then_some(planned.cost.sessions_in_use),
                    sessions_max: global.then_some(planned.cost.sessions_max),
                    fits,
                });
                if !fits {
                    refused = refusal(
                        "capacity",
                        capacity_text(
                            principal,
                            planned.cost.sessions_in_use,
                            planned.cost.sessions_max,
                        ),
                    );
                }
            }
            Err(PlanRefusal::SwitcherRequired) => {
                refused = refusal(
                    "switcher-required",
                    "Preview and Clean need a switcher (M/E) in this run",
                );
            }
            Err(PlanRefusal::OutputKindUnroutable) => {
                refused = refusal(
                    "output-kind-unroutable",
                    "This output's kind cannot switch feeds live in this release",
                );
            }
        },
    }
    let mut crosspoints = Vec::with_capacity(req.crosspoints.len());
    for xp in &req.crosspoints {
        let video = xp.plane == "video";
        crosspoints.push(PlannedCrosspoint {
            destination: xp.destination.clone(),
            path: if video { path } else { "follow" },
            on_air: false,
            cost: if video { cost.clone() } else { None },
            refusal: if video { refused.clone() } else { None },
        });
    }
    Ok(Ok(PatchPlan {
        class: "class1",
        can_take: refused.is_none(),
        crosspoints,
    }))
}

/// Take an output patch through the slot-0 take machinery. `Ok(op)` is the
/// operation id to answer `202 {operation_id, status:"accepted"}` with (a
/// replay answers its own first operation).
pub(crate) fn take(
    state: &AppState,
    principal: &Principal,
    idem: &IdempotencyKey,
    patch: &OutputPatch,
) -> ControlResult<Result<String, Response>> {
    if idem.0.as_deref().is_none_or(|k| k.trim().is_empty()) {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    }
    Ok(
        match take_output_slot0(state, principal, idem, &patch.output, &patch.video)? {
            SlotTake::Accepted(op) => Ok(op),
            SlotTake::Replay(replay) => Ok(replay.operation_id.to_string()),
            SlotTake::Refused(response) => Err(response),
        },
    )
}

/// The `outputs` section of `GET /patch`: every output the caller passes the
/// output axis for, with its format, video slot 0 (from the engine readback;
/// a feed it cannot read is `{redacted:true}`) and audio slots (from config).
pub(crate) fn outputs_view(state: &AppState, principal: &Principal) -> ControlResult<Vec<Value>> {
    let resources = crate::boot_model::accepted_resources(state, "outputs")?
        .map_or_else(|| state.outputs.list(), Ok)?;
    let live = state
        .output_routing
        .as_ref()
        .and_then(|c| c.routing.snapshot());
    let redacted = json!({ "redacted": true });
    let mut out = Vec::new();
    for versioned in resources {
        let api_id = versioned.resource.id;
        if !may_patch_output(principal, &api_id) {
            continue;
        }
        let Ok(body) = serde_json::from_value::<multiview_config::Output>(versioned.resource.body)
        else {
            continue;
        };
        let readback = live.as_ref().and_then(|l| l.outputs.get(&body.id()));
        let feed_view = |bus: &str| {
            crate::output_routing::RoutedFeed::from_bus_name(bus)
                .map(|feed| feed.video_feed())
                .filter(|feed| super::routing::may_read_feed(principal, feed))
                .and_then(|feed| serde_json::to_value(feed).ok())
                .unwrap_or_else(|| redacted.clone())
        };
        let video_source = readback.map_or_else(|| feed_view("program"), |r| feed_view(&r.bus));
        let pending = readback
            .and_then(|r| r.pending.as_ref())
            .map(|(bus, held)| json!({ "source": feed_view(bus), "held_ticks": held }));
        let path = state
            .output_routing
            .as_ref()
            .and_then(|c| c.routing.current_path(&body.id()))
            .map_or("P1", crate::output_routing::PlanPath::as_str);
        let audio: Vec<Value> = match body.slots().map(|s| s.audio.as_slice()) {
            Some(slots) if !slots.is_empty() => slots
                .iter()
                .enumerate()
                .map(|(i, slot)| {
                    json!({
                        "slot": i,
                        "track": slot.track,
                        "language": slot.language,
                        "source": "follow",
                    })
                })
                .collect(),
            _ => vec![json!({ "slot": 0, "track": null, "language": null, "source": "follow" })],
        };
        out.push(json!({
            "output": api_id,
            "format": body.format(),
            "video": [{
                "slot": 0,
                "source": video_source,
                "path": path,
                "pending": pending,
                "degraded": readback.is_some_and(|r| r.degraded),
            }],
            "audio": audio,
        }));
    }
    Ok(out)
}
