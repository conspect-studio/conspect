//! PR192-02: the bulk-add cap is enforced by the server, not only the browser.
#![cfg(feature = "catalogue")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::{
    body_json, get, harness, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

const PATH: &str = "/api/v1/catalogues/iptv-org/sources";

fn entry(id: &str) -> Value {
    json!({ "id": id, "name": id,
            "body": { "id": id, "kind": "hls", "url": "https://example.com/a.m3u8" } })
}

fn many(n: usize) -> Value {
    let v: Vec<Value> = (0..n).map(|i| entry(&format!("bulk-{i}"))).collect();
    json!({ "sources": v })
}

async fn count(h: &support::Harness) -> usize {
    body_json(send(&h.router, get("/api/v1/sources", ADMIN_TOKEN)).await)
        .await
        .as_array()
        .map_or(0, Vec::len)
}

#[tokio::test]
async fn eight_are_accepted_and_all_created() {
    let h = harness();
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &many(8))).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let b = body_json(r).await;
    assert_eq!(b["results"].as_array().unwrap().len(), 8);
    assert!(b["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["status"] == "created"));
    assert_eq!(count(&h).await, 8);
    let g = send(&h.router, get("/api/v1/sources/bulk-3", ADMIN_TOKEN)).await;
    assert_eq!(g.status(), StatusCode::OK);
}

#[tokio::test]
async fn nine_are_rejected_with_422_and_nothing_is_created() {
    let h = harness();
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &many(9))).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_json(r).await["detail"]
        .as_str()
        .unwrap()
        .contains("at most 8 sources per bulk add"));
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn one_invalid_entry_rejects_the_whole_request() {
    let h = harness();
    let mut body = many(3);
    body["sources"][1]["body"] = json!({ "id": "bulk-1", "kind": "no-such-kind" });
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &body)).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn duplicate_ids_in_request_or_store_reject_everything() {
    let h = harness();
    let mut body = many(2);
    body["sources"][1] = entry("bulk-0");
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &body)).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&h).await, 0);
    // Existing in the store.
    let r = send(
        &h.router,
        post_json("/api/v1/sources/bulk-1", OPERATOR_TOKEN, &entry("bulk-1")),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &many(3))).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&h).await, 1);
}

#[tokio::test]
async fn scoped_principal_cannot_bulk_create_outside_its_scope() {
    let h = harness();
    let body = json!({ "sources": [entry("scoped-layout"), entry("not-in-scope")] });
    let r = send(&h.router, post_json(PATH, SCOPED_TOKEN, &body)).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(count(&h).await, 0);
    let ok = json!({ "sources": [entry("scoped-layout")] });
    let r = send(&h.router, post_json(PATH, SCOPED_TOKEN, &ok)).await;
    assert_eq!(r.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn read_role_gets_403() {
    let h = harness();
    let r = send(&h.router, post_json(PATH, VIEWER_TOKEN, &many(1))).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(count(&h).await, 0);
}
