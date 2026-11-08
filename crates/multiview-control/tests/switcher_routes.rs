#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_control::routes::switcher::{authorize_refs, SwitcherRequest};
use multiview_control::{Principal, Role};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{ScenePair, SwitcherControl};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
fn primed_stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["a", "b"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
            store.publish(
                Nv12Image::solid(
                    64,
                    64,
                    128,
                    128,
                    128,
                    ColorInfo::default().resolve_defaults(64, 64),
                )
                .unwrap(),
                MediaTime::ZERO,
            );
            (id.to_owned(), store)
        })
        .collect()
}
use support::{body_json, harness, harness_with, post_json, send, OPERATOR_TOKEN};

fn request(action: &str, source: Option<&str>) -> serde_json::Value {
    json!({ "me": "me1", "program": "main", "action": action, "source": source })
}
fn scene(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: id.into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            source: Some(id.into()),
            ..Cell::default()
        }],
    })
}
fn runtime() -> (
    SwitcherControl,
    multiview_engine::switcher_control::SwitcherTickDriver,
) {
    let (handle, driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    // Only a real frame publisher may make the REST capability live. Tests
    // emulating its tick retain the driver so the completion can be verified.
    let mut driver = driver;
    driver.set_prime_stores(primed_stores());
    driver.publish_provider_status(multiview_engine::switcher_control::ProviderStatus {
        scene_version: 1,
        last_tick: 0,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    (handle, driver)
}
fn post_with_key(path: &str, token: &str, body: serde_json::Value, key: &str) -> Request<Body> {
    let mut request = post_json(path, token, &body);
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    request
}

#[tokio::test]
async fn unwired_routes_never_report_applied_or_accept_a_command() {
    let h = harness();
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "cut-unwired",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem = body_json(response).await;
    assert!(problem["detail"]
        .as_str()
        .unwrap()
        .contains("no real Program/Preview"));
}

#[tokio::test]
async fn real_handle_returns_202_then_tick_exact_correlated_completion() {
    let (handle, mut driver) = runtime();
    let mut events = handle.outcomes();
    let h = harness_with(|state| state.with_switcher(handle));
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "cut-one",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = body_json(response).await;
    let op = accepted["operation_id"].as_str().unwrap();
    assert!(!accepted.to_string().contains("applied"));
    assert!(
        events.try_recv().is_err(),
        "202 is not a claim of on-air completion"
    );
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "cut-one",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(response).await["operation_id"], op);
    driver.drain_at_tick(Tick {
        index: 9,
        pts: MediaTime::ZERO,
    });
    let outcome = events.try_recv().unwrap();
    assert_eq!(outcome.event.operation_id, op);
    assert_eq!(outcome.event.tick, 9);
    assert!(outcome.event.completed);
    assert!(
        events.try_recv().is_err(),
        "idempotent replay never queues a second cut"
    );
}

#[tokio::test]
async fn reference_bola_checks_me_source_layout_output_and_program_before_enqueue() {
    let (handle, _driver) = runtime();
    let mut cases = Vec::new();
    cases.push((
        vec!["me1", "a", "layout-a"],
        vec!["program:main", "out-a"],
        "me2",
        None,
        None,
        None,
    ));
    cases.push((
        vec!["me1", "layout-a"],
        vec!["program:main", "out-a"],
        "me1",
        Some("a"),
        None,
        None,
    ));
    cases.push((
        vec!["me1", "a"],
        vec!["program:main", "out-a"],
        "me1",
        Some("a"),
        Some("layout-a"),
        None,
    ));
    cases.push((
        vec!["me1", "a", "layout-a"],
        vec!["program:main"],
        "me1",
        Some("a"),
        Some("layout-a"),
        Some("out-a"),
    ));
    cases.push((
        vec!["me1", "a", "layout-a"],
        vec!["out-a"],
        "me1",
        Some("a"),
        Some("layout-a"),
        Some("out-a"),
    ));
    for (objects, outputs, me, source, layout, output) in cases {
        let principal = Principal {
            key_id: "case".into(),
            role: Role::Operator,
            scoped_object_ids: Some(objects.into_iter().map(str::to_owned).collect()),
            scoped_output_ids: Some(outputs.into_iter().map(str::to_owned).collect()),
            scoped_discovery_domains: None,
        };
        let body = json!({"me":me,"program":"main","action":"select_preview","source":source,"layout":layout,"output":output});
        // The route's BOLA function runs before unsupported-slice validation.
        let req: SwitcherRequest = serde_json::from_value(body).unwrap();
        assert!(matches!(
            authorize_refs(&principal, &req),
            Err(multiview_control::ControlError::Forbidden(_))
        ));
    }
    drop(handle);
}
