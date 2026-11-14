//! #307 review M1: with a switcher M/E declared, cell edits that would change
//! neither Program (the M/E bus) nor the stored layout outputs show are
//! refused on the control plane with a typed 422 `switcher-owns-program`:
//! `POST /commands/swap`, a video→cell `POST /routing/video/take`, and a salvo
//! take/fire that recalls a layout or cell sources. Authorization still runs
//! first (no existence leak), the refusal reserves no Idempotency-Key and
//! sends nothing, and without a switcher every route is unchanged. A salvo of
//! only tally/UMD recalls keeps working under a switcher.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::Salvo;
use multiview_control::AppState;
use serde_json::json;
use support::{body_json, harness_customized, send, Harness, OPERATOR_TOKEN, SCOPED_TOKEN};

/// A harness whose base document declares (or not) a switcher M/E, plus the
/// router's state so a test can read the idempotency store.
fn rig(switcher: bool) -> (Harness, AppState) {
    let captured: Arc<Mutex<Option<AppState>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&captured);
    let document = if switcher {
        json!({"switcher": {"mix_effects": [{
            "id": "me1", "program": "main",
            "program_source": "cam-a", "preview_source": "cam-b"
        }]}})
    } else {
        json!({})
    };
    let h = harness_customized(16, move |state| {
        let state = state.with_base_document(document);
        *slot.lock().unwrap() = Some(state.clone());
        state
    });
    let state = captured.lock().unwrap().take().unwrap();
    for (id, body) in [
        (
            "recall",
            json!({"id": "recall", "sources": [{"cell": "c0", "input_id": "cam-b"}]}),
        ),
        ("relayout", json!({"id": "relayout", "layout": "grid-9"})),
        (
            "lamps",
            json!({"id": "lamps", "tally": [{"cell": "c0", "color": "Red"}]}),
        ),
    ] {
        let salvo: Salvo = serde_json::from_value(body).unwrap();
        assert_eq!(salvo.id, id);
        h.salvos.create(salvo).unwrap();
    }
    (h, state)
}

fn post(path: &str, token: &str, key: Option<&str>, body: &serde_json::Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        builder = builder.header("Idempotency-Key", key);
    }
    builder
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

/// The refused cell edits: (path, body).
fn cell_edits() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "/api/v1/commands/swap",
            json!({"tile": "c0", "source": "cam-b"}),
        ),
        (
            "/api/v1/routing/video/take",
            json!({
                "target": {"kind": "video_cell", "cell": "c0"},
                "source": {"input_id": "cam-b", "kind": {"kind": "video"}, "selector": {"by": "best"}}
            }),
        ),
        ("/api/v1/salvos/recall/take", json!({})),
        ("/api/v1/salvos/relayout/take", json!({})),
        ("/api/v1/salvos/recall/fire", json!({})),
    ]
}

#[tokio::test]
async fn cell_edits_under_a_switcher_are_switcher_owns_program_and_reserve_no_key() {
    let (mut h, state) = rig(true);
    for (path, body) in cell_edits() {
        let reserved = state.idempotency.usage().0;
        let resp = send(&h.router, post(path, OPERATOR_TOKEN, Some("k"), &body)).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        let problem = body_json(resp).await;
        assert_eq!(
            problem["type"], "/problems/switcher-owns-program",
            "{path}: {problem}"
        );
        let detail = problem["detail"].as_str().unwrap();
        assert!(detail.contains("Program is the M/E bus"), "{detail}");
        assert!(detail.contains("PUT /api/v1/layouts/{id}"), "{detail}");
        assert!(detail.contains("select_preview"), "{detail}");
        assert_eq!(
            state.idempotency.usage().0,
            reserved,
            "{path}: the refusal burns no Idempotency-Key"
        );
        assert_eq!(h.commands.try_drain().len(), 0, "{path}: nothing sent");
    }
}

#[tokio::test]
async fn authorization_precedes_the_switcher_refusal() {
    let (mut h, _state) = rig(true);
    // A scoped principal outside c0/cam-b/the salvo: 403, never the 422 that
    // would confirm what the request addressed.
    for (path, body) in cell_edits() {
        let resp = send(&h.router, post(path, SCOPED_TOKEN, Some("s"), &body)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(h.commands.try_drain().len(), 0, "nothing sent");
}

#[tokio::test]
async fn a_tally_only_salvo_and_audio_takes_still_work_under_a_switcher() {
    let (mut h, _state) = rig(true);
    let resp = send(
        &h.router,
        post(
            "/api/v1/salvos/lamps/take",
            OPERATOR_TOKEN,
            Some("t"),
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let resp = send(
        &h.router,
        post(
            "/api/v1/salvos/lamps/fire",
            OPERATOR_TOKEN,
            None,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    // Arming a cell-recalling salvo is staging only; its take is refused.
    let resp = send(
        &h.router,
        post(
            "/api/v1/salvos/recall/arm",
            OPERATOR_TOKEN,
            Some("a"),
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(h.commands.try_drain().len(), 3);
}

#[tokio::test]
async fn without_a_switcher_cell_edits_are_unchanged() {
    let (mut h, _state) = rig(false);
    for (path, body) in cell_edits() {
        let resp = send(&h.router, post(path, OPERATOR_TOKEN, Some("k"), &body)).await;
        assert!(
            resp.status().is_success(),
            "{path}: {} {}",
            resp.status(),
            body_json(resp).await
        );
    }
    assert_eq!(h.commands.try_drain().len(), cell_edits().len());
}
