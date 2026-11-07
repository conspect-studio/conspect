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
//! RBAC, an audit record after a successful write, and honest
//! `X-Multiview-Apply` semantics (ADR-W015 §4): `live` when the running
//! program bus accepted the new gain table (include/exclude, `gain_db`, `mute`
//! land within a couple of ticks on a short click-free ramp), `restart` when
//! only a restart can honour it (no program bus running, a changed
//! `sample_rate_hz`, or a mixed input with no audio store on this run). Either
//! way the stored document is persisted to `active.toml`.
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
use crate::typed_resources::{with_apply, ApplyMode};
use multiview_config::{propagate_routing_change, AudioRouting, PROGRAM_TRACK};

/// One discrete-track rename a routing replace's propagation applied.
#[derive(Debug, Clone, Serialize)]
struct RenamedTrack {
    /// The track name this output's selection carried before the replace.
    from: String,
    /// The name it now carries.
    to: String,
}

/// One output a routing replace's rename/removal propagation rewrote
/// (ADR-R005 §4.3), as reported to the caller: which discrete tracks it lost
/// and which were renamed in place. Mirrors
/// [`multiview_config::OutputAudioUpdate`], but named by the output's stable
/// REST/API id (the config crate only knows the output's slot index).
#[derive(Debug, Clone, Serialize)]
struct OutputsUpdatedEntry {
    /// The output's stable REST id (`/api/v1/outputs/{id}`).
    output: String,
    /// Track names dropped from this output's selection.
    removed: Vec<String>,
    /// Discrete tracks this output's selection now follows under a new name.
    renamed: Vec<RenamedTrack>,
}

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
    /// Outputs a successful `PUT`'s rename/removal propagation rewrote
    /// (ADR-R005 §4.3), in output order. Always empty on `GET`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    outputs_updated: Vec<OutputsUpdatedEntry>,
}

impl AudioRoutingState {
    /// Project a store snapshot onto the wire envelope (`GET`, or a `PUT` that
    /// propagated nothing).
    fn from_snapshot(routing: Option<AudioRouting>) -> Self {
        Self::from_snapshot_with_updates(routing, Vec::new())
    }

    /// Project a store snapshot plus the outputs a `PUT` rewrote.
    fn from_snapshot_with_updates(
        routing: Option<AudioRouting>,
        outputs_updated: Vec<OutputsUpdatedEntry>,
    ) -> Self {
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
            outputs_updated,
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

/// Attach the document `ETag` to a `PUT` response, additionally reporting the
/// outputs the rename/removal propagation rewrote.
fn state_response_with_updates(
    status: StatusCode,
    routing: Option<AudioRouting>,
    version: Version,
    outputs_updated: Vec<OutputsUpdatedEntry>,
) -> Response {
    let mut response = (
        status,
        Json(AudioRoutingState::from_snapshot_with_updates(
            routing,
            outputs_updated,
        )),
    )
        .into_response();
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
    let (routing, version) = crate::boot_model::accepted_audio(&state)?;
    Ok(state_response(StatusCode::OK, routing, version))
}

/// Compute the rename/removal propagation (ADR-R005 §4.3) against the
/// `prospective` config's outputs (mutated in place), enforcing per-object
/// authorization on every output it would touch before returning. Returns the
/// wire-shaped report for the response (empty when nothing was touched).
///
/// # Errors
///
/// [`ControlError::Forbidden`] naming every output the principal may not
/// modify; [`ControlError::Repository`] if the output identity map is
/// inconsistent with the prospective config (a bug, never a user error).
fn apply_routing_propagation(
    principal: &Principal,
    old_routing: Option<&AudioRouting>,
    routing: &AudioRouting,
    prospective: &mut multiview_config::MultiviewConfig,
    output_ids: &[String],
) -> ControlResult<Vec<OutputsUpdatedEntry>> {
    let updates = propagate_routing_change(old_routing, routing, &mut prospective.outputs);
    if updates.is_empty() {
        return Ok(Vec::new());
    }
    // Per-object authorization (BOLA, ADR-W005/ADR-W026): every output this
    // propagation touches must be one the caller may write, not merely the
    // audio-routing singleton itself.
    let mut denied: Vec<&str> = Vec::new();
    let mut ids: Vec<&str> = Vec::with_capacity(updates.len());
    for update in &updates {
        let Some(output_id) = output_ids.get(update.output_index) else {
            return Err(ControlError::Repository(
                "output identity map is inconsistent with the prospective config".into(),
            ));
        };
        if crate::auth::authorize_object(principal, output_id).is_err() {
            denied.push(output_id);
        }
        ids.push(output_id);
    }
    if !denied.is_empty() {
        return Err(ControlError::Forbidden(format!(
            "this routing change would rename/remove discrete tracks on output(s) {denied:?}, \
             which this principal is not authorized to modify; the routing document was not \
             changed"
        )));
    }
    Ok(updates
        .iter()
        .zip(ids)
        .map(|(update, output_id)| OutputsUpdatedEntry {
            output: output_id.to_owned(),
            removed: update.removed.clone(),
            renamed: update
                .renamed
                .iter()
                .map(|(from, to)| RenamedTrack {
                    from: from.clone(),
                    to: to.clone(),
                })
                .collect(),
        })
        .collect())
}

/// Best-effort sync of the legacy output-store projection for every output
/// the propagation rewrote (mirrors `update_output`'s own warn-on-fail
/// pattern): the accepted (desired) record is already authoritative and
/// already carries the rewritten selection, so a failure here never reverts
/// or blocks the response — only a stale read-model cache.
fn sync_output_projections(state: &AppState, outputs_updated: &[OutputsUpdatedEntry]) {
    for entry in outputs_updated {
        let Ok(accepted) = crate::boot_model::accepted_resources(state, "outputs") else {
            continue;
        };
        let Some(versioned) = accepted
            .unwrap_or_default()
            .into_iter()
            .find(|resource| resource.resource.id == entry.output)
        else {
            continue;
        };
        let input = crate::resource_store::ResourceInput {
            name: versioned.resource.name.clone(),
            body: versioned.resource.body.clone(),
        };
        if let Err(error) = state.outputs.update(&entry.output, input) {
            tracing::warn!(
                output = %entry.output,
                %error,
                "audio-routing propagation committed; output store projection failed"
            );
        }
    }
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
            (status = 200, description = "The replaced document (new ETag in the response header; X-Multiview-Apply: live when the running program bus applied it, restart when it needs a restart, e.g. a changed sample rate).", body = crate::openapi_schemas::AudioRoutingStateDoc),
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
    // Serialize against every other config mutation so two racing PUTs reach
    // the engine in the same order they won the store (the lock is control-
    // plane only; the engine never takes it).
    let _mutation = state.lock_config_mutation().await;
    // Preconditions are evaluated before request content (RFC 9110 §13.2.2):
    // a missing/stale `If-Match` is reported even when the body is invalid.
    let (old_routing, current) = crate::boot_model::accepted_audio(&state)?;
    if_match.require(AUDIO_ROUTING_KIND, AUDIO_ROUTING_ID, current)?;
    let routing = validated_routing(&body)?;
    // Validate the entire prospective configuration and persist intent before
    // the store ETag or the live program-bus command can change.
    //
    // ADR-R005 §4.3 (the rename/removal deadlock fix): a routing replace that
    // renames or drops a `target_track` an output's `audio.tracks` selects
    // must rewrite that selection in the SAME atomic commit, never require the
    // operator to stage an intermediate document by hand. Compute the
    // propagation against the PROSPECTIVE config's outputs (the same document
    // the new routing will be validated against) and reject up front, naming
    // every affected output, if the caller lacks per-object authorization on
    // any of them — before any store or file changes.
    let mut outputs_updated: Vec<OutputsUpdatedEntry> = Vec::new();
    if state.boot_model.is_some() {
        let value = serde_json::to_value(&routing)
            .map_err(|e| ControlError::Repository(format!("audio serialization: {e}")))?;
        let mut prospective = crate::boot_model::prospective_config(&state, "audio", value)?;
        let output_ids = crate::boot_model::output_ids_for_config(&state, &prospective)?;
        outputs_updated = apply_routing_propagation(
            &principal,
            old_routing.as_ref(),
            &routing,
            &mut prospective,
            &output_ids,
        )?;
        crate::boot_model::commit_desired(
            &state,
            prospective,
            crate::boot_model::layout_library(&state)?,
        )
        .await?;
    }
    // The compare-and-swap re-checks the version under the store lock, so two
    // racing PUTs that both passed the read above cannot both win.
    let projected = state.audio_routing.version();
    if let Err(actual) = state.audio_routing.replace_if(projected, routing.clone()) {
        if state.boot_model.is_none() {
            return Err(ControlError::VersionConflict {
                kind: AUDIO_ROUTING_KIND,
                id: AUDIO_ROUTING_ID.to_owned(),
                expected: projected.get().to_string(),
                actual: actual.get().to_string(),
            });
        }
        tracing::warn!("accepted audio committed; store projection failed");
    }
    sync_output_projections(&state, &outputs_updated);
    // The committed candidate is already validated. Never let a fallible
    // read of the projection or envelope suppress publication to the live bus.
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        AUDIO_ROUTING_KIND,
        AUDIO_ROUTING_ID,
        serde_json::to_value(&routing).ok(),
    );
    for entry in &outputs_updated {
        state.audit(
            &principal.key_id,
            AuditAction::Update,
            crate::resource_store::OUTPUT_KIND,
            &entry.output,
            None,
        );
    }
    // Hand the new table to the running bus's bounded mailbox (wait-free; the
    // bake consumer applies it at its next tick). Never touches the bus here.
    let mode = match state.live_apply.audio_routing.as_ref() {
        Some(capability) if capability.apply(&routing) => ApplyMode::Live,
        _ => ApplyMode::Restart,
    };
    // A read failure after this point is explicitly indeterminate, not a
    // successful live response for an event that never reached the bus.
    let (_, version) = crate::boot_model::accepted_audio(&state).map_err(|error| {
        ControlError::CommitIndeterminate(format!(
            "committed audio was published, but its version cannot be reread; GET before retrying: {error}"
        ))
    })?;
    Ok(with_apply(
        mode,
        state_response_with_updates(StatusCode::OK, Some(routing), version, outputs_updated),
    ))
}
