//! End-to-end tests for the probes resource (per-cell fail-state detection:
//! black / freeze / silence / loudness): CRUD, `ETag` round-trip, `If-Match`
//! optimistic concurrency (`412`), RBAC, typed-body validation (ADR-W015 `422`
//! with the offending field path), and the `X-Multiview-Apply` semantics —
//! driven through the real router. Mirrors `tests/sources.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{
    body_json, get, harness, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

const APPLY_HEADER: &str = "x-multiview-apply";

/// BOLA ROW enumeration (OWASP API1, ADR-W005/ADR-W025): a probe is object-scoped
/// by its OWN id (`get_probe` 403s an out-of-scope id), so `list_probes` MUST
/// filter ROWS to the principal's allowlist — by the same parity as
/// `list_devices`. `SCOPED_TOKEN` is scoped to `["scoped-layout"]`.
#[tokio::test]
async fn list_filters_probe_rows_to_the_scoped_allowlist() {
    let h = harness();
    for id in ["scoped-layout", "other-probe"] {
        let resp = send(
            &h.router,
            post_json(
                &format!("/api/v1/probes/{id}"),
                ADMIN_TOKEN,
                &json!({
                    "name": id,
                    "body": {
                        "id": id,
                        "cell": "cell-a",
                        "kind": "black",
                        "luma_threshold": 16,
                        "dwell": { "up_ms": 2000, "down_ms": 1000 },
                        "severity": "Major",
                        "latched": false
                    }
                }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let resp = send(&h.router, get("/api/v1/probes", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ids: Vec<String> = body_json(resp)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout".to_owned()],
        "a scoped principal must see ONLY its allowlisted probe rows (BOLA)"
    );

    let resp = send(&h.router, get("/api/v1/probes", ADMIN_TOKEN)).await;
    assert_eq!(body_json(resp).await.as_array().unwrap().len(), 2);
}

/// A valid black-detection probe body (the canonical `multiview_config::Probe`
/// wire shape: `kind` flattened to top level).
fn probe_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": {
            "id": "probe-1",
            "cell": "cell-a",
            "kind": "black",
            "luma_threshold": 16,
            "dwell": { "up_ms": 2000, "down_ms": 1000 },
            "severity": "Major",
            "latched": false
        }
    })
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            &probe_body("Black on cam 1"),
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
    assert_eq!(etag, "W/\"1\"", "a fresh resource is version 1");
    let created = body_json(resp).await;
    assert_eq!(created["id"], "probe-1");
    assert_eq!(created["name"], "Black on cam 1");
    assert_eq!(created["body"]["kind"], "black");
    assert_eq!(created["body"]["luma_threshold"], 16);
    assert_eq!(created["body"]["cell"], "cell-a");

    let resp = send(&h.router, get("/api/v1/probes/probe-1", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let fetched = body_json(resp).await;
    assert_eq!(fetched["name"], "Black on cam 1");
}

#[tokio::test]
async fn get_unknown_probe_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/probes/missing", OPERATOR_TOKEN)).await;
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
async fn update_with_matching_if_match_succeeds_and_bumps_version() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            &probe_body("Black on cam 1"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &probe_body("Renamed"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"2\"",
        "a successful update bumps the version"
    );
    let updated = body_json(resp).await;
    assert_eq!(updated["name"], "Renamed");
}

#[tokio::test]
async fn update_with_stale_if_match_is_412() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/probes/probe-1", OPERATOR_TOKEN, &probe_body("V1")),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &probe_body("V2"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &probe_body("Clobber"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");

    let resp = send(&h.router, get("/api/v1/probes/probe-1", OPERATOR_TOKEN)).await;
    let current = body_json(resp).await;
    assert_eq!(current["name"], "V2", "the clobbering write was rejected");
}

#[tokio::test]
async fn update_without_if_match_is_precondition_required() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/probes/probe-1", OPERATOR_TOKEN, &probe_body("V1")),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            None,
            &probe_body("X"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn list_returns_created_probes_sorted() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/probes/bbb",
            OPERATOR_TOKEN,
            &json!({
                "name": "B",
                "body": { "cell": "c1", "kind": "silence", "level_dbfs": -60.0 }
            }),
        ),
    )
    .await;
    send(
        &h.router,
        post_json(
            "/api/v1/probes/aaa",
            OPERATOR_TOKEN,
            &json!({
                "name": "A",
                "body": { "cell": "c1", "kind": "freeze", "difference_threshold": 5 }
            }),
        ),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/probes", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let arr = list.as_array().expect("list is an array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "aaa", "id-sorted order");
    assert_eq!(arr[1]["id"], "bbb");
}

#[tokio::test]
async fn list_requires_authentication() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/probes")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 401);
    assert_eq!(problem["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn viewer_may_not_create() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/probes/probe-1", VIEWER_TOKEN, &probe_body("P")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_requires_admin_role() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/probes/probe-1", ADMIN_TOKEN, &probe_body("P")),
    )
    .await;

    // Operator may not delete (Administer action).
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/probes/probe-1")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Admin may delete.
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/probes/probe-1")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(&h.router, get("/api/v1/probes/probe-1", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- Typed-body validation (ADR-W015) ---------------------------------------

#[tokio::test]
async fn create_probe_with_unknown_kind_is_422_with_field_detail() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/p1",
            OPERATOR_TOKEN,
            &json!({
                "name": "P",
                "body": { "id": "p1", "cell": "c1", "kind": "flux-capacitor" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    let detail = problem["detail"].as_str().expect("detail is present");
    assert!(
        detail.contains("flux-capacitor") || detail.contains("kind"),
        "detail names the offending field/variant, got: {detail}"
    );
}

#[tokio::test]
async fn create_probe_missing_required_field_is_422() {
    let h = harness();
    // A black probe without its `luma_threshold` must be rejected.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/p1",
            OPERATOR_TOKEN,
            &json!({
                "name": "P",
                "body": { "id": "p1", "cell": "c1", "kind": "black" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or("")
            .contains("luma_threshold"),
        "detail names the missing field, got: {}",
        problem["detail"]
    );
}

#[tokio::test]
async fn create_probe_with_mismatched_body_id_is_422() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/p1",
            OPERATOR_TOKEN,
            &json!({
                "name": "P",
                "body": { "id": "other", "cell": "c1", "kind": "silence", "level_dbfs": -60.0 }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn create_probe_without_body_id_inherits_the_path_id() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/p1",
            OPERATOR_TOKEN,
            &json!({
                "name": "P",
                "body": { "cell": "c1", "kind": "silence", "level_dbfs": -60.0 }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(created["body"]["id"], "p1", "the path id is injected");
}

#[tokio::test]
async fn semantically_invalid_probes_are_422_even_when_well_typed() {
    let h = harness();
    // Well-typed but semantically wrong documents must be rejected at the API
    // boundary — they would otherwise poison /config/export.
    let cases = [
        // Freeze threshold above the per-mille scale (`0..=1000`).
        (
            "/api/v1/probes/p1",
            json!({
                "name": "P",
                "body": { "cell": "c1", "kind": "freeze", "difference_threshold": 1001 }
            }),
        ),
        // Detection zone extending beyond the unit square.
        (
            "/api/v1/probes/p2",
            json!({
                "name": "P",
                "body": {
                    "cell": "c1",
                    "kind": "black",
                    "luma_threshold": 16,
                    "zone": { "x": 0.6, "y": 0.0, "w": 0.5, "h": 1.0 }
                }
            }),
        ),
        // An empty cell reference.
        (
            "/api/v1/probes/p3",
            json!({
                "name": "P",
                "body": { "cell": "", "kind": "silence", "level_dbfs": -60.0 }
            }),
        ),
    ];
    for (path, body) in &cases {
        let resp = send(&h.router, post_json(path, OPERATOR_TOKEN, body)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path} must reject a semantically invalid body"
        );
    }
}

#[tokio::test]
async fn valid_probe_mutations_declare_restart_apply_semantics() {
    let h = harness();
    // One representative body per kind: each must be accepted and declare the
    // honest apply semantics (stored now, live after export + restart).
    let bodies = [
        json!({ "cell": "c1", "kind": "black", "luma_threshold": 16 }),
        json!({ "cell": "c1", "kind": "freeze", "difference_threshold": 5,
                "zone": { "x": 0.25, "y": 0.25, "w": 0.5, "h": 0.5 } }),
        json!({ "cell": "c1", "kind": "silence", "level_dbfs": -60.0,
                "dwell": { "up_ms": 5000, "down_ms": 1000 }, "severity": "Warning" }),
        json!({ "cell": "c1", "kind": "loudness",
                "target": { "kind": "r128", "target_lufs": -23.0, "max_true_peak_dbtp": -1.0 },
                "latched": true }),
    ];
    for (index, body) in bodies.iter().enumerate() {
        let path = format!("/api/v1/probes/probe-{index}");
        let resp = send(
            &h.router,
            post_json(&path, OPERATOR_TOKEN, &json!({ "name": "P", "body": body })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "{path} must accept");
        assert_eq!(
            resp.headers()
                .get(APPLY_HEADER)
                .expect("create declares apply semantics")
                .to_str()
                .unwrap(),
            "restart"
        );
    }
}

#[tokio::test]
async fn stale_if_match_wins_over_an_invalid_body_on_update() {
    let h = harness();
    // RFC 9110 §13.2.2: preconditions are evaluated before request content.
    send(
        &h.router,
        post_json("/api/v1/probes/probe-1", OPERATOR_TOKEN, &probe_body("V1")),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &probe_body("V2"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/probes/probe-1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "X", "body": { "cell": "c1", "kind": "flux" } }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::PRECONDITION_FAILED,
        "stale If-Match is reported before body validation"
    );
}

// ---- OpenAPI mirror drift pins (ADR-W015) ------------------------------------

/// Pin the `OpenAPI` mirror schemas (`openapi_schemas`) to the real
/// `multiview_config::Probe`: every representative document must be accepted by
/// BOTH (a mirror that drifts fails here).
#[test]
fn openapi_probe_mirror_accepts_what_the_config_type_accepts() {
    let probes = [
        json!({ "id": "p", "cell": "c", "kind": "black", "luma_threshold": 16 }),
        json!({ "id": "p", "cell": "c", "kind": "black", "luma_threshold": 0,
                "zone": { "x": 0.25, "y": 0.25, "w": 0.5, "h": 0.5 } }),
        json!({ "id": "p", "cell": "c", "kind": "freeze", "difference_threshold": 5,
                "dwell": { "up_ms": 3000, "down_ms": 500 } }),
        json!({ "id": "p", "cell": "c", "kind": "silence", "level_dbfs": -60.0,
                "severity": "Critical", "latched": true }),
        json!({ "id": "p", "cell": "c", "kind": "loudness",
                "target": { "kind": "r128", "target_lufs": -23.0, "max_true_peak_dbtp": -1.0 } }),
        json!({ "id": "p", "cell": "c", "kind": "loudness",
                "target": { "kind": "a85", "target_lkfs": -24.0, "max_true_peak_dbtp": -2.0 },
                "severity": "Minor" }),
    ];
    for doc in &probes {
        let real: Result<multiview_config::Probe, _> = serde_json::from_value(doc.clone());
        let mirror: Result<multiview_control::openapi_schemas::ProbeBodyDoc, _> =
            serde_json::from_value(doc.clone());
        assert!(real.is_ok(), "config rejects {doc}: {:?}", real.err());
        assert!(mirror.is_ok(), "mirror rejects {doc}: {:?}", mirror.err());
    }
}

/// Reject-fixtures: the mirror must REJECT what the config type rejects (the
/// both-accept fixture alone cannot catch a mirror looser than the real type).
#[test]
fn openapi_probe_mirror_rejects_what_the_config_type_rejects() {
    let bad_probes = [
        // Unknown kind tag.
        json!({ "id": "p", "cell": "c", "kind": "sparkle" }),
        // Luma threshold beyond the u8 scale.
        json!({ "id": "p", "cell": "c", "kind": "black", "luma_threshold": 256 }),
        // A zone with an unknown field (DetectionZone is deny_unknown_fields).
        json!({ "id": "p", "cell": "c", "kind": "black", "luma_threshold": 16,
                "zone": { "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0, "depth": 1.0 } }),
        // Dwell with an unknown field (Dwell is deny_unknown_fields).
        json!({ "id": "p", "cell": "c", "kind": "silence", "level_dbfs": -60.0,
                "dwell": { "up_ms": 1, "down_ms": 1, "sideways_ms": 1 } }),
        // An unknown severity variant.
        json!({ "id": "p", "cell": "c", "kind": "silence", "level_dbfs": -60.0,
                "severity": "Catastrophic" }),
        // A loudness target with the wrong field for its standard.
        json!({ "id": "p", "cell": "c", "kind": "loudness",
                "target": { "kind": "r128", "target_lkfs": -24.0, "max_true_peak_dbtp": -1.0 } }),
        // The symmetric crossed case: an A85 target with R128's field name.
        json!({ "id": "p", "cell": "c", "kind": "loudness",
                "target": { "kind": "a85", "target_lufs": -23.0, "max_true_peak_dbtp": -2.0 } }),
    ];
    for doc in &bad_probes {
        assert!(
            serde_json::from_value::<multiview_config::Probe>(doc.clone()).is_err(),
            "config must reject {doc}"
        );
        assert!(
            serde_json::from_value::<multiview_control::openapi_schemas::ProbeBodyDoc>(doc.clone())
                .is_err(),
            "mirror must reject {doc}"
        );
    }
}
