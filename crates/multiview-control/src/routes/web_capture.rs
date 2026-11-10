//! Background capture endpoints; authorization precedes admission and pinning.
use crate::{
    auth::{authorize_object, Action, Principal},
    concurrency::IdempotencyKey,
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
fn required_key(key: IdempotencyKey) -> ControlResult<String> {
    match key.0 {
        Some(k) if !k.is_empty() && k.len() <= 128 && k.is_ascii() => Ok(k),
        _ => Err(ControlError::Validation(
            "a valid Idempotency-Key header is required".into(),
        )),
    }
}
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/captures/capabilities",
        tag = "media-captures",
        responses(
            (status = 200, description = "Honest capture capabilities."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn capabilities(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<serde_json::Value>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.web_capture.capabilities()))
}
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/captures",
        tag = "media-captures",
        params(("Idempotency-Key" = String, Header, description = "Required; same key and body replays the same job.")),
        responses(
            (status = 202, description = "Capture job accepted."),
            (status = 400, description = "Invalid body, terms or missing key.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "Unknown target player.", body = crate::problem::Problem),
            (status = 409, description = "Key reused with a different body.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    key: IdempotencyKey,
    Json(body): Json<CaptureBody>,
) -> ControlResult<(StatusCode, Json<serde_json::Value>)> {
    principal.role.require(Action::Write)?;
    authorize_object(&principal, &body.fill_id)?;
    let key = required_key(key)?;
    if let Some(player) = &body.target_player {
        authorize_object(&principal, player)?;
        state.media_players.get(player)?;
    }
    let fill = body.fill_id.clone();
    let provider = state.web_capture.clone();
    let actor = principal.clone();
    let admission = tokio::task::spawn_blocking(move || provider.submit(&actor, &key, body))
        .await
        .map_err(|_| ControlError::Repository("capture admission worker failed".into()))??;
    let id = admission.job_id;
    if admission.fresh {
        state.audit(
            &principal.key_id,
            crate::audit::AuditAction::Command,
            "capture-fill",
            &fill,
            Some(serde_json::json!({"job_id":id})),
        );
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"job_id":id,"status_url":format!("/api/v1/media/captures/{id}")})),
    ))
}
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/captures/{id}",
        tag = "media-captures",
        params(("id" = u64, Path, description = "Capture job id.")),
        responses(
            (status = 200, description = "Job status."),
            (status = 404, description = "Unknown or not owned.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<u64>,
) -> ControlResult<Json<JobView>> {
    Ok(Json(state.web_capture.status(&principal, id)?))
}
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/media/captures/{id}/cancel",
        tag = "media-captures",
        params(("id" = u64, Path, description = "Capture job id."),
               ("Idempotency-Key" = String, Header, description = "Required.")),
        responses(
            (status = 202, description = "Cancellation accepted."),
            (status = 404, description = "Unknown or not owned.", body = crate::problem::Problem),
            (status = 409, description = "Already finished or key conflict.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    principal: Principal,
    key: IdempotencyKey,
    Path(id): Path<u64>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Write)?;
    let key = required_key(key)?;
    let provider = state.web_capture.clone();
    let actor = principal.clone();
    let result = tokio::task::spawn_blocking(move || provider.cancel(&actor, &key, id))
        .await
        .map_err(|_| ControlError::Repository("capture cancellation worker failed".into()))??;
    if result.fresh {
        state.audit(
            &principal.key_id,
            crate::audit::AuditAction::Command,
            "capture-job",
            &id.to_string(),
            None,
        );
    }
    Ok(StatusCode::ACCEPTED)
}
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/captures/{id}/artifact",
        tag = "media-captures",
        params(("id" = u64, Path, description = "Capture job id.")),
        responses(
            (status = 200, description = "Artifact bytes (video/quicktime)."),
            (status = 404, description = "Unknown or not owned.", body = crate::problem::Problem),
            (status = 503, description = "Stream capacity exhausted.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn artifact(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<u64>,
) -> ControlResult<Response> {
    let provider = state.web_capture.clone();
    let (lease, permit) = tokio::task::spawn_blocking(move || provider.pin(&principal, id))
        .await
        .map_err(|_| ControlError::Repository("artifact pin worker failed".into()))??;
    // Bounded channel: no whole-clip allocation and no filesystem I/O on an async worker.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(2);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let _permit = permit; // held until the reader ends
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
    let stream = async_stream::stream! {
        let mut receiver = rx;
        while let Some(chunk) = receiver.recv().await { yield chunk; }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/quicktime")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| ControlError::Repository("artifact response failed".into()))
}
