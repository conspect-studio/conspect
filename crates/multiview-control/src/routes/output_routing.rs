//! `POST /api/v1/routing/plan` (output target) and
//! `POST /api/v1/routing/output/take` (ADR-0108 slice 1, design §6.2-§6.4).
//!
//! Order of checks on both routes, so nothing leaks and nothing is burned:
//! role → destination `authorize_object(output)` → feed authorization
//! (source: `authorize_object(source)`; bus: Program scope + `me1`) — all
//! **before** existence, so an id the caller may not read is a 403 whether or
//! not it exists. Then the take checks the `Idempotency-Key` (a replay answers
//! from the take's own record), the rate limits (429 + `Retry-After`),
//! existence (404 / `unknown-feed`), and only then reserves the key in the
//! take store and enqueues (`try_send`; full → 503 `engine-busy`, the
//! reservation released).
//!
//! The take's replay decision is owned by
//! [`crate::output_routing::OutputRoutingControl`]'s take store, never by the
//! process-global [`crate::concurrency::IdempotencyStore`]: other routes
//! `release`/`rebind` keys there, and a take's replay must not depend on them.

use std::time::Instant;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::{AudioFeed, VideoFeed};
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::output_routing::{
    OutputTakeRequest, PendingTake, PlanRefusal, RoutedAudio, RoutedFeed, TakeReplay,
    TakeReservation, TakeSubmitError,
};
use crate::problem::Problem;
use crate::state::AppState;

/// The one M/E slice 1 declares (bus feeds require its object grant, §6.3).
pub const ME_ID: &str = "me1";

/// The output end of an output crosspoint (`kind = "output"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct OutputTarget {
    /// The output API id (the id `/api/v1/outputs/{id}` addresses).
    pub output: String,
    /// The stream re-pointed: `video` (the video feed, with an optional
    /// audio feed alongside) or `audio` (an audio-only take: the video is
    /// untouched).
    pub stream: OutputStream,
}

/// The stream of an output a take re-points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// The output's video feed (`feed` required, `audio` optional).
    Video,
    /// The output's audio feed only (`audio` required, no `feed`).
    Audio,
}

/// The target of an output crosspoint, tagged on `kind` (never untagged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputTargetKind {
    /// An output's feed.
    Output(OutputTarget),
}

/// The body of an output plan / take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct OutputRouteRequest {
    /// The output and stream.
    pub target: OutputTargetKind,
    /// The video feed: `{"bus":"program"|"preview"|"clean"}` or
    /// `{"source":"vt1"}`. Required with `stream: video`; absent with
    /// `stream: audio`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub feed: Option<VideoFeed>,
    /// The audio feed, externally tagged like the config `audio`:
    /// `"follow" | "program" | "none" | {"source":"<id>"} |
    /// {"tracks":["<name>"]}`. Required with `stream: audio` (an audio-only
    /// take); optional with `stream: video` (audio taken together with the
    /// video; absent keeps the output's audio choice). One track at most:
    /// the routed audio rides the output's single program-audio slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub audio: Option<AudioFeed>,
}

impl OutputRouteRequest {
    fn output(&self) -> &str {
        match &self.target {
            OutputTargetKind::Output(t) => &t.output,
        }
    }

    fn stream(&self) -> OutputStream {
        match &self.target {
            OutputTargetKind::Output(t) => t.stream,
        }
    }

    /// The video feed when the request is well formed (`Ok(None)`: an
    /// audio-only take); `Err` is the 422 detail.
    fn shape(&self) -> ControlResult<Option<&VideoFeed>> {
        match (self.stream(), &self.feed, &self.audio) {
            (OutputStream::Video, Some(feed), _) => Ok(Some(feed)),
            (OutputStream::Video, None, _) => Err(ControlError::Validation(
                "stream \"video\" requires `feed` (the video feed)".to_owned(),
            )),
            (OutputStream::Audio, None, Some(_)) => Ok(None),
            (OutputStream::Audio, Some(_), _) => Err(ControlError::Validation(
                "stream \"audio\" takes no `feed`; send the audio feed in `audio` (the video is untouched)"
                    .to_owned(),
            )),
            (OutputStream::Audio, None, None) => Err(ControlError::Validation(
                "stream \"audio\" requires `audio` (the audio feed)".to_owned(),
            )),
        }
    }
}

/// The cost of an output take (§6.2, §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputPlanCost {
    /// `0` when an existing encoder carries the feed, `1` for a new one.
    pub new_encoders: u8,
    /// `"N of M in use"` — whole-system principals only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_sessions: Option<String>,
    /// Whether the take fits the encoder budget now.
    pub fits: bool,
    /// `1` when the take changes the output's audio and the output has no
    /// private audio bus yet (+1 transient CPU AAC encode), else `0`.
    #[serde(default)]
    pub audio_encoders: u8,
    /// Operator text of the audio cost, e.g. "+1 audio encode (transient,
    /// CPU)"; absent when the take costs no audio encode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_cost: Option<String>,
}

/// A typed plan refusal (RFC 9457 `type` suffix + operator text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputPlanRefusal {
    /// `capacity` | `unknown-feed` | `switcher-required` |
    /// `output-kind-unroutable` | `audio-unsupported`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Operator-facing text (never names an encoder group).
    pub detail: String,
}

/// The `200` body of an output plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputPlan {
    /// Always `class1`: a feed change never changes the format (§4.2).
    pub class: crate::routing::RouteClass,
    /// The predicted cost.
    pub cost: OutputPlanCost,
    /// Refusals the take would meet (empty = takeable).
    pub refusals: Vec<OutputPlanRefusal>,
}

/// The `202` body of an output take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputTakeAccepted {
    /// Correlates the `route.output` outcome.
    pub operation_id: String,
    /// `accepted` (this request enqueued the take), `pending` (a replay of a
    /// reserved key whose outcome is not known yet) or `settled` (a replay
    /// carrying the recorded outcome).
    pub status: String,
    /// When the engine adopts the take: `next_tick`. Only on the request
    /// that enqueued it; a replay never claims a new adoption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopt: Option<String>,
    /// The recorded terminal outcome, on a replay once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The tick of the recorded outcome, on a replay once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<u64>,
}

/// Operator text of a capacity refusal (§6.3): counts only for whole-system
/// principals; never names a group or a shared encoder.
fn capacity_text(principal: &Principal, in_use: usize, max: usize) -> String {
    if principal.is_global() {
        format!(
            "This output needs its own encoder and the GPU has no free encoder session ({in_use} of {max} in use)"
        )
    } else {
        "No free encoder session for this output".to_owned()
    }
}

/// Program scope + the M/E grant: the rule for every switcher-bus feed.
fn authorize_bus(principal: &Principal) -> ControlResult<()> {
    crate::auth::authorize_scope(
        principal,
        multiview_events::AuthzScope::Program(multiview_config::MAIN_PROGRAM),
    )?;
    crate::auth::authorize_object(principal, ME_ID)
}

/// Authorize both ends (§6.3), before any existence check: the output, the
/// video feed when present, and the audio feed's own end (`source:x` → x;
/// `program` → the bus rule; `tracks` → each track name, and each track's
/// route input once resolved by [`resolve_audio`]; `none`/`follow` →
/// nothing beyond the video end).
fn authorize_ends(principal: &Principal, req: &OutputRouteRequest) -> ControlResult<()> {
    crate::auth::authorize_object(principal, req.output())?;
    match &req.feed {
        None => {}
        Some(VideoFeed::Source(id) | VideoFeed::Layout(id)) => {
            crate::auth::authorize_object(principal, id)?;
        }
        Some(VideoFeed::Bus(_)) => authorize_bus(principal)?,
        Some(_) => return Err(ControlError::Forbidden("unsupported feed kind".to_owned())),
    }
    match &req.audio {
        None | Some(AudioFeed::Follow | AudioFeed::None) => {}
        Some(AudioFeed::Program) => authorize_bus(principal)?,
        Some(AudioFeed::Source(id)) => crate::auth::authorize_object(principal, id)?,
        Some(AudioFeed::Tracks(tracks)) => {
            for track in tracks {
                crate::auth::authorize_object(principal, track)?;
            }
        }
        Some(_) => {
            return Err(ControlError::Forbidden(
                "unsupported audio feed kind".to_owned(),
            ))
        }
    }
    Ok(())
}

fn routing_unavailable() -> Response {
    Problem::new(409, "routing-unavailable", "Output routing unavailable")
        .with_detail("this run does not route output feeds")
        .into_response()
}

fn refusal_problem(status: u16, kind: &str, detail: String) -> Response {
    Problem::new(status, kind, "Take refused")
        .with_detail(detail)
        .into_response()
}

/// The accepted output body behind an API id (`None` = unknown).
fn output_of(state: &AppState, api_id: &str) -> ControlResult<Option<multiview_config::Output>> {
    let resources = crate::boot_model::accepted_resources(state, "outputs")?
        .map_or_else(|| state.outputs.list(), Ok)?;
    for versioned in resources {
        if versioned.resource.id == api_id {
            let body: multiview_config::Output = serde_json::from_value(versioned.resource.body)
                .map_err(|e| ControlError::Repository(format!("output {api_id:?} body: {e}")))?;
            return Ok(Some(body));
        }
    }
    Ok(None)
}

fn desired(state: &AppState) -> ControlResult<multiview_config::MultiviewConfig> {
    serde_json::from_value(crate::boot_model::current_desired_config(state)?)
        .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))
}

/// Whether `id` is a source the accepted document declares.
fn source_exists(state: &AppState, id: &str) -> ControlResult<bool> {
    Ok(desired(state)?.sources.iter().any(|s| s.id == id))
}

/// The route `input_id` of the discrete track `name` the accepted
/// `[audio].routes[].target_track` declares (`None` = no such track).
pub(crate) fn track_input(
    config: &multiview_config::MultiviewConfig,
    name: &str,
) -> Option<String> {
    config
        .audio
        .as_ref()?
        .routes
        .iter()
        .find(|r| r.target_track.as_deref() == Some(name))
        .map(|r| r.input_id.clone())
}

const ONE_TRACK: &str = "This output carries one routed audio feed; choose one track";
const NO_AUDIO_SLOT: &str = "This output cannot carry a routed audio feed";
const NO_TRACKS: &str = "This output's transport cannot select a discrete audio track";

/// The audio feed as a routed choice, after authorizing each track's route
/// input and the existence/capability checks: `Err` is the typed refusal
/// response (`unknown-feed` 422, `audio-unsupported` 422 for more than one
/// track, `audio-unsupported` 409 for a transport without tracks), with its
/// plan refusal twin. Track inputs are authorized earlier, by
/// [`authorize_track_inputs`].
fn resolve_audio(
    state: &AppState,
    output: &multiview_config::Output,
    audio: &AudioFeed,
) -> ControlResult<Result<RoutedAudio, (Response, OutputPlanRefusal)>> {
    let refused = |status: u16, kind: &str, detail: &str| {
        Ok(Err((
            refusal_problem(status, kind, detail.to_owned()),
            OutputPlanRefusal {
                kind: kind.to_owned(),
                detail: detail.to_owned(),
            },
        )))
    };
    Ok(Ok(match audio {
        AudioFeed::Follow => RoutedAudio::Follow,
        AudioFeed::Program => RoutedAudio::Program,
        AudioFeed::None => RoutedAudio::None,
        AudioFeed::Source(id) => {
            if !source_exists(state, id)? {
                return refused(422, "unknown-feed", "the audio source does not exist");
            }
            RoutedAudio::Source(id.clone())
        }
        AudioFeed::Tracks(tracks) => {
            let [name] = tracks.as_slice() else {
                return refused(422, "audio-unsupported", ONE_TRACK);
            };
            let config = desired(state)?;
            let Some(input) = track_input(&config, name) else {
                return refused(422, "unknown-feed", "the audio track does not exist");
            };
            if output.audio_capability().delivery == multiview_config::TrackDelivery::None {
                return refused(409, "audio-unsupported", NO_TRACKS);
            }
            RoutedAudio::Track {
                name: name.clone(),
                input,
            }
        }
        _ => return refused(422, "unknown-feed", "this audio feed cannot be routed"),
    }))
}

/// The second half of a `tracks` audio feed's authorization (§6.3): a track
/// carries its route input's audio, so Read on that input too. Runs right
/// after [`authorize_ends`] (which authorized every track name), before any
/// rate limit or existence answer; an undeclared track is left to existence.
fn authorize_track_inputs(
    state: &AppState,
    principal: &Principal,
    req: &OutputRouteRequest,
) -> ControlResult<()> {
    let Some(AudioFeed::Tracks(tracks)) = &req.audio else {
        return Ok(());
    };
    let config = desired(state)?;
    for track in tracks {
        if let Some(input) = track_input(&config, track) {
            crate::auth::authorize_object(principal, &input)?;
        }
    }
    Ok(())
}

/// Operator text of an audio cost.
fn audio_cost_text(needs: bool) -> Option<String> {
    needs.then(|| "+1 audio encode (transient, CPU)".to_owned())
}

/// The feed as a routed feed (Program, Preview, Clean or a source), after existence checks: `Err` is the
/// typed refusal response (`unknown-feed`, `output-kind-unroutable`).
fn resolve_feed(state: &AppState, feed: &VideoFeed) -> ControlResult<Result<RoutedFeed, Response>> {
    let Some(routed) = RoutedFeed::of(feed) else {
        return Ok(Err(refusal_problem(
            422,
            "unknown-feed",
            "this feed cannot be routed to an output in this release".to_owned(),
        )));
    };
    if let RoutedFeed::Source(id) = &routed {
        if !source_exists(state, id)? {
            return Ok(Err(refusal_problem(
                422,
                "unknown-feed",
                "the feed does not exist".to_owned(),
            )));
        }
    }
    Ok(Ok(routed))
}

/// `POST /api/v1/routing/plan` for an output target.
pub(crate) fn plan_output(
    state: &AppState,
    principal: &Principal,
    req: &OutputRouteRequest,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    authorize_ends(principal, req)?;
    authorize_track_inputs(state, principal, req)?;
    let video = req.shape()?;
    let Some(control) = state.output_routing.as_ref() else {
        return Ok(routing_unavailable());
    };
    let Some(output) = output_of(state, req.output())? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: req.output().to_owned(),
        });
    };
    let config_id = output.id();
    let mut refusals = Vec::new();
    let mut cost = OutputPlanCost {
        new_encoders: 0,
        encoder_sessions: None,
        fits: true,
        audio_encoders: 0,
        audio_cost: None,
    };
    let mut routed_video = None;
    if let Some(feed) = video {
        match resolve_feed(state, feed)? {
            Err(_) => refusals.push(OutputPlanRefusal {
                kind: "unknown-feed".to_owned(),
                detail: "the feed does not exist or cannot be routed".to_owned(),
            }),
            Ok(feed) => {
                plan_video(
                    control,
                    principal,
                    &config_id,
                    &feed,
                    &mut cost,
                    &mut refusals,
                );
                routed_video = Some(feed);
            }
        }
    }
    let routed_audio = match &req.audio {
        None => None,
        Some(audio) => match resolve_audio(state, &output, audio)? {
            Ok(audio) => Some(audio),
            Err((_, refusal)) => {
                refusals.push(refusal);
                None
            }
        },
    };
    // The audio cost of what the take would carry (an unresolvable audio
    // feed was already refused above).
    if routed_audio.is_some() || (req.audio.is_none() && routed_video.is_some()) {
        match control
            .routing
            .plan_audio(&config_id, routed_video.as_ref(), routed_audio.as_ref())
        {
            Ok(audio) => {
                cost.audio_encoders = u8::from(audio.needs_encoder);
                cost.audio_cost = audio_cost_text(audio.needs_encoder);
            }
            Err(PlanRefusal::AudioUnsupported) => refusals.push(OutputPlanRefusal {
                kind: "audio-unsupported".to_owned(),
                detail: NO_AUDIO_SLOT.to_owned(),
            }),
            Err(PlanRefusal::OutputKindUnroutable) if video.is_none() => {
                refusals.push(OutputPlanRefusal {
                    kind: "output-kind-unroutable".to_owned(),
                    detail: "This output's kind cannot switch feeds live in this release"
                        .to_owned(),
                });
            }
            // A video refusal was already reported by the video plan.
            Err(_) => {}
        }
    }
    let plan = OutputPlan {
        class: crate::routing::RouteClass::Class1,
        cost,
        refusals,
    };
    Ok((StatusCode::OK, Json(plan)).into_response())
}

/// The video half of a plan: encoder cost and the video refusals.
fn plan_video(
    control: &crate::output_routing::OutputRoutingControl,
    principal: &Principal,
    config_id: &str,
    feed: &RoutedFeed,
    cost: &mut OutputPlanCost,
    refusals: &mut Vec<OutputPlanRefusal>,
) {
    match control.routing.plan(config_id, feed) {
        Ok(planned) => {
            cost.new_encoders = u8::from(planned.needs_encoder);
            cost.fits = planned.fits();
            if principal.is_global() {
                cost.encoder_sessions = Some(format!(
                    "{} of {} in use",
                    planned.sessions_in_use, planned.sessions_max
                ));
            }
            if !cost.fits {
                refusals.push(OutputPlanRefusal {
                    kind: "capacity".to_owned(),
                    detail: capacity_text(principal, planned.sessions_in_use, planned.sessions_max),
                });
            }
        }
        Err(PlanRefusal::SwitcherRequired) => refusals.push(OutputPlanRefusal {
            kind: "switcher-required".to_owned(),
            detail: "Preview and Clean need a switcher (M/E) in this run".to_owned(),
        }),
        Err(PlanRefusal::OutputKindUnroutable) => refusals.push(OutputPlanRefusal {
            kind: "output-kind-unroutable".to_owned(),
            detail: "This output's kind cannot switch feeds live in this release".to_owned(),
        }),
        Err(PlanRefusal::AudioUnsupported) => refusals.push(OutputPlanRefusal {
            kind: "audio-unsupported".to_owned(),
            detail: NO_AUDIO_SLOT.to_owned(),
        }),
    }
}

fn accepted(body: OutputTakeAccepted) -> Response {
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// The `202` of a replayed key: the key's own op and, once known, its
/// outcome. Never re-enqueues and never answers `adopt` again: the take store
/// never evicts a record, so a replay always meets its own operation.
fn replayed(replay: TakeReplay) -> Response {
    let recorded = replay.outcome;
    accepted(OutputTakeAccepted {
        operation_id: replay.operation_id.to_string(),
        status: if recorded.is_some() {
            "settled"
        } else {
            "pending"
        }
        .to_owned(),
        adopt: None,
        outcome: recorded.as_ref().map(|r| r.outcome.clone()),
        tick: recorded.map(|r| r.tick),
    })
}

/// `429` + `Retry-After` (whole seconds, at least 1).
fn too_many(retry: std::time::Duration) -> Response {
    let secs = retry
        .as_secs()
        .saturating_add(u64::from(retry.subsec_nanos() > 0))
        .max(1);
    let mut response = Problem::new(429, "too-many-requests", "Too many takes")
        .with_detail("too many takes for this output or principal; retry shortly")
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn unroutable_kind() -> Response {
    refusal_problem(
        409,
        "output-kind-unroutable",
        "This output's kind cannot switch feeds live in this release".to_owned(),
    )
}

fn audio_unsupported() -> Response {
    refusal_problem(409, "audio-unsupported", NO_AUDIO_SLOT.to_owned())
}

/// The take-time audio gate (`Some` = refused): an explicit audio choice on
/// an output without a program-audio slot is `audio-unsupported`.
fn audio_gate(
    control: &crate::output_routing::OutputRoutingControl,
    config_id: &str,
    video: Option<&RoutedFeed>,
    audio: &RoutedAudio,
) -> Option<Response> {
    match control.routing.plan_audio(config_id, video, Some(audio)) {
        Err(PlanRefusal::AudioUnsupported) => Some(audio_unsupported()),
        Err(PlanRefusal::OutputKindUnroutable) => Some(unroutable_kind()),
        Err(PlanRefusal::SwitcherRequired) | Ok(_) => None,
    }
}

/// The take-time admission gate from the engine's plan (`Some` = refused).
fn plan_gate(
    control: &crate::output_routing::OutputRoutingControl,
    principal: &Principal,
    config_id: &str,
    feed: &RoutedFeed,
) -> Option<Response> {
    match control.routing.plan(config_id, feed) {
        Err(PlanRefusal::SwitcherRequired) => Some(refusal_problem(
            409,
            "switcher-required",
            "Preview and Clean need a switcher (M/E) in this run".to_owned(),
        )),
        Err(PlanRefusal::OutputKindUnroutable) => Some(unroutable_kind()),
        Err(PlanRefusal::AudioUnsupported) => Some(audio_unsupported()),
        Ok(cost) if !cost.fits() => Some(refusal_problem(
            409,
            "capacity",
            capacity_text(principal, cost.sessions_in_use, cost.sessions_max),
        )),
        Ok(_) => None,
    }
}

/// `POST /api/v1/routing/output/take` — take an output feed (role: write;
/// both ends authorized; `Idempotency-Key` required). `stream: video` takes
/// `feed` (and `audio` with it when present); `stream: audio` takes only
/// `audio` and leaves the video untouched. `202 {operation_id,
/// status:"accepted", adopt:"next_tick"}`; a replay of the key answers the
/// same operation with `status` `pending` or `settled` (+ outcome) and is never
/// enqueued again. The outcome rides `route.output`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/routing/output/take",
        tag = "routing",
        request_body = OutputRouteRequest,
        params(("Idempotency-Key" = String, Header, description = "Required; one per distinct take.")),
        responses(
            (status = 202, description = "Accepted (`status: accepted`, `adopt: next_tick`). A replay of the key returns the same operation id with `status: pending` (no outcome yet) or `status: settled` + the recorded outcome; it never re-enqueues.", body = OutputTakeAccepted),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized for the output or the feed.", body = crate::problem::Problem),
            (status = 404, description = "Unknown output.", body = crate::problem::Problem),
            (status = 409, description = "`routing-unavailable`, `switcher-required`, `output-kind-unroutable`, `capacity`, or `audio-unsupported` (the output has no program-audio slot for a routed audio feed, or its transport cannot select a discrete track).", body = crate::problem::Problem),
            (status = 422, description = "`unknown-feed` (video or audio), `audio-unsupported` (more than one track: the output carries one routed audio feed), a body whose `stream` and `feed`/`audio` do not match (`stream: video` needs `feed`; `stream: audio` needs `audio` and no `feed`), or a missing Idempotency-Key.", body = crate::problem::Problem),
            (status = 429, description = "Over 2 takes/s per output or 10/s per principal; see Retry-After.", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: the take mailbox is full (the key is freed); or `idempotency-capacity`: the take store holds its maximum of distinct keys (fails closed; existing keys still replay).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn take_output(
    axum::extract::State(state): axum::extract::State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<OutputRouteRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    authorize_ends(&principal, &req)?;
    authorize_track_inputs(&state, &principal, &req)?;
    let video = req.shape()?;
    let Some(control) = state.output_routing.clone() else {
        return Ok(routing_unavailable());
    };
    let Some(key) = idem.0.as_deref() else {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    };
    // A replay answers from the take's own record without re-running
    // anything. `key` is already scoped by principal, method and path.
    if let Some(replay) = control.replay(key) {
        return Ok(replayed(replay));
    }
    let Some(output) = output_of(&state, req.output())? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: req.output().to_owned(),
        });
    };
    let config_id = output.id();
    // Limits key on existing outputs only, so unknown ids cannot churn the map.
    if let Err(retry) = control
        .limiter
        .admit(req.output(), &principal.key_id, Instant::now())
    {
        return Ok(too_many(retry));
    }
    let feed = match video {
        None => None,
        Some(video) => match resolve_feed(&state, video)? {
            Ok(feed) => Some(feed),
            Err(response) => return Ok(response),
        },
    };
    let audio = match &req.audio {
        None => None,
        Some(audio) => match resolve_audio(&state, &output, audio)? {
            Ok(audio) => Some(audio),
            Err((response, _)) => return Ok(response),
        },
    };
    if let Some(feed) = &feed {
        if let Some(refused) = plan_gate(&control, &principal, &config_id, feed) {
            return Ok(refused);
        }
    }
    if let Some(audio) = &audio {
        if let Some(refused) = audio_gate(&control, &config_id, feed.as_ref(), audio) {
            return Ok(refused);
        }
    }
    // The record exists before the enqueue, so an outcome never beats it.
    let pending = PendingTake {
        api_id: req.output().to_owned(),
        feed: feed.clone(),
        audio: audio.clone(),
    };
    let op = match control.reserve_take(key, pending) {
        TakeReservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        // Lost a race with a concurrent request bearing the same key.
        TakeReservation::Replay(replay) => return Ok(replayed(replay)),
        TakeReservation::Fresh(op) => op,
    };
    let take = OutputTakeRequest {
        operation_id: op.to_string(),
        output: config_id,
        video: feed.clone(),
        audio: audio.clone(),
        principal: principal.key_id.clone(),
    };
    if let Err(error) = control.routing.try_take(take) {
        // Only this take's own failed enqueue frees its key.
        control.release_take(key, &op);
        return Ok(match error {
            TakeSubmitError::Busy => ControlError::EngineBusy.into_response(),
            TakeSubmitError::Unroutable => unroutable_kind(),
        });
    }
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "output",
        req.output(),
        Some(serde_json::json!({
            "command": "route_output_take",
            "feed": feed.as_ref().map(RoutedFeed::bus_name),
            "audio": audio.as_ref().map(RoutedAudio::label),
            "operation_id": op.to_string(),
        })),
    );
    Ok(accepted(OutputTakeAccepted {
        operation_id: op.to_string(),
        status: "accepted".to_owned(),
        adopt: Some("next_tick".to_owned()),
        outcome: None,
        tick: None,
    }))
}
