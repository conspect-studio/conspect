//! WHIP (RFC 9725) and WHEP (draft-ietf-wish-whep-03) HTTP signalling — the
//! reusable, framework-agnostic types the consumers mount.
//!
//! These are the protocol-level types `multiview-control` (which stays native-free
//! via its `WhipProvider`/`WhepProvider` seams) and the cli's provider
//! implementation share: the resource-path model
//! (`POST /api/v1/whip|whep/{id}` → `201` + `Location:
//! .../sessions/{session_id}`, `DELETE` to tear down), the offer-request
//! validation (content-type, body size cap), and the status-code mapping
//! (201/204/400/401/403/404/405/406/409/413/415/503). They carry no axum
//! dependency — control maps a [`SignalStatus`] to an axum response / RFC 9457
//! problem+json, and to its `OpenAPI` registration, exactly as it does for every
//! other route.
//!
//! ## Stated deviations (honest, per the brief)
//!
//! * `PATCH` → `405` + `Allow: DELETE, OPTIONS` (vanilla ICE; trickle / ICE
//!   restart unimplemented — RFC 9110 generic method semantics).
//! * A codec-incompatible offer → `406` (ecosystem practice; no server
//!   counter-offer `406` body mode).

use crate::session::SessionId;

/// The maximum `application/sdp` request body size (ADR-T014 §2 / ADR-0048 §4).
pub const MAX_SDP_BODY_BYTES: usize = 64 * 1024;

/// Which signalling protocol a route serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalKind {
    /// WHIP ingest (`/api/v1/whip/...`).
    Whip,
    /// WHEP egress (`/api/v1/whep/...`).
    Whep,
}

impl SignalKind {
    /// The URL path segment (`whip` / `whep`).
    #[must_use]
    pub const fn path_segment(self) -> &'static str {
        match self {
            Self::Whip => "whip",
            Self::Whep => "whep",
        }
    }
}

/// The HTTP status outcomes the signalling layer produces, mapped to codes by
/// [`SignalStatus::code`]. The consumer renders these as axum responses /
/// RFC 9457 problem+json.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalStatus {
    /// `201` — session created (offer accepted, answer in body, `Location` set).
    Created,
    /// `204` — session deleted (idempotent teardown).
    NoContent,
    /// `400` — malformed offer.
    BadRequest,
    /// `401` — missing/invalid credentials.
    Unauthorized,
    /// `403` — valid credentials lacking rights.
    Forbidden,
    /// `404` — unknown session resource.
    NotFound,
    /// `405` — method not allowed (`PATCH`).
    MethodNotAllowed,
    /// `406` — no compatible codec in the offer.
    NotAcceptable,
    /// `409` — a second publisher on a WHIP source.
    Conflict,
    /// `413` — SDP body over the 64 KiB cap.
    PayloadTooLarge,
    /// `415` — wrong content type.
    UnsupportedMediaType,
    /// `503` — endpoint at capacity (with `Retry-After`).
    ServiceUnavailable,
}

impl SignalStatus {
    /// The numeric HTTP status code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Created => 201,
            Self::NoContent => 204,
            Self::BadRequest => 400,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::NotAcceptable => 406,
            Self::Conflict => 409,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMediaType => 415,
            Self::ServiceUnavailable => 503,
        }
    }
}

/// A `201 Created` answer: the SDP answer body plus the session `Location`.
#[derive(Debug, Clone)]
pub struct SignalledAnswer {
    /// The HTTP status (always [`SignalStatus::Created`] for this constructor).
    pub status: SignalStatus,
    /// The `Location` header value (the session resource path).
    pub location: Option<String>,
    /// The response `Content-Type` (`application/sdp`).
    pub content_type: Option<String>,
    /// The SDP answer body.
    pub body: String,
}

impl SignalledAnswer {
    /// Build a `201 Created` answer for `kind`/`resource_id`/`session_id`.
    #[must_use]
    pub fn created(
        kind: SignalKind,
        resource_id: &str,
        session_id: &SessionId,
        answer_sdp: String,
    ) -> Self {
        Self {
            status: SignalStatus::Created,
            location: Some(session_resource_path(kind, resource_id, session_id)),
            content_type: Some("application/sdp".to_owned()),
            body: answer_sdp,
        }
    }
}

/// The session resource path: `/api/v1/{whip|whep}/{resource_id}/sessions/{id}`.
#[must_use]
pub fn session_resource_path(
    kind: SignalKind,
    resource_id: &str,
    session_id: &SessionId,
) -> String {
    format!(
        "/api/v1/{}/{}/sessions/{}",
        kind.path_segment(),
        resource_id,
        session_id.as_str()
    )
}

/// Validate the request line for a signalling offer `POST`.
///
/// # Errors
///
/// * [`SignalStatus::UnsupportedMediaType`] if `content_type` is not
///   `application/sdp` (parameters allowed).
/// * [`SignalStatus::BadRequest`] if the body is empty / not minimally SDP.
/// * [`SignalStatus::PayloadTooLarge`] if the body exceeds 64 KiB.
pub fn validate_offer_request(content_type: &str, body: &str) -> Result<(), SignalStatus> {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base != "application/sdp" {
        return Err(SignalStatus::UnsupportedMediaType);
    }
    if body.len() > MAX_SDP_BODY_BYTES {
        return Err(SignalStatus::PayloadTooLarge);
    }
    // A minimal SDP must at least carry a version line.
    if body.trim().is_empty() || !body.contains("v=") {
        return Err(SignalStatus::BadRequest);
    }
    Ok(())
}

/// The rejection for an unsupported `PATCH` (trickle / ICE restart): `405` with
/// the `Allow` header value.
#[must_use]
pub fn patch_rejection() -> (SignalStatus, &'static str) {
    (SignalStatus::MethodNotAllowed, "DELETE, OPTIONS")
}

/// CORS headers for the media-signalling routes (ADR-0048 §9): browsers must be
/// able to read `Location`/`Link` cross-origin or WHIP/WHEP teardown silently
/// breaks. Returns the `(allow-headers, expose-headers)` constants the consumer
/// applies alongside the configured `Access-Control-Allow-Origin`.
#[must_use]
pub const fn cors_header_policy() -> (&'static str, &'static str) {
    ("authorization, content-type", "location, link")
}
