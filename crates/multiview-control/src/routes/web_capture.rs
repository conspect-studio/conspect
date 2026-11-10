//! Background capture endpoints; role and object gates precede job admission.
use crate::{
    auth::{Action, Principal},
    error::ControlResult,
    state::AppState,
    web_capture::{CaptureBody, JobView},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

pub(crate) async fn capabilities(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<serde_json::Value>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.web_capture.capabilities()))
}
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CaptureBody>,
) -> ControlResult<(StatusCode, Json<serde_json::Value>)> {
    let fill = body.fill_id.clone();
    let id = state.web_capture.submit(&principal, body)?;
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "capture-fill",
        &fill,
        Some(serde_json::json!({"job_id":id})),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"job_id":id,"status_url":format!("/api/v1/media/captures/{id}")})),
    ))
}
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<u64>,
) -> ControlResult<Json<JobView>> {
    Ok(Json(state.web_capture.status(&principal, id)?))
}
pub(crate) async fn cancel(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<u64>,
) -> ControlResult<StatusCode> {
    state.web_capture.cancel(&principal, id)?;
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "capture-job",
        &id.to_string(),
        None,
    );
    Ok(StatusCode::ACCEPTED)
}
