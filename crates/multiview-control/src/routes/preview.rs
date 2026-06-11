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
use crate::state::AppState;

/// The JPEG quality used for preview stills (1–100): low enough to be cheap,
/// high enough to be useful for monitoring.
const PREVIEW_QUALITY: u8 = 70;

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

/// `GET /api/v1/preview/inputs/{id}.jpg` — the latest frame of input `id`
/// (role: read). `503` when the input is unknown or has produced no frame.
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
    match state.preview.input_jpeg(id, PREVIEW_QUALITY) {
        Some(bytes) => jpeg_response(bytes),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
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
    Json(state.preview.input_ids()).into_response()
}

/// The base path of a WHEP scope's focus resource (the `POST` target and the
/// prefix of the per-session `DELETE` URL). Built relative so no host is baked
/// into the `Location` header.
fn whep_base_path(scope: &WhepScope) -> String {
    match scope {
        WhepScope::Program => "/api/v1/preview/program/whep".to_owned(),
        WhepScope::Input(id) => format!("/api/v1/preview/inputs/{id}/whep"),
        WhepScope::Output(id) => format!("/api/v1/preview/outputs/{id}/whep"),
    }
}

/// Map a [`WhepReject`] onto its RFC 9457 `application/problem+json` response.
///
/// A `CapacityExceeded` carries the `fallback` transport hint as a non-standard
/// problem member so the UI can degrade to the named transport (`ws-jpeg`,
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
    match state.whep.negotiate(scope, offer) {
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
