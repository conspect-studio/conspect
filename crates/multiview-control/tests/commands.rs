//! Tests for operational commands: `202 Accepted` + operation id, idempotent
//! replay, the bounded command bus surfacing `503` on overflow, and the engine
//! draining the submitted command (the isolation seam).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::json;
use support::{
    body_json, harness, harness_with_capacity, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN,
};

#[tokio::test]
async fn start_returns_202_with_operation_id_and_reaches_engine() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/commands/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let op = body["operation_id"].as_str().expect("operation_id present");
    assert!(!op.is_empty(), "a non-empty operation id is returned");
    assert_eq!(body["kind"], "start");

    // The engine drains the command at its leisure (non-blocking).
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "exactly one command reached the engine");
    assert_eq!(drained[0].kind(), "start");
    assert_eq!(
        drained[0].operation_id().as_str(),
        op,
        "the engine sees the same correlation id the client got"
    );
}

#[tokio::test]
async fn idempotency_key_replay_returns_original_op_and_enqueues_once() {
    let mut h = harness();
    let key = "fixed-key-123";

    let req = |key: &str| -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/commands/start")
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from("{}"))
            .unwrap()
    };

    let resp1 = send(&h.router, req(key)).await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);
    let op1 = body_json(resp1).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Replay with the same key: same operation id, NOT a second enqueue.
    let resp2 = send(&h.router, req(key)).await;
    assert_eq!(resp2.status(), StatusCode::ACCEPTED);
    let op2 = body_json(resp2).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(op1, op2, "a retried key returns the original operation id");

    let drained = h.commands.try_drain();
    assert_eq!(
        drained.len(),
        1,
        "the command was enqueued exactly once despite two submissions"
    );
}

#[tokio::test]
async fn full_command_bus_sheds_to_503_without_blocking() {
    // Capacity 1, and the engine never drains: a second submission must be shed
    // (503), never block — proving control cannot force the engine to make room.
    let h = harness_with_capacity(1);

    let resp1 = send(
        &h.router,
        post_json("/api/v1/commands/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    let resp2 = send(
        &h.router,
        post_json("/api/v1/commands/stop", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(
        resp2.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a full bus sheds the request rather than blocking the engine"
    );
    let problem = body_json(resp2).await;
    assert_eq!(problem["type"], "/problems/engine-busy");
}

#[tokio::test]
async fn shed_submission_releases_its_idempotency_key_so_a_retry_actually_enqueues() {
    // Reserve-then-shed correctness: a command that is SHED (503, never reached
    // the engine) must NOT leave its idempotency key recorded. Otherwise a retry
    // with the same key would observe a false `Reservation::Replay` (kind:
    // "replay") and the engine would never receive the command at all.
    let mut h = harness_with_capacity(1);
    let key = "retry-after-shed-key";

    let keyed = |path: &str| -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from("{}"))
            .unwrap()
    };

    // 1. Occupy the single bus slot with an unrelated command (no key).
    let resp1 = send(
        &h.router,
        post_json("/api/v1/commands/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    // 2. A keyed submission now finds the bus full and is shed (503). The key
    //    must be released because the command never reached the engine.
    let resp2 = send(&h.router, keyed("/api/v1/commands/stop")).await;
    assert_eq!(
        resp2.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the keyed submission is shed when the bus is full"
    );

    // 3. The engine drains, freeing the slot (and consuming only the first,
    //    successfully-enqueued command — the shed one never arrived).
    let drained = h.commands.try_drain();
    assert_eq!(
        drained.len(),
        1,
        "only the first command ever reached the engine; the shed one did not"
    );
    assert_eq!(drained[0].kind(), "start");

    // 4. Retry with the SAME key. Because the shed reservation was released this
    //    must be a FRESH 202 (not a false "replay") AND must actually enqueue
    //    the command this time.
    let resp3 = send(&h.router, keyed("/api/v1/commands/stop")).await;
    assert_eq!(resp3.status(), StatusCode::ACCEPTED);
    let body3 = body_json(resp3).await;
    assert_eq!(
        body3["kind"], "stop",
        "the retry is a fresh submission of the real command, not a replay stub"
    );
    let op3 = body3["operation_id"]
        .as_str()
        .expect("operation id present");
    assert!(!op3.is_empty());

    let drained2 = h.commands.try_drain();
    assert_eq!(
        drained2.len(),
        1,
        "the previously-shed command is finally enqueued on retry"
    );
    assert_eq!(drained2[0].kind(), "stop");
    assert_eq!(
        drained2[0].operation_id().as_str(),
        op3,
        "the engine sees the same correlation id the retry returned"
    );
}

#[tokio::test]
async fn swap_carries_tile_and_source_to_engine() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/swap",
            OPERATOR_TOKEN,
            &json!({ "tile": "cam-1", "source": "studio-b" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1);
    match &drained[0] {
        multiview_control::Command::SwapSource { tile, source, .. } => {
            assert_eq!(tile, "cam-1");
            assert_eq!(source, "studio-b");
        }
        other => panic!("expected SwapSource, got {other:?}"),
    }
}

#[tokio::test]
async fn viewer_may_not_issue_commands() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/commands/start", VIEWER_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a viewer is read-only and cannot start output"
    );
}
