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
    send, ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
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

/// BOLA enumeration (OWASP API1, conventions §H / ADR-W005 / ADR-W025): the
/// devices COLLECTION read must filter to the principal's object allowlist, by
/// parity with `GET /devices/{id}` returning `403` for an out-of-scope id. A
/// scoped operator that lists devices must see ONLY its allowlisted rows — never
/// enumerate (and read the address/config of) devices it cannot `GET`.
///
/// `SCOPED_TOKEN` is an operator scoped to `["scoped-layout"]`. Admin creates two
/// devices: `scoped-layout` (IN the allowlist) and `dev-other` (OUT). The scoped
/// list must contain exactly the in-scope row; admin still sees both.
#[tokio::test]
async fn list_filters_devices_to_the_scoped_allowlist() {
    let h = harness();
    for id in ["scoped-layout", "dev-other"] {
        let body = json!({
            "name": id,
            "body": { "id": id, "driver": "zowietek", "address": "http://[fd00:db8::1]" }
        });
        let resp = send(
            &h.router,
            post_json(&format!("/api/v1/devices/{id}"), ADMIN_TOKEN, &body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "device seed must land");
    }

    // The scoped operator lists: only its in-scope device is visible.
    let resp = send(&h.router, get("/api/v1/devices", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout"],
        "a scoped principal must see ONLY its allowlisted device, never enumerate others (BOLA)"
    );

    // An unscoped admin sees BOTH (the filter does not over-restrict).
    let resp = send(&h.router, get("/api/v1/devices", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    assert_eq!(
        listed.as_array().unwrap().len(),
        2,
        "an unscoped admin still sees every device"
    );
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

/// Seed a `zowietek` decoder that has an explicit spare decode index — the
/// grounded index-scoped convergence path (DEV-A4 / upstream #140).
async fn seed_indexed_device(h: &support::Harness, id: &str, spare_decode_index: u32) {
    let body = json!({
        "name": id,
        "body": {
            "id": id,
            "driver": "zowietek",
            "address": "http://[fd00:db8::42]",
            "spare_decode_index": spare_decode_index
        }
    });
    let resp = send(
        &h.router,
        post_json(&format!("/api/v1/devices/{id}"), OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "seed device {id}");
}

#[tokio::test]
async fn set_mode_declares_class2_index_scoped_disruption_before_apply() {
    let h = harness();
    seed_indexed_device(&h, "dev-foyer", 3).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer/set-mode",
            OPERATOR_TOKEN,
            &json!({ "mode": "decoder", "ack_impact": "c2" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert!(
        body["operation_id"].as_str().is_some(),
        "set-mode returns an operation id: {body}"
    );
    // An index-scoped switch activates the spare as the decoder's sole active
    // index and stops whatever was playing: Class-2, declared BEFORE apply.
    assert_eq!(
        body["impact"], "c2",
        "a spare-index decoder switch declares Class-2 disruption: {body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("index 3"),
        "the declared impact names the configured spare index: {body}"
    );
    assert!(
        detail.contains("single-active") || detail.contains("sole active"),
        "the declared impact states the single-active disruption: {body}"
    );
}

#[tokio::test]
async fn set_mode_without_a_spare_index_keeps_the_dev_impact() {
    let h = harness();
    seed_device(&h, "dev-foyer").await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer/set-mode",
            OPERATOR_TOKEN,
            &json!({ "mode": "decoder" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body["impact"], "dev",
        "no configured spare index ⇒ unchanged DEV-class declaration: {body}"
    );
}

// ---- Round 3 (#55): plan/take — Class-2 impact is surfaced BEFORE dispatch ---

mod plan_take {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use multiview_config::Device;
    use multiview_control::devices::zowietek::client::{ScriptedReply, ScriptedTransport};
    use multiview_control::devices::zowietek::poller::PollerConfig;
    use multiview_control::devices::zowietek::ZowietekDriver;
    use multiview_control::devices::{
        DevicePollerFactory, DevicePollerRegistry, PollerHandle, PollerWiring, ZowietekPoller,
    };

    fn ok(data: &serde_json::Value) -> ScriptedReply {
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000", "data": data }))
    }

    /// Spawns a real poller over one shared scripted transport whose poll
    /// cadence is slow (so only the dispatched convergence touches the wire).
    struct Factory(ScriptedTransport);

    impl DevicePollerFactory for Factory {
        fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle> {
            let driver = ZowietekDriver::new(
                &device.id,
                Arc::new(self.0.clone()),
                wiring.broadcaster.clone(),
                Arc::clone(&wiring.drivers),
                "admin",
                "admin",
            )
            .with_spare_decode_index(device.spare_decode_index);
            let config = PollerConfig {
                poll_period: Duration::from_secs(3600),
                ..PollerConfig::test_fast()
            };
            Some(
                ZowietekPoller::new(
                    &device.id,
                    driver,
                    Arc::clone(wiring.broadcaster.registry()),
                    "[fd00:db8::1]",
                    config,
                )
                .spawn(),
            )
        }
    }

    fn scripted() -> ScriptedTransport {
        let t = ScriptedTransport::new();
        t.push("system", ok(&json!({ "uuid": "u", "type": 0 })));
        t.push("venc", ok(&json!({ "workmode": "encoder" })));
        // Convergence: get_workmode_id (already decoder), table, switch ack.
        t.push_opt(
            "system",
            "get_workmode_id",
            ok(&json!({ "workmode_id": 1 })),
        );
        t.push(
            "streamplay",
            ok(&json!([
                {"index": 2, "name": "p", "switch": 1, "streamplay_status": "playing",
                 "url": "rtsp://x/a"},
                {"index": 3, "name": "s", "switch": 0, "streamplay_status": "idle",
                 "url": "rtsp://x/b"}
            ])),
        );
        t.push(
            "streamplay",
            ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
        );
        t
    }

    async fn running_harness(
        transport: &ScriptedTransport,
    ) -> (
        support::Harness,
        Arc<multiview_control::realtime::CorrRegistry>,
    ) {
        let registry = Arc::new(DevicePollerRegistry::with_factory(Arc::new(Factory(
            transport.clone(),
        ))));
        let slot = Arc::new(std::sync::Mutex::new(None));
        let slot2 = Arc::clone(&slot);
        let h = harness_with(move |state| {
            *slot2.lock().unwrap() = Some(Arc::clone(&state.corr));
            state.with_device_pollers(registry)
        });
        seed_indexed_device(&h, "dev-foyer", 3).await;
        // Wait for the adopt (login + probe) to land on the wire.
        for _ in 0..200 {
            if transport.request_count("venc") > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(transport.request_count("venc") > 0, "poller adopted");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let corr = slot.lock().unwrap().take().unwrap();
        (h, corr)
    }

    fn post(path: &str, body: &serde_json::Value) -> Request<Body> {
        post_json(path, OPERATOR_TOKEN, body)
    }

    #[tokio::test]
    async fn plan_reports_class2_with_zero_wire_calls() {
        let transport = scripted();
        let (h, _corr) = running_harness(&transport).await;
        let before = transport.requests().len();

        let resp = send(
            &h.router,
            post(
                "/api/v1/devices/dev-foyer/set-mode/plan",
                &json!({ "mode": "decoder" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["impact"], "c2", "{body}");
        assert_eq!(body["requires_ack"], true, "{body}");
        assert!(
            body["detail"].as_str().unwrap().contains("index 3"),
            "{body}"
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            transport.requests().len(),
            before,
            "plan makes no wire call"
        );
    }

    #[tokio::test]
    async fn plan_for_a_device_without_a_spare_index_is_dev_and_needs_no_ack() {
        let h = harness();
        seed_device(&h, "dev-foyer").await;
        let resp = send(
            &h.router,
            post(
                "/api/v1/devices/dev-foyer/set-mode/plan",
                &json!({ "mode": "decoder" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["impact"], "dev", "{body}");
        assert_eq!(body["requires_ack"], false, "{body}");
    }

    #[tokio::test]
    async fn plan_is_object_authorized_and_404s_unknown_devices() {
        let h = harness();
        let resp = send(
            &h.router,
            post(
                "/api/v1/devices/nope/set-mode/plan",
                &json!({ "mode": "decoder" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = send(
            &h.router,
            post_json(
                "/api/v1/devices/nope/set-mode/plan",
                VIEWER_TOKEN,
                &json!({ "mode": "decoder" }),
            ),
        )
        .await;
        assert!(
            resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND,
            "a viewer may plan (read-only)"
        );
    }

    #[tokio::test]
    async fn class2_apply_without_the_ack_is_refused_and_dispatches_nothing() {
        let transport = scripted();
        let (h, _corr) = running_harness(&transport).await;
        let before = transport.requests().len();

        for body in [
            json!({ "mode": "decoder" }),
            json!({ "mode": "decoder", "ack_impact": "dev" }),
        ] {
            let resp = send(&h.router, post("/api/v1/devices/dev-foyer/set-mode", &body)).await;
            assert_eq!(resp.status(), StatusCode::CONFLICT, "{body}");
            let problem = body_json(resp).await;
            let detail = problem["detail"].as_str().unwrap();
            assert!(
                detail.contains("c2") && detail.contains("index 3"),
                "the refusal carries the impact: {problem}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(transport.requests().len(), before, "nothing was dispatched");
    }

    #[tokio::test]
    async fn a_rejected_dispatch_leaves_no_correlation_behind() {
        // A poller is "running" but its control channel is full, so dispatch is
        // rejected: the 202 stands, yet the op must not stay correlated.
        struct Wedged;
        impl DevicePollerFactory for Wedged {
            fn spawn(&self, _d: &Device, _w: &PollerWiring) -> Option<PollerHandle> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                tx.try_send(multiview_control::devices::PollerControl::SecretUpdated)
                    .unwrap();
                // Keep the receiver alive inside the task so the channel stays
                // open-but-full.
                let task = tokio::spawn(async move {
                    let _keep = rx;
                    std::future::pending::<()>().await;
                });
                Some(PollerHandle::new(tx, task))
            }
        }
        let registry = Arc::new(DevicePollerRegistry::with_factory(Arc::new(Wedged)));
        let slot = Arc::new(std::sync::Mutex::new(None));
        let slot2 = Arc::clone(&slot);
        let h = harness_with(move |state| {
            *slot2.lock().unwrap() = Some(Arc::clone(&state.corr));
            state.with_device_pollers(registry)
        });
        seed_indexed_device(&h, "dev-foyer", 3).await;
        let corr = slot.lock().unwrap().take().unwrap();

        let resp = send(
            &h.router,
            post(
                "/api/v1/devices/dev-foyer/set-mode",
                &json!({ "mode": "decoder", "ack_impact": "c2" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // A later, unrelated terminal device.mode event (no operation id: e.g.
        // an adopt/reconnect convergence) must not be attributed to the
        // rejected operation — correlation is by the id the event carries.
        let mut session = multiview_control::SessionStream::new(h.engine.subscribe(), "s", None)
            .with_corr_registry(corr);
        h.engine.publish_event(multiview_events::Event::DeviceMode(
            multiview_events::DeviceMode {
                device_id: "dev-foyer".to_owned(),
                mode: "decoder".to_owned(),
                phase: multiview_events::ModePhase::Finished,
                impact: multiview_events::ImpactClass::Class2,
                detail: None,
                operation_id: None,
                failure_stage: None,
            },
        ));
        let delta = session.next_delta().await.unwrap().expect("delivered");
        assert_eq!(delta.envelope.corr, None, "not attributed to a rejected op");
    }

    #[tokio::test]
    async fn acked_apply_dispatches_and_correlates_the_outcome() {
        let transport = scripted();
        let (h, corr) = running_harness(&transport).await;
        let mut session = multiview_control::SessionStream::new(h.engine.subscribe(), "s", None)
            .with_corr_registry(corr);

        let resp = send(
            &h.router,
            post(
                "/api/v1/devices/dev-foyer/set-mode",
                &json!({ "mode": "decoder", "ack_impact": "c2" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp).await;
        assert_eq!(body["impact"], "c2");
        for _ in 0..200 {
            if transport
                .requests()
                .iter()
                .any(|r| r.body.get("opt") == Some(&json!("streamplay_switch")))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            transport
                .requests()
                .iter()
                .any(|r| r.body.get("opt") == Some(&json!("streamplay_switch"))),
            "the acked apply dispatched the index-scoped switch"
        );
        // The terminal outcome carries this operation's id end-to-end.
        let want = body["operation_id"].as_str().unwrap().to_owned();
        let mut corrs = Vec::new();
        for _ in 0..10 {
            let Ok(Ok(Some(delta))) =
                tokio::time::timeout(Duration::from_millis(500), session.next_delta()).await
            else {
                break;
            };
            if let multiview_events::Event::DeviceMode(m) = &delta.envelope.payload {
                if m.phase != multiview_events::ModePhase::Started {
                    corrs.push(delta.envelope.corr.clone());
                }
            }
            if !corrs.is_empty() {
                break;
            }
        }
        assert_eq!(corrs, vec![Some(want)], "terminal outcome echoes the op id");
    }
}
