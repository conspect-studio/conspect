//! End-to-end tests for the sources resource: CRUD, `ETag` round-trip,
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

fn source_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": { "id": "cam1", "kind": "rtsp", "url": "rtsp://example/cam1" }
    })
}

/// BOLA embedded-reference leak (OWASP API1, ADR-W005/ADR-W025): a
/// device-projected source carries a managed **device id** in `body.device_ref`
/// (ADR-M009). A scoped principal authorized for the SOURCE (its own id is in
/// scope) must NOT learn an out-of-scope device's id through that field — it is
/// redacted, by parity with a single-device `GET` `403`'ing the device.
///
/// The source id `scoped-layout` is in `SCOPED_TOKEN`'s allowlist (so the source
/// is readable); its `device_ref` is `dev-other` (out of scope). The scoped read
/// (single + list) must omit `device_ref`; admin sees it.
#[tokio::test]
async fn source_device_ref_is_redacted_when_out_of_scope() {
    let h = harness();
    let source = json!({
        "name": "Projected",
        "body": {
            "id": "scoped-layout",
            "kind": "rtsp",
            "url": "rtsp://[fd00:db8::42]:554/main",
            "device_ref": "dev-other"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/scoped-layout", ADMIN_TOKEN, &source),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Single GET (source in scope): the out-of-scope device_ref is redacted.
    let resp = send(
        &h.router,
        get("/api/v1/sources/scoped-layout", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let src = body_json(resp).await;
    assert!(
        src["body"].get("device_ref").is_none(),
        "a scoped principal must not see an out-of-scope device_ref (BOLA): {src}"
    );
    // The source itself is still fully present (only the device link is hidden).
    assert_eq!(src["body"]["url"], "rtsp://[fd00:db8::42]:554/main");

    // The list view redacts identically.
    let resp = send(&h.router, get("/api/v1/sources", SCOPED_TOKEN)).await;
    let list = body_json(resp).await;
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "scoped-layout")
        .expect("the scoped source is listed");
    assert!(
        row["body"].get("device_ref").is_none(),
        "the list view must also redact the out-of-scope device_ref: {row}"
    );

    // An unscoped admin sees the device_ref unchanged.
    let resp = send(&h.router, get("/api/v1/sources/scoped-layout", ADMIN_TOKEN)).await;
    let src = body_json(resp).await;
    assert_eq!(
        src["body"]["device_ref"], "dev-other",
        "an unscoped admin sees the device_ref"
    );
}

/// BOLA ROW enumeration (OWASP API1, ADR-W005/ADR-W025): a source is itself
/// object-scoped by its OWN id (`get_source` 403s an out-of-scope id), so
/// `list_sources` MUST filter ROWS to the principal's allowlist — exactly as
/// `list_cast_sessions`/`list_devices` do. Redacting only the embedded
/// `device_ref` (round-3) still leaked out-of-scope SOURCE ids through the list.
///
/// `SCOPED_TOKEN` (allowlist `["scoped-layout"]`) lists with an in-scope source
/// (`scoped-layout`, whose `device_ref` points out of scope) and an out-of-scope
/// source (`other-src`). The scoped list must contain ONLY `scoped-layout`, and
/// that row's out-of-scope `device_ref` must still be redacted.
#[tokio::test]
async fn list_filters_source_rows_to_the_scoped_allowlist() {
    let h = harness();
    // In-scope source (own id allowlisted) whose device_ref is out of scope.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/scoped-layout",
            ADMIN_TOKEN,
            &json!({
                "name": "Mine",
                "body": {
                    "id": "scoped-layout",
                    "kind": "rtsp",
                    "url": "rtsp://[fd00:db8::1]/mine",
                    "device_ref": "dev-other"
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Out-of-scope source (own id NOT allowlisted).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/other-src",
            ADMIN_TOKEN,
            &json!({
                "name": "Theirs",
                "body": { "id": "other-src", "kind": "rtsp", "url": "rtsp://[fd00:db8::2]/theirs" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The scoped list contains ONLY the in-scope source row.
    let resp = send(&h.router, get("/api/v1/sources", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout"],
        "a scoped principal must see ONLY its allowlisted source rows, never enumerate others (BOLA)"
    );
    // …and the in-scope row's out-of-scope device_ref is still redacted.
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "scoped-layout")
        .unwrap();
    assert!(
        row["body"].get("device_ref").is_none(),
        "the surviving in-scope row still has its out-of-scope device_ref redacted: {row}"
    );

    // An unscoped admin sees BOTH source rows.
    let resp = send(&h.router, get("/api/v1/sources", ADMIN_TOKEN)).await;
    assert_eq!(
        body_json(resp).await.as_array().unwrap().len(),
        2,
        "an unscoped admin still sees every source"
    );
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &source_body("Cam 1"),
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
    assert_eq!(created["id"], "cam1");
    assert_eq!(created["name"], "Cam 1");
    assert_eq!(created["body"]["url"], "rtsp://example/cam1");

    let resp = send(&h.router, get("/api/v1/sources/cam1", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let fetched = body_json(resp).await;
    assert_eq!(fetched["name"], "Cam 1");
}

#[tokio::test]
async fn get_unknown_source_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/sources/missing", OPERATOR_TOKEN)).await;
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
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &source_body("Cam 1"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &source_body("Renamed"),
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
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &source_body("Cam 1"),
        ),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &source_body("V2"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &source_body("Clobber"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");

    let resp = send(&h.router, get("/api/v1/sources/cam1", OPERATOR_TOKEN)).await;
    let current = body_json(resp).await;
    assert_eq!(current["name"], "V2", "the clobbering write was rejected");
}

#[tokio::test]
async fn update_without_if_match_is_precondition_required() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &source_body("Cam 1"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            None,
            &source_body("X"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn list_returns_created_sources_sorted() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/sources/bbb",
            OPERATOR_TOKEN,
            &json!({ "name": "B", "body": { "kind": "bars" } }),
        ),
    )
    .await;
    send(
        &h.router,
        post_json(
            "/api/v1/sources/aaa",
            OPERATOR_TOKEN,
            &json!({ "name": "A", "body": { "kind": "bars" } }),
        ),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/sources", OPERATOR_TOKEN)).await;
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
    // No Authorization header: rejected before any data is read.
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/sources")
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
    // A read-only Viewer lacks Action::Write.
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/cam1", VIEWER_TOKEN, &source_body("Cam 1")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_requires_admin_role() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/sources/cam1", ADMIN_TOKEN, &source_body("Cam 1")),
    )
    .await;

    // Operator may not delete (Administer action).
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/cam1")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Admin may delete.
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/cam1")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(&h.router, get("/api/v1/sources/cam1", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
