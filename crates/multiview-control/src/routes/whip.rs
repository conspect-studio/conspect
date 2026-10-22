//! WHIP ingest signalling under `/api/v1/whip` (RFC 9725, ADR-T014 §2).
//!
//! Multiview is the **server**: a browser or contribution encoder (OBS ≥ 30,
//! `GStreamer` `whipclientsink`) `POST`s an SDP offer to
//! `/api/v1/whip/{source_id}` and the endpoint answers `201` with the SDP answer
//! plus a `Location` at the per-session resource. The control plane stays
//! native-free: it delegates to the [`WhipProvider`](crate::whip::WhipProvider)
//! seam (a trait object the binary implements over `multiview-webrtc`).
//!
//! The endpoint URL is **derived from the source id, never configured**. Auth is
//! the per-source bearer `token` **or** a Write-scope control-plane API key
//! (never anonymous, ADR-T014 §2); the route resolves both forms into a
//! [`WhipAuth`](crate::whip::WhipAuth) and lets the provider authorize.
//! Unsupported operations are honest: `PATCH` is `405` (vanilla ICE — no
//! trickle/restart), and an unwired build answers `503`. Errors are RFC 9457
//! `application/problem+json`.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::Action;
use crate::problem::Problem;
use crate::state::AppState;
use crate::whip::{WhipAuth, WhipReject};

/// The `application/sdp` body cap on the WHIP `POST` (ADR-T014 §2 / ADR-0048 §4).
const MAX_SDP_BODY_BYTES: usize = 64 * 1024;

/// The base path of a WHIP source's resource (the `POST` target / `DELETE`
/// prefix). Built relative so no host is baked into the `Location` header.
fn whip_base_path(source_id: &str) -> String {
    format!("/api/v1/whip/{source_id}")
}

/// Resolve the presented credentials into a [`WhipAuth`].
///
/// Reads the raw `Authorization: Bearer <token>` (the per-source token form),
/// and independently checks whether that same bearer verifies as a
/// **Write-scope** control-plane API key (Operator/Admin) so the provider can
/// authorize on either. When authentication is disabled (an explicit local
/// deployment mode) every request is a local admin (Write).
fn resolve_auth(state: &AppState, headers: &HeaderMap) -> WhipAuth {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|t| t.trim().to_owned());

    if state.auth_disabled {
        return WhipAuth {
            bearer,
            write_key: true,
        };
    }

    let write_key = bearer
        .as_deref()
        .and_then(|t| state.api_keys.verify(t).ok())
        .is_some_and(|p| p.role.can(Action::Write));

    WhipAuth { bearer, write_key }
}

/// Enforce per-object authorization on the **API-key** credential path (BOLA,
/// SEC-08 / ADR-W005). The WHIP bearer is either the per-source token (which the
/// provider authorizes against the source's own secret) OR a control-plane API
/// key. When it is a valid API key, it must additionally be authorized for THIS
/// `source_id` on the object axis — a scoped Write key may not publish to or tear
/// down a source outside its allowlist. The per-source token path (a bearer that
/// is not an API key) is unaffected.
///
/// Runs BEFORE the provider is touched, so a denial has zero side effect.
/// Returns `Some(403 /problems/forbidden)` for an out-of-scope API key, and
/// `None` for the token path, an in-scope key, or auth-disabled mode.
fn authorize_api_key_scope(state: &AppState, auth: &WhipAuth, source_id: &str) -> Option<Response> {
    if state.auth_disabled {
        return None;
    }
    let bearer = auth.bearer.as_deref()?;
    // A bearer that does not verify as an API key is the per-source token path —
    // the provider authorizes it against the source's own token; leave it alone.
    let principal = state.api_keys.verify(bearer).ok()?;
    // It IS a control-plane API key: it must be in object scope for this source.
    crate::auth::authorize_object(&principal, source_id)
        .err()
        .map(|_| whip_reject_response(source_id, WhipReject::Forbidden))
}

/// Map a [`WhipReject`] onto its RFC 9457 `application/problem+json` response
/// (ADR-T014 §2 status mapping).
fn whip_reject_response(source_id: &str, reject: WhipReject) -> Response {
    let instance = whip_base_path(source_id);
    match reject {
        WhipReject::Malformed(detail) => {
            Problem::new(400, "whip-malformed-offer", "Malformed WHIP offer")
                .with_detail(detail)
                .with_instance(instance)
                .into_response()
        }
        WhipReject::Unauthorized => {
            let problem = Problem::new(401, "unauthorized", "Authentication required")
                .with_detail("a WHIP publish requires the source token or a Write API key")
                .with_instance(instance);
            // RFC 6750: a 401 advertises the Bearer scheme.
            let mut response = problem.into_response();
            if let Ok(value) = header::HeaderValue::from_str("Bearer") {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
            response
        }
        WhipReject::Forbidden => Problem::new(403, "forbidden", "Not authorized to publish")
            .with_detail("the presented credential may not publish to this source")
            .with_instance(instance)
            .into_response(),
        WhipReject::NoCompatibleCodec => Problem::new(
            406,
            "whip-no-codec",
            "No compatible codec in the WHIP offer",
        )
        .with_detail("Multiview answers H.264 video and Opus audio only")
        .with_instance(instance)
        .into_response(),
        WhipReject::Conflict => Problem::new(
            409,
            "whip-publisher-conflict",
            "A publisher already holds this source",
        )
        .with_detail("one publisher per WHIP source; free the slot with DELETE")
        .with_instance(instance)
        .into_response(),
        WhipReject::Unavailable => {
            let problem = Problem::new(503, "whip-unavailable", "WHIP ingest unavailable")
                .with_detail("no ingest transport is available to admit this publisher")
                .with_instance(instance);
            let mut response = problem.into_response();
            // A 503 carries Retry-After so a publisher backs off rather than hot-loops.
            if let Ok(value) = header::HeaderValue::from_str("5") {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

/// `POST /api/v1/whip/{source_id}` — publish: an SDP offer in, `201` + answer
/// SDP + `Location` out (RFC 9725).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/whip/{source_id}",
        tag = "whip",
        params(("source_id" = String, Path, description = "The webrtc source id to publish to.")),
        request_body(content = String, description = "SDP offer", content_type = "application/sdp"),
        responses(
            (status = 201, description = "Session created; SDP answer in the body, Location at the session resource.", content_type = "application/sdp"),
            (status = 400, description = "Malformed SDP offer.", body = crate::problem::Problem),
            (status = 401, description = "Missing credentials (the source token or a Write API key).", body = crate::problem::Problem),
            (status = 403, description = "A credential lacking publish rights.", body = crate::problem::Problem),
            (status = 406, description = "The offer shares no supported codec (H.264 + Opus).", body = crate::problem::Problem),
            (status = 409, description = "A publisher already holds this source.", body = crate::problem::Problem),
            (status = 415, description = "The request body is not application/sdp.", body = crate::problem::Problem),
            (status = 503, description = "No ingest transport available to admit the publisher.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn whip_publish(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Content type must be application/sdp (415 otherwise).
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
            .with_detail("a WHIP offer must be sent as application/sdp")
            .with_instance(whip_base_path(&source_id))
            .into_response();
    }
    // Body cap (413). An explicit limit on this route, not the global one.
    if body.len() > MAX_SDP_BODY_BYTES {
        return Problem::new(413, "payload-too-large", "SDP offer too large")
            .with_detail(format!(
                "the SDP offer exceeds the {MAX_SDP_BODY_BYTES} byte cap"
            ))
            .with_instance(whip_base_path(&source_id))
            .into_response();
    }
    let Ok(offer) = std::str::from_utf8(&body) else {
        return Problem::new(400, "whip-malformed-offer", "Malformed WHIP offer")
            .with_detail("the SDP offer body is not valid UTF-8")
            .with_instance(whip_base_path(&source_id))
            .into_response();
    };

    let auth = resolve_auth(&state, &headers);
    // Per-object authz on the API-key path (BOLA, SEC-08): a scoped key may
    // publish only to sources within its object allowlist. The per-source token
    // path is unaffected; this runs before the transport (zero side effect).
    if let Some(denied) = authorize_api_key_scope(&state, &auth, &source_id) {
        return denied;
    }
    match state.whip.negotiate(&source_id, offer, &auth) {
        Ok(answer) => {
            let location = format!(
                "{}/sessions/{}",
                whip_base_path(&source_id),
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
                "whip-ingest",
                &source_id,
                Some(serde_json::json!({ "command": "publish-open" })),
            );
            response
        }
        Err(reject) => whip_reject_response(&source_id, reject),
    }
}

/// `DELETE /api/v1/whip/{source_id}/sessions/{session_id}` — tear the session
/// down. `200` (idempotent within the tombstone window), `404` for an unknown id.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/whip/{source_id}/sessions/{session_id}",
        tag = "whip",
        params(
            ("source_id" = String, Path, description = "The webrtc source id."),
            ("session_id" = String, Path, description = "The WHIP session id from the publish Location."),
        ),
        responses(
            (status = 200, description = "Session released (idempotent within the tombstone window)."),
            (status = 401, description = "Missing credentials.", body = crate::problem::Problem),
            (status = 403, description = "A credential lacking rights.", body = crate::problem::Problem),
            (status = 404, description = "No such live/known session.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn whip_delete(
    State(state): State<AppState>,
    Path((source_id, session_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let auth = resolve_auth(&state, &headers);
    // DELETE requires the same credential class as the creating POST: either the
    // source token bearer or a Write API key. A request with neither is 401/403.
    if auth.bearer.is_none() {
        return whip_reject_response(&source_id, WhipReject::Unauthorized);
    }
    // Per-object authz on the API-key path (BOLA, SEC-08): a scoped key may tear
    // down only sessions on sources within its object allowlist. The provider's
    // release does not re-check authorization, so this route guard is what makes
    // a denied teardown a 403 with zero side effect.
    if let Some(denied) = authorize_api_key_scope(&state, &auth, &source_id) {
        return denied;
    }
    if state.whip.release(&source_id, &session_id, &auth) {
        state.audit(
            &auth_key_id(&state, &auth),
            crate::audit::AuditAction::Command,
            "whip-ingest",
            &source_id,
            Some(serde_json::json!({ "command": "publish-close", "session": session_id })),
        );
        StatusCode::OK.into_response()
    } else {
        Problem::new(404, "not-found", "WHIP session not found")
            .with_detail(format!(
                "no known session {session_id:?} for webrtc source {source_id:?}"
            ))
            .with_instance(format!(
                "{}/sessions/{session_id}",
                whip_base_path(&source_id)
            ))
            .into_response()
    }
}

/// `PATCH /api/v1/whip/{source_id}/sessions/{session_id}` — **`405`** (vanilla
/// ICE; trickle / ICE restart unimplemented, ADR-T014 §2). `Allow: DELETE,
/// OPTIONS`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        patch,
        path = "/api/v1/whip/{source_id}/sessions/{session_id}",
        tag = "whip",
        params(
            ("source_id" = String, Path, description = "The webrtc source id."),
            ("session_id" = String, Path, description = "The WHIP session id."),
        ),
        responses(
            (status = 405, description = "Trickle ICE / ICE restart are not supported (vanilla ICE)."),
        ),
    )
)]
pub(crate) async fn whip_patch() -> Response {
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    if let Ok(value) = header::HeaderValue::from_str("DELETE, OPTIONS") {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
}

/// `OPTIONS /api/v1/whip/{source_id}` — CORS preflight; advertises
/// `Accept-Post: application/sdp`. Unauthenticated by browser construction.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        options,
        path = "/api/v1/whip/{source_id}",
        tag = "whip",
        params(("source_id" = String, Path, description = "The webrtc source id.")),
        responses((status = 204, description = "Preflight; Accept-Post: application/sdp.")),
    )
)]
pub(crate) async fn whip_options(Path(_source_id): Path<String>) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let h = response.headers_mut();
    if let Ok(value) = header::HeaderValue::from_str("application/sdp") {
        h.insert("accept-post", value);
    }
    response
}

/// The non-secret key id for the audit trail: the verified API-key id when a
/// Write key was used, else a stable label for a token-authenticated publisher
/// (the per-source token is a secret and is never logged).
fn auth_key_id(state: &AppState, auth: &WhipAuth) -> String {
    if !state.auth_disabled {
        if let Some(principal) = auth
            .bearer
            .as_deref()
            .and_then(|t| state.api_keys.verify(t).ok())
        {
            return principal.key_id;
        }
    }
    "whip-source-token".to_owned()
}
