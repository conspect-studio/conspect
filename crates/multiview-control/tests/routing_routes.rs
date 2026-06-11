//! RT-11 / ADR-0034 §9 — the `/api/v1/routing/plan` + `/api/v1/routing/{kind}/take`
//! HTTP surface.
//!
//! * `/routing/plan` classifies a crosspoint **without** applying, returning the
//!   #11 class (`class1` / `reset_lite` / `class2`) + a coerced-degradation flag.
//! * `/routing/{video|audio|subtitle}/take` resolves the class, submits the
//!   `Command::Route*` on the engine command bus, and returns `200 {class1,
//!   applied}` for a hot Class-1 re-point vs `202 {operation_id}` for a Class-2
//!   migration — reusing the `submit_accepted` path (Idempotency-Key, RFC 9457,
//!   BOLA `authorize_object`, shed-503).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{body_json, harness, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

fn video_route_body() -> serde_json::Value {
    json!({
        "target": { "kind": "video_cell", "cell": "c0" },
        "source": { "input_id": "cam-b", "kind": { "kind": "video" }, "selector": { "by": "best" } }
    })
}

#[tokio::test]
async fn plan_classifies_a_video_repoint_as_class1_without_applying() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/routing/plan", OPERATOR_TOKEN, &video_route_body()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["class"], "class1");
    assert_eq!(body["coerced"], false);

    // Plan never submits a command.
    let drained = h.commands.try_drain();
    assert!(drained.is_empty(), "plan must not enqueue a command");
}

#[tokio::test]
async fn plan_classifies_an_audio_layout_mismatch_breakaway_as_class2() {
    let h = harness();
    // A stereo source onto a discrete track pinned to 5.1: Class-2 (the property
    // the brief mandates — not plain Class-1).
    let body = json!({
        "target": { "kind": "audio_discrete_track", "track": "trk-5_1", "pinned_channels": 6 },
        "source": { "input_id": "cam-b", "kind": { "kind": "audio" }, "selector": { "by": "index", "index": 0 } },
        "source_channels": 2
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/routing/plan", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let class = body_json(resp).await["class"].as_str().unwrap().to_owned();
    assert_eq!(class, "class2");
}

#[tokio::test]
async fn take_class1_returns_200_applied_and_submits_the_route_command() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/routing/video/take",
            OPERATOR_TOKEN,
            &video_route_body(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["class"], "class1");
    assert_eq!(body["applied"], true);

    // The route command actually reached the engine bus.
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "a take submits exactly one route command");
    match &drained[0] {
        multiview_control::Command::RouteVideo { cell, source, .. } => {
            assert_eq!(cell, "c0");
            assert_eq!(source.input_id, "cam-b");
        }
        other => panic!("expected RouteVideo, got {other:?}"),
    }
}

#[tokio::test]
async fn take_class2_returns_202_with_operation_id() {
    let mut h = harness();
    // A stereo source onto a discrete track pinned to 5.1 → Class-2 migration.
    let body = json!({
        "target": { "kind": "audio_discrete_track", "track": "trk-5_1", "pinned_channels": 6 },
        "source": { "input_id": "cam-b", "kind": { "kind": "audio" }, "selector": { "by": "best" } },
        "source_channels": 2
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/routing/audio/take", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let out = body_json(resp).await;
    assert!(
        out["operation_id"].as_str().is_some_and(|s| !s.is_empty()),
        "a Class-2 take returns an operation id for the async migration outcome"
    );

    // The route command was still submitted (the engine drives the migration).
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind(), "route_audio");
}

#[tokio::test]
async fn take_is_idempotent_under_a_repeated_key() {
    let mut h = harness();
    let key = "rt11-take-key";
    let req = || {
        let mut r = post_json(
            "/api/v1/routing/video/take",
            OPERATOR_TOKEN,
            &video_route_body(),
        );
        r.headers_mut()
            .insert("idempotency-key", key.parse().unwrap());
        r
    };
    let resp1 = send(&h.router, req()).await;
    assert_eq!(resp1.status(), StatusCode::OK);
    let resp2 = send(&h.router, req()).await;
    // A replayed key returns success without a second enqueue.
    assert!(matches!(
        resp2.status(),
        StatusCode::OK | StatusCode::ACCEPTED
    ));
    let drained = h.commands.try_drain();
    assert_eq!(
        drained.len(),
        1,
        "a repeated idempotency key enqueues the route exactly once"
    );
}

#[tokio::test]
async fn viewer_may_not_take_a_route() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/routing/video/take",
            VIEWER_TOKEN,
            &video_route_body(),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a viewer is read-only and cannot take a crosspoint"
    );
}

#[tokio::test]
async fn unknown_route_kind_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/routing/teapot/take",
            OPERATOR_TOKEN,
            &video_route_body(),
        ),
    )
    .await;
    // An unknown crosspoint kind in the path is not a valid route resource.
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::BAD_REQUEST,
        "an unknown route kind is rejected, got {}",
        resp.status()
    );
}
