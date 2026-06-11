//! Tally operator-surface tests (tower oneshot): reading resolved tally state
//! (fed through the engine→control ingest into the shared mirror), tally-profile
//! CRUD with `ETag`/`If-Match`, manual override returning `202` + reaching the
//! engine command bus, RBAC (operator can override, viewer cannot), and the
//! engine-bound override path shedding to `503` on a full bus (never blocking).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_core::tally::TallyState;
use multiview_events::{Event, TallyEvent, TallyTarget};
use serde_json::json;
use support::{
    body_json, delete_json, get, harness, harness_with_capacity, put_json, send, ADMIN_TOKEN,
    OPERATOR_TOKEN, VIEWER_TOKEN,
};

#[tokio::test]
async fn read_tally_reflects_engine_observations_via_the_ingest() {
    // The end-to-end wiring: an engine tally event, drained by the tally ingest
    // into the SHARED mirror the router reads, becomes visible over REST.
    let h = harness();
    let mut sub = h.engine.subscribe();

    h.engine.publish_event(Event::TallyState(TallyEvent {
        target: TallyTarget::Tile { index: 3 },
        state: TallyState::program(),
    }));
    let step = multiview_control::tally_ingest_step(&mut sub, h.tally.as_ref()).await;
    assert_eq!(step, multiview_control::TallyIngestStep::Applied);

    let resp = send(&h.router, get("/api/v1/tally", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let entries = arr.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["target"]["kind"], "tile");
    assert_eq!(entries[0]["target"]["index"], 3);
    assert_eq!(entries[0]["state"]["color"], "Red");
    assert_eq!(entries[0]["state"]["source"]["kind"], "program");
}

#[tokio::test]
async fn read_tally_requires_authentication() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/tally", "bogus.token")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

fn profile_body() -> serde_json::Value {
    json!({
        "id": "main",
        "bit_colors": [{ "bit": 0, "color": "Red" }, { "bit": 1, "color": "Green" }],
        "index_cells": [{ "index": 0, "cell": "c0" }],
    })
}

#[tokio::test]
async fn profile_put_creates_then_replaces_and_lists() {
    let h = harness();

    // Create.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/tally/profiles/main",
            OPERATOR_TOKEN,
            None,
            &profile_body(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(support::etag(&resp).as_deref(), Some("W/\"1\""));

    // List as a viewer.
    let resp = send(&h.router, get("/api/v1/tally/profiles", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["id"], "main");

    // Replace with the matching If-Match.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/tally/profiles/main",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &profile_body(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(support::etag(&resp).as_deref(), Some("W/\"2\""));
}

#[tokio::test]
async fn profile_put_rejects_a_duplicate_bit_with_422() {
    let h = harness();
    let bad = json!({
        "id": "main",
        "bit_colors": [{ "bit": 0, "color": "Red" }, { "bit": 0, "color": "Green" }],
    });
    let resp = send(
        &h.router,
        put_json("/api/v1/tally/profiles/main", OPERATOR_TOKEN, None, &bad),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["type"], "/problems/validation");
}

#[tokio::test]
async fn profile_get_unknown_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/tally/profiles/missing", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manual_override_returns_202_and_reaches_the_engine() {
    let mut h = harness();
    let body = json!({
        "target": { "kind": "tile", "index": 2 },
        "color": "Amber",
    });
    let resp = send(
        &h.router,
        put_json("/api/v1/tally/override", OPERATOR_TOKEN, None, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await["kind"], "set_tally_override");

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1);
    match &drained[0] {
        multiview_control::Command::SetTallyOverride { target, color, .. } => {
            assert_eq!(*target, TallyTarget::Tile { index: 2 });
            assert_eq!(*color, Some(multiview_core::tally::TallyColor::Amber));
        }
        other => panic!("expected SetTallyOverride, got {other:?}"),
    }
}

#[tokio::test]
async fn clear_override_carries_a_none_color_to_the_engine() {
    let mut h = harness();
    let body = json!({ "target": { "kind": "tile", "index": 2 } });
    let resp = send(
        &h.router,
        delete_json("/api/v1/tally/override", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let drained = h.commands.try_drain();
    match &drained[0] {
        multiview_control::Command::SetTallyOverride { color, .. } => {
            assert_eq!(*color, None, "clearing carries a None colour");
        }
        other => panic!("expected SetTallyOverride, got {other:?}"),
    }
}

#[tokio::test]
async fn viewer_may_not_set_an_override() {
    let h = harness();
    let body = json!({
        "target": { "kind": "tile", "index": 0 },
        "color": "Red",
    });
    let resp = send(
        &h.router,
        put_json("/api/v1/tally/override", VIEWER_TOKEN, None, &body),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a viewer is read-only and cannot force a tally lamp"
    );
}

#[tokio::test]
async fn override_on_a_full_bus_sheds_to_503_without_blocking() {
    // Capacity 1, engine never drains: the override must shed (503), not block.
    let h = harness_with_capacity(1);

    let first = json!({ "target": { "kind": "tile", "index": 0 }, "color": "Red" });
    let resp1 = send(
        &h.router,
        put_json("/api/v1/tally/override", OPERATOR_TOKEN, None, &first),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    let second = json!({ "target": { "kind": "tile", "index": 1 }, "color": "Green" });
    let resp2 = send(
        &h.router,
        put_json("/api/v1/tally/override", OPERATOR_TOKEN, None, &second),
    )
    .await;
    assert_eq!(
        resp2.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a full bus sheds the override rather than blocking the engine"
    );
    assert_eq!(body_json(resp2).await["type"], "/problems/engine-busy");
}

#[tokio::test]
async fn override_idempotency_key_replay_enqueues_once() {
    let mut h = harness();
    let key = "override-key";
    let body = json!({ "target": { "kind": "tile", "index": 5 }, "color": "Red" });

    let req = || -> Request<Body> {
        Request::builder()
            .method("PUT")
            .uri("/api/v1/tally/override")
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
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
    assert_eq!(op1, op2);

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "the override was enqueued exactly once");
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_tally_mirrors_match_the_real_serde_shapes() {
    // The TallyEntryDoc and TallyProfileDoc OpenAPI mirrors must serialise to the
    // SAME JSON shape as the real types they document, or the contract lies.
    use multiview_config::TallyProfile;
    use multiview_control::openapi_schemas::{TallyEntryDoc, TallyProfileDoc};
    use multiview_control::TallyEntry;

    let entry = TallyEntry {
        target: TallyTarget::Tile { index: 4 },
        state: TallyState::preview(),
    };
    let entry_json = serde_json::to_value(&entry).unwrap();
    let entry_doc: TallyEntryDoc = serde_json::from_value(entry_json.clone()).unwrap();
    assert_eq!(
        entry_json,
        serde_json::to_value(&entry_doc).unwrap(),
        "TallyEntryDoc must match the real TallyEntry serde shape"
    );

    let profile: TallyProfile = serde_json::from_value(json!({
        "id": "main",
        "bit_colors": [{ "bit": 0, "color": "Red" }],
        "index_cells": [{ "index": 0, "cell": "c0" }],
    }))
    .unwrap();
    let profile_json = serde_json::to_value(&profile).unwrap();
    let profile_doc: TallyProfileDoc = serde_json::from_value(profile_json.clone()).unwrap();
    assert_eq!(
        profile_json,
        serde_json::to_value(&profile_doc).unwrap(),
        "TallyProfileDoc must match the real TallyProfile serde shape"
    );
}

#[tokio::test]
async fn admin_may_set_an_override_too() {
    let h = harness();
    let body = json!({ "target": { "kind": "element", "name": "wall-a" }, "color": "Green" });
    let resp = send(
        &h.router,
        put_json("/api/v1/tally/override", ADMIN_TOKEN, None, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}
