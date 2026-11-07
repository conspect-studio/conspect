//! Authorized read-only source event snapshots, and the operator
//! test-injection aid (ADR-0183 item 5). No automation hooks live here.
use crate::{
    audit::AuditAction,
    auth::{authorize_object, Action, Principal},
    problem::Problem,
    source_events::{TestEventBoundary, TestEventOutcome},
    state::AppState,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/api/v1/inputs/{id}/events", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity; no suffix normalization")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Bounded observational snapshot (possibly empty). Cache-Control: no-store.", body = crate::source_events::SourceEventsDto),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
        (status = 503, description = "Source event provider unavailable", body = Problem),
    )
))]
pub(crate) async fn get_source_events(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    let mut response = if let Err(error) = principal.role.require(Action::Read) {
        error.into_response()
    } else if let Err(error) = authorize_object(&principal, &id) {
        error.into_response()
    } else if let Some(provider) = &state.source_events {
        match provider.source_events(&id) {
            Some(snapshot) if snapshot.matches_source(&id) => Json(snapshot).into_response(),
            Some(_) => Problem::new(
                503,
                "source-events-unavailable",
                "Invalid source event snapshot",
            )
            .into_response(),
            None => Problem::new(404, "not-found", "Unknown source").into_response(),
        }
    } else {
        Problem::new(
            503,
            "source-events-unavailable",
            "Source event provider unavailable",
        )
        .into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Request body for the operator test-injection aid.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct TestEventRequest {
    /// `"start"` or `"end"` — see [`TestEventBoundary`].
    boundary: TestEventRequestBoundary,
    /// Declared break duration in nanoseconds (only meaningful for `"start"`).
    #[serde(default)]
    duration_ns: Option<i64>,
}
/// Wire boundary selector, internally tagged (never untagged).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) enum TestEventRequestBoundary {
    /// Announce/confirm a synthetic break start.
    Start,
    /// Confirm a synthetic break end.
    End,
}

/// `POST /api/v1/inputs/{id}/events/test` — inject a clearly-labelled synthetic
/// event for operator/automation testing (ADR-0183 item 5; role: administer).
///
/// This can only affect a source that already has an ACTIVE event-tracking
/// generation (a source the running engine has ingested, live or previously
/// connected) — it never allocates a new source identity or epoch, and the
/// injected evidence is tagged `origin: "external"` with a `test:` event id
/// prefix so it is unmistakably distinguishable from real evidence in the
/// API/UI. It never bypasses the bounded worker queue or payload limits real
/// evidence uses.
#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/api/v1/inputs/{id}/events/test", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity; no suffix normalization")),
    request_body = TestEventRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 202, description = "Synthetic observation accepted into the bounded worker queue."),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope, or insufficient role", body = Problem),
        (status = 404, description = "Unknown or never-active source", body = Problem),
        (status = 503, description = "Source event test-injection adapter unavailable, or the queue is full", body = Problem),
    )
))]
pub(crate) async fn post_source_event_test(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(request): Json<TestEventRequest>,
) -> Response {
    if let Err(error) = principal.role.require(Action::Administer) {
        return error.into_response();
    }
    if let Err(error) = authorize_object(&principal, &id) {
        return error.into_response();
    }
    let Some(injector) = &state.source_event_test_injector else {
        return Problem::new(
            503,
            "source-events-unavailable",
            "Source event test-injection adapter unavailable",
        )
        .into_response();
    };
    let boundary = match request.boundary {
        TestEventRequestBoundary::Start => TestEventBoundary::Start {
            duration_ns: request.duration_ns,
        },
        TestEventRequestBoundary::End => TestEventBoundary::End,
    };
    let outcome = injector.inject_test_event(&id, boundary);
    let response = match outcome {
        TestEventOutcome::Queued => StatusCode::ACCEPTED.into_response(),
        TestEventOutcome::Overflow => Problem::new(
            503,
            "source-events-unavailable",
            "Source event queue is full; this source's evidence is now degraded",
        )
        .into_response(),
        TestEventOutcome::Unknown => {
            Problem::new(404, "not-found", "Unknown or never-active source").into_response()
        }
    };
    if matches!(outcome, TestEventOutcome::Queued) {
        state.audit(
            &principal.key_id,
            AuditAction::Command,
            "source",
            &id,
            Some(serde_json::json!({ "command": "source_event_test", "boundary": match request.boundary {
                TestEventRequestBoundary::Start => "start",
                TestEventRequestBoundary::End => "end",
            } })),
        );
    }
    response
}
