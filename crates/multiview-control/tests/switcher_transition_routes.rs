//! ADR-0112 §4: selecting the M/E transition over the control API.
//!
//! `set_transition` on `/plan` and `/commands/set_transition`: BOLA before
//! anything else, typed 422s for an unrendered kind or option
//! (`unsupported-transition`) and for a shape or number the machine refuses
//! (`invalid-transition`), refusal during a take, Idempotency-Key replay,
//! 202-means-queued with the engine applying it at a tick, `GET` reporting
//! the engine's selection, and no other action carrying a transition.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]
mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_control::routes::switcher::{authorize_refs, SwitcherRequest};
use multiview_control::{ControlError, Principal, Role};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef, TransitionDescriptor};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherCommand, SwitcherControl, SwitcherTickDriver, SwitcherVerb,
    COMMAND_CAPACITY,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use support::{
    body_json, get, harness_with, post_json, send, Harness, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

fn configured_sources(ids: &[&str]) -> Arc<dyn multiview_control::ResourceRepository> {
    let store = multiview_control::InMemorySourceStore::new();
    for id in ids {
        multiview_control::ResourceRepository::create(
            &store,
            id,
            multiview_control::ResourceInput {
                name: (*id).to_owned(),
                body: json!({ "id": id, "kind": "solid" }),
            },
        )
        .unwrap();
    }
    Arc::new(store)
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

fn runtime() -> (SwitcherControl, SwitcherTickDriver) {
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
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
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    let stores: HashMap<String, Arc<TileStore<Nv12Image>>> = ["a", "b"]
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
        .collect();
    driver.set_prime_stores(stores);
    driver.publish_provider_status(ProviderStatus {
        scene_version: 1,
        last_tick: 0,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    tick(&mut driver, 0);
    (handle, driver)
}

fn tick(driver: &mut SwitcherTickDriver, index: u64) {
    let tick = Tick {
        index,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
}

fn rig() -> (Harness, SwitcherControl, SwitcherTickDriver) {
    let (handle, driver) = runtime();
    let h = harness_with(|state| {
        state
            .with_sources_store(configured_sources(&["a", "b"]))
            .with_switcher(handle.clone())
    });
    (h, handle, driver)
}

fn set(transition: Value) -> Value {
    json!({ "me": "me1", "program": "main", "action": "set_transition", "transition": transition })
}

fn wipe() -> Value {
    json!({
        "kind": "wipe", "pattern": "circle", "rate": 20, "reverse": true,
        "softness": 0.1, "border_width": 0.02, "border_color": "#00ff00"
    })
}

fn command(body: &Value, token: &str, key: &str) -> Request<Body> {
    let mut request = post_json("/api/v1/switcher/commands/set_transition", token, body);
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    request
}

async fn problem_type(response: axum::response::Response) -> String {
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(response).await;
    problem["type"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn set_transition_plans_and_queues_with_202_and_the_engine_applies_it_at_a_tick() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    let plan = send(
        &h.router,
        post_json("/api/v1/switcher/plan", OPERATOR_TOKEN, &set(wipe())),
    )
    .await;
    assert_eq!(plan.status(), StatusCode::OK);
    assert_eq!(body_json(plan).await["can_take"], true);
    let response = send(&h.router, command(&set(wipe()), OPERATOR_TOKEN, "t1")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = body_json(response).await;
    assert_eq!(accepted["kind"], "set_transition");
    let op = accepted["operation_id"].as_str().unwrap().to_owned();
    // 202 is queued, not applied: GET still reports the engine default.
    let state = body_json(send(&h.router, get("/api/v1/switcher/me1", OPERATOR_TOKEN)).await).await;
    assert_eq!(state["transition"], json!({"kind": "cut"}));
    // The idempotent replay returns the same operation and queues nothing.
    let replay = send(&h.router, command(&set(wipe()), OPERATOR_TOKEN, "t1")).await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(replay).await["operation_id"], op.as_str());
    tick(&mut driver, 1);
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(outcome.operation_id, op);
    assert!(outcome.completed && outcome.error.is_none(), "{outcome:?}");
    assert_eq!(outcome.tick, 1);
    assert!(outcomes.try_recv().is_err(), "the replay enqueued nothing");
    let state = body_json(send(&h.router, get("/api/v1/switcher/me1", OPERATOR_TOKEN)).await).await;
    let expected: TransitionDescriptor = serde_json::from_value(wipe()).unwrap();
    let reported: TransitionDescriptor =
        serde_json::from_value(state["transition"].clone()).unwrap();
    assert_eq!(reported, expected);
    assert_eq!(state["transition"]["pattern"], "circle");
    assert_eq!(state["transition"]["reverse"], true);
    // A DIP to a colour with a switch point is admitted the same way.
    let dip = json!({"kind": "dip", "rate": 30, "color": "#ffffff", "switch_point": {"num": 1, "den": 3}});
    let response = send(&h.router, command(&set(dip.clone()), OPERATOR_TOKEN, "t2")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tick(&mut driver, 2);
    let state = body_json(send(&h.router, get("/api/v1/switcher/me1", OPERATOR_TOKEN)).await).await;
    assert_eq!(state["transition"]["kind"], "dip");
    assert_eq!(state["transition"]["color"], "#ffffff");
}

#[tokio::test]
async fn set_transition_is_authorized_before_it_is_parsed_or_queued() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    // No me1 grant: 403 on plan and command, for a valid and an unsupported body.
    for body in [
        set(wipe()),
        set(json!({"kind": "dip", "rate": 10, "dip_source": "secret"})),
    ] {
        let plan = send(
            &h.router,
            post_json("/api/v1/switcher/plan", SCOPED_TOKEN, &body),
        )
        .await;
        assert_eq!(plan.status(), StatusCode::FORBIDDEN);
        let take = send(&h.router, command(&body, SCOPED_TOKEN, "k1")).await;
        assert_eq!(take.status(), StatusCode::FORBIDDEN);
    }
    // A viewer can read but never select.
    let take = send(&h.router, command(&set(wipe()), VIEWER_TOKEN, "k1")).await;
    assert_eq!(take.status(), StatusCode::FORBIDDEN);
    tick(&mut driver, 1);
    assert!(outcomes.try_recv().is_err(), "nothing was queued");
    // A source named inside the descriptor is authorized like any reference.
    let principal = Principal {
        key_id: "case".into(),
        role: Role::Operator,
        scoped_object_ids: Some(vec!["me1".into()]),
        scoped_output_ids: Some(vec!["program:main".into()]),
        scoped_discovery_domains: None,
    };
    for transition in [
        json!({"kind": "dip", "rate": 10, "dip_source": "secret"}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "border_fill_source": "secret"}),
        json!({"kind": "stinger", "player": "secret", "pre_roll": 1, "clip_duration": 30, "trigger_point": 5, "mix_rate": 10}),
    ] {
        let req: SwitcherRequest = serde_json::from_value(set(transition)).unwrap();
        assert!(matches!(
            authorize_refs(&principal, &req),
            Err(ControlError::Forbidden(_))
        ));
    }
    let req: SwitcherRequest = serde_json::from_value(set(wipe())).unwrap();
    authorize_refs(&principal, &req).unwrap();
}

#[tokio::test]
async fn unrendered_kinds_and_options_are_typed_unsupported_transition_422s() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    for (n, transition) in [
        json!({"kind": "stinger", "player": "a", "pre_roll": 1, "clip_duration": 30, "trigger_point": 5, "mix_rate": 10}),
        json!({"kind": "dve_push", "direction": "left", "rate": 10}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "flip_flop": true}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "border_fill_source": "a"}),
        json!({"kind": "dip", "rate": 10, "dip_source": "a"}),
    ]
    .into_iter()
    .enumerate()
    {
        let plan = send(
            &h.router,
            post_json("/api/v1/switcher/plan", OPERATOR_TOKEN, &set(transition.clone())),
        )
        .await;
        assert!(problem_type(plan).await.ends_with("unsupported-transition"));
        let take = send(
            &h.router,
            command(&set(transition), OPERATOR_TOKEN, &format!("u{n}")),
        )
        .await;
        assert!(problem_type(take).await.ends_with("unsupported-transition"));
    }
    tick(&mut driver, 1);
    assert!(outcomes.try_recv().is_err(), "a refusal queues nothing");
}

#[tokio::test]
async fn invalid_shapes_and_numbers_are_typed_invalid_transition_422s() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    for (n, transition) in [
        json!({"kind": "spin", "rate": 10}),
        json!({"kind": "mix"}),
        json!({"kind": "mix", "rate": 0}),
        json!({"kind": "mix", "rate": 1001}),
        json!({"kind": "wipe", "pattern": "star", "rate": 10}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "softness": 1.5}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "border_width": -0.1}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "position_xy": [0.5, 2.0]}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "border_color": "green"}),
        json!({"kind": "dip", "rate": 1}),
        json!({"kind": "dip", "rate": 10, "switch_point": {"num": 1, "den": 1}}),
        json!({"kind": "dip", "rate": 10, "switch_point": {"num": 0, "den": 2}}),
        json!("mix"),
    ]
    .into_iter()
    .enumerate()
    {
        let plan = send(
            &h.router,
            post_json(
                "/api/v1/switcher/plan",
                OPERATOR_TOKEN,
                &set(transition.clone()),
            ),
        )
        .await;
        assert!(
            problem_type(plan).await.ends_with("invalid-transition"),
            "{transition}"
        );
        let take = send(
            &h.router,
            command(&set(transition.clone()), OPERATOR_TOKEN, &format!("i{n}")),
        )
        .await;
        assert!(
            problem_type(take).await.ends_with("invalid-transition"),
            "{transition}"
        );
    }
    // A missing transition, or one beside a source/duration, is malformed.
    for (n, body) in [
        json!({"me": "me1", "program": "main", "action": "set_transition"}),
        json!({"me": "me1", "program": "main", "action": "set_transition", "transition": wipe(), "source": "a"}),
        json!({"me": "me1", "program": "main", "action": "set_transition", "transition": wipe(), "duration_frames": 10}),
        json!({"me": "me1", "program": "main", "action": "set_transition", "transition": wipe(), "input": 1}),
    ]
    .into_iter()
    .enumerate()
    {
        let take = send(&h.router, command(&body, OPERATOR_TOKEN, &format!("m{n}"))).await;
        assert_eq!(take.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    tick(&mut driver, 1);
    assert!(outcomes.try_recv().is_err(), "a refusal queues nothing");
}

/// MED-P3-4: an unknown field, or a field of another variant, is a typed
/// `invalid-transition` 422 on both routes and queues nothing.
#[tokio::test]
async fn unknown_or_foreign_descriptor_fields_are_typed_invalid_transition_422s() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    for (n, transition) in [
        json!({"kind": "mix", "rate": 10, "reverse": true}),
        json!({"kind": "mix", "rate": 10, "colour": "#ffffff"}),
        json!({"kind": "cut", "rate": 10}),
        json!({"kind": "dip", "rate": 10, "pattern": "box"}),
        json!({"kind": "dip", "rate": 10, "switch_point": {"num": 1, "den": 2, "x": 0}}),
        json!({"kind": "wipe", "pattern": "box", "rate": 10, "color": "#ffffff"}),
    ]
    .into_iter()
    .enumerate()
    {
        let plan = send(
            &h.router,
            post_json(
                "/api/v1/switcher/plan",
                OPERATOR_TOKEN,
                &set(transition.clone()),
            ),
        )
        .await;
        assert!(
            problem_type(plan).await.ends_with("invalid-transition"),
            "{transition}"
        );
        let take = send(
            &h.router,
            command(&set(transition.clone()), OPERATOR_TOKEN, &format!("u{n}")),
        )
        .await;
        assert!(
            problem_type(take).await.ends_with("invalid-transition"),
            "{transition}"
        );
    }
    tick(&mut driver, 1);
    assert!(outcomes.try_recv().is_err(), "a refusal queues nothing");
}

#[tokio::test]
async fn every_other_action_carrying_a_transition_is_refused() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    for (n, (action, extra)) in [
        ("cut", json!({})),
        ("auto", json!({"duration_frames": 10})),
        ("select_preview", json!({"source": "a"})),
        ("select_program", json!({"source": "b"})),
    ]
    .into_iter()
    .enumerate()
    {
        let mut body =
            json!({"me": "me1", "program": "main", "action": action, "transition": wipe()});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let plan = send(
            &h.router,
            post_json("/api/v1/switcher/plan", OPERATOR_TOKEN, &body),
        )
        .await;
        assert_eq!(plan.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let mut request = post_json(
            &format!("/api/v1/switcher/commands/{action}"),
            OPERATOR_TOKEN,
            &body,
        );
        request
            .headers_mut()
            .insert("Idempotency-Key", format!("o{n}").parse().unwrap());
        let take = send(&h.router, request).await;
        assert_eq!(take.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let detail = body_json(take).await["detail"].as_str().unwrap().to_owned();
        assert!(detail.contains("only set_transition"), "{detail}");
    }
    tick(&mut driver, 1);
    assert!(outcomes.try_recv().is_err(), "a refusal queues nothing");
}

#[tokio::test]
async fn a_selection_during_a_take_is_refused_and_the_flight_keeps_its_transition() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    let dip = json!({"kind": "dip", "rate": 10});
    assert_eq!(
        send(&h.router, command(&set(dip), OPERATOR_TOKEN, "d"))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    tick(&mut driver, 1);
    handle
        .try_send(SwitcherCommand {
            operation_id: "auto".into(),
            scene_version: 1,
            verb: SwitcherVerb::Auto(FrameCount::try_from(10).unwrap()),
        })
        .unwrap();
    tick(&mut driver, 2);
    assert!(handle.snapshot().unwrap().in_flight);
    for path in [
        "/api/v1/switcher/plan",
        "/api/v1/switcher/commands/set_transition",
    ] {
        let mut request = post_json(path, OPERATOR_TOKEN, &set(wipe()));
        request
            .headers_mut()
            .insert("Idempotency-Key", "during".parse().unwrap());
        let response = send(&h.router, request).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let detail = body_json(response).await["detail"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(detail.contains("active take"), "{detail}");
    }
    tick(&mut driver, 3);
    let snapshot = handle.snapshot().unwrap();
    assert!(matches!(
        snapshot.transition,
        TransitionDescriptor::Dip { .. }
    ));
    let mut ids = Vec::new();
    while let Ok(item) = outcomes.try_recv() {
        ids.push(item.event.operation_id.clone());
    }
    assert!(!ids.iter().any(|id| id == "during"), "{ids:?}");
}

#[tokio::test]
async fn a_full_mailbox_is_engine_busy_and_releases_the_key() {
    let (h, handle, mut driver) = rig();
    for n in 0..COMMAND_CAPACITY {
        handle
            .try_send(SwitcherCommand {
                operation_id: format!("fill-{n}"),
                scene_version: 1,
                verb: SwitcherVerb::Cut,
            })
            .unwrap();
    }
    let response = send(&h.router, command(&set(wipe()), OPERATOR_TOKEN, "busy")).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    // Drain the mailbox (16 a tick); the same key is then fresh, not a replay.
    for index in 1..=8 {
        tick(&mut driver, index);
    }
    let response = send(&h.router, command(&set(wipe()), OPERATOR_TOKEN, "busy")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

const DOC: &str = r##"
schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "absolute"
[[sources]]
id = "a"
kind = "bars"
[[sources]]
id = "b"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "/tmp/transition-routes/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
"##;

fn persisted(state: &multiview_control::AppState) -> Option<TransitionDescriptor> {
    let config: multiview_config::MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(state).unwrap(),
    )
    .unwrap();
    config.switcher.unwrap().mix_effects[0].transition.clone()
}

/// ADR-0112 §4: an applied selection is written to the accepted state (on an
/// M/E without numbered inputs too), never before the engine applied it and
/// never mid-flight; the persisted document validates and reloads with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_selection_is_persisted_after_the_tick_and_survives_a_reload() {
    use multiview_config::{MultiviewConfig, StartMode};
    use multiview_control::{boot_model::BootModel, command_bus, AppState, InMemoryRepository};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiview.toml");
    std::fs::write(&path, DOC).unwrap();
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let (handle, mut driver) = runtime();
    let (sender, _receiver) = command_bus(64);
    let state = AppState::new(
        Arc::new(multiview_engine::EnginePublisher::new(256)),
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.clone(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )))
    .with_switcher(handle.clone());
    let router = multiview_control::router(state.clone());
    let response = send(&router, command(&set(wipe()), OPERATOR_TOKEN, "p1")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(persisted(&state), None, "queued is not applied");
    tick(&mut driver, 1);
    let expected: TransitionDescriptor = serde_json::from_value(wipe()).unwrap();
    for _ in 0..300 {
        if persisted(&state).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(persisted(&state), Some(expected.clone()));
    // An AUTO's flight (its own rate) is not persisted as the selection.
    handle
        .try_send(SwitcherCommand {
            operation_id: "auto".into(),
            scene_version: 1,
            verb: SwitcherVerb::Auto(FrameCount::try_from(3).unwrap()),
        })
        .unwrap();
    tick(&mut driver, 2);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        persisted(&state),
        Some(expected.clone()),
        "never mid-flight"
    );
    // MED-P3-1: the completed AUTO leaves the selection (rate 20), not its
    // own rate 3, as the persisted descriptor.
    for index in 3..=6 {
        tick(&mut driver, index);
    }
    let done = handle.snapshot().unwrap();
    assert!(!done.in_flight);
    assert_eq!(done.completion_count, 1);
    assert_eq!(done.transition, expected);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        persisted(&state),
        Some(expected.clone()),
        "the selection with its own rate, after completion"
    );
    // An AUTO interrupted by a CUT leaves it too.
    for (id, verb) in [
        (
            "auto-2",
            SwitcherVerb::Auto(FrameCount::try_from(3).unwrap()),
        ),
        ("cut", SwitcherVerb::Cut),
    ] {
        handle
            .try_send(SwitcherCommand {
                operation_id: id.into(),
                scene_version: 1,
                verb,
            })
            .unwrap();
        tick(&mut driver, if id == "cut" { 8 } else { 7 });
    }
    let cut = handle.snapshot().unwrap();
    assert!(!cut.in_flight);
    assert_eq!(cut.completion_count, 2);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(persisted(&state), Some(expected.clone()), "after a CUT");
    let config: multiview_config::MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    config.validate().unwrap();
    let reloaded = MultiviewConfig::load_from_toml(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(
        reloaded.switcher.unwrap().mix_effects[0].transition,
        Some(expected),
        "a restart reads the same selection"
    );
}

/// MED-P3-3: the published schema is the descriptor's tagged union, and the
/// mirror accepts and refuses exactly what the core type does.
#[test]
fn the_documented_transition_schema_is_a_tagged_union_matching_the_core_type() {
    use multiview_control::openapi_schemas::TransitionDescriptorDoc;
    use utoipa::OpenApi;
    let doc = serde_json::to_value(multiview_control::openapi::ApiDoc::openapi()).unwrap();
    let schemas = &doc["components"]["schemas"];
    let request = &schemas["SwitcherRequest"]["properties"]["transition"];
    let state = &schemas["SwitcherStateResponse"]["properties"]["transition"];
    assert!(
        request.to_string().contains("TransitionDescriptorDoc"),
        "{request}"
    );
    assert!(
        state.to_string().contains("TransitionDescriptorDoc"),
        "{state}"
    );
    let union = &schemas["TransitionDescriptorDoc"];
    let variants = union["oneOf"].as_array().expect("oneOf");
    let mut kinds: Vec<&str> = variants
        .iter()
        .map(|v| v["properties"]["kind"]["enum"][0].as_str().unwrap())
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "cut",
            "dip",
            "dve_push",
            "dve_squeeze",
            "mix",
            "stinger",
            "wipe"
        ]
    );
    for case in [
        json!({"kind": "cut"}),
        json!({"kind": "mix", "rate": 30}),
        json!({"kind": "dip", "rate": 20, "color": "#ffffff", "switch_point": {"num": 1, "den": 3}}),
        wipe(),
        json!({"kind": "wipe", "pattern": "box", "rate": 12, "softness": 0.3, "border_width": 0.0,
            "position_xy": [0.2, 0.8], "reverse": true, "flip_flop": false}),
        json!({"kind": "dve_push", "direction": "left", "rate": 15}),
        json!({"kind": "stinger", "player": "p", "pre_roll": 5, "clip_duration": 40,
            "trigger_point": 10, "mix_rate": 20}),
    ] {
        let core: TransitionDescriptor = serde_json::from_value(case.clone()).unwrap();
        let mirror: TransitionDescriptorDoc = serde_json::from_value(case.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&core).unwrap(),
            serde_json::to_value(&mirror).unwrap(),
            "{case}"
        );
    }
    for case in [
        json!({"kind": "mix", "rate": 10, "reverse": true}),
        json!({"kind": "dip", "rate": 10, "switch_point": {"num": 1, "den": 2, "x": 0}}),
        json!({"kind": "spin"}),
    ] {
        assert!(serde_json::from_value::<TransitionDescriptor>(case.clone()).is_err());
        assert!(serde_json::from_value::<TransitionDescriptorDoc>(case.clone()).is_err());
    }
}

fn raw(path: &str, body: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {OPERATOR_TOKEN}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder.header("Idempotency-Key", key);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

/// P3R2-B: a transition number JSON cannot carry (an overflowing `1e400`, a
/// literal `NaN`) is a typed RFC 9457 `invalid-transition` 422 on both routes,
/// not the untyped body rejection; other malformed bodies keep their status.
#[tokio::test]
async fn unrepresentable_transition_numbers_are_typed_invalid_transition_422s() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    for (n, number) in ["1e400", "NaN", "-1e400"].into_iter().enumerate() {
        let body = format!(
            r#"{{"me":"me1","program":"main","action":"set_transition","transition":{{"kind":"wipe","pattern":"box","rate":10,"softness":{number}}}}}"#
        );
        for (path, key) in [
            ("/api/v1/switcher/plan", None),
            (
                "/api/v1/switcher/commands/set_transition",
                Some(format!("nan{n}")),
            ),
        ] {
            let response = send(&h.router, raw(path, &body, key.as_deref())).await;
            assert_eq!(
                response.headers()["content-type"],
                "application/problem+json",
                "{path} {number}"
            );
            assert!(
                problem_type(response).await.ends_with("invalid-transition"),
                "{path} {number}"
            );
        }
    }
    // A malformed body outside the descriptor keeps the existing rejection.
    for (body, status) in [
        (
            r#"{"me":"me1","program":"main","action":"auto","duration_frames":1e400}"#,
            StatusCode::BAD_REQUEST,
        ),
        (r#"{"me":"me1","program":"main","#, StatusCode::BAD_REQUEST),
        (
            r#"{"me":"me1","program":"main","action":"set_transition","transition":{"kind":"mix""#,
            StatusCode::BAD_REQUEST,
        ),
        (
            r#"{"me":1,"program":"main","action":"cut"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        for (path, key) in [
            ("/api/v1/switcher/plan", None),
            ("/api/v1/switcher/commands/auto", Some("other")),
        ] {
            let response = send(&h.router, raw(path, body, key)).await;
            assert_eq!(response.status(), status, "{path} {body}");
            assert_ne!(
                response
                    .headers()
                    .get("content-type")
                    .map(|v| v.to_str().unwrap().to_owned()),
                Some("application/problem+json".to_owned()),
                "{path} {body}"
            );
        }
    }
    tick(&mut driver, 1);
    assert!(outcomes.try_recv().is_err(), "a refusal queues nothing");
}

/// P3R2-C: a retry of an accepted `set_transition` with its original
/// Idempotency-Key during a later AUTO replays the original 202 and operation
/// id; it is not refused by the transient active-take check, and queues nothing.
#[tokio::test]
async fn a_replay_during_an_auto_returns_the_original_operation() {
    let (h, handle, mut driver) = rig();
    let mut outcomes = handle.outcomes();
    let dip = json!({"kind": "dip", "rate": 10});
    let first = send(
        &h.router,
        command(&set(dip.clone()), OPERATOR_TOKEN, "orig"),
    )
    .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let op = body_json(first).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tick(&mut driver, 1);
    assert_eq!(outcomes.try_recv().unwrap().event.operation_id, op);
    handle
        .try_send(SwitcherCommand {
            operation_id: "auto".into(),
            scene_version: 1,
            verb: SwitcherVerb::Auto(FrameCount::try_from(10).unwrap()),
        })
        .unwrap();
    tick(&mut driver, 2);
    assert!(handle.snapshot().unwrap().in_flight);
    while outcomes.try_recv().is_ok() {}
    let replay = send(
        &h.router,
        command(&set(dip.clone()), OPERATOR_TOKEN, "orig"),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replayed = body_json(replay).await;
    assert_eq!(replayed["operation_id"], op.as_str());
    assert_eq!(replayed["kind"], "set_transition");
    // A fresh key is still refused by the active-take check.
    let fresh = send(&h.router, command(&set(dip), OPERATOR_TOKEN, "fresh")).await;
    assert_eq!(fresh.status(), StatusCode::UNPROCESSABLE_ENTITY);
    tick(&mut driver, 3);
    let mut ids = Vec::new();
    while let Ok(item) = outcomes.try_recv() {
        ids.push(item.event.operation_id.clone());
    }
    assert!(
        !ids.iter().any(|id| id == &op),
        "the replay queued nothing: {ids:?}"
    );
}
