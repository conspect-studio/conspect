//! The **media-player transport** operator surface under
//! `/api/v1/media/players/{id}` (ADR-0057 Decision 4, ADR-0097, ADR-RT008).
//!
//! A media player is a **pre-declared, bus-selectable channel** (config
//! `media_players[]`) that owns one supervised ingest thread from boot:
//! `load`/`cue`/`play`/`pause`/`stop`/`seek` change *what* it plays without
//! spawning or tearing an engine-path thread, and the vamp-exit triad
//! (`exit/arm`/`exit/take`/`exit/cancel`) stages and commits a clean exit at the
//! next vamp boundary — mirroring the salvo arm/take/cancel triad
//! ([`crate::routes::salvos`]). Every verb is **Class-1 Hot** (invariant #11):
//! the engine drain submits the verb into the player's bounded two-class
//! transport mailbox (state verbs conflated latest-wins; targeted load/cue/seek a
//! bounded drop-oldest FIFO) at a frame boundary; the player thread drains it
//! between frames and never paces the engine (inv #1/#10).
//!
//! Routes (errors are RFC 9457 problem documents):
//!
//! * `GET /api/v1/media/players` and `GET /{id}` — list/fetch the configured
//!   players (role: read).
//! * `POST /{id}/load` — load an asset (role: write; `202`). Body `{ asset }`.
//! * `POST /{id}/cue` — cue to the in-point or a frame (role: write; `202`).
//!   Optional body `{ frame }`.
//! * `POST /{id}/play` / `/pause` / `/stop` — transport (role: write; `202`).
//! * `POST /{id}/seek` — seek to a frame (role: write; `202`). Optional body
//!   `{ frame }`.
//! * `POST /{id}/exit/arm` / `/exit/take` / `/exit/cancel` — vamp-exit triad
//!   (role: write; `202`), mirroring salvo arm/take/cancel.
//!
//! Each write authorizes the player id (per-object BOLA, OWASP API1), reserves
//! an `Idempotency-Key`, and submits to the bounded command bus — a full bus
//! sheds to `503` without ever blocking the engine.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;

use crate::auth::{Action, Principal};
use crate::command::{Command, MediaTransportVerb};
use crate::concurrency::IdempotencyKey;
use crate::error::{ControlError, ControlResult};
use crate::resource_store::Resource;
use crate::routes::submit_accepted;
use crate::state::AppState;

/// The optional JSON body a transport verb may carry: an `asset` (for `load`)
/// and/or a `frame` (for `cue`/`seek`). All fields are optional so a verb that
/// needs no body (`play`/`pause`/`stop`, or `cue`/`seek` to the in-point) may
/// carry an empty body.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TransportBody {
    /// The media-library asset id (required for `load`, ignored otherwise).
    #[serde(default)]
    pub asset: Option<String>,
    /// The target frame, asset-relative, integer frames at the output cadence
    /// (ADR-T015); used by `cue`/`seek`. Absent cues/seeks to the in-point.
    #[serde(default)]
    pub frame: Option<u64>,
}

/// Parse an optional transport body: an **empty** request body yields the
/// default (all-`None`) body; a non-empty body must be valid JSON for
/// [`TransportBody`], else `422`. Reading [`Bytes`] (the last extractor) never
/// fails on an empty body, unlike `Json<T>`.
fn parse_transport_body(bytes: &Bytes) -> ControlResult<TransportBody> {
    if bytes.is_empty() {
        return Ok(TransportBody::default());
    }
    serde_json::from_slice(bytes)
        .map_err(|e| ControlError::Validation(format!("invalid media transport body: {e}")))
}

/// Confirm a media player exists (so a transport/exit verb on an unknown id is a
/// clean `404` rather than an opaque engine no-op), enforce the write-action
/// gate and per-object authorization (BOLA), then submit the engine command.
///
/// Mirrors [`crate::routes::salvos`]'s `submit_for_existing_salvo`: the three
/// authorization checks (action gate + object BOLA + existence) all happen
/// before an idempotency key is reserved or the engine bus is touched, so an
/// unauthorized or unknown-player request enqueues nothing.
fn submit_for_existing_player(
    state: &AppState,
    principal: &Principal,
    id: &str,
    idem: &IdempotencyKey,
    build: impl FnOnce(crate::command::OperationId) -> Command,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(principal, id)?;
    // Fail fast on an unknown player before reserving an idempotency key.
    state.media_players.get(id)?;
    // The op id is minted inside the idempotency reservation and threaded into
    // the command so the 202 body and the engine command share one id (so
    // `CorrKey::for_command` correlates the realtime outcome).
    submit_accepted(state, idem, build)
}

/// Submit a [`Command::MediaTransport`] for an existing player.
fn submit_transport(
    state: &AppState,
    principal: &Principal,
    id: &str,
    idem: &IdempotencyKey,
    verb: MediaTransportVerb,
) -> ControlResult<Response> {
    let player = id.to_owned();
    submit_for_existing_player(state, principal, id, idem, move |op| {
        Command::MediaTransport { op, player, verb }
    })
}

/// `GET /api/v1/media/players` — list the configured players (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/players",
        tag = "media-players",
        responses(
            (status = 200, description = "The configured media players.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_players(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): a media player is
    // gated by its OWN id (`get_player` 403s an out-of-scope id), so the list
    // must drop rows outside the allowlist — exactly as `list_devices` does. A
    // media player carries no embedded device_ref, so the row filter alone is
    // complete; it is a no-op for an unscoped principal.
    let players = state
        .media_players
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .filter(|resource| crate::auth::authorize_object(&principal, &resource.id).is_ok())
        .collect();
    Ok(Json(players))
}

/// `GET /api/v1/media/players/{id}` — fetch one configured player (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/players/{id}",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 200, description = "The media player.", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_player(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<Resource>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.media_players.get(&id)?;
    Ok(Json(versioned.resource))
}

/// `POST /api/v1/media/players/{id}/load` — load an asset (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/load",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        request_body = TransportBody,
        responses(
            (status = 202, description = "Load accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 422, description = "Missing/invalid asset.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn load_player(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    body: Bytes,
) -> ControlResult<Response> {
    let parsed = parse_transport_body(&body)?;
    let asset = parsed.asset.ok_or_else(|| {
        ControlError::Validation("media player load requires an `asset` id".to_owned())
    })?;
    submit_transport(
        &state,
        &principal,
        &id,
        &idem,
        MediaTransportVerb::Load { asset },
    )
}

/// `POST /api/v1/media/players/{id}/cue` — cue to in-point or a frame
/// (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/cue",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        request_body = TransportBody,
        responses(
            (status = 202, description = "Cue accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn cue_player(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    body: Bytes,
) -> ControlResult<Response> {
    let parsed = parse_transport_body(&body)?;
    submit_transport(
        &state,
        &principal,
        &id,
        &idem,
        MediaTransportVerb::Cue {
            frame: parsed.frame,
        },
    )
}

/// `POST /api/v1/media/players/{id}/play` — play forward (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/play",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 202, description = "Play accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn play_player(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    submit_transport(&state, &principal, &id, &idem, MediaTransportVerb::Play)
}

/// `POST /api/v1/media/players/{id}/pause` — pause (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/pause",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 202, description = "Pause accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn pause_player(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    submit_transport(&state, &principal, &id, &idem, MediaTransportVerb::Pause)
}

/// `POST /api/v1/media/players/{id}/stop` — stop / re-cue (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/stop",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 202, description = "Stop accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn stop_player(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    submit_transport(&state, &principal, &id, &idem, MediaTransportVerb::Stop)
}

/// `POST /api/v1/media/players/{id}/seek` — seek to a frame (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/seek",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        request_body = TransportBody,
        responses(
            (status = 202, description = "Seek accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn seek_player(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    body: Bytes,
) -> ControlResult<Response> {
    let parsed = parse_transport_body(&body)?;
    submit_transport(
        &state,
        &principal,
        &id,
        &idem,
        MediaTransportVerb::Seek {
            frame: parsed.frame,
        },
    )
}

/// `POST /api/v1/media/players/{id}/exit/arm` — arm the vamp exit
/// (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/exit/arm",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 202, description = "Arm accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn arm_exit(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    let player = id.clone();
    submit_for_existing_player(&state, &principal, &id, &idem, move |op| {
        Command::ArmMediaExit { op, player }
    })
}

/// `POST /api/v1/media/players/{id}/exit/take` — take the vamp exit (arm + fire
/// at the soonest boundary) (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/exit/take",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 202, description = "Take accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn take_exit(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    let player = id.clone();
    submit_for_existing_player(&state, &principal, &id, &idem, move |op| {
        Command::TakeMediaExit { op, player }
    })
}

/// `POST /api/v1/media/players/{id}/exit/cancel` — cancel an armed vamp exit
/// (role: write; 202).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/players/{id}/exit/cancel",
        tag = "media-players",
        params(("id" = String, Path, description = "Media-player id.")),
        responses(
            (status = 202, description = "Cancel accepted; outcome on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "No media player with that id.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn cancel_exit(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    let player = id.clone();
    submit_for_existing_player(&state, &principal, &id, &idem, move |op| {
        Command::CancelMediaExit { op, player }
    })
}
