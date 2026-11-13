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
use multiview_config::VideoFeed;
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::output_routing::{
    OutputTakeRequest, PendingTake, PlanRefusal, RoutedFeed, TakeReplay, TakeReservation,
    TakeSubmitError,
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
    /// The stream re-pointed: only `video` in slice 1.
    pub stream: OutputStream,
}

/// The stream of an output a take re-points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// The output's video feed.
    Video,
}

/// The target of an output crosspoint, tagged on `kind` (never untagged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputTargetKind {
    /// An output's feed.
    Output(OutputTarget),
}

/// The audio request of an output take; slice 1 accepts only `follow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OutputTakeAudio {
    /// The audio that belongs to the video feed.
    Follow,
}

/// The body of an output plan / take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct OutputRouteRequest {
    /// The output and stream.
    pub target: OutputTargetKind,
    /// The feed: `{"bus":"program"|"preview"|"clean"}` or `{"source":"vt1"}`.
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub feed: VideoFeed,
    /// Optional audio (take only; `follow` is the only slice-1 value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<OutputTakeAudio>,
}

impl OutputRouteRequest {
    fn output(&self) -> &str {
        match &self.target {
            OutputTargetKind::Output(t) => &t.output,
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
}

/// A typed plan refusal (RFC 9457 `type` suffix + operator text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputPlanRefusal {
    /// `capacity` | `unknown-feed` | `switcher-required` | `output-kind-unroutable`.
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

/// Authorize both ends (§6.3), before any existence check. The output end
/// is the patch panel's output-slot check (§12.8, #342 review A2): the object
/// AND the output axis.
fn authorize_ends(principal: &Principal, req: &OutputRouteRequest) -> ControlResult<()> {
    authorize_output_slot(principal, req.output())?;
    match &req.feed {
        VideoFeed::Source(id) | VideoFeed::Layout(id) => {
            crate::auth::authorize_object(principal, id)?;
        }
        VideoFeed::Bus(_) => {
            crate::auth::authorize_scope(
                principal,
                multiview_events::AuthzScope::Program(multiview_config::MAIN_PROGRAM),
            )?;
            crate::auth::authorize_object(principal, ME_ID)?;
        }
        _ => return Err(ControlError::Forbidden("unsupported feed kind".to_owned())),
    }
    Ok(())
}

/// The output-slot destination check shared by `/routing/output/take` and the
/// patch routes: `ObjectAndOutput` and `authorize_output`.
///
/// # Errors
/// `Forbidden` when either axis denies `output`.
pub(crate) fn authorize_output_slot(principal: &Principal, output: &str) -> ControlResult<()> {
    crate::auth::authorize_scope(
        principal,
        multiview_events::AuthzScope::ObjectAndOutput {
            object: output,
            output,
        },
    )?;
    crate::auth::authorize_output(principal, output)
}

pub(crate) fn routing_unavailable() -> Response {
    Problem::new(409, "routing-unavailable", "Output routing unavailable")
        .with_detail("this run does not route output feeds")
        .into_response()
}

fn refusal_problem(status: u16, kind: &str, detail: String) -> Response {
    Problem::new(status, kind, "Take refused")
        .with_detail(detail)
        .into_response()
}

/// Resolve the request's output API id to its config id (`None` = unknown).
pub(crate) fn config_id_of(state: &AppState, api_id: &str) -> ControlResult<Option<String>> {
    let resources = crate::boot_model::accepted_resources(state, "outputs")?
        .map_or_else(|| state.outputs.list(), Ok)?;
    for versioned in resources {
        if versioned.resource.id == api_id {
            let body: multiview_config::Output = serde_json::from_value(versioned.resource.body)
                .map_err(|e| ControlError::Repository(format!("output {api_id:?} body: {e}")))?;
            return Ok(Some(body.id()));
        }
    }
    Ok(None)
}

/// Whether `id` is a source the accepted document declares.
pub(crate) fn source_exists(state: &AppState, id: &str) -> ControlResult<bool> {
    let config: multiview_config::MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))?;
    Ok(config.sources.iter().any(|s| s.id == id))
}

/// The feed as a routed feed (Program, Preview, Clean or a source), after existence checks: `Err` is the
/// typed refusal response (`unknown-feed`, `output-kind-unroutable`).
pub(crate) fn resolve_feed(
    state: &AppState,
    feed: &VideoFeed,
) -> ControlResult<Result<RoutedFeed, Response>> {
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
    let Some(control) = state.output_routing.as_ref() else {
        return Ok(routing_unavailable());
    };
    let Some(config_id) = config_id_of(state, req.output())? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: req.output().to_owned(),
        });
    };
    let mut refusals = Vec::new();
    let mut cost = OutputPlanCost {
        new_encoders: 0,
        encoder_sessions: None,
        fits: true,
    };
    match resolve_feed(state, &req.feed)? {
        Err(_) => refusals.push(OutputPlanRefusal {
            kind: "unknown-feed".to_owned(),
            detail: "the feed does not exist or cannot be routed".to_owned(),
        }),
        Ok(feed) => match control.routing.plan(&config_id, &feed) {
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
                        detail: capacity_text(
                            principal,
                            planned.sessions_in_use,
                            planned.sessions_max,
                        ),
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
        },
    }
    let plan = OutputPlan {
        class: crate::routing::RouteClass::Class1,
        cost,
        refusals,
    };
    Ok((StatusCode::OK, Json(plan)).into_response())
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
        Ok(cost) if !cost.fits() => Some(refusal_problem(
            409,
            "capacity",
            capacity_text(principal, cost.sessions_in_use, cost.sessions_max),
        )),
        Ok(_) => None,
    }
}

/// `POST /api/v1/routing/output/take` — take an output feed (role: write;
/// both ends authorized; `Idempotency-Key` required). `202 {operation_id,
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
            (status = 409, description = "`routing-unavailable`, `switcher-required`, `output-kind-unroutable` or `capacity`.", body = crate::problem::Problem),
            (status = 422, description = "`unknown-feed`, missing Idempotency-Key, or an unsupported audio request.", body = crate::problem::Problem),
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
    let feed = req.feed.clone();
    let output = req.output().to_owned();
    let accepted_take = match take_output_slot0(&state, &principal, &idem, &output, &feed)? {
        SlotTake::Accepted(op) => op,
        SlotTake::Replay(replay) => return Ok(replayed(replay)),
        SlotTake::Refused(response) => return Ok(response),
    };
    Ok(accepted(OutputTakeAccepted {
        operation_id: accepted_take,
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
    /// The key already names a take: answer it, never enqueue again.
    Replay(TakeReplay),
    /// A typed refusal response (409/422/429/503...).
    Refused(Response),
}

/// Take `feed` onto output `output`'s video slot 0 — the ONE take machinery
/// behind `POST /routing/output/take` and an output crosspoint of
/// `POST /patch/take` (#342 B2): the take store's replay, the per-output and
/// per-principal limits, existence, the plan gate, the reservation, the
/// bounded `try_send` and the audit. The caller has checked the role and
/// authorized both ends.
///
/// # Errors
/// Repository errors, `NotFound` for an unknown output, a missing key, or
/// `idempotency-capacity`.
pub(crate) fn take_output_slot0(
    state: &AppState,
    principal: &Principal,
    idem: &IdempotencyKey,
    output: &str,
    video: &VideoFeed,
) -> ControlResult<SlotTake> {
    let Some(control) = state.output_routing.clone() else {
        return Ok(SlotTake::Refused(routing_unavailable()));
    };
    let Some(key) = idem.0.as_deref() else {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    };
    let key = slot0_take_key(key);
    // A replay answers from the take's own record without re-running
    // anything. `key` is already scoped by principal and the shared
    // slot-0 take namespace.
    if let Some(replay) = control.replay(&key) {
        return Ok(SlotTake::Replay(replay));
    }
    let Some(config_id) = config_id_of(state, output)? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: output.to_owned(),
        });
    };
    // Limits key on existing outputs only, so unknown ids cannot churn the map.
    if let Err(retry) = control
        .limiter
        .admit(output, &principal.key_id, Instant::now())
    {
        return Ok(SlotTake::Refused(too_many(retry)));
    }
    let feed = match resolve_feed(state, video)? {
        Ok(feed) => feed,
        Err(response) => return Ok(SlotTake::Refused(response)),
    };
    if let Some(refused) = plan_gate(&control, principal, &config_id, &feed) {
        return Ok(SlotTake::Refused(refused));
    }
    // The record exists before the enqueue, so an outcome never beats it.
    let pending = PendingTake {
        api_id: output.to_owned(),
        feed: feed.clone(),
    };
    let op = match control.reserve_take(&key, pending) {
        TakeReservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        // Lost a race with a concurrent request bearing the same key.
        TakeReservation::Replay(replay) => return Ok(SlotTake::Replay(replay)),
        TakeReservation::Fresh(op) => op,
    };
    let take = OutputTakeRequest {
        operation_id: op.to_string(),
        output: config_id,
        feed: feed.clone(),
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
        output,
        Some(serde_json::json!({
            "command": "route_output_take",
            "feed": feed.bus_name(),
            "operation_id": op.to_string(),
        })),
    );
    Ok(SlotTake::Accepted(op.to_string()))
}

/// The take-store key of a slot-0 take: the scoped `Idempotency-Key` with
/// its request path replaced by one shared namespace, so `/routing/output/take`
/// and an output crosspoint of `/patch/take` replay each other (the alias,
/// #342 B2). The principal and raw key stay in the scope; only the path is
/// unified.
#[must_use]
pub(crate) fn slot0_take_key(scoped: &str) -> String {
    const ALIASES: [&str; 2] = ["/routing/output/take", "/patch/take"];
    for path in ALIASES {
        let needle = format!(":{}:{path}:", path.len());
        if let Some(at) = scoped.find(&needle) {
            let shared = "output-slot0-take";
            return format!(
                "{}:{}:{shared}:{}",
                &scoped[..at],
                shared.len(),
                &scoped[at + needle.len()..]
            );
        }
    }
    scoped.to_owned()
}
