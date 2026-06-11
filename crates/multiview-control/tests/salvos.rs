//! Salvo operator-surface tests (tower oneshot): CRUD with `ETag`/`If-Match`,
//! arm/take/cancel returning `202 Accepted` + an operation id that reaches the
//! engine command bus, idempotent replay, the bounded bus shedding to `503`
//! without blocking, RBAC (operator can take, viewer cannot), and `404` for an
//! unknown salvo.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::Salvo;
use serde_json::json;
use support::{
    body_json, delete_if_match, get, harness, harness_with_capacity, post_if_match, put_json, send,
    ADMIN_TOKEN, OPERATOR_TOKEN, OUTPUT_SCOPED_TOKEN, VIEWER_TOKEN,
};

fn salvo_body() -> serde_json::Value {
    json!({
        "id": "wide",
        "display_name": "Wide shot",
        "layout": "grid-9",
        "tally": [{ "cell": "c0", "color": "Red" }],
    })
}

fn seed_salvo(h: &support::Harness, id: &str) {
    // Seed directly into the shared store via serde so the body is the real
    // config-as-code shape.
    let salvo: Salvo = serde_json::from_value(json!({
        "id": id,
        "layout": "grid-9",
    }))
    .expect("salvo deserialises");
    h.salvos.create(salvo).expect("seed create");
}

#[tokio::test]
async fn put_creates_at_201_then_replaces_with_if_match() {
    let h = harness();

    // Create.
    let resp = send(
        &h.router,
        put_json("/api/v1/salvos/wide", OPERATOR_TOKEN, None, &salvo_body()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        support::etag(&resp).as_deref(),
        Some("W/\"1\""),
        "a fresh salvo is created at version 1"
    );
    let body = body_json(resp).await;
    assert_eq!(body["id"], "wide");
    assert_eq!(body["display_name"], "Wide shot");

    // Replace requires a matching If-Match.
    let mut changed = salvo_body();
    changed["display_name"] = json!("Wide shot v2");
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/salvos/wide",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &changed,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(support::etag(&resp).as_deref(), Some("W/\"2\""));
    let body = body_json(resp).await;
    assert_eq!(body["display_name"], "Wide shot v2");
}

#[tokio::test]
async fn replace_with_stale_if_match_is_412() {
    let h = harness();
    seed_salvo(&h, "wide");
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/salvos/wide",
            OPERATOR_TOKEN,
            Some("W/\"9\""),
            &salvo_body(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(body_json(resp).await["type"], "/problems/version-conflict");
}

#[tokio::test]
async fn put_rejects_an_empty_salvo_with_422() {
    let h = harness();
    // A salvo that changes nothing fails Salvo::validate.
    let empty = json!({ "id": "wide" });
    let resp = send(
        &h.router,
        put_json("/api/v1/salvos/wide", OPERATOR_TOKEN, None, &empty),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["type"], "/problems/validation");
}

#[tokio::test]
async fn list_and_get_return_seeded_salvos_to_a_viewer() {
    let h = harness();
    seed_salvo(&h, "zeta");
    seed_salvo(&h, "alpha");

    let resp = send(&h.router, get("/api/v1/salvos", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["alpha", "zeta"]);

    let resp = send(&h.router, get("/api/v1/salvos/alpha", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(support::etag(&resp).as_deref(), Some("W/\"1\""));
}

#[tokio::test]
async fn delete_requires_admin_and_if_match() {
    let h = harness();
    seed_salvo(&h, "wide");

    // An operator may not delete (administer action).
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/salvos/wide", OPERATOR_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // An admin with a matching If-Match deletes.
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/salvos/wide", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(&h.router, get("/api/v1/salvos/wide", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn arm_then_take_returns_202_and_reaches_the_engine() {
    let mut h = harness();
    seed_salvo(&h, "wide");

    // Arm.
    let resp = send(
        &h.router,
        post_if_match("/api/v1/salvos/wide/arm", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let arm_op = body["operation_id"].as_str().unwrap().to_owned();
    assert!(!arm_op.is_empty());
    assert_eq!(body["kind"], "arm_salvo");

    // Take.
    let resp = send(
        &h.router,
        post_if_match("/api/v1/salvos/wide/take", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let take_op = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Both commands reached the engine, in order, carrying the salvo id and the
    // same correlation ids the client received.
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 2);
    match &drained[0] {
        multiview_control::Command::ArmSalvo { op, salvo, head } => {
            assert_eq!(op.as_str(), arm_op);
            assert_eq!(salvo, "wide");
            assert_eq!(head.as_deref(), None);
        }
        other => panic!("expected ArmSalvo, got {other:?}"),
    }
    match &drained[1] {
        multiview_control::Command::TakeSalvo { op, salvo, .. } => {
            assert_eq!(op.as_str(), take_op);
            assert_eq!(salvo.as_deref(), Some("wide"));
        }
        other => panic!("expected TakeSalvo, got {other:?}"),
    }
}

#[tokio::test]
async fn take_with_head_query_scopes_the_command() {
    let mut h = harness();
    seed_salvo(&h, "wide");
    let resp = send(
        &h.router,
        post_if_match("/api/v1/salvos/wide/take?head=wall-2", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let drained = h.commands.try_drain();
    match &drained[0] {
        multiview_control::Command::TakeSalvo { head, .. } => {
            assert_eq!(head.as_deref(), Some("wall-2"));
        }
        other => panic!("expected TakeSalvo, got {other:?}"),
    }
}

#[tokio::test]
async fn take_replay_with_idempotency_key_enqueues_once() {
    let mut h = harness();
    seed_salvo(&h, "wide");
    let key = "salvo-take-key";

    let req = || -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/salvos/wide/take")
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .header("idempotency-key", key)
            .body(Body::empty())
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

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "the take was enqueued exactly once");
}

#[tokio::test]
async fn arm_on_a_full_bus_sheds_to_503_without_blocking() {
    // Capacity 1, engine never drains: the salvo arm must shed (503), not block —
    // proving the operator surface cannot back-pressure the engine (invariant #10).
    let h = harness_with_capacity(1);
    seed_salvo(&h, "wide");
    seed_salvo(&h, "tight");

    let resp1 = send(
        &h.router,
        post_if_match("/api/v1/salvos/wide/arm", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    let resp2 = send(
        &h.router,
        post_if_match("/api/v1/salvos/tight/take", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp2.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a full bus sheds the take rather than blocking the engine"
    );
    assert_eq!(body_json(resp2).await["type"], "/problems/engine-busy");
}

#[tokio::test]
async fn arm_unknown_salvo_is_404_and_enqueues_nothing() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_if_match("/api/v1/salvos/ghost/arm", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["type"], "/problems/not-found");
    assert!(
        h.commands.try_drain().is_empty(),
        "an unknown salvo never reaches the engine"
    );
}

#[tokio::test]
async fn viewer_may_read_but_may_not_take() {
    let h = harness();
    seed_salvo(&h, "wide");

    let read = send(&h.router, get("/api/v1/salvos", VIEWER_TOKEN)).await;
    assert_eq!(read.status(), StatusCode::OK);

    let take = send(
        &h.router,
        post_if_match("/api/v1/salvos/wide/take", VIEWER_TOKEN, None),
    )
    .await;
    assert_eq!(
        take.status(),
        StatusCode::FORBIDDEN,
        "a viewer is read-only and cannot take a salvo"
    );
    assert_eq!(body_json(take).await["type"], "/problems/forbidden");
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_salvo_mirror_matches_the_config_salvo_serde_shape() {
    // The SalvoDoc OpenAPI mirror must serialise to the SAME JSON shape as the
    // real config-as-code Salvo it documents, or the published contract lies.
    use multiview_control::openapi_schemas::SalvoDoc;

    let salvo: Salvo = serde_json::from_value(json!({
        "id": "wide",
        "display_name": "Wide shot",
        "layout": "grid-9",
        "sources": [{ "cell": "c0", "input_id": "cam-1" }],
        "tally": [{ "cell": "c0", "color": "Red" }],
        "umd": [{ "cell": "c0", "text": "CAM 1" }],
    }))
    .unwrap();
    let core_json = serde_json::to_value(&salvo).unwrap();
    let doc: SalvoDoc = serde_json::from_value(core_json.clone()).unwrap();
    let doc_json = serde_json::to_value(&doc).unwrap();
    assert_eq!(
        core_json, doc_json,
        "the OpenAPI mirror must match the config Salvo serde shape"
    );
}

#[tokio::test]
async fn output_scoped_role_denies_cross_output_head() {
    // An operator confined to head `wall-1` must be denied arm/take/cancel that
    // address a head OUTSIDE its allowlist (`wall-2`) — per-output BOLA (OWASP
    // API1). The deny must happen at the HTTP boundary and enqueue nothing.
    let mut h = harness();
    seed_salvo(&h, "wide");

    for action in ["arm", "take", "cancel"] {
        let resp = send(
            &h.router,
            post_if_match(
                &format!("/api/v1/salvos/wide/{action}?head=wall-2"),
                OUTPUT_SCOPED_TOKEN,
                None,
            ),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{action} against an out-of-scope head must be 403"
        );
        assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    }

    assert!(
        h.commands.try_drain().is_empty(),
        "a cross-output recall never reaches the engine"
    );
}

#[tokio::test]
async fn output_scoped_role_permits_head_inside_its_allowlist() {
    // The same output-scoped operator may arm/take/cancel its OWN head (`wall-1`)
    // — the scope confines, it does not block in-scope operations.
    let mut h = harness();
    seed_salvo(&h, "wide");

    for action in ["arm", "take", "cancel"] {
        let resp = send(
            &h.router,
            post_if_match(
                &format!("/api/v1/salvos/wide/{action}?head=wall-1"),
                OUTPUT_SCOPED_TOKEN,
                None,
            ),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "{action} against the in-scope head must be accepted"
        );
    }

    let drained = h.commands.try_drain();
    assert_eq!(
        drained.len(),
        3,
        "all three in-scope recalls reach the engine"
    );
    for cmd in &drained {
        let head = match cmd {
            multiview_control::Command::ArmSalvo { head, .. }
            | multiview_control::Command::TakeSalvo { head, .. }
            | multiview_control::Command::CancelSalvo { head, .. } => head.as_deref(),
            other => panic!("expected a salvo command, got {other:?}"),
        };
        assert_eq!(head, Some("wall-1"));
    }
}

#[tokio::test]
async fn cancel_returns_202_and_carries_the_salvo() {
    let mut h = harness();
    seed_salvo(&h, "wide");
    let resp = send(
        &h.router,
        post_if_match("/api/v1/salvos/wide/cancel", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await["kind"], "cancel_salvo");
    let drained = h.commands.try_drain();
    match &drained[0] {
        multiview_control::Command::CancelSalvo { salvo, .. } => {
            assert_eq!(salvo.as_deref(), Some("wide"));
        }
        other => panic!("expected CancelSalvo, got {other:?}"),
    }
}
