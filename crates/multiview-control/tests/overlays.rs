//! End-to-end tests for the overlays resource: CRUD, `ETag` round-trip,
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

/// BOLA ROW enumeration (OWASP API1, ADR-W005/ADR-W025): an overlay is
/// object-scoped by its OWN id (`get_overlay` 403s an out-of-scope id), so
/// `list_overlays` MUST filter ROWS to the principal's allowlist — by the same
/// parity as `list_devices`. `SCOPED_TOKEN` is scoped to `["scoped-layout"]`.
#[tokio::test]
async fn list_filters_overlay_rows_to_the_scoped_allowlist() {
    let h = harness();
    for id in ["scoped-layout", "other-overlay"] {
        let resp = send(
            &h.router,
            post_json(
                &format!("/api/v1/overlays/{id}"),
                ADMIN_TOKEN,
                &json!({
                    "name": id,
                    "body": { "id": id, "kind": "clock", "target": "canvas", "z": 10 }
                }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let resp = send(&h.router, get("/api/v1/overlays", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ids: Vec<String> = body_json(resp)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout".to_owned()],
        "a scoped principal must see ONLY its allowlisted overlay rows (BOLA)"
    );

    let resp = send(&h.router, get("/api/v1/overlays", ADMIN_TOKEN)).await;
    assert_eq!(body_json(resp).await.as_array().unwrap().len(), 2);
}

fn overlay_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "body": { "id": "clk", "kind": "clock", "target": "canvas", "z": 10 }
    })
}

#[tokio::test]
async fn create_then_get_round_trips_with_etag() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
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
    assert_eq!(created["id"], "clk");
    assert_eq!(created["name"], "Clock");
    assert_eq!(created["body"]["kind"], "clock");

    let resp = send(&h.router, get("/api/v1/overlays/clk", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let fetched = body_json(resp).await;
    assert_eq!(fetched["name"], "Clock");
}

#[tokio::test]
async fn get_unknown_overlay_is_404_problem_json() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/overlays/missing", OPERATOR_TOKEN)).await;
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
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &overlay_body("Renamed"),
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
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &overlay_body("V2"),
        ),
    )
    .await;

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &overlay_body("Clobber"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 412);
    assert_eq!(problem["type"], "/problems/version-conflict");

    let resp = send(&h.router, get("/api/v1/overlays/clk", OPERATOR_TOKEN)).await;
    let current = body_json(resp).await;
    assert_eq!(current["name"], "V2", "the clobbering write was rejected");
}

#[tokio::test]
async fn update_without_if_match_is_precondition_required() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &overlay_body("Clock"),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            None,
            &overlay_body("X"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn list_returns_created_overlays_sorted() {
    let h = harness();
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/bbb",
            OPERATOR_TOKEN,
            &json!({ "name": "B", "body": { "kind": "clock", "target": "canvas" } }),
        ),
    )
    .await;
    send(
        &h.router,
        post_json(
            "/api/v1/overlays/aaa",
            OPERATOR_TOKEN,
            &json!({ "name": "A", "body": { "kind": "clock", "target": "canvas" } }),
        ),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/overlays", OPERATOR_TOKEN)).await;
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
        .uri("/api/v1/overlays")
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
        post_json("/api/v1/overlays/clk", VIEWER_TOKEN, &overlay_body("Clock")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_requires_admin_role() {
    let h = harness();
    send(
        &h.router,
        post_json("/api/v1/overlays/clk", ADMIN_TOKEN, &overlay_body("Clock")),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/overlays/clk")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/v1/overlays/clk")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, "W/\"1\"")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(&h.router, get("/api/v1/overlays/clk", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// ADR-W022: live overlay apply — per-kind/per-build `X-Multiview-Apply` truth
// + the UpsertOverlay/RemoveOverlay commands the routes enqueue for the
// engine's frame-boundary drain. Mirrors the ADR-W018 section in
// `typed_resources.rs`.
// ---------------------------------------------------------------------------

const APPLY_HEADER: &str = "x-multiview-apply";

/// The apply header on `resp`, as a string.
fn apply_header(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.headers()
        .get(APPLY_HEADER)
        .expect("mutation declares apply semantics")
        .to_str()
        .expect("ascii header")
        .to_owned()
}

/// The overlay live-apply capability the `ffmpeg`+`overlay` run path injects
/// (ADR-W022 §3): the running renderer draws a `clock` document iff its `face`
/// param is `analog`.
fn analog_clock_caps() -> multiview_control::LiveApplyCaps {
    multiview_control::LiveApplyCaps::default().with_overlays(
        multiview_control::OverlayLiveCapability::new(|o: &multiview_config::Overlay| {
            o.kind == "clock"
                && o.params
                    .get("face")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|f| f.eq_ignore_ascii_case("analog"))
        }),
    )
}

/// A harness whose `AppState` carries the analog-clock live capability.
fn live_harness() -> support::Harness {
    support::harness_with(|state| state.with_live_apply(analog_clock_caps()))
}

fn analog_clock_body(id: &str) -> serde_json::Value {
    json!({
        "name": "Wall clock",
        "body": {
            "id": id, "kind": "clock", "target": "canvas", "z": 10,
            "face": "analog", "x": 200, "y": 120, "radius": 40
        }
    })
}

#[tokio::test]
async fn overlay_mutation_without_capability_stays_restart_and_enqueues_nothing() {
    // The default harness injects NO live-apply capability (the software run
    // path / an overlay-less build): every overlay mutation is honestly
    // `restart` and nothing rides the command bus.
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &analog_clock_body("clk"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        apply_header(&resp),
        "restart",
        "no live overlay seam ⇒ restart (ADR-W022 capability honesty)"
    );
    assert!(
        h.commands.try_drain().is_empty(),
        "without a capability no overlay command may be enqueued"
    );
}

#[tokio::test]
async fn analog_clock_create_applies_live_and_enqueues_upsert() {
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &analog_clock_body("clk"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        apply_header(&resp),
        "live",
        "an analog clock overlay applies LIVE on a rendering build (ADR-W022)"
    );
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::UpsertOverlay { overlay, .. }
                if overlay.id == "clk" && overlay.kind == "clock"
        )),
        "POST of a rendering overlay must enqueue UpsertOverlay, got {drained:?}"
    );
}

#[tokio::test]
async fn analog_clock_update_applies_live_and_enqueues_upsert() {
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &analog_clock_body("clk"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "name": "Wall clock",
                "body": {
                    "id": "clk", "kind": "clock", "target": "canvas", "z": 10,
                    "face": "analog", "x": 64, "y": 64, "radius": 24
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(apply_header(&resp), "live", "an edit applies LIVE");
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::UpsertOverlay { overlay, .. }
                if overlay.id == "clk"
                    && overlay.params.get("x") == Some(&json!(64))
        )),
        "PUT must enqueue UpsertOverlay carrying the NEW params, got {drained:?}"
    );
}

#[tokio::test]
async fn non_rendering_kind_stays_restart_but_mirrors_to_the_engine() {
    // A `label` overlay has no renderer in any current build: the header is
    // honestly `restart`, but the document still rides the bus so the engine's
    // working-set mirror stays coherent (the drain warns, never lies).
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/lbl",
            OPERATOR_TOKEN,
            &json!({
                "name": "Label",
                "body": { "id": "lbl", "kind": "label", "target": "cell_a", "text": "CAM 1" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        apply_header(&resp),
        "restart",
        "a kind with no renderer must NOT claim live (ADR-W022 truth table)"
    );
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::UpsertOverlay { overlay, .. } if overlay.id == "lbl"
        )),
        "the non-rendering document still mirrors to the engine, got {drained:?}"
    );
}

#[tokio::test]
async fn digital_clock_stays_restart() {
    // A digital-faced clock document does not change the picture (the digital
    // readout is independent always-on chrome): header `restart`, never a lie.
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/dclk",
            OPERATOR_TOKEN,
            &json!({
                "name": "Digital",
                "body": { "id": "dclk", "kind": "clock", "target": "canvas", "face": "digital" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(apply_header(&resp), "restart");
    let _ = h.commands.try_drain();
}

#[tokio::test]
async fn analog_clock_delete_applies_live_and_enqueues_remove() {
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &analog_clock_body("clk"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    let resp = send(
        &h.router,
        support::delete_if_match("/api/v1/overlays/clk", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        apply_header(&resp),
        "live",
        "deleting a rendering overlay applies LIVE (the face disappears)"
    );
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::RemoveOverlay { id, .. } if id == "clk"
        )),
        "DELETE must enqueue RemoveOverlay, got {drained:?}"
    );
}

#[tokio::test]
async fn non_rendering_delete_stays_restart_but_enqueues_remove() {
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/lbl",
            OPERATOR_TOKEN,
            &json!({
                "name": "Label",
                "body": { "id": "lbl", "kind": "label", "target": "cell_a" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    let resp = send(
        &h.router,
        support::delete_if_match("/api/v1/overlays/lbl", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        apply_header(&resp),
        "restart",
        "removing a never-rendered kind changes no pixels ⇒ restart"
    );
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::RemoveOverlay { id, .. } if id == "lbl"
        )),
        "the removal still mirrors to the engine, got {drained:?}"
    );
}

#[tokio::test]
async fn full_bus_degrades_the_header_to_restart() {
    // A shed submit (bounded bus at capacity — inv #10) degrades the header to
    // `restart`: the stored document remains the durable truth.
    let mut h = support::harness_customized(1, |state| state.with_live_apply(analog_clock_caps()));
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk1",
            OPERATOR_TOKEN,
            &analog_clock_body("clk1"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(apply_header(&resp), "live", "first submit fits the bus");

    // The bus (capacity 1) is now full; the next mutation is shed → restart.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk2",
            OPERATOR_TOKEN,
            &analog_clock_body("clk2"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        apply_header(&resp),
        "restart",
        "a shed submit must degrade the header honestly (inv #10)"
    );
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "only the first command rode the bus");
}

#[tokio::test]
async fn analog_to_digital_edit_applies_live_because_the_face_vanishes() {
    // MAJOR-1(b): editing a RENDERED analog clock to a digital face is itself
    // a live-visible change — the face disappears at the next frame — so the
    // header must say `live` (live iff submitted ∧ (renders(new) ∨
    // renders(previous))), and the new document still rides the bus.
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            &analog_clock_body("clk"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/clk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "name": "Wall clock",
                "body": { "id": "clk", "kind": "clock", "target": "canvas", "face": "digital" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        apply_header(&resp),
        "live",
        "removing a rendered face via an edit IS a live-visible change (ADR-W022)"
    );
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::UpsertOverlay { overlay, .. }
                if overlay.id == "clk"
                    && overlay.params.get("face") == Some(&json!("digital"))
        )),
        "the digital-face edit still rides the bus, got {drained:?}"
    );
}

#[tokio::test]
async fn never_rendered_edit_stays_restart() {
    // The ∨ in the PUT condition must not over-promise: an edit where NEITHER
    // the previous nor the new document renders (digital → digital) changes
    // no pixels and stays `restart`.
    let mut h = live_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/dclk",
            OPERATOR_TOKEN,
            &json!({
                "name": "Digital",
                "body": { "id": "dclk", "kind": "clock", "target": "canvas", "face": "digital" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/overlays/dclk",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "name": "Digital",
                "body": { "id": "dclk", "kind": "clock", "target": "canvas", "face": "digital", "z": 7 }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        apply_header(&resp),
        "restart",
        "an edit rendering neither before nor after must stay restart"
    );
    let _ = h.commands.try_drain();
}
