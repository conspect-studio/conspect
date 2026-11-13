//! The **ephemeral Cast sessions** surface under `/api/v1/cast/sessions`
//! (DEV-D2, ADR-M011).
//!
//! An ad-hoc cast session points a Google Cast device at an HLS rendition the
//! engine is **already serving** (the DEV-D1 `/hls/{output-id}` mounts × the
//! validated `control.cast_media_base`): encode-once is preserved and the
//! engine is untouched — starting a session spawns a control-plane
//! [`CastSessionActor`](crate::devices::cast::session::CastSessionActor)
//! through the SAME DEV-A4 [`DevicePollerRegistry`](crate::devices::DevicePollerRegistry)
//! every device driver uses (invariant #10: a session can at worst stall its
//! own task).
//!
//! Sessions are **ephemeral**: runtime-only records in the
//! [`CastSessionStore`](crate::devices::cast::store::CastSessionStore), never
//! part of the devices resource store, so a config export can never emit one.
//! `POST /{id}/save` promotes a session into a normal `Device{driver: cast}`
//! registry entry (which **does** export — desired state), replacing the
//! ephemeral actor with one keyed by the device id.
//!
//! Routes (errors are RFC 9457 problem documents):
//!
//! * `POST /api/v1/cast/sessions` — start (role: write; `201`; `422` on a bad
//!   address / unknown output; `409` when no delivery map or no live `cast`
//!   driver is built in).
//! * `GET /api/v1/cast/sessions` and `GET /{id}` — list/fetch (role: read),
//!   each carrying the live lifecycle state from the status registry.
//! * `DELETE /{id}` — stop (role: write; `204`): dispatches the voluntary
//!   [`PollerControl::StopCast`] teardown (the receiver `STOP` that actually
//!   clears the TV), joins it gracefully, then clears the registry tombstone
//!   (session ids are UUID-fresh and never reused, so clearing keeps the
//!   tombstone set bounded under churning sessions).
//! * `POST /{id}/save` — promote to a device (role: write; `201`; `409` when
//!   the device id exists).
//! * `POST /{id}/volume` — receiver-namespace volume (role: write; `202` +
//!   operation id; non-blocking dispatch).

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use multiview_config::device::net_guard::{self, DialPolicy, HostSpec};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::IdempotencyKey;
use crate::devices::cast::media::CastMediaTarget;
use crate::devices::cast::store::CastSessionRecord;
use crate::devices::PollerControl;
use crate::error::{ControlError, ControlResult};
use crate::resource_store::{ResourceInput, DEVICE_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, TypedCollection};

/// The audit/problem object kind for an ephemeral cast session.
const CAST_SESSION_KIND: &str = "cast-session";

/// How long a stopped session actor gets to exit **voluntarily** (send the
/// receiver `STOP` + `CLOSE` and return) before the registry aborts it. The
/// teardown is a couple of small writes on an open channel — two seconds is
/// generous; a wedged channel is aborted at the window so `DELETE` stays
/// bounded.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The `POST /api/v1/cast/sessions` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StartCastSessionRequest {
    /// The device authority to dial: `host[:port]`, IPv6 bracketed
    /// (`[2001:db8::20]:8009`); the CASTV2 port 8009 is the default when
    /// omitted (Cast **groups** advertise non-default ports — give the
    /// advertised one).
    pub address: String,
    /// An operator-facing name for the session (e.g. the room/TV name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The output id whose HLS rendition to cast. Omitted: the **first
    /// declared** rendition is cast (every HLS output is a rendition of the
    /// program canvas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// What to show, as `POST /cast/streams` takes it
    /// (`{"bus":"program"|"preview"|"clean"}`, `{"source":"<id>"}`,
    /// `{"layout":"<id>"}`). When the engine runs a cast output the session
    /// creates its own cast stream (same authorization as `POST
    /// /cast/streams`, owner = the caller) and LOADs
    /// `{control.cast_media_base}/cast/<token>/master.m3u8`; `output` may
    /// then name a cast output. Without a cast output a `feed` is refused
    /// (`409 cast-unavailable`) and `output` names an HLS rendition as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub feed: Option<multiview_config::VideoFeed>,
}

/// One ephemeral cast session as the API serves it: the descriptive record
/// plus the live lifecycle state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CastSessionDoc {
    /// The runtime session id (`cast-session-…`, UUID-fresh per start).
    pub id: String,
    /// The operator-facing name, if given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The device authority dialled (`host[:port]`, IPv6 bracketed).
    pub address: String,
    /// The output id whose rendition the session casts.
    pub output: String,
    /// The device-reachable media URL the session LOADs. For a session
    /// that plays a cast stream the token is redacted:
    /// `https://…/cast/<redacted>/master.m3u8`.
    pub media_url: String,
    /// The cast stream the session owns (stopping the session revokes it;
    /// revoking or expiring it ends the session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// The receiver's player state as last reported: `connecting`,
    /// `loading`, `playing`, `buffering`, `paused`, `idle`, `load-failed`,
    /// `no-media`, `preempted`, `relaunching`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_state: Option<String>,
    /// Why the receiver is idle or refused the LOAD (its `idleReason` /
    /// error reason, e.g. `ERROR`, `FINISHED`), while not playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reason: Option<String>,
    /// The cast stream's last receiver report (custom receiver only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report: Option<crate::cast_streams::ReceiverReport>,
    /// The session's live lifecycle state (the DEV-A3 wire vocabulary, e.g.
    /// `ADOPTING`/`ONLINE`/`DEGRADED`), read from the latest-wins status
    /// registry.
    pub state: String,
    /// When the receiver **accepted** the session's `LOAD` (the first
    /// `MEDIA_STATUS` attributing an active media session to the actor — the
    /// moment the cast verifiably began showing), as Unix nanoseconds from
    /// the control plane's clock. Absent until then: a session whose LOAD was
    /// refused, or is still establishing, has not started (DEV-D3.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_unix_ns: Option<i64>,
}

/// The `POST /api/v1/cast/sessions/{id}/save` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SaveCastSessionRequest {
    /// The device id to register the promoted device under.
    pub device_id: String,
    /// The promoted device's display name (defaults to the session's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// The `POST /api/v1/cast/sessions/{id}/volume` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CastVolumeRequest {
    /// The desired receiver volume as an integer percent (0–100); the actor
    /// maps it to the protocol's unit `level`.
    pub level_percent: u8,
}

/// The live lifecycle state for `id` as a wire token (`ADOPTING` when the
/// status registry has no row yet — a just-spawned actor publishes its first
/// status asynchronously, and `ADOPTING` is exactly the pre-first-probe
/// state).
fn live_state(state: &AppState, id: &str) -> String {
    state
        .device_status
        .snapshot(id)
        .and_then(|status| match serde_json::to_value(status.state) {
            Ok(serde_json::Value::String(token)) => Some(token),
            // `DeviceState` serializes as a plain string; any other shape
            // would be a wire-vocabulary change caught by the events tests.
            _ => None,
        })
        .unwrap_or_else(|| "ADOPTING".to_owned())
}

/// Project a stored record + the live state into the served document.
fn doc(state: &AppState, record: CastSessionRecord) -> CastSessionDoc {
    let session_state = live_state(state, &record.id);
    let last_report = record
        .stream_id
        .as_deref()
        .and_then(|id| state.cast_streams.get(id))
        .and_then(|(view, _token)| view.last_report);
    CastSessionDoc {
        id: record.id,
        name: record.name,
        address: record.address,
        output: record.output,
        media_url: record.media_url,
        stream_id: record.stream_id,
        player_state: record.player_state,
        idle_reason: record.idle_reason,
        last_report,
        state: session_state,
        started_unix_ns: record.started_unix_ns,
    }
}

/// Resolve the requested output (or the first-declared default) against the
/// delivery map. `422` names an unknown output; a missing/empty delivery map
/// is the caller's `409` (checked before this).
fn resolve_target<'d>(
    delivery: &'d crate::devices::cast::media::CastDelivery,
    requested: Option<&str>,
) -> ControlResult<(String, &'d CastMediaTarget)> {
    if let Some(output) = requested {
        let target = delivery.for_output(output).ok_or_else(|| {
            ControlError::Validation(format!(
                "output {output:?} is not a served HLS rendition (declare an HLS/LL-HLS \
                 output with that id, or omit `output` to cast the first rendition)"
            ))
        })?;
        return Ok((output.to_owned(), target));
    }
    let (id, target) = delivery
        .first_output_id()
        .map(str::to_owned)
        .zip(delivery.first())
        .ok_or_else(|| {
            // Unreachable behind the caller's non-empty check; kept as
            // an honest conflict rather than a panic.
            ControlError::Conflict(
                "no HLS rendition is served — declare an HLS/LL-HLS output".to_owned(),
            )
        })?;
    Ok((id, target))
}

/// Screen a Cast dial authority before the session actor is spawned (SEC-04,
/// CWE-918). Parse `host[:port]`, resolve the host, and screen the RESOLVED IP
/// against `policy`. Screening the resolved answer (not the name) defeats
/// DNS-rebind: a public name that answers with a private/loopback address is
/// refused. A parse/resolution failure or a blocked target is a `422`.
async fn screen_cast_dial_target(address: &str, policy: &DialPolicy) -> ControlResult<()> {
    let (host, port) = net_guard::parse_cast_authority(address)
        .map_err(|e| ControlError::Validation(e.to_string()))?;
    match host {
        HostSpec::Ip(ip) => {
            net_guard::screen_ip(ip, policy)
                .map_err(|e| ControlError::Validation(e.to_string()))?;
        }
        HostSpec::Name(name) => {
            let resolved = tokio::net::lookup_host((name.as_str(), port))
                .await
                .map_err(|e| {
                    ControlError::Validation(format!("address {address:?} did not resolve: {e}"))
                })?
                .map(|socket| socket.ip());
            net_guard::screen_resolved(resolved, policy)
                .map_err(|e| ControlError::Validation(e.to_string()))?;
        }
    }
    Ok(())
}

/// `POST /api/v1/cast/sessions` — start an ad-hoc cast session (role: write).
///
/// When the engine runs a cast output (ADR-0111) the session creates its own
/// cast stream (`feed` / `output` as `POST /cast/streams` takes them, same
/// authorization, owner = the caller), LAUNCHes the configured receiver app
/// (the registered custom receiver, else the Default Media Receiver
/// `CC1AD845`) and LOADs `{control.cast_media_base}/cast/<token>/master.m3u8`
/// (fMP4, LIVE). The session owns the stream: stopping the session revokes
/// it, and a revoked or expired stream ends the session. Only the receiver's
/// playlist fetches keep the token alive. The token is never served,
/// audited, published or logged (`media_url` is redacted).
///
/// Without a cast output: resolves the HLS rendition, spawns the supervised
/// session actor through the shared poller registry, and records the
/// ephemeral session. The actor CONNECTs → `LAUNCH`es the Default Media
/// Receiver (`CC1AD845`) → `LOAD`s the device-reachable HLS URL, then
/// supervises the session (ADR-M011).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/cast/sessions",
        tag = "cast",
        request_body = StartCastSessionRequest,
        responses(
            (status = 201, description = "The started ephemeral session (runtime-only; never exported).", body = CastSessionDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to start a session; with a cast output: the caller may not read the output (object and output axis) or the feed, exactly as for `POST /cast/streams`.", body = crate::problem::Problem),
            (status = 409, description = "No live cast driver in this build; with a cast output: `cast-no-public-base` (control.cast_media_base is not an absolute https:// origin) or any refusal of `POST /cast/streams` (`cast-unavailable`, `cast-capacity`, `cast-no-encoder`, routing refusals); without one: no castable rendition (control.cast_media_base unset / no HLS output) or `cast-unavailable` for a `feed`.", body = crate::problem::Problem),
            (status = 422, description = "The address is not a valid host[:port], the named output is not a served HLS rendition, or `unknown-feed`.", body = crate::problem::Problem),
            (status = 429, description = "The `POST /cast/streams` creation limit (burst 4, then 1/s per principal).", body = crate::problem::Problem),
            (status = 503, description = "`engine-busy`: the take onto the cast output did not settle within 3 s.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn start_cast_session(
    State(state): State<AppState>,
    principal: Principal,
    Json(request): Json<StartCastSessionRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // SSRF guard (SEC-04, CWE-918): the session actor dials this authority
    // (TCP+TLS). Parse it (a malformed `host[:port]` is a 422, never guessed
    // at), resolve the host, and screen the RESOLVED IP (defeats DNS-rebind)
    // against the operator dial policy — all BEFORE any side effect, so a
    // rejected target casts nothing and records nothing.
    screen_cast_dial_target(&request.address, &state.dial_policy).await?;
    // A running cast output: the session plays its own cast stream (ADR-0111).
    let cast_outputs = state
        .cast_engine
        .as_ref()
        .is_some_and(|engine| !engine.outputs().is_empty());
    if cast_outputs {
        return start_stream_session(&state, &principal, &request).await;
    }
    if request.feed.is_some() {
        return Ok(crate::routes::cast_streams::cast_problem(
            409,
            "cast-unavailable",
            "No cast output can be used: add an output with kind = \"cast\" (or check that the \
             engine is running)",
        ));
    }
    // A castable rendition requires the delivery map the binary builds from
    // `control.cast_media_base` × the served HLS mounts (ADR-M011: the media
    // URL must be device-reachable — Cast devices resolve via hardcoded
    // public DNS and cannot reach a loopback).
    let delivery = state
        .cast_delivery
        .as_ref()
        .filter(|delivery| !delivery.is_empty())
        .ok_or_else(|| {
            ControlError::Conflict(
                "no castable HLS rendition: set control.cast_media_base (a device-reachable \
                 IP-literal base) and declare at least one HLS/LL-HLS output"
                    .to_owned(),
            )
        })?;
    let (output, target) = resolve_target(delivery, request.output.as_deref())?;
    // Per-output BOLA (ADR-W005 / OWASP API1): the cast target is a program
    // rendition/head, so an output-scoped principal may only cast a rendition
    // inside its allowlist — mirroring how `salvos` gates a head. Checked on the
    // RESOLVED output and BEFORE any side effect (no record, no actor, no event),
    // so a cross-output start casts nothing.
    crate::auth::authorize_output(&principal, &output)?;

    let id = format!("cast-session-{}", uuid::Uuid::new_v4());
    // The runtime device document the factory resolves (driver gating + the
    // display assignment naming the rendition). It is built from an
    // already-validated shape, so a parse failure is a programming error
    // surfaced as a 422 — never a panic.
    let device: multiview_config::Device = serde_json::from_value(serde_json::json!({
        "id": id,
        "display_name": request.name,
        "driver": "cast",
        "address": request.address,
        "display": { "assign": { "output": output } },
    }))
    .map_err(|e| ControlError::Validation(format!("cast session document did not build: {e}")))?;

    // UUID-fresh ids are never tombstoned, but clear defensively so a (never
    // expected) collision cannot strand the start, then check-then-spawn
    // through the shared registry — the same deterministic start the devices
    // domain uses (DEV-A4).
    state.device_pollers.clear_tombstone(&id);
    state.device_status.ensure(&id);
    // The record is inserted BEFORE the actor spawn: the actor's LOAD-accept
    // hook stamps `started_unix_ns` into this record through the poller
    // wiring's store (DEV-D3.1), and a fast establishment (millisecond test
    // cadences, a quick LAN device) must never race an absent record.
    let record = CastSessionRecord {
        id: id.clone(),
        name: request.name.clone(),
        address: request.address.clone(),
        output,
        media_url: target.url.clone(),
        ..CastSessionRecord::default()
    };
    state.cast_sessions.insert(record.clone());
    let wiring = state.poller_wiring();
    if !state.device_pollers.start(&device, &wiring) {
        // The factory does not manage cast devices: this build carries no
        // live cast driver (the off-by-default `cast` feature is off).
        // Refuse honestly instead of recording a session that casts nothing —
        // and announce nothing.
        state.cast_sessions.remove(&id);
        state.device_status.forget(&id);
        return Err(ControlError::Conflict(
            "no live cast driver in this build (the `cast` feature is off) — cannot start a \
             session"
                .to_owned(),
        ));
    }
    // Membership changed: announce it on the lossless devices lane so clients
    // refresh their session list immediately (DEV-D3.1); the SPA's 15 s REST
    // re-poll stays as the degraded path. Non-blocking drop-oldest publish
    // (invariant #10).
    let _seq = wiring.broadcaster.cast_session_started(
        &id,
        record.name.clone(),
        &record.address,
        &record.output,
    );
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        CAST_SESSION_KIND,
        &id,
        Some(serde_json::json!({
            "address": record.address,
            "output": record.output,
            "media_url": record.media_url,
        })),
    );
    // Serve the store's current row: a fast establishment may already have
    // stamped started-at between the spawn and this response.
    let served = state.cast_sessions.get(&id).unwrap_or(record);
    Ok((StatusCode::CREATED, Json(doc(&state, served))).into_response())
}

/// The absolute `https://` base a cast stream URL is joined onto, or the
/// `409 cast-no-public-base` refusal: a TV resolves names through public DNS
/// and the receiver page refuses mixed content, so a relative path or an
/// `http://` base would LOAD a URL the TV cannot play.
fn public_base(state: &AppState) -> Result<String, Response> {
    state
        .cast_delivery
        .as_ref()
        .and_then(|delivery| delivery.base())
        .filter(|base| base.is_https())
        .map(|base| base.as_str().to_owned())
        .ok_or_else(|| {
            crate::routes::cast_streams::cast_problem(
                409,
                "cast-no-public-base",
                "A TV needs an absolute https:// address for the stream: set \
                 control.cast_media_base to this engine's public https origin (for example \
                 https://engine.example.org) and restart",
            )
        })
}

/// Start a session that plays its own cast stream: the stream is created
/// exactly as `POST /cast/streams` creates it (same authorization, owner =
/// the caller), the session LAUNCHes the configured receiver app and LOADs
/// the absolute token URL. The token never leaves this function except in
/// the actor's LOAD: the record, the answer, the audit entry and the events
/// carry the redacted URL.
async fn start_stream_session(
    state: &AppState,
    principal: &Principal,
    request: &StartCastSessionRequest,
) -> ControlResult<Response> {
    use crate::devices::cast::media::{CastMediaTarget, HlsSegmentFormat};
    use crate::devices::cast::protocol::{CastAppId, DEFAULT_LOAD_TITLE};
    use crate::devices::cast::store::{redact_cast_url, SessionLaunch};
    let base = match public_base(state) {
        Ok(base) => base,
        Err(refused) => return Ok(refused),
    };
    let create = crate::routes::cast_streams::CreateCastStream {
        feed: request.feed.clone(),
        output: request.output.clone(),
    };
    let minted = match crate::routes::cast_streams::mint_stream(state, principal, &create).await? {
        Ok(minted) => minted,
        Err(refused) => return Ok(refused),
    };
    let stream_id = minted.view.id.clone();
    let url = format!("{base}/cast/{}/master.m3u8", minted.token);
    let media_url = redact_cast_url(&url);
    let app_id = state
        .cast_streams
        .receiver_app_id()
        .as_deref()
        .and_then(CastAppId::parse)
        .unwrap_or_default();
    let id = format!("cast-session-{}", uuid::Uuid::new_v4());
    let device: Result<multiview_config::Device, _> = serde_json::from_value(serde_json::json!({
        "id": id,
        "display_name": request.name,
        "driver": "cast",
        "address": request.address,
        "display": { "assign": { "output": minted.view.output } },
    }));
    let Ok(device) = device else {
        revoke_stream(state, &stream_id);
        return Err(ControlError::Validation(
            "cast session document did not build".to_owned(),
        ));
    };
    state.device_pollers.clear_tombstone(&id);
    state.device_status.ensure(&id);
    let record = CastSessionRecord {
        id: id.clone(),
        name: request.name.clone(),
        address: request.address.clone(),
        output: minted.view.output.clone(),
        media_url,
        stream_id: Some(stream_id.clone()),
        launch: Some(SessionLaunch {
            media: CastMediaTarget {
                url,
                format: HlsSegmentFormat::Fmp4,
            },
            app_id,
            title: request
                .name
                .clone()
                .unwrap_or_else(|| DEFAULT_LOAD_TITLE.to_owned()),
        }),
        player_state: Some("connecting".to_owned()),
        ..CastSessionRecord::default()
    };
    state.cast_sessions.insert(record.clone());
    let wiring = state.poller_wiring();
    if !state.device_pollers.start(&device, &wiring) {
        state.cast_sessions.remove(&id);
        state.device_status.forget(&id);
        revoke_stream(state, &stream_id);
        return Err(ControlError::Conflict(
            "no live cast driver in this build (the `cast` feature is off) — cannot start a \
             session"
                .to_owned(),
        ));
    }
    let _seq = wiring.broadcaster.cast_session_started(
        &id,
        record.name.clone(),
        &record.address,
        &record.output,
    );
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        CAST_SESSION_KIND,
        &id,
        Some(serde_json::json!({
            "address": record.address,
            "output": record.output,
            "media_url": record.media_url,
            "stream_id": stream_id,
            "app_id": app_id.as_str(),
        })),
    );
    let served = state.cast_sessions.get(&id).unwrap_or(record);
    Ok((StatusCode::CREATED, Json(doc(state, served))).into_response())
}

/// End every session whose cast stream is gone (revoked through `DELETE
/// /cast/streams/{id}`, or expired because the receiver stopped fetching):
/// the receiver gets its `STOP`, the record and status go, the removal is
/// announced. Runs on the lazy sweep of the session routes and on the 5 s
/// cast sweep; never awaits a device on the caller's path (the graceful
/// join runs in its own task).
pub(crate) fn reap_streamless_sessions(state: &AppState) {
    let orphans: Vec<String> = state
        .cast_sessions
        .list()
        .into_iter()
        .filter(|record| {
            record
                .stream_id
                .as_deref()
                .is_some_and(|stream| state.cast_streams.get(stream).is_none())
        })
        .map(|record| record.id)
        .collect();
    for id in orphans {
        if state.cast_sessions.remove(&id).is_none() {
            continue;
        }
        let _dispatched = state.device_pollers.dispatch(&id, PollerControl::StopCast);
        let _seq = state.poller_wiring().broadcaster.cast_session_removed(&id);
        let task_state = state.clone();
        let finish = async move {
            task_state
                .device_pollers
                .stop_graceful(&id, STOP_GRACE)
                .await;
            task_state.device_pollers.clear_tombstone(&id);
            task_state.device_status.forget(&id);
            task_state.device_drivers.forget(&id);
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(finish));
        }
    }
}

/// Revoke a session's cast stream at once (and finish its output).
fn revoke_stream(state: &AppState, stream_id: &str) {
    let _ = state.cast_streams.remove(stream_id);
    crate::cast_streams::lifecycle::sweep_now(state);
}

/// `GET /api/v1/cast/sessions` — list the live ephemeral sessions (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/cast/sessions",
        tag = "cast",
        responses(
            (status = 200, description = "All live ephemeral sessions, id-sorted, each with its live lifecycle state.", body = [CastSessionDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_cast_sessions(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<CastSessionDoc>>> {
    principal.role.require(Action::Read)?;
    crate::cast_streams::lifecycle::sweep_now(&state);
    reap_streamless_sessions(&state);
    // Per-object visibility (BOLA, ADR-W005/ADR-W025): a scoped principal sees
    // ONLY its allowlisted sessions — by parity with `GET /{id}` 403'ing an
    // out-of-scope id, an unfiltered collection would let it enumerate (and read
    // the address/media-URL of) sessions it cannot fetch. The same
    // `authorize_object` predicate the per-object handlers use; an unscoped
    // principal (admin/unrestricted operator) keeps every row.
    let sessions = state
        .cast_sessions
        .list()
        .into_iter()
        .filter(|record| crate::auth::authorize_object(&principal, &record.id).is_ok())
        .map(|record| doc(&state, record))
        .collect();
    Ok(Json(sessions))
}

/// `GET /api/v1/cast/sessions/{id}` — fetch one session (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/cast/sessions/{id}",
        tag = "cast",
        params(("id" = String, Path, description = "Cast session id.")),
        responses(
            (status = 200, description = "The session, with its live lifecycle state.", body = CastSessionDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this session.", body = crate::problem::Problem),
            (status = 404, description = "No live session with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_cast_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<CastSessionDoc>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    crate::cast_streams::lifecycle::sweep_now(&state);
    reap_streamless_sessions(&state);
    let record = state
        .cast_sessions
        .get(&id)
        .ok_or_else(|| ControlError::NotFound {
            kind: CAST_SESSION_KIND,
            id: id.clone(),
        })?;
    Ok(Json(doc(&state, record)))
}

/// `DELETE /api/v1/cast/sessions/{id}` — stop a session (role: write; `204`).
///
/// Dispatches the voluntary [`PollerControl::StopCast`] teardown (the actor
/// `STOP`s the receiver app — what actually clears the TV — then exits), joins
/// it gracefully (abort after the grace window), drops the runtime
/// status/record, and **clears the registry tombstone**: session ids are
/// UUID-fresh and never reused, so clearing after the deterministic stop keeps
/// the tombstone set bounded under churning sessions.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/cast/sessions/{id}",
        tag = "cast",
        params(("id" = String, Path, description = "Cast session id.")),
        responses(
            (status = 204, description = "The session was stopped and forgotten."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to stop this session.", body = crate::problem::Problem),
            (status = 404, description = "No live session with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn stop_cast_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let record = state
        .cast_sessions
        .remove(&id)
        .ok_or_else(|| ControlError::NotFound {
            kind: CAST_SESSION_KIND,
            id: id.clone(),
        })?;
    // Voluntary teardown first (non-blocking try_send; a full channel just
    // means the grace window elapses and the abort tears the actor down), then
    // the graceful stop joins the exit deterministically.
    let _dispatched = state.device_pollers.dispatch(&id, PollerControl::StopCast);
    state.device_pollers.stop_graceful(&id, STOP_GRACE).await;
    // The session owns its cast stream: revoked once the receiver was told
    // to STOP (so the TV ends on the STOP, not on a 404 first).
    if let Some(stream_id) = record.stream_id.as_deref() {
        revoke_stream(&state, stream_id);
    }
    // The id is UUID-fresh and never reused: clear its tombstone so the set
    // stays bounded under churning ad-hoc sessions (the tombstone's job —
    // making a delete win over a racing adopt — is done once the stop above
    // returned, and nothing can legitimately start this id again).
    state.device_pollers.clear_tombstone(&id);
    state.device_status.forget(&id);
    state.device_drivers.forget(&id);
    // Membership changed: announce the removal on the lossless devices lane
    // (DEV-D3.1) so clients drop the row immediately instead of waiting for
    // the REST re-poll. Non-blocking drop-oldest publish (invariant #10).
    let _seq = state.poller_wiring().broadcaster.cast_session_removed(&id);
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        CAST_SESSION_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/cast/sessions/{id}/save` — promote an ephemeral session to a
/// normal `Device{driver: cast}` registry entry (role: write; `201`).
///
/// The promoted device carries the session's address and rendition assignment
/// and **does** export (desired state). One actor remains, keyed by the device
/// id: the ephemeral actor is stopped (a plain abort — **no** receiver `STOP`,
/// so the TV keeps playing across the promotion) and the device's supervised
/// actor takes over through the same registry.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/cast/sessions/{id}/save",
        tag = "cast",
        params(("id" = String, Path, description = "Cast session id to promote.")),
        request_body = SaveCastSessionRequest,
        responses(
            (status = 201, description = "The promoted device (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write devices.", body = crate::problem::Problem),
            (status = 404, description = "No live session with that id.", body = crate::problem::Problem),
            (status = 409, description = "A device with that id already exists.", body = crate::problem::Problem),
            (status = 422, description = "The promoted device document does not validate.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn save_cast_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(request): Json<SaveCastSessionRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // A promotion touches TWO objects — authorize BOTH (BOLA, ADR-W005): the
    // path session `id` being read + retired (matching get/stop/volume), and
    // the target `device_id` being created. Authorizing only the device would
    // let a scoped principal promote another tenant's session into its device.
    crate::auth::authorize_object(&principal, &id)?;
    crate::auth::authorize_object(&principal, &request.device_id)?;
    let record = state
        .cast_sessions
        .get(&id)
        .ok_or_else(|| ControlError::NotFound {
            kind: CAST_SESSION_KIND,
            id: id.clone(),
        })?;
    if record.stream_id.is_some() {
        return Err(ControlError::Conflict(
            "a session that plays a cast stream cannot be saved as a device: its stream is \
             temporary (stop it and cast again instead)"
                .to_owned(),
        ));
    }
    let display_name = request.display_name.clone().or_else(|| record.name.clone());
    let body = serde_json::json!({
        "id": request.device_id,
        "display_name": display_name,
        "driver": "cast",
        "address": record.address,
        "display": { "assign": { "output": record.output } },
    });
    // The same typed validation every devices-store write runs (ADR-W015).
    let body = validated_body(TypedCollection::Devices, &request.device_id, &body)?;
    let name = display_name.unwrap_or_else(|| request.device_id.clone());
    // Review H1: the promoted device is persisted config. Serialize with every
    // other durable mutation and commit the desired envelope before the store
    // projection; a duplicate id is refused (409) before any durable write.
    let _mutation = state.lock_config_mutation().await;
    crate::boot_model::require_absent(
        crate::boot_model::accepted_item(&state, "devices", &state.devices, &request.device_id),
        DEVICE_KIND,
        &request.device_id,
    )
    .map_err(|err| match err {
        ControlError::Validation(_) => {
            ControlError::Conflict(format!("device {:?} already exists", request.device_id))
        }
        other => other,
    })?;
    crate::boot_model::commit_resource(
        &state,
        "devices",
        &request.device_id,
        Some(body.clone()),
        Some(name.clone()),
    )
    .await?;
    // Mirror the devices create route: clear any tombstone only while owning
    // the config lock, so an in-flight delete cannot tombstone after this.
    state.device_pollers.clear_tombstone(&request.device_id);
    let versioned = crate::boot_model::project_upsert(
        &state,
        "devices",
        &state.devices,
        &request.device_id,
        || {
            state
                .devices
                .create(&request.device_id, ResourceInput { name, body })
        },
    )
    .map_err(|err| match err {
        // The store reports a duplicate id as `Validation` (the only
        // `Validation` its `create` produces — the body was validated
        // above); for the save promotion the honest status is a `409`:
        // a device with that id already exists. Race-free (the store
        // checks under its own lock), unlike a get-then-create probe.
        ControlError::Validation(_) => {
            ControlError::Conflict(format!("device {:?} already exists", request.device_id))
        }
        other => crate::boot_model::after_commit(&state, other),
    })?;
    state.device_status.ensure(&request.device_id);

    // Retire the ephemeral actor + record. A plain stop (abort) — NOT the
    // StopCast teardown — so no receiver STOP is sent and the TV keeps
    // playing; the promoted device's actor re-establishes supervision.
    state.cast_sessions.remove(&id);
    state.device_pollers.stop(&id).await;
    state.device_pollers.clear_tombstone(&id);
    state.device_status.forget(&id);
    state.device_drivers.forget(&id);
    // The EPHEMERAL session left the list (playback continues under the
    // promoted device id): announce the membership change (DEV-D3.1).
    let _seq = state.poller_wiring().broadcaster.cast_session_removed(&id);

    // Start the promoted device's supervised actor (the same registry path an
    // adopt takes). With no live cast driver in this build the device simply
    // rides ADOPTING — exactly a config-declared cast device's behaviour.
    match serde_json::from_value::<multiview_config::Device>(versioned.resource.body.clone()) {
        Ok(device) => {
            let _spawned = state.device_pollers.start(&device, &state.poller_wiring());
        }
        Err(e) => tracing::warn!(
            device = %request.device_id,
            error = %e,
            "promoted cast device body did not parse back to a Device; no actor spawned"
        ),
    }
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        DEVICE_KIND,
        &request.device_id,
        Some(versioned.resource.body.clone()),
    );

    let etag = versioned.version.to_etag();
    let mut response = (StatusCode::CREATED, Json(versioned.resource)).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    Ok(response)
}

/// `POST /api/v1/cast/sessions/{id}/volume` — set the receiver volume
/// (role: write; `202` + operation id).
///
/// Dispatches a receiver-namespace `SET_VOLUME` to the running session actor
/// — non-blocking `try_send`, never awaited (invariant #10).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/cast/sessions/{id}/volume",
        tag = "cast",
        params(("id" = String, Path, description = "Cast session id.")),
        request_body = CastVolumeRequest,
        responses(
            (status = 202, description = "Volume change accepted; applied by the session actor.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to control this session.", body = crate::problem::Problem),
            (status = 404, description = "No live session with that id.", body = crate::problem::Problem),
            (status = 422, description = "level_percent is out of range (0–100).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn set_cast_volume(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    Json(request): Json<CastVolumeRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    if state.cast_sessions.get(&id).is_none() {
        return Err(ControlError::NotFound {
            kind: CAST_SESSION_KIND,
            id,
        });
    }
    if request.level_percent > 100 {
        return Err(ControlError::Validation(format!(
            "level_percent {} is out of range (0–100)",
            request.level_percent
        )));
    }
    let op = super::devices::reserve_operation(&state, &idem)?;
    // Non-blocking dispatch to the running actor (invariant #10): a full
    // control channel or a reconnecting session sheds the command — the 202
    // stands either way (the actor is best-effort by design, ADR-M011).
    let dispatched = state.device_pollers.dispatch(
        &id,
        PollerControl::SetVolume {
            percent: request.level_percent,
        },
    );
    if !dispatched {
        tracing::debug!(
            session = %id,
            "cast volume: no running actor to dispatch to (or its channel is full); shed"
        );
    }
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        CAST_SESSION_KIND,
        &id,
        Some(serde_json::json!({
            "action": "volume",
            "level_percent": request.level_percent,
        })),
    );
    let body = crate::routes::AcceptedBody {
        operation_id: op.to_string(),
        kind: "cast-volume".to_owned(),
        applied_live: None,
        carried_only: None,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}
