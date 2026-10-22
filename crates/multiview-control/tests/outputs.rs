//! End-to-end tests for the outputs resource: CRUD, `ETag` round-trip,
//! `If-Match` optimistic concurrency (`412`), and RBAC — driven through the real
//! router. Mirrors `tests/layouts.rs`.
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

fn output_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": { "kind": "rtsp_server", "mount": "/multiview", "codec": "h264" }
    })
}

/// BOLA embedded-reference leak (OWASP API1, ADR-W005/ADR-W025): a
/// device-projected output carries a managed **device id** in `body.device_ref`
/// (ADR-M009). A scoped principal authorized for the OUTPUT must NOT learn an
/// out-of-scope device's id through that field — it is redacted, by parity with a
/// single-device `GET` `403`'ing the device.
///
/// The output resource id `scoped-layout` is in `SCOPED_TOKEN`'s allowlist (so
/// the output is readable); its `device_ref` is `dev-other` (out of scope). The
/// scoped read (single + list) must omit `device_ref`; admin sees it.
#[tokio::test]
async fn output_device_ref_is_redacted_when_out_of_scope() {
    let h = harness();
    let output = json!({
        "name": "Projected out",
        "body": {
            "kind": "rtsp_server",
            "mount": "/multiview",
            "codec": "h264",
            "device_ref": "dev-other"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/scoped-layout", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Single GET (output in scope): the out-of-scope device_ref is redacted.
    let resp = send(
        &h.router,
        get("/api/v1/outputs/scoped-layout", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let out = body_json(resp).await;
    assert!(
        out["body"].get("device_ref").is_none(),
        "a scoped principal must not see an out-of-scope device_ref (BOLA): {out}"
    );
    assert_eq!(out["body"]["mount"], "/multiview");

    // The list view redacts identically.
    let resp = send(&h.router, get("/api/v1/outputs", SCOPED_TOKEN)).await;
    let list = body_json(resp).await;
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "scoped-layout")
        .expect("the scoped output is listed");
    assert!(
        row["body"].get("device_ref").is_none(),
        "the list view must also redact the out-of-scope device_ref: {row}"
    );

    // An unscoped admin sees the device_ref unchanged.
    let resp = send(&h.router, get("/api/v1/outputs/scoped-layout", ADMIN_TOKEN)).await;
    let out = body_json(resp).await;
    assert_eq!(out["body"]["device_ref"], "dev-other");
}

/// BOLA ROW enumeration (OWASP API1, ADR-W005/ADR-W025): an output is itself
/// object-scoped by its OWN id (`get_output` 403s an out-of-scope id), so
/// `list_outputs` MUST filter ROWS to the principal's allowlist — exactly as
/// `list_devices` does. Redacting only the embedded `device_ref` (round-3) still
/// leaked out-of-scope OUTPUT ids through the list.
///
/// `SCOPED_TOKEN` (allowlist `["scoped-layout"]`) lists with an in-scope output
/// (`scoped-layout`, `device_ref` out of scope) and an out-of-scope output
/// (`other-out`). The scoped list must contain ONLY `scoped-layout`, and that
/// row's out-of-scope `device_ref` must still be redacted.
#[tokio::test]
async fn list_filters_output_rows_to_the_scoped_allowlist() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/scoped-layout",
            ADMIN_TOKEN,
            &json!({
                "name": "Mine",
                "body": { "kind": "rtsp_server", "mount": "/mine", "codec": "h264", "device_ref": "dev-other" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/other-out",
            ADMIN_TOKEN,
            &json!({
                "name": "Theirs",
                "body": { "kind": "rtsp_server", "mount": "/theirs", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(&h.router, get("/api/v1/outputs", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout"],
        "a scoped principal must see ONLY its allowlisted output rows, never enumerate others (BOLA)"
    );
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "scoped-layout")
        .unwrap();
    assert!(
        row["body"].get("device_ref").is_none(),
        "the surviving in-scope row still has its out-of-scope device_ref redacted: {row}"
    );

    let resp = send(&h.router, get("/api/v1/outputs", ADMIN_TOKEN)).await;
    assert_eq!(
        body_json(resp).await.as_array().unwrap().len(),
        2,
        "an unscoped admin still sees every output"
    );
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/main", OPERATOR_TOKEN, &output_body("Main")),
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
    assert_eq!(created["id"], "main");
    assert_eq!(created["name"], "Main");
    assert_eq!(created["body"]["mount"], "/multiview");

    let resp = send(&h.router, get("/api/v1/outputs/main", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let fetched = body_json(resp).await;
    assert_eq!(fetched["name"], "Main");
}

#[tokio::test]
async fn get_unknown_output_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/outputs/missing", OPERATOR_TOKEN)).await;
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
        post_json("/api/v1/outputs/main", OPERATOR_TOKEN, &output_body("Main")),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/outputs/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &output_body("Renamed"),
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
        post_json("/api/v1/outputs/main", OPERATOR_TOKEN, &output_body("Main")),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/outputs/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &output_body("V2"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/outputs/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &output_body("Clobber"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");

    let resp = send(&h.router, get("/api/v1/outputs/main", OPERATOR_TOKEN)).await;
    let current = body_json(resp).await;
    assert_eq!(current["name"], "V2", "the clobbering write was rejected");
}

#[tokio::test]
async fn update_without_if_match_is_precondition_required() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/outputs/main", OPERATOR_TOKEN, &output_body("Main")),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/outputs/main",
            OPERATOR_TOKEN,
            None,
            &output_body("X"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn list_returns_created_outputs_sorted() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/outputs/bbb", OPERATOR_TOKEN, &output_body("B")),
    )
    .await;
    send(
        &h.router,
        post_json("/api/v1/outputs/aaa", OPERATOR_TOKEN, &output_body("A")),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/outputs", OPERATOR_TOKEN)).await;
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
        .uri("/api/v1/outputs")
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
        post_json("/api/v1/outputs/main", VIEWER_TOKEN, &output_body("Main")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_requires_admin_role() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/outputs/main", ADMIN_TOKEN, &output_body("Main")),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/outputs/main")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/outputs/main")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(&h.router, get("/api/v1/outputs/main", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
