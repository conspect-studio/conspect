//! `/api/v1/cast/config` and `/api/v1/cast/streams` (ADR-0111, design §3):
//! cast stream tokens for the browser Cast sender.
//!
//! Order of checks on create, so nothing leaks and nothing is burned: role
//! (Write) → the chosen output on both axes and the feed end, exactly as
//! `POST /routing/output/take` authorizes them (a principal that cannot read
//! what an already-streaming output shows never gets a token for it) → the
//! per-principal creation limit → existence and engine state → claim.
//! Streams belong to the creating key id: another principal's stream is a
//! `404` (not `403`) unless the caller is Admin. The token appears only in
//! `path` of the create answer and of the owner's `GET`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::VideoFeed;
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal, Role};
use crate::cast_streams::{CastFetches, ReceiverFetch, ReceiverReport, StreamView};
use crate::error::{ControlError, ControlResult};
use crate::output_routing::RoutedFeed;
use crate::problem::Problem;
use crate::state::AppState;

/// The custom-namespace the receiver and senders talk on.
pub const CAST_NAMESPACE: &str = "urn:x-cast:studio.conspect.cast";
/// Where the receiver app is served.
pub const RECEIVER_PATH: &str = "/cast/receiver/";

/// A cast output as `GET /cast/config` lists it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CastOutputDoc {
    /// The output's API id.
    pub id: String,
    /// What it shows now: `{"bus":"program"}`, `{"source":"cam1"}`,
    /// `{"layout":"NewsOne"}`; absent when the engine has not said.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub feed: Option<VideoFeed>,
    /// A cast stream is active on it.
    pub active: bool,
    /// A receiver can start now (master + 5 video segments).
    pub ready: bool,
    /// Cast streams on it.
    pub streams: usize,
    /// Why it cannot be used, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `GET /api/v1/cast/config`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CastConfigDoc {
    /// The registered custom receiver app id (8 upper-case hex characters);
    /// `null` = none registered (use Google's Default Media Receiver).
    pub receiver_app_id: Option<String>,
    /// Where the receiver app is served (`/cast/receiver/`).
    pub receiver_path: String,
    /// The custom message namespace.
    pub namespace: String,
    /// The cast outputs this principal may use.
    pub outputs: Vec<CastOutputDoc>,
    /// The last fetch of the receiver app (no IP), `null` = never.
    pub receiver_last_fetch: Option<ReceiverFetch>,
}

/// `POST /api/v1/cast/streams` body.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateCastStream {
    /// What to show, as `POST /routing/output/take` takes it:
    /// `{"bus":"program"|"preview"|"clean"}`, `{"source":"<id>"}` or
    /// `{"layout":"<id>"}`. Absent (and no `output`) = Program.
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub feed: Option<VideoFeed>,
    /// A cast output's API id, used as it is (what it shows now).
    #[serde(default)]
    pub output: Option<String>,
}

/// One cast stream.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CastStreamDoc {
    /// The stream id (not a secret).
    pub id: String,
    /// The cast output's API id.
    pub output: String,
    /// The feed the stream was created for.
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub feed: VideoFeed,
    /// `/cast/<token>/master.m3u8`, relative to this origin. Only in the
    /// create answer and the owner's `GET`; never in a list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `application/vnd.apple.mpegurl`.
    pub content_type: String,
    /// `fmp4`.
    pub hls_segment_format: String,
    /// `fmp4`.
    pub hls_video_segment_format: String,
    /// A receiver can start now.
    pub ready: bool,
    /// Video segments published (Google's Default Media Receiver starts
    /// slowly on a young playlist: wait for 8).
    pub segments: u64,
    /// When the token expires unless a playlist is fetched (Unix ms).
    pub expires_unix_ms: u64,
    /// Media fetch counters (no token, no IP).
    pub fetches: CastFetches,
    /// The receiver's last report, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_report: Option<ReceiverReport>,
}

pub(crate) fn cast_problem(status: u16, reason: &str, detail: impl Into<String>) -> Response {
    Problem::new(status, reason, "Cast refused")
        .with_detail(detail)
        .into_response()
}

fn unavailable(detail: impl Into<String>) -> Response {
    cast_problem(409, "cast-unavailable", detail)
}

fn may_see(principal: &Principal, owner: &str) -> bool {
    principal.role == Role::Admin || principal.key_id == owner
}

fn api_id(state: &AppState, config_id: &str) -> Option<String> {
    crate::routes::routing::api_id_of(state, config_id)
}

/// The doc of a stream view (`path` only when `token` is given).
pub(crate) fn stream_doc(state: &AppState, view: StreamView, token: Option<&str>) -> CastStreamDoc {
    let status = state.cast_engine.as_ref().and_then(|engine| {
        engine
            .outputs()
            .into_iter()
            .find(|o| o.id == view.config_id)
    });
    CastStreamDoc {
        id: view.id,
        output: view.output,
        feed: view.feed.video_feed(),
        path: token.map(|t| format!("/cast/{t}/master.m3u8")),
        content_type: "application/vnd.apple.mpegurl".to_owned(),
        hls_segment_format: "fmp4".to_owned(),
        hls_video_segment_format: "fmp4".to_owned(),
        ready: status.as_ref().is_some_and(|s| s.active && s.ready),
        segments: status.as_ref().map_or(0, |s| s.video_segments),
        expires_unix_ms: view.expires_unix_ms,
        fetches: view.fetches,
        last_report: view.last_report,
    }
}

/// `GET /api/v1/cast/config` — the receiver app id, namespace and the cast
/// outputs this principal may use (role: read; filtered per output on both
/// axes).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/cast/config",
        tag = "cast",
        responses(
            (status = 200, description = "The cast configuration.", body = CastConfigDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_cast_config(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<CastConfigDoc>> {
    principal.role.require(Action::Read)?;
    crate::cast_streams::lifecycle::sweep_now(&state);
    let slots = state.cast_streams.slots();
    let mut outputs = Vec::new();
    for status in state
        .cast_engine
        .as_ref()
        .map(|e| e.outputs())
        .unwrap_or_default()
    {
        let Some(id) = api_id(&state, &status.id) else {
            continue;
        };
        if crate::routes::output_routing::authorize_output_end(&principal, &id).is_err() {
            continue;
        }
        let feed =
            crate::cast_streams::lifecycle::live_feed(&state, &status.id).map(|f| f.video_feed());
        let streams = slots
            .iter()
            .find(|s| s.config_id == status.id)
            .map_or(0, |s| s.streams);
        outputs.push(CastOutputDoc {
            id,
            feed,
            active: status.active,
            ready: status.ready,
            streams,
            last_error: status.last_error,
        });
    }
    Ok(Json(CastConfigDoc {
        receiver_app_id: state.cast_streams.receiver_app_id(),
        receiver_path: RECEIVER_PATH.to_owned(),
        namespace: CAST_NAMESPACE.to_owned(),
        outputs,
        receiver_last_fetch: state.cast_streams.receiver_last_fetch(),
    }))
}

/// A stream id unknown to (or hidden from) this principal: the same 404.
fn not_found(id: &str) -> ControlError {
    ControlError::NotFound {
        kind: "cast stream",
        id: id.to_owned(),
    }
}

/// `GET /api/v1/cast/streams` — the caller's own streams (Admin: all);
/// never carries `path`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/cast/streams",
        tag = "cast",
        responses(
            (status = 200, description = "The caller's cast streams (an Admin sees all), without tokens.", body = [CastStreamDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_cast_streams(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<CastStreamDoc>>> {
    principal.role.require(Action::Read)?;
    crate::cast_streams::lifecycle::sweep_now(&state);
    Ok(Json(
        state
            .cast_streams
            .list()
            .into_iter()
            .filter(|v| may_see(&principal, &v.owner))
            .map(|v| stream_doc(&state, v, None))
            .collect(),
    ))
}

/// `GET /api/v1/cast/streams/{id}` — one stream with its readiness and the
/// receiver's last report; `path` only for the owner.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/cast/streams/{id}",
        tag = "cast",
        params(("id" = String, Path, description = "Cast stream id")),
        responses(
            (status = 200, description = "The stream (`path` only for its owner).", body = CastStreamDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 404, description = "No such stream, it expired, or it is another principal's.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_cast_stream(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<CastStreamDoc>> {
    principal.role.require(Action::Read)?;
    crate::cast_streams::lifecycle::sweep_now(&state);
    let Some((view, token)) = state.cast_streams.get(&id) else {
        return Err(not_found(&id));
    };
    if !may_see(&principal, &view.owner) {
        return Err(not_found(&id));
    }
    let owner = view.owner == principal.key_id;
    Ok(Json(stream_doc(
        &state,
        view,
        owner.then_some(token.as_str()),
    )))
}

/// `DELETE /api/v1/cast/streams/{id}` — revoke the token at once (owner or
/// Admin; role: write).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/cast/streams/{id}",
        tag = "cast",
        params(("id" = String, Path, description = "Cast stream id")),
        responses(
            (status = 204, description = "Revoked: the next media request with its token is 404."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "The role cannot write.", body = crate::problem::Problem),
            (status = 404, description = "No such stream, it expired, or it is another principal's.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_cast_stream(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::cast_streams::lifecycle::sweep_now(&state);
    let visible = state
        .cast_streams
        .get(&id)
        .is_some_and(|(view, _)| may_see(&principal, &view.owner));
    if !visible {
        return Err(not_found(&id));
    }
    let Some(view) = state.cast_streams.remove(&id) else {
        return Err(not_found(&id));
    };
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Delete,
        "cast_stream",
        &view.id,
        Some(serde_json::json!({ "output": view.output })),
    );
    crate::cast_streams::lifecycle::sweep_now(&state);
    // A server-side cast session that played this stream ends with it.
    crate::routes::cast_sessions::reap_streamless_sessions(&state);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A usable cast output: (status, API id).
type Candidate = (crate::cast_engine::CastOutputStatus, String);

/// The cast outputs of the run this principal may use (both axes), in
/// configured order.
fn candidates(state: &AppState, principal: &Principal) -> Vec<Candidate> {
    let outputs = state
        .cast_engine
        .as_ref()
        .map(|e| e.outputs())
        .unwrap_or_default();
    outputs
        .into_iter()
        .filter_map(|status| {
            let id = api_id(state, &status.id)?;
            crate::routes::output_routing::authorize_output_end(principal, &id)
                .is_ok()
                .then_some((status, id))
        })
        .collect()
}

/// Where a new stream goes.
enum Choice {
    /// The output already shows the feed: no take.
    Shows(Candidate),
    /// A free output that must be taken to the feed.
    Take(Candidate),
}

/// The feed choice of design §3: an output that shows the feed and has
/// streams; else a free output (one already showing the feed first) taken
/// to it; else `cast-capacity`.
fn choose(
    state: &AppState,
    principal: &Principal,
    feed: &RoutedFeed,
) -> Result<Choice, Box<Response>> {
    let usable: Vec<Candidate> = candidates(state, principal)
        .into_iter()
        .filter(|(s, _)| s.last_error.is_none())
        .collect();
    if usable.is_empty() {
        return Err(Box::new(unavailable(
            "No cast output can be used: add an output with kind = \"cast\" (or check that the engine is running)",
        )));
    }
    let slots = state.cast_streams.slots();
    let streams_on = |config_id: &str| {
        slots
            .iter()
            .find(|s| s.config_id == config_id)
            .map(|s| (s.streams, s.feed.clone()))
    };
    let shows = |config_id: &str| {
        crate::cast_streams::lifecycle::live_feed(state, config_id).as_ref() == Some(feed)
    };
    if let Some(found) = usable
        .iter()
        .find(|(s, _)| streams_on(&s.id).is_some_and(|(n, f)| n > 0 && f == *feed) && shows(&s.id))
    {
        return Ok(Choice::Shows(found.clone()));
    }
    let free =
        |s: &crate::cast_engine::CastOutputStatus| streams_on(&s.id).is_none_or(|(n, _)| n == 0);
    if let Some(found) = usable.iter().find(|(s, _)| free(s) && shows(&s.id)) {
        return Ok(Choice::Shows(found.clone()));
    }
    if let Some(found) = usable.iter().find(|(s, _)| free(s)) {
        return Ok(Choice::Take(found.clone()));
    }
    Err(Box::new(cast_problem(
        409,
        "cast-capacity",
        "Every cast output is in use: add a cast output or stop a cast",
    )))
}

/// The take-time admission of a cast take (the take route's plan gate, with
/// an encoder refusal named `cast-no-encoder`).
fn cast_plan_gate(
    state: &AppState,
    principal: &Principal,
    config_id: &str,
    feed: &RoutedFeed,
) -> Option<Response> {
    use crate::output_routing::PlanRefusal;
    use crate::routes::output_routing::{
        aux_capacity_text, capacity_text, refusal_problem, routing_unavailable, unroutable_kind,
    };
    let Some(control) = state.output_routing.as_ref() else {
        return Some(routing_unavailable());
    };
    match control.routing.plan(config_id, feed) {
        Err(PlanRefusal::SwitcherRequired) => Some(refusal_problem(
            409,
            "switcher-required",
            "Preview and Clean need a switcher (M/E) in this run".to_owned(),
        )),
        Err(PlanRefusal::OutputKindUnroutable) => Some(unroutable_kind()),
        Err(PlanRefusal::AuxCapacity { active, max }) => Some(refusal_problem(
            409,
            "aux-capacity",
            aux_capacity_text(principal, active, max),
        )),
        Ok(cost) if !cost.fits() => Some(cast_problem(
            409,
            "cast-no-encoder",
            capacity_text(principal, cost.sessions_in_use, cost.sessions_max),
        )),
        Err(PlanRefusal::AudioUnsupported) | Ok(_) => None,
    }
}

/// The feed a request resolves to, authorized: `(feed, layout read)`.
async fn authorized_feed(
    state: &AppState,
    principal: &Principal,
    feed: &VideoFeed,
) -> ControlResult<Option<crate::routes::output_routing::LayoutRead>> {
    let layout =
        crate::routes::output_routing::read_feed_layout(state, principal, Some(feed)).await?;
    crate::routes::output_routing::authorize_video_feed(principal, feed, layout.as_ref())?;
    Ok(layout)
}

/// `POST /api/v1/cast/streams` — mint a cast stream token (role: write; no
/// `Idempotency-Key`). `output` uses that cast output as it is (the caller
/// must be authorized for the output on both axes AND for the feed it
/// shows); `feed` picks an output already showing it, else takes a free one
/// to it (operation `cast:<uuid>`, never persisted); neither = Program.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/cast/streams",
        tag = "cast",
        request_body = CreateCastStream,
        responses(
            (status = 201, description = "Created; `path` carries the token (shown only here and in the owner's GET).", body = CastStreamDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "The role cannot write, or the caller may not read the output (object AND output axis) or the feed (a layout: its whole source closure).", body = crate::problem::Problem),
            (status = 404, description = "Unknown output.", body = crate::problem::Problem),
            (status = 409, description = "`cast-unavailable` (no usable cast output, the engine is not running, or the output's own error), `cast-capacity` (every cast output in use, or 32 streams / 8 for this caller), `cast-no-encoder` (no free encoder session for the take), or a routing refusal of the take passed through (`switcher-required`, `aux-capacity`, `output-kind-unroutable`, `routing-unavailable`, `refused:*`).", body = crate::problem::Problem),
            (status = 422, description = "`unknown-feed` / `layout-not-admissible`, or a malformed body.", body = crate::problem::Problem),
            (status = 429, description = "Over 4 creations in a burst, then 1/s, per principal; see Retry-After.", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: the take could not be enqueued or did not settle within 3 s.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_cast_stream(
    State(state): State<AppState>,
    principal: Principal,
    body: Option<Json<CreateCastStream>>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    Ok(match mint_stream(&state, &principal, &req).await? {
        Ok(minted) => {
            let doc = stream_doc(&state, minted.view, Some(&minted.token));
            (StatusCode::CREATED, Json(doc)).into_response()
        }
        Err(refused) => refused,
    })
}

/// A stream just created: its view and its token (secret).
pub(crate) struct Minted {
    /// The stream (no token).
    pub view: StreamView,
    /// The plaintext token (for the owner, or the session that owns it).
    pub token: String,
}

/// Create a cast stream for `principal` exactly as `POST /cast/streams`
/// does (authorization of both ends, limiter, feed choice, take, activate,
/// audit). `Ok(Err(response))` is a refusal to answer as it is; the owner of
/// the stream is `principal`.
pub(crate) async fn mint_stream(
    state: &AppState,
    principal: &Principal,
    req: &CreateCastStream,
) -> ControlResult<Result<Minted, Response>> {
    let state = state.clone();
    let principal = principal.clone();
    crate::cast_streams::lifecycle::sweep_now(&state);
    // Authorization of both ends before any existence answer (as the take).
    let chosen_output = match &req.output {
        Some(output) => {
            crate::routes::output_routing::authorize_output_end(&principal, output)?;
            Some(output.clone())
        }
        None => None,
    };
    let wanted = match (&req.feed, &chosen_output) {
        (Some(feed), _) => Some(feed.clone()),
        (None, None) => Some(VideoFeed::PROGRAM),
        (None, Some(_)) => None,
    };
    let layout = match &wanted {
        Some(feed) => authorized_feed(&state, &principal, feed).await?,
        None => None,
    };
    if let Err(retry) = state.cast_streams.admit_create(&principal.key_id) {
        return Ok(Err(crate::routes::output_routing::too_many(retry)));
    }
    if state.cast_engine.is_none() {
        return Ok(Err(unavailable("The engine is not running a cast output")));
    }
    let (choice, feed, layout_body) = if let Some(output) = chosen_output {
        match by_output(&state, &principal, &output, wanted.as_ref(), layout).await? {
            Ok(found) => found,
            Err(response) => return Ok(Err(response)),
        }
    } else {
        {
            let feed = wanted.unwrap_or(VideoFeed::PROGRAM);
            let (routed, body) = match crate::routes::output_routing::resolve_feed(
                &state,
                &principal,
                &feed,
                layout.as_ref(),
            )? {
                Ok(found) => found,
                Err((status, kind, detail)) => {
                    return Ok(Err(crate::routes::output_routing::refusal_problem(
                        status, kind, detail,
                    )))
                }
            };
            match choose(&state, &principal, &routed) {
                Ok(choice) => (choice, routed, body),
                Err(response) => return Ok(Err(*response)),
            }
        }
    };
    open_stream(&state, &principal, choice, feed, layout_body).await
}

type Opened = (
    Choice,
    RoutedFeed,
    Option<crate::output_routing::LayoutBody>,
);

/// The `output` form: that cast output, showing `wanted` (taken to it when
/// free) or, without a feed, what it shows now (authorized like a feed).
async fn by_output(
    state: &AppState,
    principal: &Principal,
    output: &str,
    wanted: Option<&VideoFeed>,
    layout: Option<crate::routes::output_routing::LayoutRead>,
) -> ControlResult<Result<Opened, Response>> {
    let Some(candidate) = candidates(state, principal)
        .into_iter()
        .find(|(_, id)| id == output)
    else {
        return Err(ControlError::NotFound {
            kind: "cast output",
            id: output.to_owned(),
        });
    };
    if let Some(error) = &candidate.0.last_error {
        return Ok(Err(unavailable(error.clone())));
    }
    let shows = crate::cast_streams::lifecycle::live_feed(state, &candidate.0.id);
    let (feed, layout) = if let Some(feed) = wanted {
        (feed.clone(), layout)
    } else {
        {
            let Some(current) = shows.clone() else {
                return Ok(Err(unavailable(
                    "The cast output has not reported what it shows yet; try again",
                )));
            };
            let feed = current.video_feed();
            let layout = authorized_feed(state, principal, &feed).await?;
            (feed, layout)
        }
    };
    let (routed, body) = match crate::routes::output_routing::resolve_feed(
        state,
        principal,
        &feed,
        layout.as_ref(),
    )? {
        Ok(found) => found,
        Err((status, kind, detail)) => {
            return Ok(Err(crate::routes::output_routing::refusal_problem(
                status, kind, detail,
            )))
        }
    };
    if shows.as_ref() == Some(&routed) {
        return Ok(Ok((Choice::Shows(candidate), routed, body)));
    }
    let busy = state
        .cast_streams
        .slots()
        .iter()
        .any(|s| s.config_id == candidate.0.id && s.streams > 0);
    if busy {
        return Ok(Err(cast_problem(
            409,
            "cast-capacity",
            "This cast output is casting another feed: choose another cast output or stop that cast",
        )));
    }
    Ok(Ok((Choice::Take(candidate), routed, body)))
}

fn engine_refused(error: &crate::cast_engine::CastEngineError) -> Response {
    unavailable(error.to_string())
}

/// Claim, prepare the output (take + activate) and answer `201`.
async fn open_stream(
    state: &AppState,
    principal: &Principal,
    choice: Choice,
    feed: RoutedFeed,
    layout: Option<crate::output_routing::LayoutBody>,
) -> ControlResult<Result<Minted, Response>> {
    use crate::cast_streams::lifecycle::{take_and_wait, TakeResult};
    use crate::cast_streams::ClaimError;
    let (take, (status, output)) = match choice {
        Choice::Shows(c) => (false, c),
        Choice::Take(c) => (true, c),
    };
    if take {
        if let Some(refused) = cast_plan_gate(state, principal, &status.id, &feed) {
            return Ok(Err(refused));
        }
    }
    let claim = match state
        .cast_streams
        .claim(&principal.key_id, &output, &status.id, &feed, take)
    {
        Ok(claim) => claim,
        Err(ClaimError::Capacity | ClaimError::PrincipalCapacity | ClaimError::Taken) => {
            return Ok(Err(cast_problem(
                409,
                "cast-capacity",
                "Too many casts: stop a cast and try again",
            )))
        }
    };
    let engine = state.cast_engine.clone();
    let Some(engine) = engine else {
        state.cast_streams.release_claim(&claim.id, claim.first);
        return Ok(Err(unavailable("The engine is not running a cast output")));
    };
    if take {
        let result = take_and_wait(state, &status.id, &feed, layout, &principal.key_id).await;
        // Anything enqueued may have moved (or still move) the output.
        let enqueued = !matches!(result, TakeResult::NotEnqueued(_));
        let refused = match result {
            TakeResult::Applied => None,
            TakeResult::Refused(outcome) => Some(cast_problem(
                409,
                &outcome.replace(':', "-"),
                format!("The engine refused the take onto the cast output ({outcome})"),
            )),
            TakeResult::TimedOut => Some(ControlError::EngineBusy.into_response()),
            TakeResult::NotEnqueued(crate::output_routing::TakeSubmitError::Busy) => {
                Some(ControlError::EngineBusy.into_response())
            }
            TakeResult::NotEnqueued(crate::output_routing::TakeSubmitError::Unroutable) => {
                Some(crate::routes::output_routing::unroutable_kind())
            }
        };
        if let Some(response) = refused {
            if enqueued {
                // A take that may still land: let the output linger and be
                // taken back to Program like any ended cast.
                state.cast_streams.mark_moved(&status.id);
                state.cast_streams.remove(&claim.id);
            } else {
                state.cast_streams.release_claim(&claim.id, claim.first);
            }
            crate::cast_streams::lifecycle::sweep_now(state);
            return Ok(Err(response));
        }
        state.cast_streams.mark_moved(&status.id);
    }
    // Pre-roll only when no take was needed: the buffered packets are the
    // picture the receiver asked for.
    if let Err(error) = engine.activate(&status.id, !take) {
        if take {
            // Moved already: linger, then back to Program.
            state.cast_streams.remove(&claim.id);
        } else {
            state.cast_streams.release_claim(&claim.id, claim.first);
        }
        crate::cast_streams::lifecycle::sweep_now(state);
        return Ok(Err(engine_refused(&error)));
    }
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Create,
        "cast_stream",
        &claim.id,
        Some(serde_json::json!({
            "output": output,
            "feed": feed.bus_name(),
            "take": take,
        })),
    );
    let Some((view, token)) = state.cast_streams.get(&claim.id) else {
        return Ok(Err(unavailable("The cast stream ended before it started")));
    };
    Ok(Ok(Minted { view, token }))
}
