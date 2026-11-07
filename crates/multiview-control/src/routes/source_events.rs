//! Authorized read-only source event snapshots; no automation hooks.
use crate::{
    auth::{authorize_object, Action, Principal},
    problem::Problem,
    state::AppState,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};

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
