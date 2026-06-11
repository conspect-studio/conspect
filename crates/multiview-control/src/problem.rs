//! [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) problem-details responses.
//!
//! Every error the API returns is an `application/problem+json` document with
//! the standard members `type`, `title`, `status`, `detail`, and `instance`
//! (conventions §6). The HTTP `status` line mirrors the `status` member. The
//! `type` is a stable relative slug under `/problems/` so clients can branch on
//! it without parsing prose.
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// The base URI prefix for problem `type` slugs.
///
/// A problem `type` of `not-found` renders as `/problems/not-found`. Keeping it
/// relative avoids baking an external host into the contract.
const PROBLEM_TYPE_BASE: &str = "/problems/";

/// The `application/problem+json` media type (RFC 9457 §3).
pub const PROBLEM_JSON: &str = "application/problem+json";

/// An RFC 9457 problem-details document.
///
/// Serialized as `application/problem+json`. The `status` field is authoritative
/// and is also used as the HTTP status code by the [`IntoResponse`] impl. A
/// `status` outside the valid `100..=599` range falls back to `500`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Problem {
    /// A URI reference identifying the problem type (here a `/problems/<slug>`).
    #[serde(rename = "type")]
    pub problem_type: String,
    /// A short, human-readable summary of the problem type.
    pub title: String,
    /// The HTTP status code, duplicated into the body per RFC 9457.
    pub status: u16,
    /// A human-readable explanation specific to this occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A URI reference identifying this specific occurrence (e.g. the request
    /// path or an operation id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl Problem {
    /// Construct a problem with the given status, `type` slug, and title.
    ///
    /// The `slug` is appended to the `/problems/` base to form the `type` URI.
    #[must_use]
    pub fn new(status: u16, slug: &str, title: impl Into<String>) -> Self {
        Self {
            problem_type: format!("{PROBLEM_TYPE_BASE}{slug}"),
            title: title.into(),
            status,
            detail: None,
            instance: None,
        }
    }

    /// Attach a per-occurrence `detail` string.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach a per-occurrence `instance` URI.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// The HTTP status code this problem maps to (clamping an invalid value to
    /// `500`).
    #[must_use]
    pub fn http_status(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.http_status();
        // `serde_json::to_vec` on this fixed struct cannot fail; if it somehow
        // did we still must return a body, so fall back to a minimal literal.
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| {
            br#"{"type":"/problems/repository","title":"serialization failed","status":500}"#
                .to_vec()
        });
        (
            status,
            [(header::CONTENT_TYPE, PROBLEM_JSON)],
            axum::body::Body::from(body),
        )
            .into_response()
    }
}
