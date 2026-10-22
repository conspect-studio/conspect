//! WHEP **output-viewer** signalling under `/api/v1/whep` (draft-ietf-wish-whep,
//! ADR-0049 §5.1).
//!
//! Multiview is the **server**: a browser WHEP player `POST`s an SDP offer to
//! `/api/v1/whep/{output_id}` and the endpoint answers `201` with the SDP answer
//! plus a `Location` at the per-session resource — then sends the real encoded
//! program rendition over SRTP. The control plane stays native-free: it delegates
//! to the [`WhepOutputProvider`](crate::whep_output::WhepOutputProvider) seam (a
//! trait object the binary implements over `multiview-webrtc`).
//!
//! These routes are a **real-output** surface distinct from the preview
//! `/api/v1/preview/…/whep` focus routes: a viewer here receives the *real* coded
//! program (never the FocusGate-capped, sheddable preview encode). Auth is the
//! per-output bearer `token` **or** a View-scope control-plane API key (never
//! anonymous, ADR-0049 §5.1; viewing the program is read-shaped, so **View**
//! suffices). `PATCH` is `405` (vanilla ICE — no trickle/restart); an unwired
//! build answers `503`. Errors are RFC 9457 `application/problem+json`.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::Action;
use crate::problem::Problem;
use crate::state::AppState;
use crate::whep_output::{WhepOutputAuth, WhepOutputReject};

/// The `application/sdp` body cap on the WHEP `POST` (ADR-0048 §4).
const MAX_SDP_BODY_BYTES: usize = 64 * 1024;

/// The base path of a WHEP output's resource (the `POST` target / `DELETE`
/// prefix). Built relative so no host is baked into the `Location` header.
fn whep_base_path(output_id: &str) -> String {
    format!("/api/v1/whep/{output_id}")
}

/// Resolve the presented credentials into a [`WhepOutputAuth`].
///
/// Reads the raw `Authorization: Bearer <token>` (the per-output token form), and
/// independently checks whether that bearer verifies as a control-plane API key
/// with at least **View** scope (read-shaped) so the provider can authorize on
/// either. When authentication is disabled every request is a local admin.
fn resolve_auth(state: &AppState, headers: &HeaderMap) -> WhepOutputAuth {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|t| t.trim().to_owned());

    if state.auth_disabled {
        return WhepOutputAuth {
            bearer,
            view_key: true,
        };
    }

    // Viewing is read-shaped: a key with at least Read/View scope suffices.
    let view_key = bearer
        .as_deref()
        .and_then(|t| state.api_keys.verify(t).ok())
        .is_some_and(|p| p.role.can(Action::Read));

    WhepOutputAuth { bearer, view_key }
}

/// Enforce per-**output** authorization on the **API-key** credential path (BOLA,
/// SEC-08 / ADR-W005/W026). The WHEP bearer is either the per-output token (which
/// the provider authorizes against the output's own secret) OR a control-plane
/// API key. When it is a valid API key, it must additionally be authorized for
/// THIS `output_id` on the output axis — a scoped View key may not view or tear
/// down an output outside its allowlist. The per-output token path (a bearer that
/// is not an API key) is unaffected.
///
/// Runs BEFORE the provider is touched, so a denial has zero side effect.
/// Returns `Some(403 /problems/forbidden)` for an out-of-scope API key, and
/// `None` for the token path, an in-scope key, or auth-disabled mode.
fn authorize_api_key_scope(
    state: &AppState,
    auth: &WhepOutputAuth,
    output_id: &str,
) -> Option<Response> {
    if state.auth_disabled {
        return None;
    }
    let bearer = auth.bearer.as_deref()?;
    // A bearer that does not verify as an API key is the per-output token path —
    // the provider authorizes it against the output's own token; leave it alone.
    let principal = state.api_keys.verify(bearer).ok()?;
    // It IS a control-plane API key: it must be in output scope for this output.
    crate::auth::authorize_output(&principal, output_id)
        .err()
        .map(|_| whep_reject_response(output_id, WhepOutputReject::Forbidden))
}

/// Map a [`WhepOutputReject`] onto its RFC 9457 `application/problem+json`
/// response.
fn whep_reject_response(output_id: &str, reject: WhepOutputReject) -> Response {
    let instance = whep_base_path(output_id);
    match reject {
        WhepOutputReject::Malformed(detail) => {
            Problem::new(400, "whep-malformed-offer", "Malformed WHEP offer")
                .with_detail(detail)
                .with_instance(instance)
                .into_response()
        }
        WhepOutputReject::Unauthorized => {
            let problem = Problem::new(401, "unauthorized", "Authentication required")
                .with_detail("a WHEP view requires the output token or a View API key")
                .with_instance(instance);
            let mut response = problem.into_response();
            if let Ok(value) = header::HeaderValue::from_str("Bearer") {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
            response
        }
        WhepOutputReject::Forbidden => Problem::new(403, "forbidden", "Not authorized to view")
            .with_detail("the presented credential may not view this output")
            .with_instance(instance)
            .into_response(),
        WhepOutputReject::NotFound => Problem::new(404, "not-found", "No such webrtc output")
            .with_detail("no configured webrtc output by that id")
            .with_instance(instance)
            .into_response(),
        WhepOutputReject::NoCompatibleCodec => Problem::new(
            406,
            "whep-no-codec",
            "No compatible codec in the WHEP offer",
        )
        .with_detail("Multiview serves H.264 video and Opus audio only")
        .with_instance(instance)
        .into_response(),
        WhepOutputReject::Unavailable => {
            let problem = Problem::new(503, "whep-unavailable", "WHEP viewer capacity exhausted")
                .with_detail(
                    "the per-output max_viewers or the endpoint viewer pool is full, \
                     or no serve transport is available",
                )
                .with_instance(instance);
            let mut response = problem.into_response();
            // A 503 carries Retry-After so a viewer backs off rather than hot-loops.
            if let Ok(value) = header::HeaderValue::from_str("5") {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

/// `POST /api/v1/whep/{output_id}` — view: an SDP offer in, `201` + answer SDP +
/// `Location` out (the program is then sent over SRTP).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/whep/{output_id}",
        tag = "whep",
        params(("output_id" = String, Path, description = "The webrtc output id to view.")),
        request_body(content = String, description = "SDP offer", content_type = "application/sdp"),
        responses(
            (status = 201, description = "Session created; SDP answer in the body, Location at the session resource.", content_type = "application/sdp"),
            (status = 400, description = "Malformed SDP offer.", body = crate::problem::Problem),
            (status = 401, description = "Missing credentials (the output token or a View API key).", body = crate::problem::Problem),
            (status = 403, description = "A credential lacking view rights.", body = crate::problem::Problem),
            (status = 404, description = "No configured webrtc output by that id.", body = crate::problem::Problem),
            (status = 406, description = "The offer shares no supported codec (H.264 + Opus).", body = crate::problem::Problem),
            (status = 415, description = "The request body is not application/sdp.", body = crate::problem::Problem),
            (status = 503, description = "Over max_viewers / the viewer pool, or no serve transport.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn whep_view(
    State(state): State<AppState>,
    Path(output_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let base_ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base_ct != "application/sdp" {
        return Problem::new(415, "unsupported-media-type", "Expected application/sdp")
            .with_detail("a WHEP offer must be sent as application/sdp")
            .with_instance(whep_base_path(&output_id))
            .into_response();
    }
    if body.len() > MAX_SDP_BODY_BYTES {
        return Problem::new(413, "payload-too-large", "SDP offer too large")
            .with_detail(format!(
                "the SDP offer exceeds the {MAX_SDP_BODY_BYTES} byte cap"
            ))
            .with_instance(whep_base_path(&output_id))
            .into_response();
    }
    let Ok(offer) = std::str::from_utf8(&body) else {
        return Problem::new(400, "whep-malformed-offer", "Malformed WHEP offer")
            .with_detail("the SDP offer body is not valid UTF-8")
            .with_instance(whep_base_path(&output_id))
            .into_response();
    };

    let auth = resolve_auth(&state, &headers);
    // Per-output authz on the API-key path (BOLA, SEC-08): a scoped key may view
    // only outputs within its output allowlist. The per-output token path is
    // unaffected; this runs before the transport (zero side effect).
    if let Some(denied) = authorize_api_key_scope(&state, &auth, &output_id) {
        return denied;
    }
    match state.whep_output.negotiate(&output_id, offer, &auth) {
        Ok(answer) => {
            let location = format!(
                "{}/sessions/{}",
                whep_base_path(&output_id),
                answer.session_id
            );
            let mut response = (StatusCode::CREATED, answer.sdp).into_response();
            let h = response.headers_mut();
            if let Ok(value) = header::HeaderValue::from_str("application/sdp") {
                h.insert(header::CONTENT_TYPE, value);
            }
            if let Ok(value) = header::HeaderValue::from_str(&location) {
                h.insert(header::LOCATION, value);
            }
            if let Ok(value) = header::HeaderValue::from_str("no-store") {
                h.insert(header::CACHE_CONTROL, value);
            }
            state.audit(
                &auth_key_id(&state, &auth),
                crate::audit::AuditAction::Command,
                "whep-output",
                &output_id,
                Some(serde_json::json!({ "command": "view-open" })),
            );
            response
        }
        Err(reject) => whep_reject_response(&output_id, reject),
    }
}

/// `DELETE /api/v1/whep/{output_id}/sessions/{session_id}` — tear the viewer
/// session down. `200` (idempotent within the tombstone window), `404` for an
/// unknown id.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/whep/{output_id}/sessions/{session_id}",
        tag = "whep",
        params(
            ("output_id" = String, Path, description = "The webrtc output id."),
            ("session_id" = String, Path, description = "The WHEP session id from the view Location."),
        ),
        responses(
            (status = 200, description = "Session released (idempotent within the tombstone window)."),
            (status = 401, description = "Missing credentials.", body = crate::problem::Problem),
            (status = 403, description = "A credential lacking rights.", body = crate::problem::Problem),
            (status = 404, description = "No such live/known session.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn whep_delete(
    State(state): State<AppState>,
    Path((output_id, session_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let auth = resolve_auth(&state, &headers);
    // DELETE requires the same credential class as the creating POST.
    if auth.bearer.is_none() {
        return whep_reject_response(&output_id, WhepOutputReject::Unauthorized);
    }
    // Per-output authz on the API-key path (BOLA, SEC-08): a scoped key may tear
    // down only sessions on outputs within its output allowlist. The provider's
    // release does not re-check authorization, so this route guard is what makes
    // a denied teardown a 403 with zero side effect.
    if let Some(denied) = authorize_api_key_scope(&state, &auth, &output_id) {
        return denied;
    }
    if state.whep_output.release(&output_id, &session_id, &auth) {
        state.audit(
            &auth_key_id(&state, &auth),
            crate::audit::AuditAction::Command,
            "whep-output",
            &output_id,
            Some(serde_json::json!({ "command": "view-close", "session": session_id })),
        );
        StatusCode::OK.into_response()
    } else {
        Problem::new(404, "not-found", "WHEP session not found")
            .with_detail(format!(
                "no known session {session_id:?} for webrtc output {output_id:?}"
            ))
            .with_instance(format!(
                "{}/sessions/{session_id}",
                whep_base_path(&output_id)
            ))
            .into_response()
    }
}

/// `PATCH /api/v1/whep/{output_id}/sessions/{session_id}` — **`405`** (vanilla
/// ICE; trickle / ICE restart unimplemented, ADR-0049 §5.1). `Allow: DELETE,
/// OPTIONS`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        patch,
        path = "/api/v1/whep/{output_id}/sessions/{session_id}",
        tag = "whep",
        params(
            ("output_id" = String, Path, description = "The webrtc output id."),
            ("session_id" = String, Path, description = "The WHEP session id."),
        ),
        responses(
            (status = 405, description = "Trickle ICE / ICE restart are not supported (vanilla ICE)."),
        ),
    )
)]
pub(crate) async fn whep_patch() -> Response {
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    if let Ok(value) = header::HeaderValue::from_str("DELETE, OPTIONS") {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
}

/// `OPTIONS /api/v1/whep/{output_id}` — CORS preflight; advertises
/// `Accept-Post: application/sdp`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        options,
        path = "/api/v1/whep/{output_id}",
        tag = "whep",
        params(("output_id" = String, Path, description = "The webrtc output id.")),
        responses((status = 204, description = "Preflight; Accept-Post: application/sdp.")),
    )
)]
pub(crate) async fn whep_options(Path(_output_id): Path<String>) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let h = response.headers_mut();
    if let Ok(value) = header::HeaderValue::from_str("application/sdp") {
        h.insert("accept-post", value);
    }
    response
}

/// The non-secret key id for the audit trail: the verified API-key id when a
/// key was used, else a stable label for a token-authenticated viewer (the
/// per-output token is a secret and is never logged).
fn auth_key_id(state: &AppState, auth: &WhepOutputAuth) -> String {
    if !state.auth_disabled {
        if let Some(principal) = auth
            .bearer
            .as_deref()
            .and_then(|t| state.api_keys.verify(t).ok())
        {
            return principal.key_id;
        }
    }
    "whep-output-token".to_owned()
}
