//! Auth tests: API-key HMAC verification, RBAC action gating, and per-object
//! (BOLA) authorization — at the unit level and through the router.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use multiview_control::{authorize_object, Action, ApiKeyStore, ControlError, Principal, Role};
use serde_json::json;
use support::{get, harness, post_json, send, ADMIN_TOKEN, SCOPED_TOKEN};

#[test]
fn api_key_verify_accepts_correct_secret_and_rejects_wrong() {
    let mut keys = ApiKeyStore::new(b"pepper".to_vec());
    keys.register(
        "k1",
        "right-secret",
        Principal {
            key_id: "k1".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: None,
        },
    );

    let ok = keys
        .verify("k1.right-secret")
        .expect("correct key verifies");
    assert_eq!(ok.role, Role::Operator);
    assert_eq!(ok.key_id, "k1");

    // Wrong secret, unknown key id, and malformed token all fail.
    assert!(matches!(
        keys.verify("k1.wrong-secret"),
        Err(ControlError::Unauthenticated)
    ));
    assert!(matches!(
        keys.verify("unknown.right-secret"),
        Err(ControlError::Unauthenticated)
    ));
    assert!(matches!(
        keys.verify("no-dot-token"),
        Err(ControlError::Unauthenticated)
    ));
}

#[test]
fn pepper_binds_digest_to_deployment() {
    let mut a = ApiKeyStore::new(b"pepper-a".to_vec());
    let mut b = ApiKeyStore::new(b"pepper-b".to_vec());
    let principal = Principal {
        key_id: "k".to_owned(),
        role: Role::Viewer,
        scoped_object_ids: None,
        scoped_output_ids: None,
    };
    a.register("k", "s", principal.clone());
    b.register("k", "s", principal);
    // The same key/secret under a different pepper must not cross-verify.
    assert!(a.verify("k.s").is_ok());
    assert!(b.verify("k.s").is_ok());
    assert_ne!(a.digest("s"), b.digest("s"));
}

#[test]
fn rbac_action_gating_matches_role_hierarchy() {
    assert!(Role::Viewer.can(Action::Read));
    assert!(!Role::Viewer.can(Action::Write));
    assert!(!Role::Viewer.can(Action::Administer));

    assert!(Role::Operator.can(Action::Read));
    assert!(Role::Operator.can(Action::Write));
    assert!(!Role::Operator.can(Action::Administer));

    assert!(Role::Admin.can(Action::Read));
    assert!(Role::Admin.can(Action::Write));
    assert!(Role::Admin.can(Action::Administer));
}

#[test]
fn per_object_authz_denies_objects_outside_scope() {
    let scoped = Principal {
        key_id: "scoped".to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(vec!["allowed-1".to_owned()]),
        scoped_output_ids: None,
    };
    assert!(authorize_object(&scoped, "allowed-1").is_ok());
    assert!(matches!(
        authorize_object(&scoped, "other-object"),
        Err(ControlError::Forbidden(_))
    ));

    // An unscoped principal is allowed any object (its role still gates actions).
    let unscoped = Principal {
        key_id: "admin".to_owned(),
        role: Role::Admin,
        scoped_object_ids: None,
        scoped_output_ids: None,
    };
    assert!(authorize_object(&unscoped, "anything").is_ok());
}

#[tokio::test]
async fn missing_or_bad_bearer_is_401() {
    let h = harness();

    // No Authorization header.
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/layouts")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Bad token.
    let resp = send(&h.router, get("/api/v1/layouts", "bogus.token")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn scoped_principal_denied_other_objects_through_router() {
    let h = harness();
    // Admin creates two layouts.
    send(
        &h.router,
        post_json(
            "/api/v1/layouts/scoped-layout",
            ADMIN_TOKEN,
            &json!({ "name": "Scoped", "body": {} }),
        ),
    )
    .await;
    send(
        &h.router,
        post_json(
            "/api/v1/layouts/other-layout",
            ADMIN_TOKEN,
            &json!({ "name": "Other", "body": {} }),
        ),
    )
    .await;

    // The scoped operator may read its own object...
    let resp = send(
        &h.router,
        get("/api/v1/layouts/scoped-layout", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // ...but is denied another object even though its role permits reads (BOLA).
    let resp = send(&h.router, get("/api/v1/layouts/other-layout", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = support::body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
}

#[tokio::test]
async fn problem_json_has_content_type_header() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/layouts")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
}
