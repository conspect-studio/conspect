//! PR192-02 (+ the activation follow-up): bulk add is bounded by a request cap
//! and by an explicit activation choice, not by the browser only.
//!
//! The old rule was "at most 8 sources, each silently always on". The rule now:
//! one request creates at most [`MAX`] sources, and a request above the old cap
//! of [`EXPLICIT_ABOVE`] must state every entry's `activation` — the choice the
//! operator made in the browser. Below that threshold an absent `activation`
//! still means `always_on` (the config default), so nothing changes for callers
//! that predate the choice.
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

/// Mirrors `BULK_ADD_MAX` in `routes/catalogue.rs` (an integration test cannot
/// import a `pub(crate)` constant).
const MAX: usize = 200;
/// Mirrors `BULK_ADD_EXPLICIT_ACTIVATION_ABOVE` in `routes/catalogue.rs`.
const EXPLICIT_ABOVE: usize = 8;

/// An entry that leaves the activation implicit (the pre-choice contract).
fn entry(id: &str) -> Value {
    json!({ "id": id, "name": id,
            "body": { "id": id, "kind": "hls", "url": "https://example.com/a.m3u8" } })
}

/// An entry that states its activation (what the browser now always sends).
fn entry_activation(id: &str, activation: &str) -> Value {
    json!({ "id": id, "name": id,
            "body": { "id": id, "kind": "hls", "url": "https://example.com/a.m3u8",
                      "activation": activation } })
}

fn many_with(n: usize, make: impl Fn(&str) -> Value) -> Value {
    let v: Vec<Value> = (0..n).map(|i| make(&format!("bulk-{i}"))).collect();
    json!({ "sources": v })
}

fn many(n: usize) -> Value {
    many_with(n, entry)
}

fn many_activation(n: usize, activation: &str) -> Value {
    many_with(n, |id| entry_activation(id, activation))
}

async fn count(h: &support::Harness) -> usize {
    body_json(send(&h.router, get("/api/v1/sources", ADMIN_TOKEN)).await)
        .await
        .as_array()
        .map_or(0, Vec::len)
}

/// The `activation` the durable source document carries (`null` when it leaves
/// the choice implicit, which the config reads as `always_on`).
async fn stored_activation(h: &support::Harness, id: &str) -> Value {
    let r = send(
        &h.router,
        get(&format!("/api/v1/sources/{id}"), ADMIN_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "GET /sources/{id}");
    body_json(r).await["body"]["activation"].clone()
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
    // At or below the old cap an implicit activation stays implicit: the config
    // default (`always_on`) is unchanged for callers that predate the choice.
    assert_eq!(stored_activation(&h, "bulk-3").await, Value::Null);
}

#[tokio::test]
async fn nine_on_demand_are_accepted_and_stay_on_demand_durably() {
    let h = harness();
    let r = send(
        &h.router,
        post_json(PATH, OPERATOR_TOKEN, &many_activation(9, "on_demand")),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let b = body_json(r).await;
    assert_eq!(b["results"].as_array().unwrap().len(), 9);
    assert!(b["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["status"] == "created"));
    assert_eq!(count(&h).await, 9);
    // The operator's choice is what is durably stored, not a default.
    for i in 0..9 {
        assert_eq!(
            stored_activation(&h, &format!("bulk-{i}")).await,
            json!("on_demand"),
            "bulk-{i}"
        );
    }
}

#[tokio::test]
async fn nine_always_on_are_accepted_when_every_entry_states_it() {
    let h = harness();
    let r = send(
        &h.router,
        post_json(PATH, OPERATOR_TOKEN, &many_activation(9, "always_on")),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(count(&h).await, 9);
    for i in 0..9 {
        assert_eq!(
            stored_activation(&h, &format!("bulk-{i}")).await,
            json!("always_on"),
            "bulk-{i}"
        );
    }
}

#[tokio::test]
async fn above_the_old_cap_without_a_stated_activation_is_rejected() {
    let h = harness();
    let r = send(
        &h.router,
        post_json(PATH, OPERATOR_TOKEN, &many(EXPLICIT_ABOVE + 1)),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let detail = body_json(r).await["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("must state its `activation`"), "{detail}");
    // This case leaves every entry implicit, so the count names all nine; the
    // single-implicit-entry case is covered by its own test below.
    assert!(detail.contains("9 of 9"), "{detail}");
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn one_implicit_entry_rejects_the_whole_large_batch() {
    let h = harness();
    let mut body = many_activation(EXPLICIT_ABOVE + 1, "on_demand");
    // The last entry drops its choice; nothing may be created.
    body["sources"][EXPLICIT_ABOVE] = entry(&format!("bulk-{EXPLICIT_ABOVE}"));
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &body)).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn exactly_the_request_cap_is_accepted() {
    let h = harness();
    let r = send(
        &h.router,
        post_json(PATH, OPERATOR_TOKEN, &many_activation(MAX, "on_demand")),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(count(&h).await, MAX);
}

#[tokio::test]
async fn above_the_request_cap_is_rejected_even_with_stated_activations() {
    let h = harness();
    let r = send(
        &h.router,
        post_json(PATH, OPERATOR_TOKEN, &many_activation(MAX + 1, "on_demand")),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_json(r).await["detail"]
        .as_str()
        .unwrap()
        .contains(&format!("at most {MAX} sources per bulk add")));
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn an_unknown_activation_token_is_rejected() {
    let h = harness();
    let mut body = many_activation(2, "on_demand");
    body["sources"][1]["body"]["activation"] = json!("sometimes");
    let r = send(&h.router, post_json(PATH, OPERATOR_TOKEN, &body)).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
async fn a_large_batch_is_still_authorised_per_source() {
    let h = harness();
    // Nine entries with stated activations (the lifted-cap path) from a
    // principal scoped to `scoped-layout` alone: per-object authorization
    // refuses the whole batch before anything is created (BOLA).
    let r = send(
        &h.router,
        post_json(
            PATH,
            SCOPED_TOKEN,
            &many_activation(EXPLICIT_ABOVE + 1, "on_demand"),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn read_role_gets_403() {
    let h = harness();
    let r = send(&h.router, post_json(PATH, VIEWER_TOKEN, &many(1))).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(count(&h).await, 0);
}
