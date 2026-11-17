//! Upstream keyer control API (ADR-0113 §8, design note §11.7, slice K3):
//! the `arm` verb (plan and command pair, `Idempotency-Key`, 202 +
//! operation id, outcome on `switcher.operation`), its BOLA matrix (Control
//! on the M/E, Program scope, Control on every armed keyer object id, Read
//! on every armed key's sources, before any existence answer), USK fields in
//! `keyer_set` (admitted against the shared budget, persisted to the M/E),
//! and `GET` rows for USKs with the armed set, filtered and redacted alike
//! with `switcher.keyer` events for a scoped principal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]
mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::realtime::SessionStream;
use multiview_control::state::EngineStateSnapshot;
use multiview_control::{boot_model::BootModel, command_bus, AppState, InMemoryRepository};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::keyer::{self, usk_slot};
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherControl, SwitcherTickDriver,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::{json, Value};
use support::{body_json, get, post_json, send};

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
[[sources]]
id = "gfx"
kind = "bars"
[[sources]]
id = "gfx2"
kind = "bars"
[[sources]]
id = "secret"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "/tmp/switcher-keyers-usk/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
keyer_budget = 20.0
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
[[switcher.mix_effects.keyers]]
id = "usk1"
fill = "gfx"
key = "gfx2"
fly = { x = 0, y = 0, w = 32, h = 32 }
max_source = { w = 64, h = 64 }
z = 1
[[switcher.mix_effects.keyers]]
id = "usk2"
fill = "gfx2"
z = 2
"##;

/// Every grant: me1, main, both USKs, every source but `secret`.
const FULL: &str = "k-full.k-full-secret";
const NO_ME: &str = "k-no-me.k-no-me-secret";
const NO_PROGRAM: &str = "k-no-program.k-no-program-secret";
/// Without the keyer object `me1/usk1`.
const NO_USK1: &str = "k-no-usk1.k-no-usk1-secret";
/// Without `gfx2` (usk1's key, usk2's fill).
const NO_GFX2: &str = "k-no-gfx2.k-no-gfx2-secret";
const SCOPED_VIEWER: &str = "k-viewer.k-viewer-secret";
const ADMIN: &str = support::OPERATOR_TOKEN;
const VIEWER: &str = support::VIEWER_TOKEN;

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    let all = [
        "me1", "me1/usk1", "me1/usk2", "me1/usk3", "a", "b", "gfx", "gfx2",
    ];
    let mut add = |id: &str, without: Option<&str>, program: bool, role: Role| {
        keys.register(
            id,
            &format!("{id}-secret"),
            Principal {
                key_id: id.to_owned(),
                role,
                scoped_object_ids: Some(
                    all.iter()
                        .filter(|o| Some(**o) != without)
                        .map(|o| (*o).to_owned())
                        .collect(),
                ),
                scoped_output_ids: Some(if program {
                    vec!["program:main".to_owned()]
                } else {
                    vec![]
                }),
                scoped_discovery_domains: None,
            },
        );
    };
    add("k-full", None, true, Role::Operator);
    add("k-no-me", Some("me1"), true, Role::Operator);
    add("k-no-program", None, false, Role::Operator);
    add("k-no-usk1", Some("me1/usk1"), true, Role::Operator);
    add("k-no-gfx2", Some("gfx2"), true, Role::Operator);
    add("k-viewer", None, true, Role::Viewer);
    keys
}

fn stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["a", "b", "gfx", "gfx2", "secret"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
            store.publish(
                Nv12Image::solid(
                    64,
                    64,
                    200,
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
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
}

fn tick(driver: &mut SwitcherTickDriver, index: u64) {
    let tick = Tick {
        index,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
}

fn build(path: &Path, doc: &str) -> Rig {
    build_buses(path, doc, "a", "b")
}

/// [`build`] with the engine's PGM and PVW set to `program` / `preview`.
fn build_buses(path: &Path, doc: &str, program: &str, preview: &str) -> Rig {
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let publisher = Arc::new(EnginePublisher::new(256));
    let (sender, _receiver) = command_bus(64);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new(program),
        SourceRef::new(preview),
        vec![],
    ));
    driver.advertise_auto();
    let stores = stores();
    driver.set_prime_stores(stores.clone());
    let switcher = config.switcher.as_ref().unwrap();
    let me = &switcher.mix_effects[0];
    let defs = me
        .keyers
        .iter()
        .map(|cfg| keyer::KeyerDef::from_usk(cfg, "me1", "main", &stores, 64, 64).unwrap())
        .collect();
    driver.set_keyers(keyer::admit(defs, 64, 64, switcher.keyer_budget.unwrap()).unwrap());
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
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
        Arc::clone(&publisher),
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
    tokio::spawn(
        multiview_control::routes::switcher_keyers::forward_keyer_events(
            handle.clone(),
            Arc::clone(&publisher),
            "me1".into(),
            "main".into(),
        ),
    );
    Rig {
        router: multiview_control::router(state.clone()),
        state,
        handle,
        driver,
        publisher,
    }
}

fn temp_boot(doc: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiview.toml");
    std::fs::write(&path, doc).unwrap();
    (dir, path)
}

fn body(action: &str, extra: Value) -> Value {
    let mut base = json!({ "me": "me1", "program": "main", "action": action });
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

async fn plan(rig: &Rig, token: &str, body: &Value) -> axum::response::Response {
    send(&rig.router, post_json("/api/v1/switcher/plan", token, body)).await
}

async fn command(rig: &Rig, token: &str, key: &str, body: &Value) -> axum::response::Response {
    let kind = body["action"].as_str().unwrap();
    let mut request: Request<Body> =
        post_json(&format!("/api/v1/switcher/commands/{kind}"), token, body);
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    send(&rig.router, request).await
}

fn on_air(rig: &Rig, key: u32) -> bool {
    rig.handle.snapshot().unwrap().keyers[usk_slot(key).unwrap()]
        .unwrap()
        .on_air
}

/// BOLA for `arm` on plan and command: the M/E, the Program scope, every
/// armed keyer object id and every armed key's fill and key. An armed key
/// the caller may not see is 403 (never 404), a denied request burns no key
/// and enqueues nothing, and a Viewer cannot plan or command it.
#[tokio::test]
async fn arm_is_authorized_per_object_on_plan_and_command() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    let arm1 = body("arm", json!({ "background": true, "keys": ["usk1"] }));
    for token in [NO_ME, NO_PROGRAM, NO_USK1, NO_GFX2, SCOPED_VIEWER, VIEWER] {
        assert_eq!(
            plan(&rig, token, &arm1).await.status(),
            StatusCode::FORBIDDEN,
            "{token}"
        );
        assert_eq!(
            command(&rig, token, "arm-bola", &arm1).await.status(),
            StatusCode::FORBIDDEN,
            "{token}"
        );
    }
    // An undefined key the caller may not see: 403, nothing about existence.
    let ghost = body("arm", json!({ "keys": ["usk9"] }));
    assert_eq!(
        plan(&rig, FULL, &ghost).await.status(),
        StatusCode::FORBIDDEN
    );
    // One it may see (me1/usk3 is granted but undefined): 404.
    let unknown = body("arm", json!({ "keys": ["usk3"] }));
    assert_eq!(
        plan(&rig, FULL, &unknown).await.status(),
        StatusCode::NOT_FOUND
    );
    // A background-only arm needs only the M/E and the program (NO_GFX2
    // may arm neither key: both name gfx2).
    let bg = body("arm", json!({ "background": true }));
    assert_eq!(plan(&rig, NO_GFX2, &bg).await.status(), StatusCode::OK);
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().is_err(), "nothing was enqueued");
    // The denied key was never burned.
    assert_eq!(
        command(&rig, FULL, "arm-bola", &arm1).await.status(),
        StatusCode::ACCEPTED
    );
}

/// `arm` shape: background and/or keys, distinct, at most four, nothing
/// else; refused (409 `in-flight`) during a transition.
#[tokio::test]
async fn arm_shape_and_in_flight_refusal() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    for extra in [
        json!({}),
        json!({ "keys": ["usk1", "usk1"] }),
        json!({ "keys": ["usk1", "usk2", "usk1", "usk2", "usk1"] }),
        json!({ "keys": ["usk1"], "keyer": "usk1" }),
        json!({ "keys": ["usk1"], "source": "a" }),
        json!({ "keys": ["me1/usk1"] }),
    ] {
        let response = plan(&rig, ADMIN, &body("arm", extra.clone())).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{extra}"
        );
    }
    // keys on another verb are refused.
    let wrong = body(
        "keyer_cut",
        json!({ "keyer": "usk1", "family": "usk", "on": true, "keys": [] }),
    );
    assert_eq!(
        plan(&rig, ADMIN, &wrong).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // In flight: an AUTO started on the engine directly.
    rig.handle
        .try_send(multiview_engine::switcher_control::SwitcherCommand {
            operation_id: "auto".into(),
            scene_version: 1,
            verb: multiview_engine::switcher_control::SwitcherVerb::Auto(
                multiview_core::switcher::FrameCount::try_from(20).unwrap(),
            ),
            expect: None,
        })
        .unwrap();
    rig.handle
        .try_send(multiview_engine::switcher_control::SwitcherCommand {
            operation_id: "mix".into(),
            scene_version: 1,
            verb: multiview_engine::switcher_control::SwitcherVerb::SetTransition(
                serde_json::from_value(json!({"kind": "mix", "rate": 20})).unwrap(),
            ),
            expect: None,
        })
        .unwrap();
    tick(&mut rig.driver, 1);
    assert!(rig.handle.snapshot().unwrap().in_flight);
    let arm = body("arm", json!({ "keys": ["usk1"] }));
    let response = command(&rig, ADMIN, "arm-flight", &arm).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(body_json(response).await["type"]
        .as_str()
        .unwrap()
        .ends_with("in-flight"));
}

/// `arm` + `cut`: 202 with an operation id, the outcome on the engine tick,
/// an idempotent replay, the armed set in GET, and the key on air.
#[tokio::test]
async fn arm_then_cut_puts_a_key_on_air_with_an_outcome_and_replays() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    let arm = body("arm", json!({ "background": false, "keys": ["usk2"] }));
    let response = command(&rig, FULL, "arm-1", &arm).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = body_json(response).await;
    let op = accepted["operation_id"].as_str().unwrap().to_owned();
    assert_eq!(accepted["kind"], "arm");
    // A replay answers the first 202 and enqueues nothing.
    let replay = body_json(command(&rig, FULL, "arm-1", &arm).await).await;
    assert_eq!(replay["operation_id"], op.as_str());
    tick(&mut rig.driver, 1);
    let outcome = outcomes.try_recv().unwrap();
    assert_eq!(outcome.event.operation_id, op);
    assert!(outcome.event.completed);
    assert!(outcomes.try_recv().is_err(), "the replay enqueued nothing");
    let get_full = body_json(send(&rig.router, get("/api/v1/switcher/me1", FULL)).await).await;
    assert_eq!(
        get_full["armed"],
        json!({ "background": false, "keys": ["usk2"] })
    );
    let rows = get_full["keyers"].as_array().unwrap();
    let usk2 = rows.iter().find(|r| r["id"] == "usk2").unwrap();
    assert_eq!(
        (usk2["family"].as_str(), usk2["armed"].as_bool()),
        (Some("usk"), Some(true))
    );
    // A key-only CUT through the existing cut verb.
    let cut = body("cut", json!({}));
    assert_eq!(
        command(&rig, FULL, "cut-1", &cut).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 2);
    assert!(on_air(&rig, 2) && !on_air(&rig, 1));
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "a");
}

/// `keyer_set` with `family = "usk"`: edits a USK (Control on `me1/<id>`,
/// Read on the sources), defines a new one against the shared budget, and
/// refuses DSK-only fields; the applied definitions persist to the M/E.
/// `keyer_cut` drives a USK outside a transition; `keyer_auto` is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usk_keyer_set_cut_and_persistence() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    let set = body(
        "keyer_set",
        json!({ "keyer": "usk2", "family": "usk", "fly": {"x": 32, "y": 32, "w": 32, "h": 32},
                "type": "luma", "clip": 0.2 }),
    );
    // BOLA: the USK object id is me1/<id>.
    assert_eq!(
        plan(
            &rig,
            NO_USK1,
            &body(
                "keyer_set",
                json!({ "keyer": "usk1", "family": "usk", "clip": 0.1 })
            )
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        plan(
            &rig,
            NO_GFX2,
            &body(
                "keyer_set",
                json!({ "keyer": "usk1", "family": "usk", "key": "gfx2" })
            )
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let planned = body_json(plan(&rig, FULL, &set).await).await;
    assert_eq!(planned["keyer"], "me1/usk2");
    assert_eq!(planned["can_take"], true);
    assert_eq!(
        command(&rig, FULL, "set-1", &set).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    // DSK-only fields on a USK are 422, chroma (K7) included: a USK has
    // no `color`, and `type = chroma` is refused by name (`usk-chroma`).
    for extra in [
        json!({ "keyer": "usk2", "family": "usk", "rate": 5 }),
        json!({ "keyer": "usk2", "family": "usk", "on_at_start": true }),
        json!({ "keyer": "usk2", "family": "usk", "type": "chroma", "color": "#00b140" }),
        json!({ "keyer": "usk2", "family": "usk", "color": "#00b140" }),
    ] {
        assert_eq!(
            plan(&rig, FULL, &body("keyer_set", extra)).await.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let chroma = body_json(
        plan(
            &rig,
            FULL,
            &body(
                "keyer_set",
                json!({ "keyer": "usk2", "family": "usk", "type": "chroma" }),
            ),
        )
        .await,
    )
    .await;
    assert!(chroma.to_string().contains("usk-chroma"), "{chroma}");
    let auto = body(
        "keyer_auto",
        json!({ "keyer": "usk2", "family": "usk", "on": true }),
    );
    assert_eq!(
        plan(&rig, FULL, &auto).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // keyer_cut outside a transition.
    let cut = body(
        "keyer_cut",
        json!({ "keyer": "usk2", "family": "usk", "on": true }),
    );
    assert_eq!(
        command(&rig, FULL, "kc-1", &cut).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 2);
    assert!(on_air(&rig, 2));
    // Persisted to the M/E, not to downstream_keyers.
    for _ in 0..400 {
        let config: MultiviewConfig = serde_json::from_value(
            multiview_control::boot_model::current_desired_config(&rig.state).unwrap(),
        )
        .unwrap();
        let me = &config.switcher.as_ref().unwrap().mix_effects[0];
        if me
            .keyers
            .get(1)
            .is_some_and(|k| (k.clip - 0.2).abs() < 1e-6)
        {
            assert_eq!(config.switcher.as_ref().unwrap().downstream_keyers.len(), 0);
            assert_eq!(me.keyers[1].fly.unwrap().x, 32);
            config.validate().unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the USK edit was not persisted to its M/E");
}

async fn next_keyer(s: &mut SessionStream) -> Option<multiview_events::SwitcherKeyer> {
    loop {
        match tokio::time::timeout(Duration::from_millis(300), s.next_delta()).await {
            Ok(Ok(Some(frame))) => {
                if let Event::SwitcherKeyer(e) = frame.envelope.payload {
                    return Some(e);
                }
            }
            Ok(Ok(None)) => {}
            _ => return None,
        }
    }
}

/// GET rows for USKs and the armed set are filtered by keyer object id and
/// redacted; `switcher.keyer` events for a USK carry the same projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usk_rows_armed_set_and_events_filter_and_redact_alike() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let session = |objects: &[&str]| {
        SessionStream::new(rig.publisher.subscribe(), "s", None).with_scopes(
            Some(objects.iter().map(|o| (*o).to_owned()).collect()),
            Some(vec!["program:main".into()]),
            None,
        )
    };
    let mut no_usk1 = session(&["me1", "me1/usk2", "a", "b", "gfx", "gfx2"]);
    let mut no_gfx2 = session(&["me1", "me1/usk1", "me1/usk2", "a", "b", "gfx"]);
    command(
        &rig,
        ADMIN,
        "arm",
        &body("arm", json!({ "keys": ["usk1", "usk2"] })),
    )
    .await;
    // The arm lands first: a take is authorized against the committed set.
    tick(&mut rig.driver, 1);
    command(&rig, ADMIN, "cut", &body("cut", json!({}))).await;
    tick(&mut rig.driver, 2);
    assert!(on_air(&rig, 1) && on_air(&rig, 2));
    // Events: usk1's event is invisible without its object; usk2's fill is
    // redacted without gfx2.
    let mut seen = Vec::new();
    while let Some(e) = next_keyer(&mut no_usk1).await {
        seen.push(e.keyer);
    }
    assert_eq!(seen, vec!["me1/usk2".to_owned()]);
    let mut redacted = None;
    while let Some(e) = next_keyer(&mut no_gfx2).await {
        if e.keyer == "me1/usk2" {
            redacted = Some(e);
        }
    }
    let redacted = redacted.expect("usk2 event visible to no_gfx2");
    assert_eq!(redacted.fill, "redacted");
    // GET parity.
    let full = body_json(send(&rig.router, get("/api/v1/switcher/me1", ADMIN)).await).await;
    assert_eq!(full["keyers"].as_array().unwrap().len(), 2);
    assert_eq!(full["armed"]["keys"], json!(["usk1", "usk2"]));
    let usk1 = body_json(send(&rig.router, get("/api/v1/switcher/me1", NO_USK1)).await).await;
    let ids: Vec<_> = usk1["keyers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("usk2")], "usk1 row filtered");
    assert_eq!(
        usk1["armed"]["keys"],
        json!(["usk2"]),
        "armed set filtered alike"
    );
    let gfx2 = body_json(send(&rig.router, get("/api/v1/switcher/me1", NO_GFX2)).await).await;
    let row1 = gfx2["keyers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "usk1")
        .unwrap();
    assert_eq!(
        (row1["fill"].as_str(), row1["key"].as_str()),
        (Some("gfx"), Some("redacted"))
    );
    let row2 = gfx2["keyers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "usk2")
        .unwrap();
    assert_eq!(
        row2["fill"].as_str(),
        Some(redacted.fill.as_str()),
        "GET equals the event"
    );
    assert_eq!(row2["on_air"], redacted.on_air);
}

/// Review MEDIA-1: a key-only CUT or AUTO (the background not armed) is
/// admitted with PGM == PVW (it changes no bus); a background take with
/// identical buses is still refused.
#[tokio::test]
async fn a_key_only_take_is_admitted_with_identical_buses() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build_buses(&path, DOC, "a", "a");
    let mut outcomes = rig.handle.outcomes();
    let cut = body("cut", json!({}));
    let auto = body("auto", json!({ "duration_frames": 4 }));
    // The default arm set moves the background: refused.
    for verb in [&cut, &auto] {
        assert_eq!(
            plan(&rig, ADMIN, verb).await.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "background take on identical buses: {verb}"
        );
    }
    let arm = body("arm", json!({ "background": false, "keys": ["usk2"] }));
    assert_eq!(
        command(&rig, ADMIN, "arm", &arm).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    assert_eq!(plan(&rig, ADMIN, &cut).await.status(), StatusCode::OK);
    assert_eq!(plan(&rig, ADMIN, &auto).await.status(), StatusCode::OK);
    assert_eq!(
        command(&rig, ADMIN, "kcut", &cut).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 2);
    let outcome = outcomes.try_recv().unwrap().event;
    assert!(outcome.completed, "{outcome:?}");
    assert!(on_air(&rig, 2));
    assert_eq!(
        command(&rig, ADMIN, "kauto", &auto).await.status(),
        StatusCode::ACCEPTED
    );
}

/// Review AUTHZ-1: a CUT or AUTO toggles the armed USKs, so it needs Control
/// on every armed keyer object and Read on their fill and key, on plan and
/// command, before any existence answer. A principal holding the M/E and the
/// program but not `me1/usk1` (or not `gfx2`, usk1's key) cannot take while
/// usk1 is armed; a background-only arm needs no keyer grant.
#[tokio::test]
async fn a_take_is_authorized_on_every_armed_key() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    let arm = body("arm", json!({ "background": true, "keys": ["usk1"] }));
    assert_eq!(
        command(&rig, ADMIN, "arm", &arm).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    let cut = body("cut", json!({}));
    let auto = body("auto", json!({ "duration_frames": 4 }));
    for verb in [&cut, &auto] {
        for token in [NO_USK1, NO_GFX2] {
            assert_eq!(
                plan(&rig, token, verb).await.status(),
                StatusCode::FORBIDDEN,
                "{token} {verb}"
            );
            assert_eq!(
                command(&rig, token, "take-bola", verb).await.status(),
                StatusCode::FORBIDDEN,
                "{token} {verb}"
            );
        }
        assert_eq!(plan(&rig, FULL, verb).await.status(), StatusCode::OK);
    }
    tick(&mut rig.driver, 2);
    assert!(outcomes.try_recv().is_err(), "nothing was enqueued");
    assert!(!on_air(&rig, 1));
    // Background only: the keyer grants are not needed.
    let bg = body("arm", json!({ "background": true }));
    assert_eq!(
        command(&rig, ADMIN, "arm-bg", &bg).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 3);
    assert!(outcomes.try_recv().unwrap().event.completed);
    assert_eq!(plan(&rig, NO_USK1, &cut).await.status(), StatusCode::OK);
}

/// Review AUTHZ-1 (binding): the authorization is for the arm set the
/// caller saw. A take enqueued against arm version N is refused with a
/// typed outcome when the set changed before the tick (here: re-armed to
/// include usk1, which the caller may not control), and nothing toggles.
#[tokio::test]
async fn a_take_authorized_against_a_stale_arm_set_is_refused() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    let bg = body("arm", json!({ "background": true }));
    assert_eq!(
        command(&rig, ADMIN, "arm-bg", &bg).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    // An admin re-arms usk1 (queued, not yet applied: the committed set
    // NO_USK1 sees is still background-only)...
    rig.handle
        .try_send(multiview_engine::switcher_control::SwitcherCommand {
            operation_id: "rearm".into(),
            scene_version: 1,
            verb: multiview_engine::switcher_control::SwitcherVerb::Arm(Arc::new(
                multiview_core::switcher::ArmedSet::default()
                    .with_background()
                    .with_key(1),
            )),
            expect: None,
        })
        .unwrap();
    // ...so NO_USK1 is admitted to CUT the set it saw, and the engine must
    // refuse that take once the set changed under it.
    let cut = body("cut", json!({}));
    let accepted = command(&rig, NO_USK1, "cut-stale", &cut).await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let op = body_json(accepted).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tick(&mut rig.driver, 2);
    let mut refused = None;
    while let Ok(item) = outcomes.try_recv() {
        if item.event.operation_id == op {
            refused = Some(item.event.clone());
        }
    }
    let refused = refused.expect("the take has a terminal outcome");
    assert!(!refused.completed);
    assert_eq!(
        refused.error.as_deref(),
        Some(multiview_engine::switcher_control::REFUSE_ARM_CHANGED),
        "{refused:?}"
    );
    assert!(!on_air(&rig, 1), "usk1 never toggled");
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "a");
}

/// Review AUTHZ-2: a USK `keyer_cut` needs Read on the keyer's resolved fill
/// and key (as `arm` does), on plan and command; and it is bound to the
/// definition version it was authorized against.
#[tokio::test]
async fn usk_keyer_cut_is_authorized_on_its_sources_and_bound_to_its_definition() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    for keyer in ["usk1", "usk2"] {
        let cut = body(
            "keyer_cut",
            json!({ "keyer": keyer, "family": "usk", "on": true }),
        );
        assert_eq!(
            plan(&rig, NO_GFX2, &cut).await.status(),
            StatusCode::FORBIDDEN,
            "{keyer}"
        );
        assert_eq!(
            command(&rig, NO_GFX2, "kc-bola", &cut).await.status(),
            StatusCode::FORBIDDEN,
            "{keyer}"
        );
    }
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().is_err(), "nothing was enqueued");
    // An admin re-points usk2 to a source FULL may not read (queued, not
    // yet applied), then FULL cuts usk2 against the definition it saw: the
    // engine must refuse the cut once the definition changed under it.
    let set = body(
        "keyer_set",
        json!({ "keyer": "usk2", "family": "usk", "fill": "secret" }),
    );
    assert_eq!(
        command(&rig, ADMIN, "set-secret", &set).await.status(),
        StatusCode::ACCEPTED
    );
    let cut = body(
        "keyer_cut",
        json!({ "keyer": "usk2", "family": "usk", "on": true }),
    );
    let accepted = command(&rig, FULL, "kc-stale", &cut).await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let op = body_json(accepted).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tick(&mut rig.driver, 2);
    let mut cut_outcome = None;
    while let Ok(item) = outcomes.try_recv() {
        if item.event.operation_id == op {
            cut_outcome = Some(item.event.clone());
        }
    }
    let cut_outcome = cut_outcome.expect("terminal outcome");
    assert_eq!(
        cut_outcome.error.as_deref(),
        Some(multiview_engine::keyer::REFUSE_KEYER_DEFINITION_CHANGED),
        "{cut_outcome:?}"
    );
    assert!(!on_air(&rig, 2));
}

/// Review round 2 (IDEMP-1): a CUT/AUTO retry with the same
/// `Idempotency-Key` answers its original 202 and operation id even after
/// another operator armed a key the caller may not control: the implicit
/// armed set is authorized (and bound) only for a NEW admission. The
/// request-level checks still run first: a principal without the M/E is
/// 403 on the same key path.
#[tokio::test]
async fn a_take_replay_answers_its_first_202_after_an_inaccessible_key_is_armed() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = build(&path, DOC);
    let mut outcomes = rig.handle.outcomes();
    let bg = body("arm", json!({ "background": true }));
    assert_eq!(
        command(&rig, ADMIN, "arm-bg", &bg).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    let mut next = 2;
    for (verb, key) in [
        (body("cut", json!({})), "take-replay-cut"),
        (
            body("auto", json!({ "duration_frames": 4 })),
            "take-replay-auto",
        ),
    ] {
        let first = command(&rig, NO_USK1, key, &verb).await;
        assert_eq!(first.status(), StatusCode::ACCEPTED, "{verb}");
        let op = body_json(first).await["operation_id"].clone();
        // Let the take (an AUTO runs 4 ticks) settle before re-arming.
        for _ in 0..6 {
            tick(&mut rig.driver, next);
            next += 1;
        }
        assert!(!rig.handle.snapshot().unwrap().in_flight);
        // Another operator arms usk1, which NO_USK1 may not control.
        let arm = body("arm", json!({ "background": true, "keys": ["usk1"] }));
        assert_eq!(
            command(&rig, ADMIN, &format!("{key}-arm"), &arm)
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        tick(&mut rig.driver, next);
        next += 1;
        // A new take is refused on the armed key...
        assert_eq!(
            command(&rig, NO_USK1, &format!("{key}-new"), &verb)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        // ...but the retry answers its original 202.
        let replay = command(&rig, NO_USK1, key, &verb).await;
        assert_eq!(replay.status(), StatusCode::ACCEPTED, "{verb}");
        assert_eq!(body_json(replay).await["operation_id"], op);
        // Request-level authorization still precedes the replay.
        assert_eq!(
            command(&rig, NO_ME, key, &verb).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            command(&rig, ADMIN, &format!("{key}-rearm"), &bg)
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        tick(&mut rig.driver, next);
        next += 1;
        while outcomes.try_recv().is_ok() {}
    }
}
