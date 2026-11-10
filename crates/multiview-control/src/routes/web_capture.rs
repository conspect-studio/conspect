//! Background capture endpoints; authorization precedes admission and pinning.
use crate::{
    auth::{Action, Principal},
    error::{ControlError, ControlResult},
    state::AppState,
    web_capture::{CaptureBody, JobView},
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::Response,
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
    let provider = state.web_capture.clone();
    let actor = principal.clone();
    let id = tokio::task::spawn_blocking(move || provider.submit(&actor, body))
        .await
        .map_err(|_| ControlError::Repository("capture admission worker failed".into()))??;
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
    let provider = state.web_capture.clone();
    let actor = principal.clone();
    tokio::task::spawn_blocking(move || provider.cancel(&actor, id))
        .await
        .map_err(|_| ControlError::Repository("capture cancellation worker failed".into()))??;
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Command,
        "capture-job",
        &id.to_string(),
        None,
    );
    Ok(StatusCode::ACCEPTED)
}
pub(crate) async fn artifact(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<u64>,
) -> ControlResult<Response> {
    let provider = state.web_capture.clone();
    let lease = tokio::task::spawn_blocking(move || provider.pin(&principal, id))
        .await
        .map_err(|_| ControlError::Repository("artifact pin worker failed".into()))??;
    // Bounded channel: no whole-clip allocation and no filesystem I/O on an async worker.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(2);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut lease = lease;
        loop {
            let mut chunk = vec![0; 64 * 1024];
            match lease.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    chunk.truncate(n);
                    if tx.blocking_send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/quicktime")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| ControlError::Repository("artifact response failed".into()))
}
