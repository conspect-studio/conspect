#![cfg(unix)]
//! Capture route authorization and honest unavailable behavior.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use axum::http::StatusCode;
use serde_json::json;
use support::{
    body_json, get, harness_with, post_json as plain_post_json, send, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};
fn post_json(
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> axum::http::Request<axum::body::Body> {
    let mut request = plain_post_json(path, token, body);
    request
        .headers_mut()
        .insert("idempotency-key", "capture-test".parse().unwrap());
    request
}
fn body() -> serde_json::Value {
    json!({"fill_id":"radar","target_player":null,"view":{"lat":-33.946,"lon":150.37,"zoom":10,"overlay":"radar","width":1280,"height":720,"fps":2,"frames":12,"public":true},"terms":{"accepted":true,"version":"windy-2023-09-01"}})
}
#[tokio::test]
async fn capture_routes_report_unavailable_without_creating_fake_jobs() {
    let h = harness_with(|s| s);
    let response = send(
        &h.router,
        get("/api/v1/media/captures/capabilities", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["capture_available"], false);
    let response = send(
        &h.router,
        post_json("/api/v1/media/captures", OPERATOR_TOKEN, &body()),
    )
    .await;
    let status = response.status();
    let details = body_json(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{details}");
}
#[tokio::test]
async fn viewer_and_other_object_are_denied_before_provider_admission() {
    let h = harness_with(|s| s);
    for token in [VIEWER_TOKEN, SCOPED_TOKEN] {
        let r = send(
            &h.router,
            post_json("/api/v1/media/captures", token, &body()),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
#[test]
fn symlink_root_is_rejected_at_startup() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    let real = root.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(multiview_control::web_capture_startup::reject_symlink(&link).is_err());
    assert!(multiview_control::web_capture_startup::reject_symlink(&real.join("new")).is_ok());
}
#[test]
fn ledger_replay_divergence_and_bounds() {
    use multiview_control::web_capture_store::{Ledger, Reserve};
    let l = Ledger::memory();
    assert_eq!(l.reserve("a", "submit", "k", "d1"), Ok(Reserve::Fresh));
    l.complete("a", "submit", "k", 7).unwrap();
    assert_eq!(l.reserve("a", "submit", "k", "d1"), Ok(Reserve::Replay(7)));
    assert_eq!(l.reserve("a", "submit", "k", "d2"), Err(true));
    assert_eq!(l.reserve("b", "submit", "k", "d2"), Ok(Reserve::Fresh));
}
#[tokio::test]
async fn terms_and_untrusted_fields_are_rejected() {
    let h = harness_with(|s| s);
    let mut input = body();
    input["terms"]["accepted"] = json!(false);
    let r = send(
        &h.router,
        post_json("/api/v1/media/captures", OPERATOR_TOKEN, &input),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let mut input = body();
    input["html"] = json!("<script>bad()</script>");
    let r = send(
        &h.router,
        post_json("/api/v1/media/captures", OPERATOR_TOKEN, &input),
    )
    .await;
    assert!(r.status().is_client_error());
}
