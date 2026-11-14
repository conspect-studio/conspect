//! `POST /api/v1/routing/plan` (output target) and
//! `POST /api/v1/routing/output/take` (ADR-0108 slice 1, design §6.2-§6.4).
//!
//! Order of checks on both routes, so nothing leaks and nothing is burned:
//! role → destination on both axes (`ObjectAndOutput{output, output}`) → feed authorization
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
    LayoutBody, OutputTakeRequest, PendingTake, PlanRefusal, RoutedAudio, RoutedFeed, RoutedLayout,
    TakeReplay, TakeReservation, TakeSubmitError,
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
    /// The video feed: `{"bus":"program"|"preview"|"clean"}`,
    /// `{"source":"vt1"}` or `{"layout":"NewsOne"}` (a stored layout,
    /// ADR-0108 slice 3). Required with `stream: video`; absent with
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
    /// `1` when the take starts a new layout canvas (`routing-aux`), else `0`.
    #[serde(default)]
    pub new_aux_canvases: u8,
    /// The audio the output carries on this video feed when its audio
    /// follows: `program` (the mix), `source` (the source's own), `preview`
    /// (the PVW source's) or `none` (silence). A layout carries its `audio`
    /// attribute: the mix by default, `none`, or one cell's `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_audio: Option<String>,
}

/// A typed plan refusal (RFC 9457 `type` suffix + operator text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputPlanRefusal {
    /// `capacity` | `unknown-feed` | `switcher-required` |
    /// `output-kind-unroutable` | `audio-unsupported` | `layout-not-admissible`
    /// | `aux-capacity`.
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
pub(crate) fn capacity_text(principal: &Principal, in_use: usize, max: usize) -> String {
    if principal.is_global() {
        format!(
            "This output needs its own encoder and the GPU has no free encoder session ({in_use} of {max} in use)"
        )
    } else {
        "No free encoder session for this output".to_owned()
    }
}

/// Authorize the output end on BOTH axes (§6.3): the object axis
/// (`scoped_object_ids`) and the output axis (`scoped_output_ids`, ADR-W026),
/// so a key confined to some outputs can neither take nor read another. The
/// same rule filters `GET /routing` rows and `route.output` delivery.
pub(crate) fn authorize_output_end(principal: &Principal, output: &str) -> ControlResult<()> {
    crate::auth::authorize_scope(
        principal,
        multiview_events::AuthzScope::ObjectAndOutput {
            object: output,
            output,
        },
    )
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
fn authorize_ends(
    principal: &Principal,
    req: &OutputRouteRequest,
    layout: Option<&LayoutRead>,
) -> ControlResult<()> {
    authorize_output_end(principal, req.output())?;
    match &req.feed {
        None => {}
        Some(VideoFeed::Source(id)) => crate::auth::authorize_object(principal, id)?,
        Some(VideoFeed::Layout(id)) => authorize_layout_feed(principal, id, layout)?,
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

pub(crate) fn routing_unavailable() -> Response {
    Problem::new(409, "routing-unavailable", "Output routing unavailable")
        .with_detail("this run does not route output feeds")
        .into_response()
}

pub(crate) fn refusal_problem(status: u16, kind: &str, detail: String) -> Response {
    Problem::new(status, kind, "Take refused")
        .with_detail(detail)
        .into_response()
}

/// The accepted output body behind an API id (`None` = unknown).
pub(crate) fn output_of(
    state: &AppState,
    api_id: &str,
) -> ControlResult<Option<multiview_config::Output>> {
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

/// Resolve an output API id to its config id (`None` = unknown).
pub(crate) fn config_id_of(state: &AppState, api_id: &str) -> ControlResult<Option<String>> {
    Ok(output_of(state, api_id)?.map(|o| o.id()))
}

fn desired(state: &AppState) -> ControlResult<multiview_config::MultiviewConfig> {
    serde_json::from_value(crate::boot_model::current_desired_config(state)?)
        .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))
}

/// Whether `id` is a source the accepted document declares.
pub(crate) fn source_exists(state: &AppState, id: &str) -> ControlResult<bool> {
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

/// A feed refusal before the engine is asked: `(status, type, detail)`.
pub(crate) type FeedRefusal = (u16, &'static str, String);

/// Concurrent stored-layout reads the slot-0 routes admit (#342 review
/// T-I1): the repository is synchronous (SQLite behind it, a 5 s busy
/// timeout), so a `{layout}` flood must never park the async workers.
pub const LAYOUT_READ_PERMITS: usize = 8;

/// The process-wide layout-read admission ([`LAYOUT_READ_PERMITS`]).
#[must_use]
pub fn layout_read_semaphore() -> std::sync::Arc<tokio::sync::Semaphore> {
    std::sync::Arc::new(tokio::sync::Semaphore::new(LAYOUT_READ_PERMITS))
}

/// The ONE read of a request's stored layout (#342 review T-I1): fetched
/// once, off the async worker, before any authorization answer, and reused
/// by the closure authorization, the resolve and a replay's
/// re-authorization. `None` = the layout does not exist.
#[derive(Debug, Clone)]
pub(crate) struct LayoutRead {
    /// The layout id the request named.
    pub id: String,
    /// The layout resolution revision (#306), taken BEFORE the library read:
    /// an edit or delete takes its revision after its commit, so a take that
    /// read the pre-commit body always has the lower revision and the engine
    /// refuses it as stale.
    pub revision: u64,
    /// Its stored body, `None` when no such layout exists.
    pub body: Option<serde_json::Value>,
}

/// The stored body of layout `id` (sync; run only through
/// [`read_layout`]): the accepted library when a boot model is installed,
/// else the repository.
fn fetch_layout(state: &AppState, id: &str) -> ControlResult<Option<serde_json::Value>> {
    let versioned = match crate::routes::authoritative_layouts(state)? {
        Some(desired) => crate::routes::authoritative_layout(&desired, id),
        None => state.repository.get_layout(id),
    };
    match versioned {
        Ok(v) => Ok(Some(v.layout.body)),
        Err(ControlError::NotFound { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Read a request's stored layout ONCE on `spawn_blocking`, admitted by the
/// process-wide [`AppState::layout_reads`] semaphore (`try_acquire`): with
/// no free permit the request is 503 `engine-busy` (generic: nothing about
/// the layout or the output) and the repository is never called.
///
/// # Errors
/// `EngineBusy` without a permit or when the blocking task fails;
/// repository errors.
pub(crate) async fn read_layout(state: &AppState, id: &str) -> ControlResult<LayoutRead> {
    let Ok(permit) = std::sync::Arc::clone(&state.layout_reads).try_acquire_owned() else {
        return Err(ControlError::EngineBusy);
    };
    let revision = crate::output_routing::next_layout_revision();
    let task_state = state.clone();
    let task_id = id.to_owned();
    let body = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        fetch_layout(&task_state, &task_id)
    })
    .await
    .map_err(|_| ControlError::EngineBusy)??;
    Ok(LayoutRead {
        id: id.to_owned(),
        revision,
        body,
    })
}

/// The layout a video feed names, read once (`None` for any other feed),
/// after Read on the layout id itself: a caller without it never costs a
/// read.
///
/// # Errors
/// 403 for the layout id; as [`read_layout`].
pub(crate) async fn read_feed_layout(
    state: &AppState,
    principal: &Principal,
    feed: Option<&VideoFeed>,
) -> ControlResult<Option<LayoutRead>> {
    match feed {
        Some(VideoFeed::Layout(id)) => {
            crate::auth::authorize_object(principal, id)?;
            Ok(Some(read_layout(state, id).await?))
        }
        _ => Ok(None),
    }
}

/// The pre-read body of layout `id` (`None` = no such layout). A read for
/// another id (never by construction) fails closed as not found.
fn layout_body<'a>(read: Option<&'a LayoutRead>, id: &str) -> Option<&'a serde_json::Value> {
    read.filter(|r| r.id == id).and_then(|r| r.body.as_ref())
}

/// Resolve a stored layout for routing (ADR-0108 slice 3): it must exist in
/// the accepted layouts library, parse and solve, be authored for the running
/// canvas (the Class-1 pinned-canvas gate of `apply-layout`), and be drawable
/// by the aux integer path (every cell opaque). Pure CPU on the control
/// plane; the engine only composes the result.
///
/// Authorization (§6.3, "Read on L AND on every source its cells bind"): the
/// layout id is authorized before this runs; here every source the layout's
/// cells bind is authorized too, so a principal cannot view a source it may
/// not read by routing a layout that shows it. A refusal is a bare 403 that
/// names nothing (the caller already holds Read on the layout body).
fn resolve_layout(
    state: &AppState,
    principal: &Principal,
    id: &str,
    read: Option<&LayoutRead>,
) -> ControlResult<Result<RoutedLayout, FeedRefusal>> {
    let Some(body) = layout_body(read, id) else {
        return Ok(Err((
            422,
            "unknown-feed",
            "the layout does not exist".to_owned(),
        )));
    };
    // The closure is authorized BEFORE any parse/solve/canvas answer, so no
    // 422 detail reaches a principal who may not read every bound source.
    authorize_document_closure(principal, body)?;
    let resolved =
        match crate::command::resolve_layout_document(id, body, state.running_canvas.as_ref()) {
            Ok(resolved) => resolved,
            Err(ControlError::Validation(detail)) => {
                return Ok(Err((422, "layout-not-admissible", detail)));
            }
            Err(other) => return Err(other),
        };
    authorize_layout_sources(principal, &resolved.solved)?;
    // The revision of the request's one read, taken before that read (#306).
    let revision = read.map_or(0, |r| r.revision);
    let routed = RoutedLayout {
        id: id.to_owned(),
        slates: resolved.document.cell_slates(),
        audio: resolved.document.audio.clone(),
        overlays: resolved.document.overlays,
        layout: resolved.solved,
        revision,
    };
    if !routed.integer_drawable() {
        return Ok(Err((
            422,
            "layout-not-admissible",
            "this layout has a translucent cell; a routed layout draws opaque cells only \
             (set every cell's opacity to 1)"
                .to_owned(),
        )));
    }
    Ok(Ok(routed))
}

/// The outputs (config ids) whose live feed or pending take is layout `id`.
fn outputs_on_layout(state: &AppState, id: &str) -> Vec<String> {
    let bus = RoutedFeed::Layout(id.to_owned()).bus_name();
    let Some(live) = state
        .output_routing
        .as_ref()
        .and_then(|control| control.routing.snapshot())
    else {
        return Vec::new();
    };
    let mut outputs: Vec<String> = live
        .outputs
        .iter()
        .filter(|(_, o)| o.bus == bus || o.pending.as_ref().is_some_and(|(p, _)| *p == bus))
        .map(|(output, _)| output.clone())
        .collect();
    outputs.sort_unstable();
    outputs
}

/// §6.3 "the rule is checked on every change": an edit of a layout that is
/// on air (or pending) on any output needs Write on each such output and
/// Read on every source the NEW body binds. Otherwise 403 `routed-layout`
/// (one generic refusal; it names nothing). A layout routed nowhere passes.
///
/// # Errors
/// [`ControlError::Forbidden`] as above.
pub(crate) fn authorize_routed_layout_edit(
    state: &AppState,
    principal: &Principal,
    id: &str,
    body: Option<&serde_json::Value>,
) -> ControlResult<()> {
    let outputs = outputs_on_layout(state, id);
    if outputs.is_empty() {
        return Ok(());
    }
    let forbidden = || {
        ControlError::Forbidden(
            "routed-layout: this layout is on air on an output; changing it needs Write on \
             every output it feeds and Read on every source it shows"
                .to_owned(),
        )
    };
    let writable = outputs.iter().all(|output| {
        crate::routes::routing::api_id_of(state, output)
            .is_some_and(|api| authorize_output_end(principal, &api).is_ok())
    });
    if !writable {
        return Err(forbidden());
    }
    // A body that does not parse is answered by the update's own validation;
    // here it binds no source.
    if let Some(Ok(document)) = body.map(multiview_config::LayoutDocument::from_body) {
        let readable = document
            .cells
            .iter()
            .filter_map(|cell| cell.source.input_id.as_deref())
            .all(|source| crate::auth::authorize_object(principal, source).is_ok());
        if !readable {
            return Err(forbidden());
        }
    }
    Ok(())
}

/// After a committed layout edit (`body: Some`) or delete (`None`): tell the
/// engine, so outputs on it follow the edit live (at an IDR) or fall back to
/// Program, and a take still queued with an older body is refused. Sent for
/// every edit (a take may be queued but not yet in the readback); the engine
/// ignores a layout it does not route. An edited body that no longer routes
/// (another canvas, a translucent cell, a bad audio source) is a removal.
/// Non-blocking and coalesced per layout on the engine side.
pub(crate) fn notify_layout_change(state: &AppState, id: &str, body: Option<&serde_json::Value>) {
    let Some(control) = state.output_routing.as_ref() else {
        return;
    };
    let resolved = body.and_then(|body| {
        let resolved =
            crate::command::resolve_layout_document(id, body, state.running_canvas.as_ref())
                .ok()?;
        let routed = RoutedLayout {
            id: id.to_owned(),
            slates: resolved.document.cell_slates(),
            audio: resolved.document.audio.clone(),
            overlays: resolved.document.overlays,
            layout: resolved.solved,
            revision: crate::output_routing::next_layout_revision(),
        };
        routed
            .integer_drawable()
            .then(|| LayoutBody(std::sync::Arc::new(routed)))
    });
    if !control.routing.layout_changed(id, resolved) {
        tracing::warn!(layout = %id, "output routing: a layout change was not delivered to the engine; outputs on it keep their picture until a re-take");
    }
}

/// Read on every source `layout`'s cells bind (§6.3). One generic 403 for
/// any unreadable source: never which one, never whether it exists.
pub(crate) fn authorize_layout_sources(
    principal: &Principal,
    layout: &multiview_core::layout::Layout,
) -> ControlResult<()> {
    let readable = layout
        .cells
        .iter()
        .filter_map(|cell| cell.source.as_deref())
        .all(|source| crate::auth::authorize_object(principal, source).is_ok());
    if readable {
        Ok(())
    } else {
        Err(closure_forbidden())
    }
}

/// Read on every source a stored layout body's cells bind. A body that does
/// not parse fails closed with the same generic 403.
fn authorize_document_closure(
    principal: &Principal,
    body: &serde_json::Value,
) -> ControlResult<()> {
    let Ok(document) = multiview_config::LayoutDocument::from_body(body) else {
        return Err(closure_forbidden());
    };
    let readable = document
        .cells
        .iter()
        .filter_map(|cell| cell.source.input_id.as_deref())
        .all(|source| crate::auth::authorize_object(principal, source).is_ok());
    if readable {
        Ok(())
    } else {
        Err(closure_forbidden())
    }
}

/// Authorize a `{layout}` video feed (§6.3): Read on the layout id AND on
/// every source its CURRENT stored body binds, one generic 403. The ONE
/// authorize step of the alias, `/patch` output crosspoints and their plans
/// (#342 review T-A1): it runs with the other end checks, BEFORE any output
/// or feed existence answer, so a denied closure can never become a
/// 404-vs-403 oracle on the output. An unknown layout passes here and is
/// answered by existence later (`unknown-feed`).
///
/// # Errors
/// 403 for the id or an unreadable source; repository errors.
pub(crate) fn authorize_layout_feed(
    principal: &Principal,
    id: &str,
    read: Option<&LayoutRead>,
) -> ControlResult<()> {
    crate::auth::authorize_object(principal, id)?;
    authorize_layout_closure(principal, id, read)
}

/// Authorize a layout's source closure from the request's ONE pre-read body
/// (#342 review T-I1): the stored layout as it is NOW. A layout that does
/// not exist has no closure to read (existence answers later; a replay
/// answers its own recorded operation).
fn authorize_layout_closure(
    principal: &Principal,
    id: &str,
    read: Option<&LayoutRead>,
) -> ControlResult<()> {
    layout_body(read, id).map_or(Ok(()), |body| authorize_document_closure(principal, body))
}

fn closure_forbidden() -> ControlError {
    ControlError::Forbidden("not authorized for every source this layout shows".to_owned())
}

/// The video feed as a routed feed (and, for a layout, its resolved body and
/// its source closure authorized, §6.3), after existence checks: `Err` is the
/// typed refusal. Shared by the alias, `/patch` output crosspoints and their
/// plans (#342 B2), so a layout feed is resolved and authorized once, alike.
pub(crate) fn resolve_feed(
    state: &AppState,
    principal: &Principal,
    feed: &VideoFeed,
    layout: Option<&LayoutRead>,
) -> ControlResult<Result<(RoutedFeed, Option<LayoutBody>), FeedRefusal>> {
    let Some(routed) = RoutedFeed::of(feed) else {
        return Ok(Err((
            422,
            "unknown-feed",
            "this feed cannot be routed to an output in this release".to_owned(),
        )));
    };
    match &routed {
        RoutedFeed::Source(id) => {
            if !source_exists(state, id)? {
                return Ok(Err((
                    422,
                    "unknown-feed",
                    "the feed does not exist".to_owned(),
                )));
            }
        }
        RoutedFeed::Layout(id) => {
            return Ok(resolve_layout(state, principal, id, layout)?.map(|layout| {
                (
                    routed.clone(),
                    Some(LayoutBody(std::sync::Arc::new(layout))),
                )
            }));
        }
        _ => {}
    }
    Ok(Ok((routed, None)))
}

/// The audio a routed layout carries when the output's audio follows: its
/// `audio` attribute (§5.1).
fn layout_follow_audio(audio: &multiview_config::LayoutAudio) -> &'static str {
    match audio {
        multiview_config::LayoutAudio::None => "none",
        multiview_config::LayoutAudio::Source(_) => "source",
        _ => "program",
    }
}

/// The audio a video feed carries when the output's audio follows (§5.1).
fn follow_audio(feed: &RoutedFeed) -> &'static str {
    match feed {
        RoutedFeed::Source(_) => "source",
        RoutedFeed::Preview => "preview",
        RoutedFeed::Program | RoutedFeed::Clean | RoutedFeed::Layout(_) => "program",
    }
}

pub(crate) fn aux_capacity_text(principal: &Principal, active: usize, max: usize) -> String {
    if principal.is_global() {
        format!(
            "No free layout canvas: {active} of {max} routed layouts are already composing; \
             take an output off a layout first"
        )
    } else {
        "No free layout canvas for this output".to_owned()
    }
}

/// `POST /api/v1/routing/plan` for an output target.
pub(crate) async fn plan_output(
    state: &AppState,
    principal: &Principal,
    req: &OutputRouteRequest,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    // The request's ONE layout read, before any authorization answer: the
    // closure is then authorized from it before the output lookup (T-A1,
    // T-I1).
    authorize_output_end(principal, req.output())?;
    let layout = read_feed_layout(state, principal, req.feed.as_ref()).await?;
    authorize_ends(principal, req, layout.as_ref())?;
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
        new_aux_canvases: 0,
        follow_audio: None,
    };
    let mut routed_video = None;
    let mut routed_layout: Option<LayoutBody> = None;
    if let Some(feed) = video {
        match resolve_feed(state, principal, feed, layout.as_ref())? {
            Err((_, kind, detail)) => refusals.push(OutputPlanRefusal {
                kind: kind.to_owned(),
                detail: if kind == "unknown-feed" {
                    "the feed does not exist or cannot be routed".to_owned()
                } else {
                    detail
                },
            }),
            Ok((feed, layout)) => {
                plan_video(
                    control,
                    principal,
                    &config_id,
                    &feed,
                    &mut cost,
                    &mut refusals,
                );
                if let (Some(body), Some(_)) = (&layout, &cost.follow_audio) {
                    cost.follow_audio = Some(layout_follow_audio(&body.0.audio).to_owned());
                }
                routed_layout = layout;
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
        let planned = plan_take_audio(
            control,
            &config_id,
            routed_video.as_ref(),
            routed_layout.as_ref(),
            routed_audio.as_ref(),
        );
        match planned {
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

/// The engine's audio plan for a take: a layout's `audio` attribute is what
/// `follow` resolves to on it.
fn plan_take_audio(
    control: &crate::output_routing::OutputRoutingControl,
    config_id: &str,
    video: Option<&RoutedFeed>,
    layout: Option<&LayoutBody>,
    audio: Option<&RoutedAudio>,
) -> Result<crate::output_routing::AudioPlanCost, PlanRefusal> {
    match (video, layout) {
        (Some(feed), Some(body)) => {
            control
                .routing
                .plan_layout_audio(config_id, feed, &body.0.audio, audio)
        }
        _ => control.routing.plan_audio(config_id, video, audio),
    }
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
            cost.follow_audio = Some(follow_audio(feed).to_owned());
            cost.new_encoders = u8::from(planned.needs_encoder);
            cost.new_aux_canvases = u8::from(planned.needs_aux_canvas);
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
        Err(PlanRefusal::AuxCapacity { active, max }) => {
            cost.fits = false;
            refusals.push(OutputPlanRefusal {
                kind: "aux-capacity".to_owned(),
                detail: aux_capacity_text(principal, active, max),
            });
        }
    }
}

fn accepted(body: OutputTakeAccepted) -> Response {
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// The `202` of a replayed key: the key's own op and, once known, its
/// outcome. Never re-enqueues and never answers `adopt` again: the take store
/// never evicts a pending record, so a replay while pending always meets its
/// own operation (a settled record may be evicted under pressure; its key is
/// then a new take — the documented trade-off, design §6.2).
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

/// [`too_many`] for sibling routes (one live format reset = one take).
pub(crate) fn too_many_pub(retry: std::time::Duration) -> Response {
    too_many(retry)
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

pub(crate) fn unroutable_kind() -> Response {
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
pub(crate) fn audio_gate(
    control: &crate::output_routing::OutputRoutingControl,
    config_id: &str,
    video: Option<&RoutedFeed>,
    audio: &RoutedAudio,
) -> Option<Response> {
    match control.routing.plan_audio(config_id, video, Some(audio)) {
        Err(PlanRefusal::AudioUnsupported) => Some(audio_unsupported()),
        Err(PlanRefusal::OutputKindUnroutable) => Some(unroutable_kind()),
        // The video plan answers the video refusals.
        Err(PlanRefusal::SwitcherRequired | PlanRefusal::AuxCapacity { .. }) | Ok(_) => None,
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
        Err(PlanRefusal::AuxCapacity { active, max }) => Some(refusal_problem(
            409,
            "aux-capacity",
            aux_capacity_text(principal, active, max),
        )),
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
            (status = 403, description = "Not authorized for the output (object AND output axis) or the feed; a replayed key is re-authorized under the caller's current grants.", body = crate::problem::Problem),
            (status = 404, description = "Unknown output.", body = crate::problem::Problem),
            (status = 409, description = "`routing-unavailable`, `switcher-required`, `output-kind-unroutable`, `capacity`, `aux-capacity` (no free layout canvas), `audio-unsupported` (the output has no program-audio slot for a routed audio feed, or its transport cannot select a discrete track), or `conflict` (the Idempotency-Key was first used for another output, feed or audio; nothing is enqueued).", body = crate::problem::Problem),
            (status = 422, description = "`unknown-feed` (video or audio, including an unknown layout), `layout-not-admissible` (a layout authored for another canvas, one that does not solve, or one with a translucent cell), `audio-unsupported` (more than one track: the output carries one routed audio feed), a body whose `stream` and `feed`/`audio` do not match (`stream: video` needs `feed`; `stream: audio` needs `audio` and no `feed`), or a missing Idempotency-Key.", body = crate::problem::Problem),
            (status = 429, description = "Over 2 takes/s per output or 10/s per principal; see Retry-After.", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: the take mailbox is full (the key is freed), or a `{layout}` feed could not be read now (every layout-read slot is in use; nothing reserved); or `idempotency-capacity`: the caller's (or every) take record is still pending at its cap (fails closed; pending keys still replay; settled records are evicted oldest-first otherwise).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn take_output(
    axum::extract::State(state): axum::extract::State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<OutputRouteRequest>,
) -> ControlResult<Response> {
    take_output_as(&state, &principal, &idem, &req).await
}

/// The whole output take — authorization of both ends, the Idempotency-Key
/// contract, limits, gates, reservation and enqueue — shared by
/// `POST /routing/output/take` and its `apply-layout` alias
/// (`target: {output}`, #307), so the alias can never authorize less.
pub(crate) async fn take_output_as(
    state: &AppState,
    principal: &Principal,
    idem: &IdempotencyKey,
    req: &OutputRouteRequest,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // The request's ONE layout read (T-I1), after the output end and before
    // the closure answer (T-A1).
    authorize_output_end(principal, req.output())?;
    let layout = read_feed_layout(state, principal, req.feed.as_ref()).await?;
    authorize_ends(principal, req, layout.as_ref())?;
    authorize_track_inputs(state, principal, req)?;
    let video = req.shape()?;
    let output = req.output().to_owned();
    let op = match take_output_slot0(
        state,
        principal,
        idem,
        &output,
        video,
        req.audio.as_ref(),
        layout.as_ref(),
    )? {
        SlotTake::Accepted(op) => op,
        SlotTake::Replay(replay) => return Ok(replayed(replay)),
        SlotTake::Refused(response) => return Ok(response),
    };
    Ok(accepted(OutputTakeAccepted {
        operation_id: op,
        status: "accepted".to_owned(),
        adopt: Some("next_tick".to_owned()),
        outcome: None,
        tick: None,
    }))
}

/// What [`take_output_slot0`] did with a take.
pub(crate) enum SlotTake {
    /// Enqueued under this operation id.
    Accepted(String),
    /// The key already names this very take: answer it, never enqueue again.
    Replay(TakeReplay),
    /// A typed refusal response (409/422/429/503...).
    Refused(Response),
}

/// Take onto output `output`'s slot 0 — the ONE take machinery behind
/// `POST /routing/output/take` and an output crosspoint of `POST /patch/take`
/// (#342 B2): the take store's replay (bound to the request, #342 review A1),
/// existence, the per-output and per-principal limits, the feed and audio
/// resolution, the plan and audio gates, the reservation, the bounded
/// `try_send` and the audit. `video` is the video feed (`None`: an audio-only
/// take), `audio` the explicit audio feed (`None`: keep the output's choice).
/// The caller has checked the role, authorized both ends (output on both
/// axes, feed and audio reads, track inputs) under the principal's CURRENT
/// grants, and validated the body's shape.
///
/// # Errors
/// Repository errors, `NotFound` for an unknown output, a missing key, a key
/// reused for another take (409), or `idempotency-capacity`.
#[allow(clippy::too_many_arguments)]
// reason: the slot-0 take's inputs are the request's parts plus its ONE
// pre-read layout (T-I1); a struct would only rename them.
pub(crate) fn take_output_slot0(
    state: &AppState,
    principal: &Principal,
    idem: &IdempotencyKey,
    output_id: &str,
    video: Option<&VideoFeed>,
    audio: Option<&AudioFeed>,
    layout: Option<&LayoutRead>,
) -> ControlResult<SlotTake> {
    let Some(control) = state.output_routing.clone() else {
        return Ok(SlotTake::Refused(routing_unavailable()));
    };
    let key = slot0_key_of(idem)?;
    // A replay answers only the SAME take (output, video, audio; review A1),
    // re-authorized under current grants (the closure from the request's one
    // pre-read layout); a different body is refused, nothing enqueued.
    if let Some(replay) = replay_slot0(
        state, principal, &control, &key, output_id, video, audio, layout,
    )? {
        return Ok(SlotTake::Replay(replay));
    }
    let Some(output) = output_of(state, output_id)? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: output_id.to_owned(),
        });
    };
    let config_id = output.id();
    // Limits key on existing outputs only, so unknown ids cannot churn the map.
    if let Err(retry) = control
        .limiter
        .admit(output_id, &principal.key_id, Instant::now())
    {
        return Ok(SlotTake::Refused(too_many(retry)));
    }
    let (feed, layout) = match video {
        None => (None, None),
        Some(video) => match resolve_feed(state, principal, video, layout)? {
            Ok((feed, layout)) => (Some(feed), layout),
            Err((status, kind, detail)) => {
                return Ok(SlotTake::Refused(refusal_problem(status, kind, detail)))
            }
        },
    };
    let audio = match audio {
        None => None,
        Some(audio) => match resolve_audio(state, &output, audio)? {
            Ok(audio) => Some(audio),
            Err((response, _)) => return Ok(SlotTake::Refused(response)),
        },
    };
    if let Some(feed) = &feed {
        if let Some(refused) = plan_gate(&control, principal, &config_id, feed) {
            return Ok(SlotTake::Refused(refused));
        }
    }
    if let Some(audio) = &audio {
        if let Some(refused) = audio_gate(&control, &config_id, feed.as_ref(), audio) {
            return Ok(SlotTake::Refused(refused));
        }
    }
    // The record exists before the enqueue, so an outcome never beats it.
    let pending = PendingTake {
        api_id: output_id.to_owned(),
        feed: feed.clone(),
        audio: audio.clone(),
    };
    let pending_check = pending.clone();
    let op = match control.reserve_take(&key, &principal.key_id, pending) {
        TakeReservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        // Lost a race with a concurrent request bearing the same key and a
        // DIFFERENT take: refused atomically by the store (#342 review A1-r2).
        TakeReservation::Conflict => return Err(key_reused()),
        // Lost a race with a concurrent request bearing the same key and the
        // same take. Re-check the binding (defense in depth: the store
        // already compared the takes under its lock).
        TakeReservation::Replay(replay) => {
            let same = control
                .replay_with_take(&key)
                .is_some_and(|(_, recorded)| recorded == pending_check);
            if !same {
                return Err(key_reused());
            }
            return Ok(SlotTake::Replay(replay));
        }
        TakeReservation::Fresh(op) => op,
    };
    let take = OutputTakeRequest {
        operation_id: op.to_string(),
        output: config_id,
        video: feed.clone(),
        audio: audio.clone(),
        layout,
        principal: principal.key_id.clone(),
    };
    if let Err(error) = control.routing.try_take(take) {
        // Only this take's own failed enqueue frees its key.
        control.release_take(&key, &op);
        return Ok(SlotTake::Refused(match error {
            TakeSubmitError::Busy => ControlError::EngineBusy.into_response(),
            TakeSubmitError::Unroutable => unroutable_kind(),
        }));
    }
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "output",
        output_id,
        Some(serde_json::json!({
            "command": "route_output_take",
            "feed": feed.as_ref().map(RoutedFeed::bus_name),
            "audio": audio.as_ref().map(RoutedAudio::label),
            "operation_id": op.to_string(),
        })),
    );
    Ok(SlotTake::Accepted(op.to_string()))
}

/// A replay of a slot-0 take key: `Some` when the key names THIS take
/// (bound to output, video and audio, #342 review A1). A layout take's
/// replay re-authorizes the layout's whole source closure under the CURRENT
/// grants too (§6.3), exactly as a fresh take does: the caller authorized
/// only the layout id.
///
/// # Errors
/// `conflict` for a key first used for another take; 403 for an unreadable
/// layout closure; repository errors.
#[allow(clippy::too_many_arguments)]
// reason: as `take_output_slot0`, plus the take store.
fn replay_slot0(
    state: &AppState,
    principal: &Principal,
    control: &crate::output_routing::OutputRoutingControl,
    key: &str,
    output_id: &str,
    video: Option<&VideoFeed>,
    audio: Option<&AudioFeed>,
    layout: Option<&LayoutRead>,
) -> ControlResult<Option<TakeReplay>> {
    let Some((replay, recorded)) = control.replay_with_take(key) else {
        return Ok(None);
    };
    if !same_take(state, &recorded, output_id, video, audio)? {
        return Err(key_reused());
    }
    // The recorded feed equals this request's (`same_take`), so the
    // request's one pre-read layout is the recorded layout's body (T-I1).
    if let Some(RoutedFeed::Layout(id)) = &recorded.feed {
        authorize_layout_closure(principal, id, layout)?;
    }
    Ok(Some(replay))
}

/// The refusal of a take key first used for another take (409 `conflict`).
pub(crate) fn key_reused() -> ControlError {
    ControlError::Conflict("Idempotency-Key reused with a different request".into())
}

/// Whether a replayed key's recorded take is this request's take: the same
/// output API id, video feed and audio choice (#342 review A1). The request
/// side is resolved the way a fresh take would be; anything that does not
/// resolve is not the same take.
fn same_take(
    state: &AppState,
    recorded: &PendingTake,
    output_id: &str,
    video: Option<&VideoFeed>,
    audio: Option<&AudioFeed>,
) -> ControlResult<bool> {
    if recorded.api_id != output_id {
        return Ok(false);
    }
    let feed = video.map(RoutedFeed::of);
    let same_video = match (feed, &recorded.feed) {
        (None, None) => true,
        (Some(Some(feed)), Some(recorded)) => feed == *recorded,
        _ => false,
    };
    if !same_video {
        return Ok(false);
    }
    Ok(match (audio, &recorded.audio) {
        (None, None) => true,
        (Some(audio), Some(recorded)) => match output_of(state, output_id)? {
            Some(output) => {
                resolve_audio(state, &output, audio)?.is_ok_and(|resolved| resolved == *recorded)
            }
            None => false,
        },
        _ => false,
    })
}

/// The shared slot-0 take-store key of a request ([`slot0_take_key`]).
///
/// # Errors
/// A missing `Idempotency-Key` (422).
fn slot0_key_of(idem: &IdempotencyKey) -> ControlResult<String> {
    idem.0
        .as_deref()
        .map(slot0_take_key)
        .ok_or_else(|| ControlError::Validation("Idempotency-Key is required".into()))
}

/// The take-store key of a slot-0 take: the scoped `Idempotency-Key` with
/// its request path replaced by one shared namespace, so `/routing/output/take`,
/// an output crosspoint of `/patch/take` (the alias, #342 B2) and
/// `/commands/apply-layout` with `target: {output}` (the alias, #307) replay
/// each other — and a different body under the same key is the take store's
/// 409, whichever route reserved it. The principal and raw key stay in the
/// scope; only the path is unified. (Only the `target` form of apply-layout
/// reaches the take store; its no-target form uses the global store.)
#[must_use]
pub(crate) fn slot0_take_key(scoped: &str) -> String {
    const ALIASES: [&str; 3] = [
        "/routing/output/take",
        "/patch/take",
        "/commands/apply-layout",
    ];
    // The scope is `{len}:{method}:{len}:{key_id}:{len}:{path}:{len}:{raw}`
    // (`IdempotencyKey`). Walk the length prefixes to the REAL path field:
    // a substring search could match a path spelled inside the client's raw
    // token and split one key into two namespaces (#307 review A1 r2).
    let Some((path_start, path_end)) = scope_path_span(scoped) else {
        return scoped.to_owned();
    };
    let (Some(head), Some(path), Some(tail)) = (
        scoped.get(..path_start),
        scoped.get(path_start..path_end),
        scoped.get(path_end..),
    ) else {
        return scoped.to_owned();
    };
    if !ALIASES.contains(&path) {
        return scoped.to_owned();
    }
    // `head` ends with `{path.len()}:`; replace that prefix and the path.
    let Some(prefix) = head
        .strip_suffix(':')
        .and_then(|h| h.strip_suffix(path.len().to_string().as_str()))
    else {
        return scoped.to_owned();
    };
    let shared = "output-slot0-take";
    format!("{prefix}{}:{shared}{tail}", shared.len())
}

/// The byte span of the path field (the third length-prefixed field) of an
/// `IdempotencyKey` scope, or `None` when the scope is not well formed.
fn scope_path_span(scoped: &str) -> Option<(usize, usize)> {
    let mut at = 0_usize;
    for field in 0..3 {
        let rest = scoped.get(at..)?;
        let colon = rest.find(':')?;
        let len: usize = rest.get(..colon)?.parse().ok()?;
        let start = at.checked_add(colon)?.checked_add(1)?;
        let end = start.checked_add(len)?;
        if scoped.get(end..end.checked_add(1)?)? != ":" {
            return None;
        }
        if field == 2 {
            return Some((start, end));
        }
        at = end + 1;
    }
    None
}
