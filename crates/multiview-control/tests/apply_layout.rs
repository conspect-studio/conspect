//! Tests for the `POST /api/v1/commands/apply-layout` command route (CTL-4 /
//! ADR-W019): the route resolves + solves the STORED layout body at request
//! time (off the engine hot path) and only then returns `202 Accepted` + an
//! operation id, with the solved document riding the command; an unknown id or
//! a body that does not parse/solve is an honest `422` BEFORE any `202`. Also:
//! write-role enforcement, the bounded command bus shedding to `503` under
//! saturation, and the route appearing in the served `OpenAPI` document.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{body_json, harness, harness_with, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

/// A valid stored GRID layout body (`{canvas, layout, cells}` — the seeded
/// working-layout shape): a 320x240 25 fps canvas, two grid areas, two cells.
fn grid_body() -> serde_json::Value {
    json!({
        "canvas": { "width": 320, "height": 240, "fps": "25/1" },
        "layout": {
            "kind": "grid",
            "columns": ["1fr", "1fr"],
            "rows": ["1fr"],
            "areas": ["a b"]
        },
        "cells": [
            { "id": "cell_a", "area": "a", "source": { "input_id": "in_a" } },
            { "id": "cell_b", "area": "b", "source": {} }
        ]
    })
}

/// A valid stored ABSOLUTE layout body — the minimal shape the `WebUI` layout
/// editor saves (canvas `width`/`height`/`fps` only, per-cell `rect`).
fn absolute_body() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "canvas": { "width": 320, "height": 240, "fps": "25/1" },
        "layout": { "kind": "absolute" },
        "cells": [
            {
                "id": "full",
                "label": "Full frame",
                "rect": { "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0 },
                "z": 0,
                "rotation": 0,
                "source": { "input_id": "in_a" }
            }
        ]
    })
}

/// Store a layout body under `id` through the public CRUD route.
async fn create_layout(h: &support::Harness, id: &str, body: &serde_json::Value) {
    let resp = send(
        &h.router,
        post_json(
            &format!("/api/v1/layouts/{id}"),
            OPERATOR_TOKEN,
            &json!({ "name": id, "body": body }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "stored layout {id}");
}

#[tokio::test]
async fn apply_layout_returns_202_with_op_id() {
    let mut h = apply_harness();
    create_layout(&h, "grid-3x3", &grid_body()).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "grid-3x3" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let op = body["operation_id"].as_str().expect("operation_id present");
    assert!(!op.is_empty(), "a non-empty operation id is returned");
    assert_eq!(body["kind"], "apply_layout");

    // The engine drains the command at its leisure (non-blocking): exactly one
    // ApplyLayout reached the engine, carrying the requested layout id and the
    // same correlation id the client received.
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "exactly one command reached the engine");
    match &drained[0] {
        multiview_control::Command::ApplyLayout {
            op: drained_op,
            layout,
            ..
        } => {
            assert_eq!(layout, "grid-3x3");
            assert_eq!(
                drained_op.as_str(),
                op,
                "the engine sees the same correlation id the client got"
            );
        }
        other => panic!("expected ApplyLayout, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_layout_unknown_id_is_422_before_202() {
    // ADR-W019: the stored layout is resolved AT THE ROUTE; an id that does not
    // exist in the layouts repository is an honest 422 problem — never a 202
    // whose command the engine then silently ignores.
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "no-such-layout" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown stored-layout id must fail before any 202"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("no-such-layout"),
        "the problem names the unknown id, got {detail:?}"
    );
    // Nothing reached the engine.
    assert!(
        h.commands.try_drain().is_empty(),
        "a refused apply must enqueue no command"
    );
}

#[tokio::test]
async fn apply_layout_unsolvable_body_is_422_before_202() {
    // A stored body that PARSES but does not SOLVE (a cell referencing an
    // unknown grid area) is refused at the route — fail before 202.
    let mut h = harness();
    create_layout(
        &h,
        "bad-grid",
        &json!({
            "canvas": { "width": 320, "height": 240, "fps": "25/1" },
            "layout": { "kind": "grid", "columns": ["1fr"], "rows": ["1fr"], "areas": ["a"] },
            "cells": [ { "id": "x", "area": "nope", "source": {} } ]
        }),
    )
    .await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "bad-grid" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unsolvable stored body must fail before any 202"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    assert!(
        h.commands.try_drain().is_empty(),
        "a refused apply must enqueue no command"
    );
}

#[tokio::test]
async fn apply_layout_unparseable_body_is_422_before_202() {
    // A stored body that does not even PARSE as `{canvas, layout, cells}`
    // (canvas width is a string) is refused at the route.
    let mut h = harness();
    create_layout(
        &h,
        "garbage",
        &json!({ "canvas": { "width": "wide" }, "cells": [] }),
    )
    .await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "garbage" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(h.commands.try_drain().is_empty());
}

#[tokio::test]
async fn apply_layout_202_carries_the_solved_document_and_apply_classes() {
    // ADR-W019: the command ships the document SOLVED at the route (the
    // frame-boundary drain only swaps), and the 202 body states which per-cell
    // property classes apply live vs are carried-but-not-yet-rendered.
    let mut h = apply_harness();
    create_layout(&h, "wall-a", &absolute_body()).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "wall-a" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let live = body["applied_live"]
        .as_array()
        .expect("202 body lists the property classes applied live");
    let live: Vec<&str> = live.iter().filter_map(|v| v.as_str()).collect();
    for class in ["geometry", "bindings", "z_order", "opacity", "on_loss"] {
        assert!(live.contains(&class), "applied_live includes {class:?}");
    }
    let carried = body["carried_only"]
        .as_array()
        .expect("202 body lists the carried-but-not-rendered classes");
    let carried: Vec<&str> = carried.iter().filter_map(|v| v.as_str()).collect();
    for class in ["border", "qos"] {
        assert!(carried.contains(&class), "carried_only includes {class:?}");
    }

    // The drained command carries the layout SOLVED at the route (ADR-W019):
    // the frame-boundary drain only swaps — no repository read, no re-solve.
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1);
    match &drained[0] {
        multiview_control::Command::ApplyLayout {
            layout, document, ..
        } => {
            assert_eq!(layout, "wall-a");
            let resolved = document
                .as_deref()
                .expect("the command carries the resolved stored layout");
            assert_eq!(resolved.solved.name, "wall-a");
            assert_eq!(resolved.solved.cells.len(), 1);
            assert_eq!(
                resolved.solved.cells[0].source.as_deref(),
                Some("in_a"),
                "the solved cells carry their source bindings"
            );
        }
        other => panic!("expected ApplyLayout, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_layout_canvas_mismatch_is_422_class2() {
    // ADR-R004 / ADR-W019: output geometry + cadence are PINNED for the session.
    // A stored layout authored for a different canvas is a Class-2 change and is
    // refused live (422 naming the mismatch), never silently held.
    // The running session's pinned canvas: 320x240@25 (the immutable snapshot
    // the gate compares against — ADR-W019 MAJOR-1).
    let mut h = apply_harness();
    // A stored layout authored for a DIFFERENT canvas (1920x1080@30).
    let mut other = absolute_body();
    other["canvas"] = json!({ "width": 1920, "height": 1080, "fps": "30/1" });
    create_layout(&h, "hd-wall", &other).await;

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "hd-wall" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a pinned-canvas mismatch is a Class-2 change and must be refused live"
    );
    let problem = body_json(resp).await;
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("canvas") || detail.contains("Class-2"),
        "the problem explains the pinned-canvas refusal, got {detail:?}"
    );
    assert!(h.commands.try_drain().is_empty());
}

/// The running session's pinned canvas used across these tests (matches
/// `grid_body`/`absolute_body`): 320x240 @ 25/1.
fn running_canvas_320x240_25() -> multiview_config::LayoutCanvas {
    multiview_config::LayoutCanvas::new(320, 240, "25/1".parse().expect("fps parses"))
}

/// A harness carrying the immutable pinned-canvas snapshot (as `multiview run`
/// seeds it), so document-carrying applies pass the fail-closed Class-1 gate.
fn apply_harness() -> support::Harness {
    harness_with(|state| state.with_running_canvas(running_canvas_320x240_25()))
}

/// Build a `POST` request with a Bearer token, JSON body, and an
/// `Idempotency-Key` header.
fn post_json_idem(
    path: &str,
    token: &str,
    key: &str,
    body: &serde_json::Value,
) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", key)
        .body(axum::body::Body::from(
            serde_json::to_vec(body).expect("serialize body"),
        ))
        .expect("request should build")
}

#[tokio::test]
async fn apply_layout_canvas_gate_survives_working_layout_rewrite() {
    // MAJOR-1 (ADR-W019 review): the Class-1 canvas gate must compare against
    // an IMMUTABLE pinned-canvas snapshot captured at seed time — NOT the
    // mutable layouts repository. Rewriting the working layout's body (a plain
    // PUT any operator can do) must not smuggle a canvas-mismatched apply past
    // the gate into a silent post-202 drain hold.
    let mut h = harness_with(|state| {
        state
            .repository
            .create_layout(
                "schema_v1",
                multiview_control::LayoutInput {
                    name: "schema_v1".to_owned(),
                    body: grid_body(),
                },
            )
            .expect("seed working layout");
        state
            .with_working_layout_id("schema_v1")
            .with_running_canvas(running_canvas_320x240_25())
    });

    // Rewrite the WORKING layout's stored body to claim an HD canvas.
    let current = send(
        &h.router,
        support::get("/api/v1/layouts/schema_v1", OPERATOR_TOKEN),
    )
    .await;
    let tag = support::etag(&current).expect("working layout etag");
    let mut hd = grid_body();
    hd["canvas"] = json!({ "width": 1920, "height": 1080, "fps": "30/1" });
    let rewrite = send(
        &h.router,
        support::put_json(
            "/api/v1/layouts/schema_v1",
            OPERATOR_TOKEN,
            Some(&tag),
            &json!({ "name": "schema_v1", "body": hd }),
        ),
    )
    .await;
    assert_eq!(rewrite.status(), StatusCode::OK, "working layout rewritten");

    // A stored layout matching the REWRITTEN body (but not the running canvas)
    // must still be refused: the pinned canvas is 320x240@25, immutably.
    let mut other = absolute_body();
    other["canvas"] = json!({ "width": 1920, "height": 1080, "fps": "30/1" });
    create_layout(&h, "hd-wall", &other).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "hd-wall" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "rewriting the working layout must not bypass the pinned-canvas gate"
    );
    assert!(h.commands.try_drain().is_empty());
}

#[tokio::test]
async fn apply_layout_without_a_running_canvas_fails_closed() {
    // MAJOR-1 (ADR-W019 review): when the control plane holds NO pinned-canvas
    // snapshot, a document-carrying apply must fail CLOSED (422 naming the
    // unknown running canvas) — never 202 into a silent drain hold.
    let mut h = harness();
    create_layout(&h, "wall-a", &absolute_body()).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "wall-a" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "no pinned-canvas snapshot ⇒ the Class-1 gate fails closed"
    );
    let problem = body_json(resp).await;
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("running canvas"),
        "the problem names the unknown running canvas, got {detail:?}"
    );
    assert!(h.commands.try_drain().is_empty());
}

#[tokio::test]
async fn apply_layout_equivalent_cadence_is_class1() {
    // MINOR-3 (ADR-W019 review): cadence equality is by VALUE (cross-
    // multiplied), not structural num/den — a stored "50/2" against a running
    // "25/1" is the SAME cadence and must apply live, not refuse as Class-2.
    // (Seeds the working layout too, so the gate is active under both the old
    // repository-read implementation and the immutable-snapshot one.)
    let mut h = harness_with(|state| {
        state
            .repository
            .create_layout(
                "schema_v1",
                multiview_control::LayoutInput {
                    name: "schema_v1".to_owned(),
                    body: grid_body(),
                },
            )
            .expect("seed working layout");
        state
            .with_working_layout_id("schema_v1")
            .with_running_canvas(running_canvas_320x240_25())
    });
    let mut body = absolute_body();
    body["canvas"] = json!({ "width": 320, "height": 240, "fps": "50/2" });
    create_layout(&h, "fifty-over-two", &body).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "fifty-over-two" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "an equivalent (non-reduced) cadence is Class-1, not a canvas mismatch"
    );
    assert_eq!(h.commands.try_drain().len(), 1);
}

#[tokio::test]
async fn apply_layout_replay_returns_original_op_without_re_resolving() {
    // MINOR-4 (ADR-W019, pinned semantics): the idempotency reservation
    // happens BEFORE resolution. A retried key answers from the reservation —
    // the original operation id, kind "replay", no applied_live/carried_only —
    // WITHOUT re-resolving the layout, even if the layout has since been
    // deleted (the original command was already enqueued; the retry asks
    // "did it land?", and the honest answer is yes).
    let mut h = apply_harness();
    create_layout(&h, "wall-a", &absolute_body()).await;
    let key = "apply-key-1";

    let resp1 = send(
        &h.router,
        post_json_idem(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            key,
            &json!({ "layout": "wall-a" }),
        ),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);
    let body1 = body_json(resp1).await;
    let op1 = body1["operation_id"].as_str().expect("op id").to_owned();
    assert_eq!(h.commands.try_drain().len(), 1, "enqueued exactly once");

    // Delete the layout out from under the reservation (admin + If-Match).
    let current = send(
        &h.router,
        support::get("/api/v1/layouts/wall-a", OPERATOR_TOKEN),
    )
    .await;
    let tag = support::etag(&current).expect("layout etag");
    let deleted = send(
        &h.router,
        support::delete_if_match("/api/v1/layouts/wall-a", support::ADMIN_TOKEN, Some(&tag)),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    // The replay still answers 202 with the ORIGINAL op id and no re-enqueue.
    let resp2 = send(
        &h.router,
        post_json_idem(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            key,
            &json!({ "layout": "wall-a" }),
        ),
    )
    .await;
    assert_eq!(
        resp2.status(),
        StatusCode::ACCEPTED,
        "a replayed key answers from the reservation, never re-resolves"
    );
    let body2 = body_json(resp2).await;
    assert_eq!(body2["kind"], "replay");
    assert_eq!(body2["operation_id"].as_str(), Some(op1.as_str()));
    assert!(
        body2.get("applied_live").is_none(),
        "a replay body carries no applied_live classes"
    );
    assert!(
        h.commands.try_drain().is_empty(),
        "a replay never re-enqueues"
    );
}

#[tokio::test]
async fn apply_layout_resolve_failure_releases_the_idempotency_key() {
    // MINOR-4 guard: a refused apply (422 — the command never reached the
    // engine) must NOT consume the idempotency key; a corrected retry with the
    // same key must actually submit (mirrors the shed-release rule).
    let mut h = apply_harness();
    let key = "apply-key-2";

    let refused = send(
        &h.router,
        post_json_idem(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            key,
            &json!({ "layout": "ghost" }),
        ),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(h.commands.try_drain().is_empty());

    create_layout(&h, "ghost", &absolute_body()).await;
    let retried = send(
        &h.router,
        post_json_idem(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            key,
            &json!({ "layout": "ghost" }),
        ),
    )
    .await;
    assert_eq!(retried.status(), StatusCode::ACCEPTED);
    let body = body_json(retried).await;
    assert_eq!(
        body["kind"], "apply_layout",
        "the corrected retry is a FRESH submit, not a stale replay"
    );
    assert_eq!(h.commands.try_drain().len(), 1);
}

#[tokio::test]
async fn apply_layout_requires_write_role() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            VIEWER_TOKEN,
            &json!({ "layout": "grid-3x3" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a viewer is read-only and cannot apply a layout"
    );
}

#[tokio::test]
async fn apply_layout_sheds_503_when_bus_full() {
    // Capacity 1, and the engine never drains: the first command fills the bus,
    // and a second apply-layout must be shed (503), never block — proving the
    // handler only try_submits and can never back-pressure the engine (inv #10).
    // The layout must exist (resolution happens BEFORE the submit, ADR-W019) so
    // the failure under test is the saturated bus, not a 422.
    let h = support::harness_customized(1, |state| {
        state.with_running_canvas(running_canvas_320x240_25())
    });
    create_layout(&h, "grid-3x3", &grid_body()).await;

    let resp1 = send(
        &h.router,
        post_json("/api/v1/commands/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    let resp2 = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "grid-3x3" }),
        ),
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

#[test]
fn apply_layout_path_is_documented_in_openapi() {
    use multiview_control::openapi::ApiDoc;
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).expect("OpenAPI serializes");
    let paths = &json["paths"];

    let item = paths
        .get("/api/v1/commands/apply-layout")
        .expect("POST /api/v1/commands/apply-layout is in the OpenAPI document");
    let post = item
        .get("post")
        .expect("the apply-layout path documents its POST operation");

    // The accepted (202) outcome is advertised so the generated SPA client knows
    // the command is asynchronous (result arrives on the realtime stream).
    assert!(
        post["responses"].get("202").is_some(),
        "apply-layout documents the 202 Accepted response"
    );

    // It is also enumerated in the static REST-surface list the crate advertises.
    let routes: Vec<&str> = ApiDoc::rest_routes().iter().map(|(_, p)| *p).collect();
    assert!(
        routes.contains(&"/api/v1/commands/apply-layout"),
        "apply-layout route enumerated in rest_routes()"
    );
}
