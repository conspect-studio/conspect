//! M6 router integration: config-version rollback through the API restores the
//! prior state, and OAuth 2.0 / JWT auth works as an alternative to API keys.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use multiview_control::{
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository, JwtValidator, Role,
};
use multiview_core::time::MediaTime;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::json;
use sha2::Sha256;
use support::{body_json, get, post_json, put_json, seeded_keys, send, ADMIN_TOKEN};

type HmacSha256 = Hmac<Sha256>;

const JWT_SECRET: &[u8] = b"router-jwt-secret-shared-with-as";
const JWT_ISS: &str = "https://auth.facility.example";
const JWT_AUD: &str = "multiview";
// A fixed validation "now": the harness ack clock is in nanoseconds; the JWT
// validator reads seconds, so this must be consistent with ACK_NANOS below.
const ACK_NANOS: i64 = 1_800_000_000_000_000_000; // 1_800_000_000 s

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn sign(header: &serde_json::Value, payload: &serde_json::Value, secret: &[u8]) -> String {
    let input = format!(
        "{}.{}",
        b64(&serde_json::to_vec(header).unwrap()),
        b64(&serde_json::to_vec(payload).unwrap())
    );
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).unwrap();
    mac.update(input.as_bytes());
    format!("{input}.{}", b64(&mac.finalize().into_bytes()))
}

/// Build a router whose `AppState` has JWT auth enabled and a deterministic
/// clock fixed at `ACK_NANOS`.
fn jwt_router() -> axum::Router {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (tx, _rx) = command_bus(8);
    let validator = Arc::new(JwtValidator::new_hs256(
        JWT_SECRET.to_vec(),
        JWT_ISS,
        JWT_AUD,
    ));
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(seeded_keys()),
    )
    .with_jwt(validator, JWT_AUD)
    .with_ack_clock(Arc::new(|| MediaTime::from_nanos(ACK_NANOS)));
    multiview_control::router(state)
}

#[tokio::test]
async fn config_version_rollback_through_the_router_restores_prior_state() {
    let h = support::harness();

    // Commit revision 1, then revision 2.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/layout:wall",
            ADMIN_TOKEN,
            None,
            &json!({ "document": { "cells": 4 }, "message": "v1" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["revision"], 1);

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/layout:wall",
            ADMIN_TOKEN,
            None,
            &json!({ "document": { "cells": 9 }, "message": "v2" }),
        ),
    )
    .await;
    assert_eq!(body_json(resp).await["revision"], 2);

    // Diff revision 1 -> 2 reports the changed key.
    let resp = send(
        &h.router,
        get("/api/v1/config/layout:wall/diff?from=1&to=2", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let diff = body_json(resp).await;
    assert_eq!(diff["changed"], json!(["cells"]));

    // Roll back to revision 1: appends revision 3 with r1's document.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/config/layout:wall/rollback",
            ADMIN_TOKEN,
            &json!({ "to": 1 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let rolled = body_json(resp).await;
    assert_eq!(rolled["revision"], 3);
    assert_eq!(rolled["document"], json!({ "cells": 4 }));

    // The head (history newest-first) now reflects the rolled-back document.
    let resp = send(&h.router, get("/api/v1/config/layout:wall", ADMIN_TOKEN)).await;
    let history = body_json(resp).await;
    let arr = history.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["revision"], 3);
    assert_eq!(arr[0]["document"], json!({ "cells": 4 }));

    // The rollback was audited.
    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    let entries = body_json(resp).await;
    assert_eq!(entries.as_array().unwrap()[0]["action"], "rollback");
}

#[tokio::test]
async fn jwt_bearer_is_accepted_as_an_alternative_to_api_keys() {
    let router = jwt_router();
    // A valid HS256 token with a write grant -> Operator, can list layouts (read)
    // and create one (write).
    let token = sign(
        &json!({ "alg": "HS256", "typ": "JWT" }),
        &json!({
            "iss": JWT_ISS, "sub": "jwt-operator", "aud": [JWT_AUD],
            "exp": 1_900_000_000i64, "iat": 1_700_000_000i64,
            "x-nmos-api": { "version": "1.0", "access": { "multiview": "write" } }
        }),
        JWT_SECRET,
    );

    // Read works.
    let resp = send(&router, get("/api/v1/layouts", &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Write works (Operator).
    let resp = send(
        &router,
        post_json(
            "/api/v1/layouts/jwt-made",
            &token,
            &json!({ "name": "JWT", "body": {} }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The audit attributes the mutation to the JWT subject.
    let resp = send(&router, get("/api/v1/audit", &token)).await;
    let entries = body_json(resp).await;
    assert_eq!(entries.as_array().unwrap()[0]["actor"], "jwt-operator");
}

#[tokio::test]
async fn jwt_with_alg_none_is_rejected() {
    let router = jwt_router();
    // alg=none downgrade with empty signature.
    let input = format!(
        "{}.{}",
        b64(&serde_json::to_vec(&json!({ "alg": "none", "typ": "JWT" })).unwrap()),
        b64(&serde_json::to_vec(&json!({
            "iss": JWT_ISS, "sub": "attacker", "aud": [JWT_AUD],
            "exp": 1_900_000_000i64, "iat": 1_700_000_000i64,
            "x-nmos-api": { "version": "1.0", "access": { "multiview": "write" } }
        }))
        .unwrap())
    );
    let token = format!("{input}.");
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/layouts")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn jwt_with_bad_signature_is_rejected() {
    let router = jwt_router();
    let token = sign(
        &json!({ "alg": "HS256", "typ": "JWT" }),
        &json!({
            "iss": JWT_ISS, "sub": "x", "aud": [JWT_AUD],
            "exp": 1_900_000_000i64, "iat": 1_700_000_000i64,
            "x-nmos-api": { "version": "1.0", "access": { "multiview": "write" } }
        }),
        b"the-WRONG-secret",
    );
    let resp = send(&router, get("/api/v1/layouts", &token)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn jwt_expired_and_wrong_audience_are_rejected() {
    let router = jwt_router();
    // Expired (exp < now=1_800_000_000).
    let expired = sign(
        &json!({ "alg": "HS256" }),
        &json!({
            "iss": JWT_ISS, "sub": "x", "aud": [JWT_AUD],
            "exp": 1_000i64, "iat": 0i64,
            "x-nmos-api": { "version": "1.0", "access": { "multiview": "read" } }
        }),
        JWT_SECRET,
    );
    assert_eq!(
        send(&router, get("/api/v1/layouts", &expired))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Wrong audience.
    let wrong_aud = sign(
        &json!({ "alg": "HS256" }),
        &json!({
            "iss": JWT_ISS, "sub": "x", "aud": ["some-other-service"],
            "exp": 1_900_000_000i64, "iat": 0i64,
            "x-nmos-api": { "version": "1.0", "access": { "multiview": "read" } }
        }),
        JWT_SECRET,
    );
    assert_eq!(
        send(&router, get("/api/v1/layouts", &wrong_aud))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn api_keys_still_work_when_jwt_is_enabled() {
    // Enabling JWT must not break the native API-key path (alternative, not
    // replacement).
    let router = jwt_router();
    let resp = send(&router, get("/api/v1/layouts", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // An admin JWT does not exist (claims map only to viewer/operator); the
    // native admin key remains the admin path. Confirm a read-grant JWT maps to
    // a viewer (read ok, write forbidden).
    let viewer_jwt = sign(
        &json!({ "alg": "HS256" }),
        &json!({
            "iss": JWT_ISS, "sub": "jwt-viewer", "aud": [JWT_AUD],
            "exp": 1_900_000_000i64, "iat": 0i64,
            "x-nmos-api": { "version": "1.0", "access": { "multiview": "read" } }
        }),
        JWT_SECRET,
    );
    let resp = send(&router, get("/api/v1/layouts", &viewer_jwt)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        &router,
        post_json(
            "/api/v1/layouts/forbidden",
            &viewer_jwt,
            &json!({ "name": "x", "body": {} }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let _ = Role::Viewer;
}
