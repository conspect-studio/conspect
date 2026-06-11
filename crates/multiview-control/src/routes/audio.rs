//! The **audio-routing** singleton surface at `/api/v1/audio-routing`.
//!
//! One document per deployment — the config-as-code `[audio]` block
//! (`multiview_config::AudioRouting`, ADR-R005 §4.1): the working sample rate
//! plus one route per input declaring program-bus membership/gain/mute and an
//! optional named discrete track. The per-output *selection* of those tracks
//! lives on each output resource; this document is what those selections
//! resolve against.
//!
//! * `GET` is **404-free**: an unconfigured deployment answers `200` with
//!   `configured: false` and a `null` document (the singleton always exists).
//! * `PUT` **replaces** the whole document, validated at the boundary: typed
//!   deserialization against `multiview_config::AudioRouting`
//!   (`serde_path_to_error` → `422` naming the field path) plus the routing
//!   block's own semantic validation (duplicate tracks/inputs, the reserved
//!   `prog` name, an all-muted program bus). Resolution against the *declared
//!   sources* is deliberately deferred to `GET /api/v1/config/export`, where
//!   the composed document is validated as a whole — so routing can be authored
//!   before its sources exist.
//!
//! Both verbs ride the standard machinery: `ETag`/`If-Match` optimistic
//! concurrency (ADR-W006; `428` without a precondition, `412` when stale —
//! enforced atomically by
//! [`AudioRoutingStore::replace_if`](crate::audio_routing::AudioRoutingStore::replace_if)),
//! RBAC, an audit record after a successful write, and
//! `X-Multiview-Apply: restart` apply semantics (ADR-W015 §4).
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::audio_routing::{AUDIO_ROUTING_ID, AUDIO_ROUTING_KIND};
use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::{IfMatch, Version};
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;
use crate::typed_resources::with_apply_restart;
use multiview_config::{AudioRouting, PROGRAM_TRACK};

/// The wire envelope both verbs return: whether a document is configured, the
/// document itself (`null` when unconfigured), and the **selectable tracks**
/// the document declares — the program bus `"prog"` (always first, always
/// available) plus every named discrete track in declaration order. This is
/// the same set per-output `audio.tracks` selections resolve against.
#[derive(Debug, Clone, Serialize)]
struct AudioRoutingState {
    /// Whether an audio-routing document is configured.
    configured: bool,
    /// The typed routing document (serialized verbatim), or `null`.
    routing: Option<AudioRouting>,
    /// `"prog"` + every declared discrete track, in declaration order.
    selectable_tracks: Vec<String>,
}

impl AudioRoutingState {
    /// Project a store snapshot onto the wire envelope.
    fn from_snapshot(routing: Option<AudioRouting>) -> Self {
        let selectable_tracks = routing.as_ref().map_or_else(
            || vec![PROGRAM_TRACK.to_owned()],
            |r| {
                r.declared_tracks()
                    .iter()
                    .map(|t| (*t).to_owned())
                    .collect()
            },
        );
        Self {
            configured: routing.is_some(),
            routing,
            selectable_tracks,
        }
    }
}

/// Attach the document `ETag` to a response carrying the routing state.
fn state_response(status: StatusCode, routing: Option<AudioRouting>, version: Version) -> Response {
    let mut response = (status, Json(AudioRoutingState::from_snapshot(routing))).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&version.to_etag()) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// Validate a `PUT` body into the typed routing document.
///
/// Typed deserialization first (the failure detail names the field path), then
/// the routing block's own semantic validation. The *source* cross-check is
/// intentionally vacuous here — each route's own `input_id` is offered as the
/// declared set — because resolution against the real sources happens when the
/// whole configuration is composed (`GET /api/v1/config/export`); the
/// per-route/per-document invariants (empty ids, finite gain, duplicate
/// inputs/tracks, the reserved `prog` name, an all-muted bus, a zero sample
/// rate) are all enforced now.
fn validated_routing(body: &serde_json::Value) -> Result<AudioRouting, ControlError> {
    let routing: AudioRouting = serde_path_to_error::deserialize(body.clone()).map_err(|err| {
        let path = err.path().to_string();
        let message = err.into_inner().to_string();
        ControlError::Validation(if path == "." {
            format!("audio routing document invalid: {message}")
        } else {
            format!("audio routing document invalid at `{path}`: {message}")
        })
    })?;
    let input_ids: Vec<&str> = routing
        .routes
        .iter()
        .map(|route| route.input_id.as_str())
        .collect();
    let declared = routing.declared_tracks();
    routing.validate(&input_ids, &declared).map_err(|err| {
        ControlError::Validation(format!("audio routing document invalid: {err}"))
    })?;
    Ok(routing)
}

/// `GET /api/v1/audio-routing` — the singleton document (role: read).
///
/// **404-free**: an unconfigured deployment answers `200` with
/// `configured: false`, `routing: null`, and `selectable_tracks: ["prog"]`
/// (the program bus is always selectable). The response `ETag` is the document
/// version a later `PUT` must present as `If-Match`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/audio-routing",
        tag = "audio",
        responses(
            (status = 200, description = "The audio-routing document (404-free: `configured: false` + a null document when none is set; ETag in the response header).", body = crate::openapi_schemas::AudioRoutingStateDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_audio_routing(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let (routing, version) = state.audio_routing.snapshot();
    Ok(state_response(StatusCode::OK, routing, version))
}

/// `PUT /api/v1/audio-routing` — replace the singleton document (role: write;
/// `If-Match` required → `428`/`412`).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/audio-routing",
        tag = "audio",
        request_body = crate::openapi_schemas::AudioRoutingDoc,
        responses(
            (status = 200, description = "The replaced document (new ETag in the response header; X-Multiview-Apply: restart — it takes effect via config export + restart).", body = crate::openapi_schemas::AudioRoutingStateDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid audio-routing document (detail names the field path or the violated routing invariant; references to undeclared sources are checked at config export, not here).", body = crate::problem::Problem),
            (status = 428, description = "No If-Match precondition was sent.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn put_audio_routing(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Json(body): Json<serde_json::Value>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // Preconditions are evaluated before request content (RFC 9110 §13.2.2):
    // a missing/stale `If-Match` is reported even when the body is invalid.
    let current = state.audio_routing.version();
    if_match.require(AUDIO_ROUTING_KIND, AUDIO_ROUTING_ID, current)?;
    let routing = validated_routing(&body)?;
    // The compare-and-swap re-checks the version under the store lock, so two
    // racing PUTs that both passed the read above cannot both win.
    let version = state
        .audio_routing
        .replace_if(current, routing.clone())
        .map_err(|actual| ControlError::VersionConflict {
            kind: AUDIO_ROUTING_KIND,
            id: AUDIO_ROUTING_ID.to_owned(),
            expected: current.get().to_string(),
            actual: actual.get().to_string(),
        })?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        AUDIO_ROUTING_KIND,
        AUDIO_ROUTING_ID,
        serde_json::to_value(&routing).ok(),
    );
    Ok(with_apply_restart(state_response(
        StatusCode::OK,
        Some(routing),
        version,
    )))
}
