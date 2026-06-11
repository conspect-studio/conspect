//! The per-stream **crosspoint routing** surface under `/api/v1/routing`
//! (ADR-0034 §9 / RT-11).
//!
//! * `POST /api/v1/routing/plan` — classify a crosspoint take **without**
//!   applying. Returns the #11 class (`class1` / `reset_lite` / `class2`) + a
//!   coerced-degradation flag (invariant #11: the class is surfaced *before* the
//!   take).
//! * `POST /api/v1/routing/{video|audio|subtitle}/take` — resolve the class,
//!   submit the matching `Command::Route*` on the engine command bus, and return
//!   **`200 {class, applied}`** for a hot Class-1 (or Reset-lite) re-point vs
//!   **`202 {operation_id}`** for a Class-2 migration (the engine drives the
//!   make-before-break asynchronously; the outcome rides the realtime stream).
//!
//! Reuses [`submit_accepted`](crate::routes::submit_accepted) so every take
//! inherits the `Idempotency-Key` dedupe, the RFC 9457 `problem+json` errors, the
//! BOLA [`authorize_object`](crate::auth::authorize_object) per-destination guard,
//! and the shed-to-`503` non-blocking submit (invariant #10 — a full bus never
//! blocks the engine). The classifier reads the source's
//! [`StreamInventory`](multiview_core::stream::StreamInventory) from the
//! **off-engine** cached snapshot (inv #10), never the output-clock thread.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::routing::StreamRef;
use multiview_core::stream::{
    StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::routes::submit_accepted;
use crate::routing::{
    classify, DestinationProfile, RouteClass, RoutePlan, RouteRequest, RouteTarget,
};
use crate::state::AppState;

/// The body of a `POST /routing/plan` or `/routing/{kind}/take` request.
///
/// Carries the destination + source, plus optional classifier hints used when the
/// engine snapshot does not (yet) carry the source inventory or the destination's
/// pinned params (so a plan/take is classifiable before the first probe):
///
/// * `source_channels` — the source audio track's channel count (used for the
///   discrete-track layout comparison when the inventory is absent);
/// * `coerce` — the operator confirms a down/up-mix to the destination's pinned
///   layout (turns a would-be Class-2 audio mismatch into Class-1-with-degradation);
/// * `primed` — whether the video target source is warm (absent ⇒ assumed primed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RouteTakeRequest {
    /// The destination the take re-points.
    #[cfg_attr(feature = "openapi", schema(value_type = crate::openapi_schemas::RouteTargetDoc))]
    pub target: RouteTarget,
    /// The source elementary stream feeding the destination.
    #[cfg_attr(feature = "openapi", schema(value_type = crate::openapi_schemas::StreamRefDoc))]
    pub source: StreamRef,
    /// Optional source audio channel-count hint (discrete-track layout compare).
    #[serde(default)]
    pub source_channels: Option<u16>,
    /// Optional operator-confirmed coercion to the pinned layout.
    #[serde(default)]
    pub coerce: bool,
    /// Optional video-target priming hint (absent ⇒ assumed primed).
    #[serde(default)]
    pub primed: Option<bool>,
}

/// The `200` body of a hot (Class-1 / Reset-lite) `/routing/{kind}/take`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TakeApplied {
    /// The #11 class the take resolved to (`class1` / `reset_lite`).
    pub class: RouteClass,
    /// Always `true` for a hot take (the route command was submitted).
    pub applied: bool,
    /// Whether the take was coerced to Class-1 with a down/up-mix degradation.
    pub coerced: bool,
    /// The operation id correlating the take's outcome on the realtime stream.
    pub operation_id: String,
}

/// The label of the destination resource for BOLA / audit / `404`s.
const ROUTING_KIND: &str = "crosspoint";

/// `POST /api/v1/routing/plan` — classify a crosspoint take without applying
/// (role: read; per-destination authz). Returns the #11 [`RoutePlan`].
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/routing/plan",
        tag = "routing",
        request_body = RouteTakeRequest,
        responses(
            (status = 200, description = "The #11 classification (class1/reset_lite/class2 + coerced).", body = crate::routing::RoutePlan),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to plan this crosspoint.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn plan_route(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<RouteTakeRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, destination_id(&req.target))?;
    let plan = classify_request(&state, &req);
    Ok((StatusCode::OK, Json(plan)).into_response())
}

/// `POST /api/v1/routing/{kind}/take` — take (apply) a crosspoint (role: write;
/// per-destination authz; Idempotency-Key). `200 {class, applied}` for a hot
/// Class-1/Reset-lite re-point; `202 {operation_id}` for a Class-2 migration.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/routing/{kind}/take",
        tag = "routing",
        params(("kind" = String, Path, description = "Crosspoint kind: video | audio | subtitle.")),
        request_body = RouteTakeRequest,
        responses(
            (status = 200, description = "A hot Class-1/Reset-lite re-point was submitted.", body = TakeApplied),
            (status = 202, description = "A Class-2 migration was accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 400, description = "The path kind does not match the target kind.", body = crate::problem::Problem),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to take this crosspoint.", body = crate::problem::Problem),
            (status = 404, description = "Unknown crosspoint kind.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn take_route(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(kind): Path<String>,
    Json(req): Json<RouteTakeRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // The path kind must address one of the known crosspoint kinds, and must
    // agree with the target's kind (a `/audio/take` cannot re-point a video cell).
    let path_kind = CrosspointKind::parse(&kind).ok_or_else(|| ControlError::NotFound {
        kind: ROUTING_KIND,
        id: kind.clone(),
    })?;
    if path_kind != CrosspointKind::of_target(&req.target) {
        return Err(ControlError::Validation(format!(
            "routing/{kind}/take target is not a {kind} crosspoint"
        )));
    }
    let destination = destination_id(&req.target).to_owned();
    crate::auth::authorize_object(&principal, &destination)?;

    // Classify against the destination's pinned params (inv #11: surfaced first).
    let plan = classify_request(&state, &req);

    // Build the route command for the engine command bus, then submit it
    // non-blocking (idempotency + shed-503 via `submit_accepted`).
    let source = req.source.clone();
    let target = req.target.clone();
    let response = submit_accepted(&state, &idem, |op| {
        build_route_command(op, &target, &source)
    })?;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        ROUTING_KIND,
        &destination,
        Some(serde_json::json!({
            "command": "route_take",
            "class": plan.class.as_str(),
            "coerced": plan.coerced,
        })),
    );

    // A Class-2 migration is asynchronous: return the `202 Accepted` body
    // `submit_accepted` produced (carrying the operation id). A hot Class-1 /
    // Reset-lite re-point applies at the next frame boundary: surface `200`
    // with the class + applied flag, reusing the same submitted operation id.
    if plan.class == RouteClass::Class2 {
        return Ok(response);
    }
    let operation_id = accepted_operation_id(response).await?;
    let body = TakeApplied {
        class: plan.class,
        applied: true,
        coerced: plan.coerced,
        operation_id,
    };
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// Re-read the `operation_id` `submit_accepted` minted from its `202` body, so a
/// hot take can echo the same id on its `200` (idempotency replay included). The
/// body is the crate's own `AcceptedBody` JSON; a malformed one is a loud
/// repository error rather than a panic.
async fn accepted_operation_id(response: Response) -> Result<String, ControlError> {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .map_err(|e| ControlError::Repository(format!("reading the accepted body: {e}")))?;
    let body: crate::routes::AcceptedBody = serde_json::from_slice(&bytes)
        .map_err(|e| ControlError::Repository(format!("parsing the accepted body: {e}")))?;
    Ok(body.operation_id)
}

/// Build the `Command::Route*` a crosspoint take submits, desugaring the target +
/// source into the matching per-stream route command (ADR-0034 / RT-11).
fn build_route_command(op: OperationId, target: &RouteTarget, source: &StreamRef) -> Command {
    match target {
        RouteTarget::VideoCell { cell } => Command::RouteVideo {
            op,
            cell: cell.clone(),
            source: source.clone(),
        },
        RouteTarget::AudioProgramBus { channel } => Command::RouteAudio {
            op,
            target: channel.clone(),
            source: source.clone(),
            gain_db: 0.0,
            mute: false,
        },
        RouteTarget::AudioDiscreteTrack { track, .. } => Command::RouteAudio {
            op,
            target: track.clone(),
            source: source.clone(),
            gain_db: 0.0,
            mute: false,
        },
        RouteTarget::SubtitleLayer { layer } => Command::RouteSubtitle {
            op,
            layer: layer.clone(),
            source: source.clone(),
        },
    }
}

/// Classify a take request: resolve the source inventory (from the off-engine
/// snapshot, else a hint-built synthetic one) + the destination profile, then run
/// the #11 [`classify`].
fn classify_request(state: &AppState, req: &RouteTakeRequest) -> RoutePlan {
    let inventory = source_inventory(state, req);
    let dest = destination_profile(req);
    let domain = RouteRequest {
        target: req.target.clone(),
        source: req.source.clone(),
    };
    classify(&domain, &inventory, &dest)
}

/// The source input's [`StreamInventory`] for classification: the cached
/// off-engine snapshot entry (inv #10) when present, else a synthetic inventory
/// built from the `source_channels` hint so a pre-probe take is still
/// classifiable. Falls back to an empty inventory (the classifier then treats an
/// unknown source layout as the absorbing case).
fn source_inventory(state: &AppState, req: &RouteTakeRequest) -> StreamInventory {
    if let Some(inv) = snapshot_inventory(state, &req.source.input_id) {
        return inv;
    }
    // No snapshot inventory: synthesise from the channel hint so the discrete-
    // track layout comparison can still run before the first probe.
    if let Some(channels) = req.source_channels {
        if req.source.kind == StreamKind::Audio {
            return StreamInventory::from_streams(vec![StreamDescriptor::new(
                StableStreamId::from_ts_pid(StreamKind::Audio, 0),
                StreamKind::Audio,
                "pcm",
                StreamDetail::Audio {
                    channels,
                    sample_rate: 48_000,
                },
            )])
            .with_input_id(req.source.input_id.clone());
        }
    }
    StreamInventory::new().with_input_id(req.source.input_id.clone())
}

/// Pull one input's inventory out of the conflated engine snapshot blob (the same
/// projection `routes/inputs.rs` uses), or [`None`] when absent / malformed.
fn snapshot_inventory(state: &AppState, input_id: &str) -> Option<StreamInventory> {
    let snapshot = state.engine.state.latest()?;
    let entry = snapshot
        .get("inputs")
        .and_then(|inputs| inputs.get(input_id))
        .and_then(|input| input.get("streams"))?;
    serde_json::from_value::<StreamInventory>(entry.clone()).ok()
}

/// Build the destination's pinned-params profile from the request (the
/// classifier's TIER-2 side). The pinned audio layout / coercion / video priming
/// come from the request hints; a program bus / existing layer is the absorbing
/// Class-1 default.
fn destination_profile(req: &RouteTakeRequest) -> DestinationProfile {
    match &req.target {
        RouteTarget::VideoCell { .. } => DestinationProfile::video_cell(req.primed.unwrap_or(true)),
        RouteTarget::AudioProgramBus { .. } => DestinationProfile::audio_program_bus(),
        RouteTarget::AudioDiscreteTrack {
            pinned_channels, ..
        } => {
            // Prefer the explicit pinned-channel count on the target; fall back to
            // the absorbing program-bus-equivalent when unknown.
            let profile = match pinned_channels {
                Some(channels) => DestinationProfile::audio_discrete_track(*channels),
                None => DestinationProfile::audio_program_bus(),
            };
            if req.coerce {
                profile.coerce_to_pinned()
            } else {
                profile
            }
        }
        // A subtitle layer take always addresses an existing layer here (a new
        // track set would be a CRUD create, not a take).
        RouteTarget::SubtitleLayer { .. } => DestinationProfile::subtitle_layer(true),
    }
}

/// The destination id a crosspoint addresses (for BOLA / audit).
fn destination_id(target: &RouteTarget) -> &str {
    match target {
        RouteTarget::VideoCell { cell } => cell,
        RouteTarget::AudioProgramBus { channel } => channel,
        RouteTarget::AudioDiscreteTrack { track, .. } => track,
        RouteTarget::SubtitleLayer { layer } => layer,
    }
}

/// The three crosspoint kinds a `/routing/{kind}/take` path addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrosspointKind {
    Video,
    Audio,
    Subtitle,
}

impl CrosspointKind {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            "subtitle" => Some(Self::Subtitle),
            _ => None,
        }
    }

    fn of_target(target: &RouteTarget) -> Self {
        match target {
            RouteTarget::VideoCell { .. } => Self::Video,
            RouteTarget::AudioProgramBus { .. } | RouteTarget::AudioDiscreteTrack { .. } => {
                Self::Audio
            }
            RouteTarget::SubtitleLayer { .. } => Self::Subtitle,
        }
    }
}
