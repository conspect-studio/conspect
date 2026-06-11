//! The webhook notifier: a pure builder over an alarm transition.
//!
//! [`WebhookRequest::build`] turns an alarm transition plus a destination URL
//! (and optional bearer token) into a fully-formed HTTP request value — method,
//! URL, headers, and a canonical JSON body — with **no network I/O**. That makes
//! the wire contract (what we POST for a given alarm) exhaustively unit-testable.
//!
//! The actual send is deliberately a thin, separate step
//! ([`WebhookRequest::into_http_request`]) that hands the built value to the
//! crate's existing `axum`/`http` stack. The control plane drives the send off
//! the request path on its own task; it is never on the engine's data plane, so
//! a slow or failing webhook endpoint can never back-pressure the engine
//! (invariant #10).
use multiview_core::alarm::AlarmRecord;
use serde::{Deserialize, Serialize};

use super::AlarmTransitionKind;

/// The JSON body posted to a webhook for an alarm transition.
///
/// A stable, self-describing payload: the transition kind plus the full X.733
/// [`AlarmRecord`]. Receivers branch on `transition` and read the record fields
/// (severity, scope, kind, dwell, ack state) without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookPayload {
    /// Which lifecycle transition this notification reports.
    pub transition: String,
    /// The full alarm record after the transition.
    pub alarm: AlarmRecord,
}

/// A fully-formed webhook request, built purely from an alarm transition.
///
/// Holds everything needed to issue the POST: the target `url`, an optional
/// bearer `token`, and the serialised JSON `body`. Constructed with
/// [`WebhookRequest::build`]; converted to a live `http::Request` with
/// [`WebhookRequest::into_http_request`] only at send time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRequest {
    /// The absolute endpoint URL.
    pub url: String,
    /// The optional bearer token sent in the `Authorization` header.
    pub token: Option<String>,
    /// The canonical JSON body.
    pub body: String,
}

impl WebhookRequest {
    /// Build the webhook request for `record` undergoing `transition`, posted to
    /// `url` with an optional bearer `token`.
    ///
    /// This is a **pure** function: it serialises the payload and assembles the
    /// request value but performs no I/O.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the alarm record cannot be serialised, which is
    /// not possible for the fixed [`AlarmRecord`] shape — callers may treat an
    /// error as "skip this notification".
    pub fn build(
        url: impl Into<String>,
        token: Option<String>,
        transition: AlarmTransitionKind,
        record: &AlarmRecord,
    ) -> Result<Self, serde_json::Error> {
        let payload = WebhookPayload {
            transition: transition.as_str().to_owned(),
            alarm: record.clone(),
        };
        let body = serde_json::to_string(&payload)?;
        Ok(Self {
            url: url.into(),
            token,
            body,
        })
    }

    /// Convert this built request into a live `axum`/`http` request ready to be
    /// driven by an HTTP client.
    ///
    /// Sets `POST`, `Content-Type: application/json`, and (when a token is
    /// present) `Authorization: Bearer <token>`.
    ///
    /// # Errors
    ///
    /// [`axum::http::Error`] if the URL or header values are not valid HTTP
    /// (e.g. a URL with control characters).
    pub fn into_http_request(self) -> Result<axum::http::Request<String>, axum::http::Error> {
        use axum::http::{header, Method, Request};
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(self.url)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(self.body)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
    use multiview_core::time::MediaTime;

    use super::{AlarmTransitionKind, WebhookPayload, WebhookRequest};

    fn record() -> AlarmRecord {
        AlarmRecord::new(
            AlarmId::new("probe-7"),
            AlarmKind::Freeze,
            PerceivedSeverity::Major,
            AlarmScope::Tile { index: 3 },
            MediaTime::from_nanos(42),
        )
    }

    #[test]
    fn build_serialises_transition_and_record_into_the_body() {
        let req = WebhookRequest::build(
            "https://hook.example/alarms",
            Some("secret".to_owned()),
            AlarmTransitionKind::Raised,
            &record(),
        )
        .expect("build is infallible for a valid record");

        assert_eq!(req.url, "https://hook.example/alarms");
        assert_eq!(req.token.as_deref(), Some("secret"));

        // The body is the canonical JSON of the payload and round-trips back to
        // the same transition + record.
        let payload: WebhookPayload = serde_json::from_str(&req.body).unwrap();
        assert_eq!(payload.transition, "raised");
        assert_eq!(payload.alarm, record());
    }

    #[test]
    fn into_http_request_sets_method_content_type_and_bearer() {
        let req = WebhookRequest::build(
            "https://hook.example/alarms",
            Some("tok-123".to_owned()),
            AlarmTransitionKind::Cleared,
            &record(),
        )
        .unwrap();
        let body = req.body.clone();
        let http = req.into_http_request().expect("valid http request");

        assert_eq!(http.method(), axum::http::Method::POST);
        assert_eq!(http.uri(), "https://hook.example/alarms");
        assert_eq!(
            http.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("application/json")
        );
        assert_eq!(
            http.headers()
                .get(axum::http::header::AUTHORIZATION)
                .map(|v| v.to_str().unwrap()),
            Some("Bearer tok-123")
        );
        assert_eq!(http.body(), &body);
    }

    #[test]
    fn into_http_request_omits_authorization_without_a_token() {
        let req = WebhookRequest::build(
            "https://hook.example/x",
            None,
            AlarmTransitionKind::Acked,
            &record(),
        )
        .unwrap();
        let http = req.into_http_request().unwrap();
        assert!(http
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .is_none());
    }
}
