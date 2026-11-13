//! Output slots in the patch panel (ADR-0110, output-routing §12.4, §12.8,
//! #342 B2): the `outputs` section of `GET /api/v1/patch`, and the output
//! crosspoints of `POST /patch/plan` and `POST /patch/take`.
//!
//! A request patches the slots of ONE output: its video slot 0 (`{source}`,
//! `{bus}` or `{layout}`: a stored layout, ADR-0108 slice 3) and optionally audio slot 0 with `"follow"`, which rides the
//! video take. Other slots are fixed by config in this slice. The take goes
//! through [`super::output_routing::take_output_slot0`], the same machinery
//! as `POST /routing/output/take` (that route is the slot-0 alias).

use axum::response::Response;
use multiview_config::{AudioFeed, OutputBus, VideoFeed};
use serde_json::{json, Value};

use super::output_routing::{
    capacity_text, config_id_of, resolve_feed, routing_unavailable, take_output_slot0, SlotTake,
};
use super::patch::{
    problem, PatchCrosspoint, PatchPlan, PatchRefusal, PatchRequest, PlannedCrosspoint, SlotCost,
};
use crate::auth::Principal;
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::output_routing::PlanRefusal;
use crate::state::AppState;

/// Whether `principal` passes the output end for `output`: object AND output
/// axis, the one destination check of the patch routes, the alias and
/// `GET /routing` ([`super::output_routing::authorize_output_end`]).
pub(crate) fn may_patch_output(principal: &Principal, output: &str) -> bool {
    super::output_routing::authorize_output_end(principal, output).is_ok()
}

/// One validated output-slot request: the output, its video feed and
/// whether audio slot 0 `"follow"` was named. Named, it rides the video take
/// exactly as the alias's `audio: "follow"` does (the output's audio follows
/// its video from then on, replacing an explicit choice); omitted, the
/// output keeps its current audio choice, as the alias without `audio`
/// (#342 review T-M1).
pub(crate) struct OutputPatch {
    /// The output API id.
    pub output: String,
    /// Video slot 0's feed.
    pub video: VideoFeed,
    /// Audio slot 0 `"follow"` was named in the patch.
    pub audio_follow: bool,
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
            "an output video slot takes {\"source\": id}, {\"layout\": id} or {\"bus\": \"program\"|\"preview\"|\"clean\"}",
        ));
    };
    if map.len() != 1 {
        return Err(refuse("a crosspoint source names exactly one feed"));
    }
    if let Some(id) = map.get("source").and_then(Value::as_str) {
        return Ok(VideoFeed::Source(id.to_owned()));
    }
    // A stored layout (ADR-0108 slice 3, #306): its id is authorized by the
    // patch route's `authorize`, its whole source closure by `resolve_feed`
    // (plan, take and a replay alike).
    if let Some(id) = map.get("layout").and_then(Value::as_str) {
        return Ok(VideoFeed::Layout(id.to_owned()));
    }
    match map.get("bus").and_then(Value::as_str) {
        Some("program") => Ok(VideoFeed::Bus(OutputBus::Program)),
        Some("preview") => Ok(VideoFeed::Bus(OutputBus::Preview)),
        Some("clean") => Ok(VideoFeed::Bus(OutputBus::Clean)),
        Some(_) => Err(refuse("unknown bus; use program, preview or clean")),
        None => Err(refuse(
            "an output video slot takes {\"source\": id}, {\"layout\": id} or {\"bus\": ...}",
        )),
    }
}

/// Shape of an output request: one output, video slot 0 (required) and
/// optionally audio slot 0 `follow`; nothing else.
pub(crate) fn shape(req: &PatchRequest) -> Result<OutputPatch, Box<Response>> {
    let mut output: Option<&str> = None;
    let mut video: Option<VideoFeed> = None;
    let mut audio_follow = false;
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
                if audio_follow {
                    return Err(Box::new(problem(
                        422,
                        "unknown-slot",
                        "audio slot 0 appears twice in one patch",
                    )));
                }
                audio_follow = true;
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
        audio_follow,
    })
}

fn refusal(kind: &str, detail: impl Into<String>) -> PatchRefusal {
    PatchRefusal {
        kind: kind.to_owned(),
        detail: detail.into(),
    }
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
    let mut planned_path = "P1";
    let mut cost = None;
    let mut refused: Option<PatchRefusal> = None;
    match resolve_feed(state, principal, &patch.video)? {
        Err((_, "unknown-feed", _)) => {
            refused = Some(refusal(
                "unknown-source",
                "the feed does not exist or cannot be routed",
            ));
        }
        // `layout-not-admissible` (another canvas, unsolvable, translucent).
        Err((_, kind, detail)) => refused = Some(refusal(kind, detail)),
        Ok((feed, _)) => match control.routing.plan_slot(&config_id, &feed) {
            Ok(planned) => {
                planned_path = planned.path.as_str();
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
                    refused = Some(refusal(
                        "capacity",
                        capacity_text(
                            principal,
                            planned.cost.sessions_in_use,
                            planned.cost.sessions_max,
                        ),
                    ));
                }
            }
            Err(PlanRefusal::SwitcherRequired) => {
                refused = Some(refusal(
                    "switcher-required",
                    "Preview and Clean need a switcher (M/E) in this run",
                ));
            }
            Err(PlanRefusal::OutputKindUnroutable) => {
                refused = Some(refusal(
                    "output-kind-unroutable",
                    "This output's kind cannot switch feeds live in this release",
                ));
            }
            Err(PlanRefusal::AudioUnsupported) => {
                refused = Some(refusal(
                    "audio-unsupported",
                    "This output cannot carry a routed audio feed",
                ));
            }
            Err(PlanRefusal::AuxCapacity { active, max }) => {
                refused = Some(refusal(
                    "aux-capacity",
                    super::output_routing::aux_capacity_text(principal, active, max),
                ));
            }
        },
    }
    let mut crosspoints = Vec::with_capacity(req.crosspoints.len());
    for xp in &req.crosspoints {
        let video = xp.plane == "video";
        crosspoints.push(PlannedCrosspoint {
            destination: xp.destination.clone(),
            path: if video { planned_path } else { "follow" },
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
        // Audio slot 0 `follow`, when named, rides the video take as the
        // alias's `audio: "follow"` (same engine request, same take record,
        // so the two routes replay each other); omitted, the output's audio
        // choice is kept (`None`), as the alias without `audio` (T-M1).
        match take_output_slot0(
            state,
            principal,
            idem,
            &patch.output,
            Some(&patch.video),
            patch.audio_follow.then_some(&AudioFeed::Follow),
        )? {
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
    // The accepted crosspoints (#342 review M3): audio slot 0 reports the
    // output's accepted audio feed exactly as `GET /routing` projects it.
    let config: multiview_config::MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))?;
    let feeds = config.output_feeds();
    let live = state
        .output_routing
        .as_ref()
        .and_then(|c| c.routing.snapshot());
    let redacted = json!({ "redacted": true });
    // A layout feed is readable only with Read on the layout AND every
    // source it shows (§6.3), exactly as `GET /routing` redacts it.
    let layouts = super::routing::layout_sources(state)?;
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
                .filter(|feed| super::routing::may_read_feed(principal, &layouts, feed))
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
        // Slot 0 carries the accepted audio feed (default `follow`, redacted
        // per GET /routing's rule); slots i > 0 their configured track.
        let accepted_audio =
            super::routing::audio_feed_view(principal, &config, &feeds, &body.id());
        let audio: Vec<Value> = match body.slots().map(|s| s.audio.as_slice()) {
            Some(slots) if !slots.is_empty() => slots
                .iter()
                .enumerate()
                .map(|(i, slot)| {
                    let source = if i == 0 {
                        accepted_audio.clone()
                    } else {
                        json!({ "track": slot.track })
                    };
                    json!({
                        "slot": i,
                        "track": slot.track,
                        "language": slot.language,
                        "source": source,
                    })
                })
                .collect(),
            _ => vec![
                json!({ "slot": 0, "track": null, "language": null, "source": accepted_audio }),
            ],
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
