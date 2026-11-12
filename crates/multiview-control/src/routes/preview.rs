//! Live preview snapshots under `/api/v1/preview`.
//!
//! Low-rate JPEG stills of the composited **program** and of each **input**, for
//! the web UI's monitoring view. The pixels come from the engine via the
//! isolation-safe [`PreviewProvider`](crate::preview::PreviewProvider) (a
//! wait-free latest-frame read + on-request encode — never on the output-clock
//! loop). `image/jpeg` with `Cache-Control: no-store`; `503` when no frame is
//! available yet (freshly started engine / unknown input), so the UI shows a
//! placeholder rather than an error.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::auth::{Action, Principal};
use crate::preview::{WhepReject, WhepScope};
use crate::problem::{Problem, PROBLEM_JSON};
use crate::source_demand::SourceDemandPhase;
use crate::state::AppState;

/// The JPEG quality used for preview stills (1–100): low enough to be cheap,
/// high enough to be useful for monitoring.
const PREVIEW_QUALITY: u8 = 70;

/// Longest source `detail` echoed in a still problem, in characters. The engine's
/// reason is a short operator-safe sentence; the cap keeps this response bounded
/// whatever a future provider puts there.
const STILL_DETAIL_MAX_CHARS: usize = 160;

/// True for a Unicode **format** character (general category Cf): the bidi
/// controls and zero-width marks that can reorder or hide the text around them in
/// an operator's UI. Dropped from a detail rather than spaced.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'                // soft hyphen
        | '\u{061C}'              // Arabic letter mark
        | '\u{180E}'              // Mongolian vowel separator
        | '\u{200B}'..='\u{200F}' // zero-width space..right-to-left mark
        | '\u{202A}'..='\u{202E}' // LRE..RLO (bidi embedding/override)
        | '\u{2060}'..='\u{2064}' // word joiner..invisible plus
        | '\u{2066}'..='\u{2069}' // LRI..PDI (bidi isolates)
        | '\u{FEFF}'              // zero-width no-break space
    )
}

/// A bounded, single-line, control-character-free copy of a source `detail`.
/// The provider contract says a detail is operator-safe; this route does not
/// trust even that with unbounded output. `None` when nothing survives.
fn bounded_still_detail(raw: &str) -> Option<String> {
    let flattened: String = raw
        .chars()
        .filter(|c| !is_format_char(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= STILL_DETAIL_MAX_CHARS {
        return Some(collapsed);
    }
    let mut truncated: String = collapsed.chars().take(STILL_DETAIL_MAX_CHARS).collect();
    truncated.push('…');
    Some(truncated)
}

/// The `503` problem for a still that has no frame, typed by source phase.
///
/// RFC 9457 with a stable `type` slug per phase so a client branches on the URI,
/// never on prose: `source-failed`, `source-starting`, or `preview-not-served`
/// (no demand bound / not started yet, and never a phase for an unknown id).
/// The failed case carries the engine's bounded, operator-safe reason.
fn still_unavailable_problem(phase: Option<SourceDemandPhase>, detail: Option<&str>) -> Problem {
    match phase {
        Some(SourceDemandPhase::Failed) => {
            let problem = Problem::new(
                503,
                "source-failed",
                "The source has no picture: it is failing",
            );
            match detail.and_then(bounded_still_detail) {
                Some(detail) => problem.with_detail(detail),
                None => problem,
            }
        }
        Some(SourceDemandPhase::Idle | SourceDemandPhase::Starting) => Problem::new(
            503,
            "source-starting",
            "The source is starting and has not produced a frame yet",
        ),
        Some(SourceDemandPhase::Ready) | None => Problem::new(
            503,
            "preview-not-served",
            "No preview frame is available for this input",
        ),
    }
}

/// Build a `200 image/jpeg` response (no-store) for an encoded still.
fn jpeg_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /api/v1/preview/capabilities` — what preview transports this build can
/// serve, so the SPA picks WHEP vs the JPEG ladder before issuing an offer
/// (ADR-P006 move 6, ADR-W020). Role: read.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/preview/capabilities",
        tag = "preview",
        responses(
            (status = 200, description = "Preview transport capabilities.", body = crate::preview::PreviewCapabilities),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn capabilities(State(state): State<AppState>, principal: Principal) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    let mut capabilities = crate::preview::PreviewCapabilities::from_provider(state.whep.as_ref());
    capabilities.moq = state.moq.available();
    Json(capabilities).into_response()
}

/// `GET /api/v1/preview/program.jpg` — the latest composited program frame
/// (role: read). `503` when no frame has been produced yet.
pub(crate) async fn program_jpeg(State(state): State<AppState>, principal: Principal) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    match state.preview.program_jpeg(PREVIEW_QUALITY) {
        Some(bytes) => jpeg_response(bytes),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// `GET /api/v1/preview/switcher/{me}/preview.jpg` — an actual PVW tap,
/// never a silent copy of the PGM monitor. M/E+program authorization applies.
pub(crate) async fn switcher_preview_jpeg(
    State(state): State<AppState>,
    principal: Principal,
    Path(me): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    if let Err(err) = crate::auth::authorize_object(&principal, &me) {
        return err.into_response();
    }
    if let Err(err) =
        crate::auth::authorize_scope(&principal, multiview_events::AuthzScope::Program("main"))
    {
        return err.into_response();
    }
    if me != "me1" {
        return crate::error::ControlError::NotFound {
            kind: "mix_effects",
            id: me,
        }
        .into_response();
    }
    if state.switcher.is_none() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match state.preview.switcher_preview_jpeg(PREVIEW_QUALITY) {
        Some(bytes) => jpeg_response(bytes),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// `GET /api/v1/preview/inputs/{id}.jpg` — the latest frame of input `id`
/// (role: read). `503` with an RFC 9457 problem typed by source phase when the
/// input is unknown or has produced no frame.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/preview/inputs/{id}",
        tag = "preview",
        params(("id" = String, Path, description = "The input (source) id, optionally with the `.jpg` suffix the SPA appends.")),
        responses(
            (status = 200, description = "The newest frame of the input, `Cache-Control: no-store`.", content_type = "image/jpeg"),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized for this input (per-object scope).", body = crate::problem::Problem),
            (status = 503, description = "No frame available. `type` is `/problems/source-failed`, `/problems/source-starting` or `/problems/preview-not-served`; `X-Multiview-Source-State` repeats the coarse phase.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn input_jpeg(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    // `{id}.jpg` — strip the extension the UI requests so the id matches the
    // engine's source id.
    let id = id.strip_suffix(".jpg").unwrap_or(&id);
    // Per-object authz (BOLA, SEC-05 / ADR-W005): a preview input id is a source
    // id (the same object-scope axis as `GET /inputs/{id}/streams`), so a scoped
    // principal may view only its in-scope inputs. Checked on the stripped id,
    // BEFORE the provider is consulted — a denial never samples the frame.
    if let Err(err) = crate::auth::authorize_object(&principal, id) {
        return err.into_response();
    }
    // Polling a preview is a consumer: a short lease, extended by each poll,
    // starts an idle source off-clock.
    let phase = state
        .source_demand
        .as_ref()
        .and_then(|demand| demand.touch_preview(id));
    // A source that holds a frame keeps serving it whatever its phase says.
    if let Some(bytes) = state.preview.input_jpeg(id, PREVIEW_QUALITY) {
        return jpeg_response(bytes);
    }
    // No picture: say why. The phase and detail are read only AFTER the
    // per-object authorization above, so a caller without access to this source
    // gets its refusal and never a phase (BOLA).
    let detail = if phase == Some(SourceDemandPhase::Failed) {
        state
            .source_demand
            .as_ref()
            .and_then(|demand| demand.status(id).detail)
    } else {
        None
    };
    let mut response = still_unavailable_problem(phase, detail.as_deref()).into_response();
    if let Some(phase) = phase {
        let token = match phase {
            SourceDemandPhase::Idle | SourceDemandPhase::Starting => "starting",
            SourceDemandPhase::Ready => "ready",
            SourceDemandPhase::Failed => "failed",
        };
        response.headers_mut().insert(
            "x-multiview-source-state",
            header::HeaderValue::from_static(token),
        );
    }
    response
}

/// `GET /api/v1/preview/wall` — the shared monitor-wall geometry (ADR-0103).
///
/// Whole-system read: the wall carries every source's pixels, so only an
/// unscoped principal may read it (a scoped one gets `403` and keeps stills).
/// Each read renews the short preview-demand lease of every cell source, the
/// same lease a still poll takes, so a watched wall keeps its sources warm.
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/preview/wall", tag = "preview", responses((status = 200, description = "Wall geometry", body = crate::preview::WallGeometry), (status = 403, description = "Scoped principal", body = crate::problem::Problem), (status = 503, description = "No wall configured or not ready", body = crate::problem::Problem))))]
pub(crate) async fn wall_geometry(State(state): State<AppState>, principal: Principal) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    if let Err(err) = crate::routes::require_unscoped_for_whole_system(&principal) {
        return err.into_response();
    }
    let Some(mut wall) = state.preview.wall() else {
        return crate::problem::Problem::new(503, "wall-unavailable", "monitor wall unavailable")
            .into_response();
    };
    if let Some(demand) = state.source_demand.as_ref() {
        for cell in &wall.cells {
            let _ = demand.touch_preview(&cell.source);
        }
    }
    wall.preview_source = state
        .switcher
        .as_ref()
        .and_then(multiview_engine::switcher_control::SwitcherControl::snapshot)
        .map(|snapshot| snapshot.preview.as_str().to_owned());
    ([(header::CACHE_CONTROL, "no-store")], Json(wall)).into_response()
}

/// `GET /api/v1/preview/inputs` — the ids of inputs that can be previewed
/// (role: read), for the UI to enumerate thumbnails.
pub(crate) async fn list_input_ids(
    State(state): State<AppState>,
    principal: Principal,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    // Per-object ROW visibility (BOLA, SEC-05 / ADR-W005): drop input ids outside
    // the principal's object scope so the enumeration cannot leak inputs a
    // single-id `GET` would 403 (wholesale-enumeration defense). A no-op for an
    // unscoped principal — exactly the `list_layouts` row-filter idiom.
    let ids: Vec<String> = state
        .preview
        .input_ids()
        .into_iter()
        .filter(|id| crate::auth::authorize_object(&principal, id).is_ok())
        .collect();
    Json(ids).into_response()
}

/// The base path of a WHEP scope's focus resource (the `POST` target and the
/// prefix of the per-session `DELETE` URL). Built relative so no host is baked
/// into the `Location` header.
fn whep_base_path(scope: &WhepScope) -> String {
    match scope {
        WhepScope::Program => "/api/v1/preview/program/whep".to_owned(),
        WhepScope::Input(id) => format!("/api/v1/preview/inputs/{id}/whep"),
        WhepScope::Output(id) => format!("/api/v1/preview/outputs/{id}/whep"),
        WhepScope::Wall => "/api/v1/preview/wall/whep".to_owned(),
    }
}

/// Map a [`WhepReject`] onto its RFC 9457 `application/problem+json` response.
///
/// A `CapacityExceeded` carries the `fallback` transport hint as a non-standard
/// problem member so the UI can degrade to the named transport (`jpeg`,
/// `llhls`) honestly rather than silently failing.
fn whep_reject_response(scope: &WhepScope, reject: WhepReject) -> Response {
    let instance = whep_base_path(scope);
    match reject {
        WhepReject::Malformed(detail) => {
            Problem::new(400, "whep-malformed-offer", "Malformed WHEP offer")
                .with_detail(detail)
                .with_instance(instance)
                .into_response()
        }
        WhepReject::UnsupportedCodec => Problem::new(
            415,
            "whep-unsupported-codec",
            "No supported preview codec in the WHEP offer",
        )
        .with_detail("the offer advertises no video codec this build can preview-encode")
        .with_instance(instance)
        .into_response(),
        WhepReject::UnknownEntity => Problem::new(404, "not-found", "Focus entity not found")
            .with_detail(format!("{} cannot be focused", scope.label()))
            .with_instance(instance)
            .into_response(),
        WhepReject::CapacityExceeded { fallback } => {
            // `503` with the fallback transport hint as an extension member.
            let problem = Problem::new(
                503,
                "whep-capacity",
                "Focus capacity exhausted; preview shed",
            )
            .with_detail(
                "the concurrent-focus cap or preview-encode budget is exhausted; \
                 use the fallback transport",
            )
            .with_instance(instance);
            // Re-serialize with the extra `fallback` member. `Problem` itself is a
            // fixed RFC 9457 struct; the fallback hint is a preview-specific
            // extension the SPA reads, so fold it in as JSON here.
            let mut value = serde_json::to_value(&problem)
                .unwrap_or_else(|_| serde_json::json!({ "status": 503 }));
            if let Some(obj) = value.as_object_mut() {
                obj.insert("fallback".to_owned(), serde_json::Value::String(fallback));
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, PROBLEM_JSON)],
                axum::body::Body::from(value.to_string()),
            )
                .into_response()
        }
    }
}

/// Per-scope authorization for a WHEP focus (BOLA, SEC-06/07 / ADR-W005/W026).
///
/// A focus session exposes the live pixels of exactly one entity, so opening or
/// releasing one must be authorized on that entity's axis — the same per-object /
/// per-output checks the REST twins use, enforced HERE at the route layer because
/// the [`crate::preview::WhepProvider`] seam is codec-only and never sees a
/// [`Principal`]:
///
/// * [`WhepScope::Input`] → the object (source) axis ([`crate::auth::authorize_object`]).
/// * [`WhepScope::Output`] → the output (rendition) axis ([`crate::auth::authorize_output`]).
/// * [`WhepScope::Program`] → the whole composited canvas, which embeds every
///   object and output, so only an unrestricted principal may focus it
///   ([`crate::routes::require_unscoped_for_whole_system`]).
///
/// The match carries no wildcard, so a newly-added [`WhepScope`] variant
/// compile-forces its authorization policy here rather than silently defaulting
/// to allow.
///
/// # Errors
///
/// [`crate::error::ControlError::Forbidden`] if the principal's relevant scope
/// axis denies the focus.
fn authorize_whep_scope(
    principal: &Principal,
    scope: &WhepScope,
) -> Result<(), crate::error::ControlError> {
    match scope {
        WhepScope::Program | WhepScope::Wall => {
            crate::routes::require_unscoped_for_whole_system(principal)
        }
        WhepScope::Input(id) => crate::auth::authorize_object(principal, id),
        WhepScope::Output(id) => crate::auth::authorize_output(principal, id),
    }
}

/// Negotiate a WHEP focus session for `scope` from an SDP `offer` body.
///
/// Shared by the program / input / output `POST …/whep` handlers. Opening a
/// focus session is an **operational change** (it allocates a preview-encode
/// session), so it requires [`Action::Write`]: a View/ReadOnly token gets `403`,
/// no token `401`. On success the answer SDP is returned as `201 Created` with a
/// `Location` header pointing at the per-session WHEP resource the client
/// `DELETE`s to release it (draft-ietf-wish-whep).
///
/// Isolation (invariant #10): the handler holds only the [`crate::preview::WhepProvider`]
/// seam (a trait object) — never the engine. The provider reads engine taps
/// lossily; nothing here can back-pressure the protected output path.
fn negotiate_whep(
    state: &AppState,
    principal: &Principal,
    scope: &WhepScope,
    offer: &str,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    // Per-scope authz (BOLA, SEC-06/07): the focus exposes one entity's live
    // pixels, so authorize the scope's axis BEFORE negotiating — a denial never
    // reaches the transport (zero side effect).
    if let Err(err) = authorize_whep_scope(principal, scope) {
        return err.into_response();
    }
    match state.whep.negotiate_for(scope, offer, &principal.key_id) {
        Ok(answer) => {
            let location = format!("{}/{}", whep_base_path(scope), answer.session_id);
            let mut response = (StatusCode::CREATED, answer.sdp).into_response();
            let headers = response.headers_mut();
            if let Ok(value) = header::HeaderValue::from_str("application/sdp") {
                headers.insert(header::CONTENT_TYPE, value);
            }
            if let Ok(value) = header::HeaderValue::from_str(&location) {
                headers.insert(header::LOCATION, value);
            }
            if let Ok(value) = header::HeaderValue::from_str("no-store") {
                headers.insert(header::CACHE_CONTROL, value);
            }
            state.audit(
                &principal.key_id,
                crate::audit::AuditAction::Command,
                "preview-whep",
                &scope.label(),
                Some(serde_json::json!({ "command": "focus-open" })),
            );
            response
        }
        Err(reject) => whep_reject_response(scope, reject),
    }
}

/// Release a WHEP focus session for `scope` by its `session_id`.
///
/// Shared by the program / input / output `DELETE …/whep/{session_id}` handlers.
/// Requires [`Action::Write`] (a focus teardown is an operational change).
/// `204 No Content` when a live session was found and freed; `404` when the id
/// is unknown or already released (idempotent at the transport, explicit at the
/// HTTP edge so the UI can tell "freed" from "never existed").
fn release_whep(
    state: &AppState,
    principal: &Principal,
    scope: &WhepScope,
    session_id: &str,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    // Per-scope authz (BOLA, SEC-06/07): a teardown is scoped exactly like an
    // open — a principal may release only a focus on an entity within its scope.
    if let Err(err) = authorize_whep_scope(principal, scope) {
        return err.into_response();
    }
    if state.whep.release(scope, session_id) {
        state.audit(
            &principal.key_id,
            crate::audit::AuditAction::Command,
            "preview-whep",
            &scope.label(),
            Some(serde_json::json!({ "command": "focus-close", "session": session_id })),
        );
        StatusCode::NO_CONTENT.into_response()
    } else {
        Problem::new(404, "not-found", "Focus session not found")
            .with_detail(format!(
                "no live focus session {session_id:?} for {}",
                scope.label()
            ))
            .with_instance(format!("{}/{session_id}", whep_base_path(scope)))
            .into_response()
    }
}

/// Open the video-only whole-system wall focus (Write + unscoped principal).
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/preview/wall/whep", tag = "preview", request_body(content = String, description = "SDP offer", content_type = "application/sdp"), responses((status = 201, description = "Wall focus opened; SDP answer and Location.", content_type = "application/sdp"), (status = 401, description = "Missing credentials", body = crate::problem::Problem), (status = 403, description = "Scoped or read-only principal", body = crate::problem::Problem), (status = 415, description = "Unsupported codec", body = crate::problem::Problem), (status = 503, description = "Wall unavailable or focus capacity exhausted", body = crate::problem::Problem))))]
pub(crate) async fn wall_whep_open(
    State(state): State<AppState>,
    principal: Principal,
    offer: String,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    if let Err(err) = authorize_whep_scope(&principal, &WhepScope::Wall) {
        return err.into_response();
    }
    if state.preview.wall().is_none() || !state.whep.wall_available() {
        return Problem::new(503, "wall-unavailable", "monitor wall unavailable")
            .with_instance(whep_base_path(&WhepScope::Wall))
            .into_response();
    }
    negotiate_whep(&state, &principal, &WhepScope::Wall, &offer)
}

/// Release the whole-system wall WHEP session.
#[cfg_attr(feature = "openapi", utoipa::path(delete, path = "/api/v1/preview/wall/whep/{session_id}", tag = "preview", params(("session_id" = String, Path, description = "WHEP session id")), responses((status = 204, description = "Wall focus released"), (status = 401, description = "Missing credentials", body = crate::problem::Problem), (status = 403, description = "Scoped or read-only principal", body = crate::problem::Problem), (status = 404, description = "No such session", body = crate::problem::Problem))))]
pub(crate) async fn wall_whep_close(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Response {
    release_whep(&state, &principal, &WhepScope::Wall, &session_id)
}

/// `POST /api/v1/preview/program/whep` — open a WHEP focus on the program
/// canvas (role: write). SDP offer in, `201` + answer SDP + `Location` out.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/preview/program/whep",
        tag = "preview",
        request_body(content = String, description = "SDP offer", content_type = "application/sdp"),
        responses(
            (status = 201, description = "Focus opened; SDP answer in the body, Location at the session resource.", content_type = "application/sdp"),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to open a focus (a View token cannot).", body = crate::problem::Problem),
            (status = 415, description = "The offer advertises no supported preview codec.", body = crate::problem::Problem),
            (status = 503, description = "Focus capacity exhausted; body carries a `fallback` transport hint.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn program_whep_open(
    State(state): State<AppState>,
    principal: Principal,
    offer: String,
) -> Response {
    negotiate_whep(&state, &principal, &WhepScope::Program, &offer)
}

/// `DELETE /api/v1/preview/program/whep/{session_id}` — release a program focus
/// session (role: write). `204` on success, `404` if unknown.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/preview/program/whep/{session_id}",
        tag = "preview",
        params(("session_id" = String, Path, description = "The WHEP session id from the open response's Location.")),
        responses(
            (status = 204, description = "Focus session released."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to release a focus.", body = crate::problem::Problem),
            (status = 404, description = "No such live focus session.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn program_whep_close(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Response {
    release_whep(&state, &principal, &WhepScope::Program, &session_id)
}

/// `POST /api/v1/preview/inputs/{id}/whep` — open a WHEP focus on input `id`
/// (role: write).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/preview/inputs/{id}/whep",
        tag = "preview",
        params(("id" = String, Path, description = "The input (source) id to focus.")),
        request_body(content = String, description = "SDP offer", content_type = "application/sdp"),
        responses(
            (status = 201, description = "Focus opened; SDP answer in the body, Location at the session resource.", content_type = "application/sdp"),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to open a focus.", body = crate::problem::Problem),
            (status = 404, description = "The input id is not focusable.", body = crate::problem::Problem),
            (status = 415, description = "The offer advertises no supported preview codec.", body = crate::problem::Problem),
            (status = 503, description = "Focus capacity exhausted; body carries a `fallback` transport hint.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn input_whep_open(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    offer: String,
) -> Response {
    negotiate_whep(&state, &principal, &WhepScope::Input(id), &offer)
}

/// `DELETE /api/v1/preview/inputs/{id}/whep/{session_id}` — release an input
/// focus session (role: write).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/preview/inputs/{id}/whep/{session_id}",
        tag = "preview",
        params(
            ("id" = String, Path, description = "The input (source) id."),
            ("session_id" = String, Path, description = "The WHEP session id."),
        ),
        responses(
            (status = 204, description = "Focus session released."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to release a focus.", body = crate::problem::Problem),
            (status = 404, description = "No such live focus session.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn input_whep_close(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, session_id)): Path<(String, String)>,
) -> Response {
    release_whep(&state, &principal, &WhepScope::Input(id), &session_id)
}

/// `POST /api/v1/preview/outputs/{id}/whep` — open a WHEP focus on output `id`
/// (role: write).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/preview/outputs/{id}/whep",
        tag = "preview",
        params(("id" = String, Path, description = "The output (rendition) id to focus.")),
        request_body(content = String, description = "SDP offer", content_type = "application/sdp"),
        responses(
            (status = 201, description = "Focus opened; SDP answer in the body, Location at the session resource.", content_type = "application/sdp"),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to open a focus.", body = crate::problem::Problem),
            (status = 404, description = "The output id is not focusable.", body = crate::problem::Problem),
            (status = 415, description = "The offer advertises no supported preview codec.", body = crate::problem::Problem),
            (status = 503, description = "Focus capacity exhausted; body carries a `fallback` transport hint.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn output_whep_open(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    offer: String,
) -> Response {
    negotiate_whep(&state, &principal, &WhepScope::Output(id), &offer)
}

/// `DELETE /api/v1/preview/outputs/{id}/whep/{session_id}` — release an output
/// focus session (role: write).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/preview/outputs/{id}/whep/{session_id}",
        tag = "preview",
        params(
            ("id" = String, Path, description = "The output (rendition) id."),
            ("session_id" = String, Path, description = "The WHEP session id."),
        ),
        responses(
            (status = 204, description = "Focus session released."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to release a focus.", body = crate::problem::Problem),
            (status = 404, description = "No such live focus session.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn output_whep_close(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, session_id)): Path<(String, String)>,
) -> Response {
    release_whep(&state, &principal, &WhepScope::Output(id), &session_id)
}
