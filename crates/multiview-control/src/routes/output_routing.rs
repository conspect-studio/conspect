//! `POST /api/v1/routing/plan` (output target) and
//! `POST /api/v1/routing/output/take` (ADR-0108 slice 1, design §6.2-§6.4).
//!
//! Order of checks on both routes, so nothing leaks and nothing is burned:
//! role → destination `authorize_object(output)` → feed authorization
//! (source: `authorize_object(source)`; bus: Program scope + `me1`) — all
//! **before** existence, so an id the caller may not read is a 403 whether or
//! not it exists. Then the take checks the `Idempotency-Key` (a replay answers
//! from the record), the rate limits (429 + `Retry-After`), existence (404 /
//! `unknown-feed`), and only then reserves the key and enqueues
//! (`try_send`; full → 503 `engine-busy`, the reservation released).

use std::time::Instant;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::VideoFeed;
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::concurrency::{IdempotencyKey, Reservation};
use crate::error::{ControlError, ControlResult};
use crate::output_routing::{
    OutputTakeRequest, PendingTake, PlanRefusal, RoutedFeed, TakeSubmitError,
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
    /// The feed: `{"bus":"program"|"preview"}` or `{"source":"vt1"}`.
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
fn capacity_text(principal: &Principal, in_use: usize, max: usize) -> String {
    if principal.is_global() {
        format!(
            "This output needs its own encoder and the GPU has no free encoder session ({in_use} of {max} in use)"
        )
    } else {
        "No free encoder session for this output".to_owned()
    }
}

/// Authorize both ends (§6.3), before any existence check.
fn authorize_ends(principal: &Principal, req: &OutputRouteRequest) -> ControlResult<()> {
    crate::auth::authorize_object(principal, req.output())?;
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

/// Resolve the request's output API id to its config id (`None` = unknown).
fn config_id_of(state: &AppState, api_id: &str) -> ControlResult<Option<String>> {
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
fn source_exists(state: &AppState, id: &str) -> ControlResult<bool> {
    let config: multiview_config::MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))?;
    Ok(config.sources.iter().any(|s| s.id == id))
}

/// The feed as a slice-1 routed feed, after existence checks: `Err` is the
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
                detail: "Preview needs a switcher (M/E) in this run".to_owned(),
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

/// The `202` of a replayed key: the reserved op and, once known, its outcome.
/// Never re-enqueues and never answers `adopt` again: the take record lives
/// exactly as long as the key, so a replay always meets its own operation.
fn replayed(control: &crate::output_routing::OutputRoutingControl, op: &str) -> Response {
    let recorded = control.outcome_of(op);
    accepted(OutputTakeAccepted {
        operation_id: op.to_owned(),
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
            "Preview needs a switcher (M/E) in this run".to_owned(),
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
            (status = 503, description = "`engine-busy`: the take mailbox is full.", body = crate::problem::Problem),
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
    let Some(control) = state.output_routing.clone() else {
        return Ok(routing_unavailable());
    };
    let Some(key) = idem.0.as_deref() else {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    };
    // A replay answers from the record without re-running anything.
    if let Some(op) = state.idempotency.peek(key) {
        return Ok(replayed(&control, op.as_str()));
    }
    let Some(config_id) = config_id_of(&state, req.output())? else {
        return Err(ControlError::NotFound {
            kind: "output",
            id: req.output().to_owned(),
        });
    };
    // Limits key on existing outputs only, so unknown ids cannot churn the map.
    if let Err(retry) = control
        .limiter
        .admit(req.output(), &principal.key_id, Instant::now())
    {
        return Ok(too_many(retry));
    }
    let feed = match resolve_feed(&state, &req.feed)? {
        Ok(feed) => feed,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = plan_gate(&control, &principal, &config_id, &feed) {
        return Ok(refused);
    }
    let op = match state.idempotency.reserve(Some(key)) {
        Reservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        // Lost a race with a concurrent request bearing the same key.
        Reservation::Replay(op) => return Ok(replayed(&control, op.as_str())),
        Reservation::Fresh(op) => op,
    };
    // Track before the enqueue so an outcome can never beat its record.
    control.track(
        op.as_str(),
        PendingTake {
            api_id: req.output().to_owned(),
            feed: feed.clone(),
        },
    );
    let take = OutputTakeRequest {
        operation_id: op.to_string(),
        output: config_id,
        feed: feed.clone(),
        principal: principal.key_id.clone(),
    };
    if let Err(error) = control.routing.try_take(take) {
        control.untrack(op.as_str());
        state.idempotency.release(Some(key), &op);
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
            "feed": feed.bus_name(),
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
