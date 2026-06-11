//! End-to-end tests for the Devices domain REST surface (ADR-M008/M009/W017):
//! `/devices` CRUD with `ETag`/`If-Match` (`412`), `Idempotency-Key` replay,
//! `404` problem documents, `DELETE` `409` while a source/output is bound via
//! `device_ref`, the read-only status snapshot, the bare-verb actions
//! (`probe`/`set-mode`/`reboot`/`identify`/`test-pattern`), and the two
//! projection endpoints (`source-candidates`/`output-targets`) — all driven
//! through the real router via `tower::oneshot`. Mirrors `tests/probes.rs` and
//! `tests/salvos.rs`.
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
use multiview_engine::EnginePublisher;
use serde_json::json;
use support::{
    body_json, delete_if_match, get, harness, harness_with, post_if_match, post_json, put_json,
    send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN,
};

/// A valid `zowietek` device body (the canonical `multiview_config::Device`
/// wire shape), IPv6-first per ADR-0042.
fn device_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": {
            "id": "dev-foyer",
            "driver": "zowietek",
            "address": "http://[fd00:db8::42]",
            "desired_mode": "decoder"
        }
    })
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            &device_body("Foyer decoder"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .expect("create must return an ETag")
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(etag, "W/\"1\"", "a fresh device is version 1");
    let created = body_json(resp).await;
    assert_eq!(created["id"], "dev-foyer");
    assert_eq!(created["name"], "Foyer decoder");
    assert_eq!(created["body"]["driver"], "zowietek");
    assert_eq!(created["body"]["address"], "http://[fd00:db8::42]");

    let resp = send(&h.router, get("/api/v1/devices/dev-foyer", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
}

#[tokio::test]
async fn list_is_id_sorted() {
    let h = harness();
    for id in ["dev-zeta", "dev-alpha"] {
        let body = json!({
            "name": id,
            "body": { "id": id, "driver": "zowietek", "address": "http://[fd00:db8::1]" }
        });
        send(
            &h.router,
            post_json(&format!("/api/v1/devices/{id}"), OPERATOR_TOKEN, &body),
        )
        .await;
    }
    let resp = send(&h.router, get("/api/v1/devices", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["dev-alpha", "dev-zeta"]);
}

#[tokio::test]
async fn get_unknown_device_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/devices/missing", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["type"], "/problems/not-found");
}

#[tokio::test]
async fn update_with_stale_if_match_is_412() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            &device_body("Foyer decoder"),
        ),
    )
    .await;

    // First update at version 1 succeeds and bumps to 2.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &device_body("Renamed"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"2\""
    );

    // A second update presenting the now-stale version 1 is rejected 412.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &device_body("Conflict"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");
}

#[tokio::test]
async fn invalid_device_body_is_422_problem_json() {
    let h = harness();
    // `cast` requires an address; omitting it must fail typed validation.
    let body = json!({
        "name": "Bad",
        "body": { "id": "dev-bad", "driver": "cast" }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/devices/dev-bad", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 422);
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn delete_is_409_while_a_source_is_bound_via_device_ref() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            &device_body("Foyer decoder"),
        ),
    )
    .await;
    // A source carrying this device's `device_ref` (the ADR-M009 projection
    // binding) must block deletion until it is unbound.
    let source = json!({
        "name": "Foyer RTSP",
        "body": {
            "id": "src-foyer",
            "kind": "rtsp",
            "url": "rtsp://[fd00:db8::42]:554/main",
            "device_ref": "dev-foyer"
        }
    });
    send(
        &h.router,
        post_json("/api/v1/sources/src-foyer", OPERATOR_TOKEN, &source),
    )
    .await;

    let resp = send(
        &h.router,
        delete_if_match("/api/v1/devices/dev-foyer", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 409);
    // The problem detail must name the bound source so the operator knows what
    // to unbind (ADR-M009 consequence).
    assert!(
        problem["detail"].as_str().unwrap().contains("src-foyer"),
        "problem detail names the bound source: {problem}"
    );

    // The device still exists (the delete was refused, not partially applied).
    let resp = send(&h.router, get("/api/v1/devices/dev-foyer", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn delete_succeeds_when_no_binding_exists() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            &device_body("Foyer decoder"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/devices/dev-foyer", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(&h.router, get("/api/v1/devices/dev-foyer", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn status_snapshot_is_adopting_after_create() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            &device_body("Foyer decoder"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        get("/api/v1/devices/dev-foyer/status", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status = body_json(resp).await;
    // A freshly-adopted device with no driver yet sits in ADOPTING (no live I/O
    // in this slice — DEV-A4/A5 own the driver actors).
    assert_eq!(status["state"], "ADOPTING");
    assert_eq!(status["device_id"], "dev-foyer");
}

#[tokio::test]
async fn status_of_unknown_device_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/devices/missing/status", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn probe_returns_200() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        post_if_match("/api/v1/devices/dev-foyer/probe", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn set_mode_returns_202_with_operation_id_and_declared_impact() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer/set-mode",
            OPERATOR_TOKEN,
            &json!({ "mode": "encoder" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert!(
        body["operation_id"].as_str().is_some(),
        "set-mode returns an operation id: {body}"
    );
    // The DEV-class impact is declared in the body BEFORE apply (ADR-M009).
    assert_eq!(body["impact"], "dev");
    assert!(
        body["detail"].as_str().is_some(),
        "set-mode declares its impact statement: {body}"
    );
}

#[tokio::test]
async fn set_mode_replay_with_idempotency_key_returns_the_same_op() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let key = "set-mode-key";
    let req = || -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/devices/dev-foyer/set-mode")
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from(
                serde_json::to_vec(&json!({ "mode": "encoder" })).unwrap(),
            ))
            .unwrap()
    };
    let resp1 = send(&h.router, req()).await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);
    let op1 = body_json(resp1).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resp2 = send(&h.router, req()).await;
    assert_eq!(resp2.status(), StatusCode::ACCEPTED);
    let op2 = body_json(resp2).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(op1, op2, "a retried key returns the original operation id");
}

#[tokio::test]
async fn reboot_returns_202() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        post_if_match("/api/v1/devices/dev-foyer/reboot", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn identify_returns_204() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        post_if_match("/api/v1/devices/dev-foyer/identify", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_pattern_returns_204() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        post_if_match(
            "/api/v1/devices/dev-foyer/test-pattern",
            OPERATOR_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn action_on_unknown_device_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        post_if_match("/api/v1/devices/missing/probe", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn source_candidates_returns_declared_projection_for_known_device() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        get(
            "/api/v1/devices/dev-foyer/source-candidates",
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // No live driver in this slice: the projection is honestly empty/declared,
    // never fabricated live telemetry (ADR-M009).
    assert!(body.is_array(), "source-candidates is an array: {body}");
}

#[tokio::test]
async fn output_targets_returns_declared_projection_for_known_device() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        get("/api/v1/devices/dev-foyer/output-targets", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.is_array(), "output-targets is an array: {body}");
}

#[tokio::test]
async fn projection_of_unknown_device_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/devices/missing/source-candidates", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn viewer_cannot_create_a_device() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            VIEWER_TOKEN,
            &device_body("Foyer decoder"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// After a live `zowietek` driver enumerates a device's facets into the shared
/// driver registry (what the spawned poller does on adopt), the projection
/// routes return the REAL candidates/targets — not the honest-empty placeholder.
#[tokio::test]
async fn projection_routes_serve_real_facets_a_live_driver_enumerated() {
    use multiview_control::devices::zowietek::client::{ScriptedReply, ScriptedTransport};
    use multiview_control::devices::zowietek::ZowietekDriver;
    use multiview_control::devices::{
        DeviceBroadcaster, DeviceDriverRegistry, DeviceStatusRegistry,
    };

    // Share ONE driver registry between the router (via AppState) and a live
    // driver, exactly as the spawned poller shares the AppState registry.
    let drivers = Arc::new(DeviceDriverRegistry::new());
    let drivers_for_state = Arc::clone(&drivers);
    let h = harness_with(move |state| state.with_device_drivers(drivers_for_state));
    seed_device(&h, "dev-foyer").await;

    // The projection routes are honestly empty BEFORE the driver enumerates.
    let resp = send(
        &h.router,
        get(
            "/api/v1/devices/dev-foyer/source-candidates",
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(0),
        "no candidates before a driver enumerates: {body}"
    );

    // A live driver (the poller's adopt path) probes the device and enumerates
    // the three facets into the shared registry — encoder serves source mounts;
    // a decode table serves output targets.
    let transport = ScriptedTransport::new();
    transport.push(
        "system",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "uuid": "u" } }),
        ),
    );
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "decoder" } }),
        ),
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "entries": [ { "index": 0, "proto": "rtsp" } ] }
        })),
    );
    let engine = Arc::new(EnginePublisher::new(64));
    let status = Arc::new(DeviceStatusRegistry::new());
    let broadcaster = DeviceBroadcaster::new(engine, status);
    let driver = ZowietekDriver::new(
        "dev-foyer",
        Arc::new(transport),
        broadcaster,
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver
        .probe_and_adopt()
        .await
        .expect("driver adopts device");
    driver
        .enumerate_source_candidates("[fd00:db8::42]")
        .expect("source facet enumerates");
    driver
        .enumerate_output_targets()
        .await
        .expect("output facet enumerates");

    // The source-candidates route now returns the REAL enumerated candidates.
    let resp = send(
        &h.router,
        get(
            "/api/v1/devices/dev-foyer/source-candidates",
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let body = body_json(resp).await;
    let urls: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["url"].as_str())
        .collect();
    assert!(
        urls.iter().any(|u| u.contains("/main/av")),
        "the route serves the driver-enumerated source candidates: {body}"
    );

    // The output-targets route now returns the REAL decode-slot targets.
    let resp = send(
        &h.router,
        get("/api/v1/devices/dev-foyer/output-targets", OPERATOR_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert!(
        !body.as_array().unwrap().is_empty(),
        "the route serves the driver-enumerated output targets: {body}"
    );
}

/// Seed a `zowietek` device over HTTP so an action/projection test has a
/// target (the device store is not exposed on the harness, so we adopt it
/// through the real create route).
async fn seed_device(h: &support::Harness, id: &str) {
    let body = json!({
        "name": id,
        "body": { "id": id, "driver": "zowietek", "address": "http://[fd00:db8::42]" }
    });
    let resp = send(
        &h.router,
        post_json(&format!("/api/v1/devices/{id}"), OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "seed device {id}");
}
