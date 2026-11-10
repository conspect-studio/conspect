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
            (status = 422, description = "Invalid body, terms or missing key.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized.", body = crate::problem::Problem),
            (status = 404, description = "Unknown target player.", body = crate::problem::Problem),
            (status = 409, description = "Key reused with a different body.", body = crate::problem::Problem),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 503, description = "Capture unavailable or queue full.", body = crate::problem::Problem),
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
    let terms_version = body.terms.version.clone();
    let provider = state.web_capture.clone();
    let actor = principal.clone();
    let admission = tokio::task::spawn_blocking(move || provider.submit(&actor, &key, &body))
        .await
        .map_err(|_| ControlError::Repository("capture admission worker failed".into()))??;
    let id = admission.job_id;
    if admission.fresh {
        state.audit(
            &principal.key_id,
            crate::audit::AuditAction::Command,
            "capture-fill",
            &fill,
            Some(serde_json::json!({"job_id":id,"terms_version":terms_version,"accepted_or_coalesced":true})),
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
            (status = 200, description = "Job status.", body = crate::web_capture::JobView),
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
    let content_length = lease.artifact().bytes;
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let _permit = permit; // held until the reader ends
        let mut lease = lease;
        loop {
            if std::time::Instant::now() >= deadline {
                let _ = tx.try_send(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "artifact transfer deadline",
                )));
                break;
            }
            let mut chunk = vec![0; 64 * 1024];
            match lease.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    chunk.truncate(n);
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let wait = remaining.min(std::time::Duration::from_secs(5));
                    if !matches!(
                        runtime.block_on(send_chunk(&tx, Ok(chunk), wait)),
                        Ok(Ok(()))
                    ) {
                        let _ = tx.try_send(Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "artifact client idle timeout",
                        )));
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.try_send(Err(e));
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
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| ControlError::Repository("artifact response failed".into()))
}

async fn send_chunk(
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    chunk: Result<Vec<u8>, std::io::Error>,
    wait: std::time::Duration,
) -> Result<
    Result<(), tokio::sync::mpsc::error::SendError<Result<Vec<u8>, std::io::Error>>>,
    tokio::time::error::Elapsed,
> {
    tokio::time::timeout(wait, tx.send(chunk)).await
}
#[cfg(test)]
mod transfer_tests {
    #[tokio::test]
    async fn a_full_channel_expires_without_waiting_for_a_client() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(tx.send(Ok(vec![1])).await.is_ok());
        assert!(
            super::send_chunk(&tx, Ok(vec![2]), std::time::Duration::ZERO)
                .await
                .is_err()
        );
    }
}
