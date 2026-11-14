//! The patch panel, mixer-input slice (ADR-0110 §12.8, #341): authorization
//! before existence and before the key, typed 422s, plan, atomic take with
//! replay, engine-busy release, rate limit, GET redaction from the engine's
//! readback, select by input, and persistence only after `applied`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::{boot_model::BootModel, command_bus, AppState, InMemoryRepository};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherCommand, SwitcherControl, SwitcherTickDriver, SwitcherVerb,
    COMMAND_CAPACITY,
};
use multiview_engine::switcher_inputs::MixerInputs;
use multiview_engine::EnginePublisher;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::{json, Value};
use support::{body_json, get, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

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
id = "abc-news"
kind = "bars"
[[sources]]
id = "vt1"
kind = "bars"
[[sources]]
id = "bars"
kind = "bars"
[[sources]]
id = "secret"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "abc-news"
[[outputs]]
kind = "hls"
path = "/tmp/patch-routes/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "abc-news"
preview_source = "vt1"
program_input = 1
preview_input = 2
[[switcher.mix_effects.inputs]]
number = 1
label = "Source 1"
video = { source = "abc-news" }
[[switcher.mix_effects.inputs]]
number = 2
video = { source = "vt1" }
[[switcher.mix_effects.inputs]]
number = 3
video = { source = "secret" }
[[switcher.mix_effects.inputs]]
number = 4
"##;

/// me1 + Program scope + abc-news, vt1, bars (not `secret`).
const LIMITED: &str = "patch-limited.patch-limited-secret";
/// Sources only; no me1 object grant.
const NO_ME: &str = "patch-no-me.patch-no-me-secret";
/// me1 + sources, but no `program:main` grant.
const NO_PROGRAM: &str = "patch-no-program.patch-no-program-secret";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    let scoped = |id: &str, objects: &[&str], program: bool| Principal {
        key_id: id.to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(objects.iter().map(|s| (*s).to_owned()).collect()),
        scoped_output_ids: Some(if program {
            vec!["program:main".to_owned()]
        } else {
            vec![]
        }),
        scoped_discovery_domains: None,
    };
    keys.register(
        "patch-limited",
        "patch-limited-secret",
        scoped("patch-limited", &["me1", "abc-news", "vt1", "bars"], true),
    );
    keys.register(
        "patch-no-me",
        "patch-no-me-secret",
        scoped("patch-no-me", &["abc-news", "vt1", "bars"], true),
    );
    keys.register(
        "patch-no-program",
        "patch-no-program-secret",
        scoped(
            "patch-no-program",
            &["me1", "abc-news", "vt1", "bars"],
            false,
        ),
    );
    keys
}

fn stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["abc-news", "vt1", "bars", "secret"]
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

struct Rig {
    router: axum::Router,
    state: AppState,
    handle: SwitcherControl,
    driver: SwitcherTickDriver,
}

fn tick(driver: &mut SwitcherTickDriver, index: u64) {
    let tick = Tick {
        index,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
}

fn rig(path: &Path) -> Rig {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::new(256));
    let (sender, _receiver) = command_bus(64);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::input(1),
        SourceRef::input(2),
        vec![],
    ));
    driver.set_prime_stores(stores());
    driver.set_inputs(
        MixerInputs::new([
            (1, Some("Source 1".into()), Some(SourceRef::new("abc-news"))),
            (2, None, Some(SourceRef::new("vt1"))),
            (3, None, Some(SourceRef::new("secret"))),
            (4, None, None),
        ])
        .unwrap(),
    );
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("abc-news"),
            preview: scene("vt1"),
        })
        .unwrap();
    driver.publish_provider_status(ProviderStatus {
        scene_version: 1,
        last_tick: 0,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    tick(&mut driver, 0);
    let state = AppState::new(
        engine,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )))
    .with_switcher(handle.clone());
    Rig {
        router: multiview_control::router(state.clone()),
        state,
        handle,
        driver,
    }
}

fn with_key(mut request: Request<Body>, key: &str) -> Request<Body> {
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    request
}

fn xp(input: u16, source: &str) -> Value {
    let source = if source == "none" {
        json!("none")
    } else {
        json!({ "source": source })
    };
    json!({ "plane": "video", "destination": {"mixer": "me1", "input": input}, "source": source })
}

fn body(crosspoints: Vec<Value>, on_air: &str) -> Value {
    json!({ "crosspoints": crosspoints, "on_air": on_air })
}

async fn take(rig: &Rig, token: &str, key: &str, body: Value) -> axum::response::Response {
    send(
        &rig.router,
        with_key(post_json("/api/v1/patch/take", token, &body), key),
    )
    .await
}

fn temp_boot() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiview.toml");
    std::fs::write(&path, DOC).unwrap();
    (dir, path)
}

#[tokio::test]
async fn authorization_runs_before_existence_and_burns_no_key() {
    let (_dir, path) = temp_boot();
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    for (token, crosspoint) in [
        (NO_ME, xp(4, "bars")),
        (NO_PROGRAM, xp(4, "bars")),
        // A real source the principal may not read, and an unknown one.
        (LIMITED, xp(4, "secret")),
        (LIMITED, xp(4, "ghost")),
    ] {
        let response = take(&rig, token, "k1", body(vec![crosspoint], "protect")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    // Plan is authorized the same way; a viewer cannot plan or take.
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/patch/plan",
            LIMITED,
            &body(vec![xp(4, "secret")], "protect"),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = take(
        &rig,
        VIEWER_TOKEN,
        "k1",
        body(vec![xp(4, "bars")], "protect"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // The same key is still fresh for the principal: a 202 that really enqueues.
    let response = take(&rig, LIMITED, "k1", body(vec![xp(4, "bars")], "protect")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tick(&mut rig.driver, 1);
    let outcome = outcomes.try_recv().unwrap().event;
    assert!(outcome.completed, "{outcome:?}");
}

#[tokio::test]
async fn unsupported_shapes_are_typed_422s() {
    let (_dir, path) = temp_boot();
    let rig = rig(&path);
    // #342 B2: an output slot is now patchable on its own; beside a mixer
    // crosspoint it is a multi-destination set (#308), still refused.
    let output = json!({ "plane": "video", "destination": {"output": "hls-1", "slot": 0}, "source": {"source": "bars"} });
    let audio = json!({ "plane": "audio", "destination": {"mixer": "me1", "input": 4}, "source": {"source": "bars"} });
    // #307 PR 2: a stored layout is now a mixer-input source; a bus on a
    // mixer input is still a later slice.
    let bus = json!({ "plane": "video", "destination": {"mixer": "me1", "input": 4}, "source": {"bus": "program"} });
    let too_many: Vec<Value> = (0..65).map(|_| xp(4, "bars")).collect();
    let cases = [
        (vec![xp(4, "bars"), output], "unknown-slot", "#308"),
        (vec![audio], "slot-medium-mismatch", "video"),
        (vec![xp(4, "bars"), xp(4, "vt1")], "unknown-slot", "twice"),
        (vec![xp(9, "bars")], "unknown-slot", "not declared"),
        (vec![bus], "unknown-source", "later slice"),
        (vec![], "", "1..=64"),
        (too_many, "", "1..=64"),
    ];
    for (crosspoints, kind, detail) in cases {
        let response = send(
            &rig.router,
            post_json(
                "/api/v1/patch/plan",
                OPERATOR_TOKEN,
                &body(crosspoints, "protect"),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let problem = body_json(response).await;
        assert!(
            problem["type"].as_str().unwrap().ends_with(kind),
            "{problem}"
        );
        assert!(
            problem["detail"].as_str().unwrap().contains(detail),
            "{problem}"
        );
    }
}

#[tokio::test]
async fn plan_reports_path_and_on_air_and_protect_refuses_the_program_input() {
    let (_dir, path) = temp_boot();
    let rig = rig(&path);
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/patch/plan",
            OPERATOR_TOKEN,
            &body(vec![xp(1, "bars"), xp(4, "vt1")], "protect"),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let plan = body_json(response).await;
    assert_eq!(plan["class"], "class1");
    assert_eq!(plan["can_take"], false);
    assert_eq!(plan["crosspoints"][0]["path"], "mixer-input");
    assert_eq!(plan["crosspoints"][0]["on_air"], true);
    assert_eq!(
        plan["crosspoints"][0]["refusal"]["type"],
        "on-air-protected"
    );
    assert_eq!(plan["crosspoints"][1]["on_air"], false);
    assert!(plan["crosspoints"][1]["refusal"].is_null());
    let response = take(
        &rig,
        OPERATOR_TOKEN,
        "p1",
        body(vec![xp(1, "bars")], "protect"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(body_json(response).await["type"]
        .as_str()
        .unwrap()
        .ends_with("on-air-protected"));
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/patch/plan",
            OPERATOR_TOKEN,
            &body(vec![xp(1, "bars")], "allow"),
        ),
    )
    .await;
    assert_eq!(body_json(response).await["can_take"], true);
}

fn persisted_inputs(state: &AppState) -> MultiviewConfig {
    serde_json::from_value(multiview_control::boot_model::current_desired_config(state).unwrap())
        .unwrap()
}

async fn wait_for<F: Fn() -> bool>(f: F) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_is_202_replays_lands_on_one_tick_and_persists_after_applied() {
    let (_dir, path) = temp_boot();
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    // Acceptance mda001: on_air=allow re-patches the PGM input (cut to bars);
    // three members land on one tick.
    let set = body(
        vec![xp(1, "bars"), xp(2, "abc-news"), xp(4, "vt1")],
        "allow",
    );
    let response = take(&rig, OPERATOR_TOKEN, "t1", set.clone()).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = body_json(response).await;
    assert_eq!(accepted["status"], "accepted");
    let op = accepted["operation_id"].as_str().unwrap().to_owned();
    let response = take(&rig, OPERATOR_TOKEN, "t1", set).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(response).await["operation_id"], op.as_str());
    assert!(
        persisted_inputs(&rig.state).switcher.unwrap().mix_effects[0].input_source(1)
            == Some("abc-news"),
        "nothing is persisted before the engine applied it"
    );
    tick(&mut rig.driver, 5);
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(outcome.operation_id, op);
    assert!(outcome.completed && outcome.error.is_none());
    assert!(outcomes.try_recv().is_err(), "the replay enqueued nothing");
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!((snapshot.tick, outcome.tick), (5, 5));
    assert_eq!(snapshot.program.as_str(), "bars");
    // GET /patch is the engine's live readback.
    let live = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let mixer = &live["mixer"][0];
    assert_eq!(mixer["me"], "me1");
    assert_eq!(mixer["inputs_version"], 1);
    assert_eq!(mixer["inputs"][0]["video"]["source"], "bars");
    assert_eq!(mixer["inputs"][0]["on_program"], true);
    assert_eq!(mixer["inputs"][0]["label"], "Source 1");
    assert_eq!(mixer["inputs"][3]["video"]["source"], "vt1");
    // Persisted after applied: the accepted state boots the same patch.
    wait_for(|| {
        persisted_inputs(&rig.state).switcher.unwrap().mix_effects[0].input_source(1)
            == Some("bars")
    })
    .await;
    let config = persisted_inputs(&rig.state);
    config.validate().unwrap();
    let me = &config.switcher.as_ref().unwrap().mix_effects[0];
    assert_eq!(me.input_source(2), Some("abc-news"));
    assert_eq!(me.input_source(4), Some("vt1"));
    assert_eq!(me.input_source(3), Some("secret"));
    assert_eq!((me.program_input, me.preview_input), (Some(1), Some(2)));
    assert_eq!(me.program_source, "bars");
    assert_eq!(me.preview_source, "abc-news");
    assert_eq!(
        config.cells[0].source.input_id.as_deref(),
        Some("bars"),
        "the Program placeholder cell follows program_source (admission)"
    );
    let reloaded = MultiviewConfig::load_from_toml(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(
        reloaded.switcher, config.switcher,
        "a restart reads the same table"
    );
}

#[tokio::test]
async fn get_patch_redacts_unreadable_sources_and_hides_an_unreadable_mixer() {
    let (_dir, path) = temp_boot();
    let rig = rig(&path);
    let live = body_json(send(&rig.router, get("/api/v1/patch", LIMITED)).await).await;
    let inputs = &live["mixer"][0]["inputs"];
    assert_eq!(inputs[0]["video"]["source"], "abc-news");
    assert_eq!(inputs[0]["on_program"], true);
    assert_eq!(inputs[1]["on_preview"], true);
    assert_eq!(inputs[2]["video"], json!({ "redacted": true }));
    assert!(!live.to_string().contains("secret"));
    assert!(inputs[3]["video"].is_null());
    for token in [NO_ME, NO_PROGRAM] {
        let live = body_json(send(&rig.router, get("/api/v1/patch", token)).await).await;
        assert_eq!(live["mixer"], json!([]), "{token}");
    }
}

#[tokio::test]
async fn engine_busy_releases_the_key() {
    let (_dir, path) = temp_boot();
    let mut rig = rig(&path);
    for i in 0..COMMAND_CAPACITY {
        rig.handle
            .try_send(SwitcherCommand {
                operation_id: format!("fill-{i}"),
                scene_version: 1,
                verb: SwitcherVerb::Cut,
                expect: None,
            })
            .unwrap();
    }
    let set = body(vec![xp(4, "bars")], "protect");
    let response = take(&rig, OPERATOR_TOKEN, "busy", set.clone()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    tick(&mut rig.driver, 1);
    let response = take(&rig, OPERATOR_TOKEN, "busy", set).await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "the released key is fresh again, not a false replay"
    );
}

#[tokio::test]
async fn takes_are_rate_limited_per_destination_with_retry_after() {
    let (_dir, path) = temp_boot();
    let rig = rig(&path);
    for (i, source) in ["bars", "vt1"].into_iter().enumerate() {
        let response = take(
            &rig,
            OPERATOR_TOKEN,
            &format!("r{i}"),
            body(vec![xp(4, source)], "protect"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
    let response = take(
        &rig,
        OPERATOR_TOKEN,
        "r2",
        body(vec![xp(4, "abc-news")], "protect"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get(header::RETRY_AFTER).is_some());
    // The limited request burned no key: once the window passes, the same key
    // is a fresh take that really enqueues (not a replay of nothing).
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    let mut outcomes = rig.handle.outcomes();
    let mut rig = rig;
    let response = take(
        &rig,
        OPERATOR_TOKEN,
        "r2",
        body(vec![xp(4, "abc-news")], "protect"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tick(&mut rig.driver, 1);
    let mut applied = 0;
    while let Ok(item) = outcomes.try_recv() {
        assert!(item.event.completed);
        applied += 1;
    }
    assert_eq!(
        applied, 3,
        "r0, r1 and the retried r2 each reached the engine"
    );
}

#[tokio::test]
async fn select_accepts_an_input_and_translates_a_bare_source() {
    let (_dir, path) = temp_boot();
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    let select = |input: Option<u16>, source: Option<&str>| json!({ "me": "me1", "program": "main", "action": "select_preview", "input": input, "source": source });
    let path = "/api/v1/switcher/commands/select_preview";
    // Unpatched/undeclared input and a source on no input: typed 422s.
    for (request, kind) in [
        (select(Some(4), None), "unknown-slot"),
        (select(Some(9), None), "unknown-slot"),
        (select(None, Some("bars")), "not-on-an-input"),
    ] {
        let response = send(
            &rig.router,
            with_key(post_json(path, OPERATOR_TOKEN, &request), kind),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_json(response).await["type"]
            .as_str()
            .unwrap()
            .ends_with(kind));
    }
    // Input 3 carries `secret`, which LIMITED may not read: 403.
    let response = send(
        &rig.router,
        with_key(post_json(path, LIMITED, &select(Some(3), None)), "s3"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A bare source selects its input; the engine then holds in:1 / in:3.
    let response = send(
        &rig.router,
        with_key(
            post_json(path, OPERATOR_TOKEN, &select(Some(3), None)),
            "s-ok",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    assert_eq!(rig.driver.selections().1, &SourceRef::input(3));
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(
        (snapshot.preview.as_str(), snapshot.preview_input),
        ("secret", Some(3))
    );
    let response = send(
        &rig.router,
        with_key(
            post_json(path, OPERATOR_TOKEN, &select(None, Some("vt1"))),
            "s-src",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tick(&mut rig.driver, 2);
    assert_eq!(rig.driver.selections().1, &SourceRef::input(2));
    // `input` and `source` together are refused.
    let response = send(
        &rig.router,
        with_key(
            post_json(path, OPERATOR_TOKEN, &select(Some(2), Some("vt1"))),
            "s-both",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
