//! HTTP authorization must precede even unavailable transport admission.
#![allow(clippy::unwrap_used)]
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use support::{harness, send, OPERATOR_TOKEN, OUTPUT_SCOPED_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN};

#[tokio::test]
async fn http_scope_authorization_precedes_unavailable_provider() {
    let h = harness();
    for (token, path, status) in [
        (VIEWER_TOKEN, "program", StatusCode::FORBIDDEN),
        (SCOPED_TOKEN, "program", StatusCode::FORBIDDEN),
        (OUTPUT_SCOPED_TOKEN, "program", StatusCode::FORBIDDEN),
        (SCOPED_TOKEN, "inputs/other", StatusCode::FORBIDDEN),
        (OUTPUT_SCOPED_TOKEN, "outputs/other", StatusCode::FORBIDDEN),
        (
            SCOPED_TOKEN,
            "inputs/scoped-layout",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            OUTPUT_SCOPED_TOKEN,
            "outputs/wall-1",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (OPERATOR_TOKEN, "program", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let response = send(
            &h.router,
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/preview/{path}/moq"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), status, "{path}");
    }
}

#[tokio::test]
async fn http_revoke_requires_generation_and_scope() {
    let h = harness();
    for (token, path, status) in [
        (OPERATOR_TOKEN, "program/moq/id", StatusCode::BAD_REQUEST),
        (
            OPERATOR_TOKEN,
            "program/moq/id?generation=bad",
            StatusCode::BAD_REQUEST,
        ),
        (
            VIEWER_TOKEN,
            "program/moq/id?generation=1",
            StatusCode::FORBIDDEN,
        ),
        (
            SCOPED_TOKEN,
            "inputs/other/moq/id?generation=1",
            StatusCode::FORBIDDEN,
        ),
        (
            OUTPUT_SCOPED_TOKEN,
            "outputs/other/moq/id?generation=1",
            StatusCode::FORBIDDEN,
        ),
        (
            OPERATOR_TOKEN,
            "program/moq/id?generation=1",
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = send(
            &h.router,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/preview/{path}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), status, "{path}");
    }
}
