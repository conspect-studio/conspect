//! Downstream keyer control API (ADR-0113 §10, design note §8 and §11.7,
//! slice K4): `keyer_cut`, `keyer_auto`, `keyer_tie` and `keyer_set` on the
//! existing switcher plan/command routes; BOLA on the M/E, the program, the
//! keyer object and every source, on plan and command; `GET` row filtering
//! and redaction with a GET/events parity test for a scoped principal;
//! idempotent replay; 503 on a full mailbox; `capacity`, `on-air-protected`
//! and the Clean rule, each releasing what it took; `in:<n>` refused;
//! persistence after `applied` and a restart with keyers off.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    // `body` takes the `json!` value by value for call-site ergonomics.
    clippy::needless_pass_by_value
)]
mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanRefusal, RoutedFeed,
    RoutingLive, TakeSubmitError,
};
use multiview_control::realtime::SessionStream;
use multiview_control::state::EngineStateSnapshot;
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, InMemoryRepository, SourceDemandLease,
    SourceDemandPhase, SourceDemandProvider, SourceDemandView,
};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::keyer;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherControl, SwitcherTickDriver,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::{json, Value};
use support::{body_json, etag, get, post_json, put_json, send};

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
path = "/tmp/switcher-keyers/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
[[switcher.downstream_keyers]]
id = "dsk1"
program = "main"
fill = "gfx"
z = 1
max_source = { w = 64, h = 64 }
"##;

/// Full rights on me1, main, the keyer and every source but `secret`.
const FULL: &str = "k-full.k-full-secret";
/// Like FULL but without the keyer object `main/dsk1`.
const NO_KEYER: &str = "k-no-keyer.k-no-keyer-secret";
/// Like FULL but without the `gfx` source (dsk1's fill).
const NO_FILL: &str = "k-no-fill.k-no-fill-secret";
/// Like FULL but without the `me1` object.
const NO_ME: &str = "k-no-me.k-no-me-secret";
/// Like FULL but without `program:main`.
const NO_PROGRAM: &str = "k-no-program.k-no-program-secret";
/// An unscoped operator.
const ADMIN: &str = support::OPERATOR_TOKEN;
/// A viewer (Read only).
const VIEWER: &str = support::VIEWER_TOKEN;
/// A Viewer holding every object and Program grant FULL holds.
const SCOPED_VIEWER: &str = "k-viewer.k-viewer-secret";
/// A `ReadOnly` principal holding every object and Program grant FULL holds.
const SCOPED_READ_ONLY: &str = "k-read-only.k-read-only-secret";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    let all = ["me1", "main/dsk1", "main/dsk2", "a", "b", "gfx", "gfx2"];
    let mut add_as = |id: &str, without: Option<&str>, program: bool, role: Role| {
        let objects: Vec<String> = all
            .iter()
            .filter(|o| Some(**o) != without)
            .map(|o| (*o).to_owned())
            .collect();
        keys.register(
            id,
            &format!("{id}-secret"),
            Principal {
                key_id: id.to_owned(),
                role,
                scoped_object_ids: Some(objects),
                scoped_output_ids: Some(if program {
                    vec!["program:main".to_owned()]
                } else {
                    vec![]
                }),
                scoped_discovery_domains: None,
            },
        );
    };
    let mut add = |id: &str, without: Option<&str>, program: bool| {
        add_as(id, without, program, Role::Operator);
    };
    add("k-full", None, true);
    add("k-no-keyer", Some("main/dsk1"), true);
    add("k-no-fill", Some("gfx"), true);
    add("k-no-me", Some("me1"), true);
    add("k-no-program", None, false);
    add_as("k-viewer", None, true, Role::Viewer);
    add_as("k-read-only", None, true, Role::ReadOnly);
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

/// A routing seam whose live snapshot says whether an output carries Clean.
struct Routing {
    live: Mutex<Option<RoutingLive>>,
}

impl Routing {
    fn with_clean(clean: bool) -> Arc<Self> {
        let live = clean.then(|| {
            let mut outputs = HashMap::new();
            outputs.insert(
                "out-a".to_owned(),
                multiview_control::output_routing::OutputLive {
                    bus: "clean".to_owned(),
                    since_tick: 0,
                    pending: None,
                    degraded: false,
                    audio: None,
                    pending_audio: None,
                    private_audio: false,
                },
            );
            RoutingLive {
                outputs,
                ..RoutingLive::default()
            }
        });
        Arc::new(Self {
            live: Mutex::new(live),
        })
    }
}

impl OutputRouting for Routing {
    fn plan(&self, _output: &str, _feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        Err(PlanRefusal::OutputKindUnroutable)
    }
    fn plan_audio(
        &self,
        _output: &str,
        _video: Option<&RoutedFeed>,
        _audio: Option<&multiview_control::output_routing::RoutedAudio>,
    ) -> Result<multiview_control::output_routing::AudioPlanCost, PlanRefusal> {
        Ok(multiview_control::output_routing::AudioPlanCost::default())
    }
    fn try_take(&self, _take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        Err(TakeSubmitError::Unroutable)
    }
    fn snapshot(&self) -> Option<RoutingLive> {
        self.live.lock().unwrap().clone()
    }
    fn subscribe_outcomes(&self) -> Option<std::sync::mpsc::Receiver<OutputOutcome>> {
        None
    }
}

/// Records lease acquisitions and drops.
#[derive(Default)]
struct LeaseLog {
    live: Mutex<Vec<String>>,
    acquired: Mutex<Vec<String>>,
}

struct Lease {
    tag: String,
    log: Arc<LeaseLog>,
}
impl SourceDemandLease for Lease {}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut live = self.log.live.lock().unwrap();
        if let Some(at) = live.iter().position(|t| *t == self.tag) {
            live.remove(at);
        }
    }
}

struct Demand {
    log: Arc<LeaseLog>,
}
impl SourceDemandProvider for Demand {
    fn status(&self, id: &str) -> SourceDemandView {
        SourceDemandView::idle(id)
    }
    fn active(&self) -> Vec<SourceDemandView> {
        Vec::new()
    }
    fn preview_lease(&self, id: &str, label: &str) -> Option<Box<dyn SourceDemandLease>> {
        let tag = format!("{label}:{id}");
        self.log.live.lock().unwrap().push(tag.clone());
        self.log.acquired.lock().unwrap().push(tag.clone());
        Some(Box::new(Lease {
            tag,
            log: Arc::clone(&self.log),
        }))
    }
    fn touch_preview(&self, _id: &str) -> Option<SourceDemandPhase> {
        None
    }
}

struct Rig {
    /// The router's Clean reservation when the rig starts with Clean routed
    /// (the real router holds one per output on or taking Clean).
    #[allow(dead_code)]
    clean: Option<multiview_engine::clean_gate::CleanReservation>,
    router: axum::Router,
    state: AppState,
    handle: SwitcherControl,
    driver: SwitcherTickDriver,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    leases: Arc<LeaseLog>,
}

fn tick(driver: &mut SwitcherTickDriver, index: u64) {
    let tick = Tick {
        index,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
}

fn build(path: &Path, doc: &str, clean_routed: bool) -> Rig {
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let publisher = Arc::new(EnginePublisher::new(256));
    let (sender, _receiver) = command_bus(64);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    let stores = stores();
    driver.set_prime_stores(stores.clone());
    let switcher = config.switcher.as_ref().unwrap();
    let defs = switcher
        .downstream_keyers
        .iter()
        .map(|cfg| keyer::KeyerDef::from_config(cfg, &stores, 64, 64).unwrap())
        .collect();
    driver.set_keyers(keyer::admit(defs, 64, 64, switcher.keyer_budget.unwrap_or(3.0)).unwrap());
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
    let leases = Arc::new(LeaseLog::default());
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
    .with_switcher(handle.clone())
    .with_output_routing(Routing::with_clean(clean_routed))
    .with_source_demand(Arc::new(Demand {
        log: Arc::clone(&leases),
    }));
    tokio::spawn(
        multiview_control::routes::switcher_keyers::forward_keyer_events(
            handle.clone(),
            Arc::clone(&publisher),
            "me1".into(),
            "main".into(),
        ),
    );
    let clean = clean_routed.then(|| handle.clean_gate().try_reserve_clean().unwrap());
    Rig {
        clean,
        router: multiview_control::router(state.clone()),
        state,
        handle,
        driver,
        publisher,
        leases,
    }
}

fn rig(path: &Path) -> Rig {
    build(path, DOC, false)
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

async fn kind_of(response: axum::response::Response) -> (StatusCode, String) {
    let status = response.status();
    let body = body_json(response).await;
    (
        status,
        body["type"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("/problems/")
            .to_owned(),
    )
}

async fn wait_for<F: Fn() -> bool>(f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

fn persisted(state: &AppState) -> MultiviewConfig {
    serde_json::from_value(multiview_control::boot_model::current_desired_config(state).unwrap())
        .unwrap()
}

/// The BOLA matrix: each verb, on plan and on command, refuses every
/// principal missing one grant (M/E, program, keyer object, a named source),
/// with 403 before any existence answer, and never enqueues.
#[tokio::test]
async fn every_keyer_verb_is_authorized_per_object_on_plan_and_command() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    let verbs = [
        body("keyer_cut", json!({ "keyer": "dsk1", "on": true })),
        body("keyer_auto", json!({ "keyer": "dsk1", "on": true })),
        body("keyer_tie", json!({ "keyer": "dsk1", "tie": true })),
        body("keyer_set", json!({ "keyer": "dsk1", "rate": 20 })),
    ];
    for verb in &verbs {
        for token in [NO_ME, NO_PROGRAM, NO_KEYER] {
            assert_eq!(
                plan(&rig, token, verb).await.status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                command(&rig, token, "bola", verb).await.status(),
                StatusCode::FORBIDDEN,
                "{token} {verb}"
            );
        }
        assert_eq!(
            command(&rig, VIEWER, "bola-viewer", verb).await.status(),
            StatusCode::FORBIDDEN,
            "a viewer cannot command"
        );
    }
    // Read on every named source: a keyer_set naming an unreadable fill or
    // key, and an unknown keyer that the principal may not see, are 403, not
    // 404 or 422 (nothing about existence leaks).
    for (token, extra) in [
        (FULL, json!({ "keyer": "dsk1", "fill": "secret" })),
        (FULL, json!({ "keyer": "dsk1", "key": "secret" })),
        (NO_FILL, json!({ "keyer": "dsk1", "fill": "gfx" })),
        (FULL, json!({ "keyer": "dsk9", "fill": "gfx2" })),
    ] {
        let verb = body("keyer_set", extra);
        assert_eq!(
            plan(&rig, token, &verb).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            command(&rig, token, "bola-src", &verb).await.status(),
            StatusCode::FORBIDDEN,
            "{verb}"
        );
    }
    // A principal that may not read the current fill can still drive the
    // keyer it controls (the fill is not named by the request).
    let cut = body("keyer_cut", json!({ "keyer": "dsk1", "on": true }));
    assert_eq!(plan(&rig, NO_FILL, &cut).await.status(), StatusCode::OK);
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().is_err(), "nothing was enqueued");
    // The denied keys were never burned: the same key now succeeds.
    let response = command(&rig, FULL, "bola", &cut).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

/// Review H1: a keyer plan is a control-plane rehearsal of a command, so it
/// needs the command's role (`Write`), not `Read`: a Viewer or `ReadOnly`
/// principal holding every object grant is refused 403 on `/plan` for all
/// four verbs, before any existence or admission answer.
#[tokio::test]
async fn viewer_and_read_only_cannot_plan_any_keyer_verb() {
    let (_dir, path) = temp_boot(DOC);
    let rig = rig(&path);
    let verbs = [
        body("keyer_cut", json!({ "keyer": "dsk1", "on": true })),
        body("keyer_auto", json!({ "keyer": "dsk1", "on": true })),
        body("keyer_tie", json!({ "keyer": "dsk1", "tie": true })),
        body("keyer_set", json!({ "keyer": "dsk1", "rate": 20 })),
        // A keyer not defined yet: still 403, never an admission answer.
        body("keyer_set", json!({ "keyer": "dsk2", "fill": "gfx2" })),
    ];
    for verb in &verbs {
        for token in [SCOPED_VIEWER, SCOPED_READ_ONLY, VIEWER] {
            assert_eq!(
                plan(&rig, token, verb).await.status(),
                StatusCode::FORBIDDEN,
                "{token} {verb}"
            );
        }
        // The same grants with the Operator role plan normally.
        assert_ne!(
            plan(&rig, FULL, verb).await.status(),
            StatusCode::FORBIDDEN,
            "{verb}"
        );
    }
}

#[tokio::test]
async fn cut_auto_and_tie_are_202_with_an_outcome_on_the_engine_tick() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    let response = plan(
        &rig,
        FULL,
        &body("keyer_cut", json!({"keyer":"dsk1","on":true})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let planned = body_json(response).await;
    assert_eq!(planned["class"], "class1");
    assert_eq!(planned["keyer"], "main/dsk1");
    assert_eq!(planned["can_take"], true);
    assert_eq!(planned["on_air"], false);

    let cut = body("keyer_cut", json!({"keyer":"dsk1","on":true}));
    let accepted = body_json(command(&rig, FULL, "cut-1", &cut).await).await;
    assert_eq!(accepted["kind"], "keyer_cut");
    let op = accepted["operation_id"].as_str().unwrap().to_owned();
    // Replay: the first 202, nothing enqueued twice.
    let again = body_json(command(&rig, FULL, "cut-1", &cut).await).await;
    assert_eq!(again["operation_id"], op.as_str());
    tick(&mut rig.driver, 1);
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(
        (outcome.operation_id.as_str(), outcome.tick),
        (op.as_str(), 1)
    );
    assert!(outcome.completed);
    assert!(outcomes.try_recv().is_err(), "the replay enqueued nothing");
    assert!(rig.handle.snapshot().unwrap().keyers[0].unwrap().on_air);

    let auto = body(
        "keyer_auto",
        json!({"keyer":"dsk1","on":false,"duration_frames":4}),
    );
    assert_eq!(
        command(&rig, FULL, "auto-1", &auto).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 2);
    let ramp = rig.handle.snapshot().unwrap().keyers[0]
        .unwrap()
        .ramp
        .unwrap();
    assert_eq!((ramp.start_tick, ramp.rate, ramp.to_on), (2, 4, false));
    // A second AUTO while ramping is refused by the plan and as a 409.
    let response = plan(&rig, FULL, &auto).await;
    assert_eq!(
        body_json(response).await["refusal"]["type"],
        "keyer-ramping"
    );
    let (status, kind) = kind_of(command(&rig, FULL, "auto-2", &auto).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::CONFLICT, "keyer-ramping")
    );
    for index in 3..=6 {
        tick(&mut rig.driver, index);
    }
    assert!(!rig.handle.snapshot().unwrap().keyers[0].unwrap().on_air);

    let tie = body("keyer_tie", json!({"keyer":"dsk1","tie":true}));
    assert_eq!(
        command(&rig, FULL, "tie-1", &tie).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 7);
    assert!(rig.handle.snapshot().unwrap().keyers[0].unwrap().tie);

    // Shape: bounds and the verb's own fields only.
    for (bad, kind) in [
        (
            body(
                "keyer_auto",
                json!({"keyer":"dsk1","on":true,"duration_frames":301}),
            ),
            "keyer-rate",
        ),
        (body("keyer_cut", json!({"keyer":"dsk1"})), "validation"),
        (
            body("keyer_cut", json!({"keyer":"dsk1","on":true,"fill":"gfx"})),
            "validation",
        ),
        (body("keyer_cut", json!({"on":true})), "validation"),
        (
            body("keyer_cut", json!({"keyer":"dsk1","on":true,"source":"a"})),
            "validation",
        ),
    ] {
        let (status, got) = kind_of(command(&rig, FULL, "bad", &bad).await).await;
        assert_eq!(
            (status, got.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, kind),
            "{bad}"
        );
    }
    // An unknown keyer the principal may read: 404, nothing burned.
    let unknown = body("keyer_cut", json!({"keyer":"dsk2","on":true}));
    assert_eq!(
        command(&rig, FULL, "u", &unknown).await.status(),
        StatusCode::NOT_FOUND
    );
    // Keyer fields on a background verb are refused.
    let cut_bg = body("cut", json!({"keyer":"dsk1"}));
    assert_eq!(
        command(&rig, FULL, "bg", &cut_bg).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

/// Review H2: a retry of a `keyer_auto` whose ramp has started answers the
/// original 202 and `operation_id` (the idempotency record is resolved before
/// the state-dependent `keyer-ramping` admission), and enqueues nothing; a
/// fresh key for the same request is still refused 409 `keyer-ramping`.
#[tokio::test]
async fn a_keyer_auto_retry_during_its_ramp_replays_the_first_202() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    let auto = body(
        "keyer_auto",
        json!({"keyer":"dsk1","on":true,"duration_frames":10}),
    );
    let response = command(&rig, FULL, "ramp-1", &auto).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let op = body_json(response).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tick(&mut rig.driver, 1);
    assert!(rig.handle.snapshot().unwrap().keyers[0]
        .unwrap()
        .ramp
        .is_some());
    let replay = command(&rig, FULL, "ramp-1", &auto).await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED, "the retry replays");
    let replay = body_json(replay).await;
    assert_eq!(replay["operation_id"], op.as_str());
    assert_eq!(replay["kind"], "keyer_auto");
    // A different key is a new request: the ramp refuses it.
    let (status, kind) = kind_of(command(&rig, FULL, "ramp-2", &auto).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::CONFLICT, "keyer-ramping")
    );
    tick(&mut rig.driver, 2);
    // Only the ramp's own start outcome so far: the replay enqueued nothing.
    while let Ok(item) = outcomes.try_recv() {
        assert_eq!(item.event.operation_id, op, "nothing else was enqueued");
    }
}

#[tokio::test]
async fn a_full_mailbox_is_503_and_releases_the_key_and_the_leases() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut sent = 0;
    loop {
        let verb = body("keyer_tie", json!({"keyer":"dsk1","tie": sent % 2 == 0}));
        let response = command(&rig, FULL, &format!("flood-{sent}"), &verb).await;
        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        sent += 1;
        assert!(sent <= 64, "the mailbox is bounded");
    }
    assert_eq!(sent, 64);
    let define = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx2"}));
    let (status, kind) = kind_of(command(&rig, FULL, "set-busy", &define).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "engine-busy")
    );
    assert!(
        rig.leases.live.lock().unwrap().is_empty(),
        "a refused enqueue holds no lease"
    );
    tick(&mut rig.driver, 1);
    tick(&mut rig.driver, 2);
    tick(&mut rig.driver, 3);
    tick(&mut rig.driver, 4);
    // The key was released: the same key is admitted now.
    assert_eq!(
        command(&rig, FULL, "set-busy", &define).await.status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn in_n_is_refused_with_422_for_fill_and_key() {
    let (_dir, path) = temp_boot(DOC);
    let rig = rig(&path);
    let keys = support::seeded_keys();
    drop(keys);
    for extra in [
        json!({"keyer":"dsk1","fill":"in:1"}),
        json!({"keyer":"dsk1","key":"in:2"}),
    ] {
        let verb = body("keyer_set", extra);
        let (status, kind) = kind_of(command(&rig, ADMIN, "in", &verb).await).await;
        assert_eq!(
            (status, kind.as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "keyer-source-not-a-source"
            )
        );
        let (status, _) = kind_of(plan(&rig, ADMIN, &verb).await).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}

/// The merge with the transition API (#429): `transition` belongs to
/// `set_transition` only, so a keyer verb carrying one is a 422 on both the
/// plan and the command, and nothing is enqueued.
#[tokio::test]
async fn a_keyer_verb_carrying_a_transition_is_refused_with_422() {
    let (_dir, path) = temp_boot(DOC);
    let rig = rig(&path);
    let verb = body(
        "keyer_cut",
        json!({"keyer":"dsk1","on":true,"transition":{"kind":"cut"}}),
    );
    let (status, _) = kind_of(command(&rig, ADMIN, "kt", &verb).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = kind_of(plan(&rig, ADMIN, &verb).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn capacity_refusal_is_generic_for_scoped_callers_and_releases_everything() {
    let (_dir, path) = temp_boot(DOC);
    let rig = rig(&path);
    // dsk1 (full canvas, fill only) costs 2.0 of the 3.0 default budget; a
    // full-canvas dsk2 would cost 2.0 more.
    let verb = body("keyer_set", json!({"keyer":"dsk2","fill":"gfx2"}));
    let scoped = body_json(plan(&rig, FULL, &verb).await).await;
    assert_eq!(scoped["can_take"], false);
    assert_eq!(scoped["refusal"]["type"], "capacity");
    assert_eq!(scoped["cost"]["delta"], 2.0);
    assert!(scoped["cost"].get("reserved").is_none(), "{scoped}");
    assert!(scoped["cost"].get("budget").is_none());
    assert!(scoped["cost"].get("holders").is_none());
    assert!(!scoped["refusal"]["detail"]
        .as_str()
        .unwrap()
        .contains("main/dsk1"));
    let unscoped = body_json(plan(&rig, ADMIN, &verb).await).await;
    assert_eq!(unscoped["cost"]["reserved"], 4.0);
    assert_eq!(unscoped["cost"]["budget"], 3.0);
    assert_eq!(unscoped["cost"]["holders"], json!(["main/dsk1"]));
    // A repeated refused command leaves no lease, no reservation, no key.
    for _ in 0..3 {
        let (status, kind) = kind_of(command(&rig, FULL, "cap", &verb).await).await;
        assert_eq!((status, kind.as_str()), (StatusCode::CONFLICT, "capacity"));
    }
    assert!(rig.leases.acquired.lock().unwrap().is_empty());
    assert!(rig.state.idempotency.peek("cap").is_none());
    assert_eq!(rig.handle.keyer_definitions().unwrap().version, 0);
    // A fly small enough fits: 1×(16·16)/4096 + 1×(16·16)/4096.
    let small = body(
        "keyer_set",
        json!({"keyer":"dsk2","fill":"gfx2","fly":{"x":0,"y":0,"w":16,"h":16},"max_source":{"w":16,"h":16}}),
    );
    assert_eq!(
        body_json(plan(&rig, FULL, &small).await).await["can_take"],
        true
    );
    // Raising max_source is an admitted cost increase like any other.
    let raise = body(
        "keyer_set",
        json!({"keyer":"dsk1","max_source":{"w":128,"h":128}}),
    );
    let raised = body_json(plan(&rig, FULL, &raise).await).await;
    assert_eq!(raised["refusal"]["type"], "capacity", "{raised}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_air_protected_and_a_make_before_break_fill_change() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    command(
        &rig,
        FULL,
        "on",
        &body("keyer_cut", json!({"keyer":"dsk1","on":true})),
    )
    .await;
    tick(&mut rig.driver, 1);
    outcomes.try_recv().unwrap();
    let change = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx2"}));
    let planned = body_json(plan(&rig, FULL, &change).await).await;
    assert_eq!(planned["on_air"], true);
    assert_eq!(planned["refusal"]["type"], "on-air-protected");
    let (status, kind) = kind_of(command(&rig, FULL, "protect", &change).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::CONFLICT, "on-air-protected")
    );
    assert!(
        rig.leases.acquired.lock().unwrap().is_empty(),
        "no lease taken"
    );
    // A parameter change that keeps the sources is not protected.
    let rate = body("keyer_set", json!({"keyer":"dsk1","rate":30}));
    assert_eq!(
        body_json(plan(&rig, FULL, &rate).await).await["can_take"],
        true
    );
    let allow = body(
        "keyer_set",
        json!({"keyer":"dsk1","fill":"gfx2","on_air":"allow"}),
    );
    let op = body_json(command(&rig, FULL, "allow", &allow).await).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Make-before-break: the new fill is leased before the tick applies it.
    assert_eq!(
        *rig.leases.live.lock().unwrap(),
        vec!["keyer:main/dsk1:gfx2".to_owned()]
    );
    tick(&mut rig.driver, 2);
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(outcome.operation_id, op);
    assert!(outcome.completed, "{outcome:?}");
    let defs = rig.handle.keyer_definitions().unwrap();
    assert_eq!(&*defs.defs[0].as_ref().unwrap().fill_id, "gfx2");
    assert!(rig.handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    // After the outcome the request's lease is released (the demand set
    // holds the new sources) and the definition is persisted.
    wait_for(|| rig.leases.live.lock().unwrap().is_empty()).await;
    wait_for(|| persisted(&rig.state).switcher.unwrap().downstream_keyers[0].fill == "gfx2").await;
}

#[tokio::test]
async fn defining_the_first_dsk_while_clean_is_routed_is_refused() {
    let doc = DOC.split("[[switcher.downstream_keyers]]").next().unwrap();
    let (_dir, path) = temp_boot(doc);
    let rig = build(&path, doc, true);
    assert!(!rig.handle.keyer_definitions().unwrap().any());
    let verb = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx"}));
    let planned = body_json(plan(&rig, FULL, &verb).await).await;
    assert_eq!(planned["refusal"]["type"], "clean-unavailable-with-dsk");
    let (status, kind) = kind_of(command(&rig, FULL, "clean", &verb).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::CONFLICT, "clean-unavailable-with-dsk")
    );
    assert!(rig.leases.acquired.lock().unwrap().is_empty());
    assert!(
        !rig.handle.clean_gate().clean_refused(),
        "the refusal holds no Clean reservation"
    );
    // Without a Clean subscriber the first DSK is defined, and the shared
    // cell refuses Clean from the enqueue on.
    let (_dir2, path2) = temp_boot(doc);
    let mut rig = build(&path2, doc, false);
    let mut outcomes = rig.handle.outcomes();
    assert_eq!(
        command(&rig, FULL, "first", &verb).await.status(),
        StatusCode::ACCEPTED
    );
    assert!(rig.handle.clean_gate().clean_refused());
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    assert!(rig.handle.keyer_definitions().unwrap().any());
}

/// Review H4: a `keyer_set` still queued when the control plane's old
/// wall-clock settle deadline passed must never apply, and nothing may be
/// released while it still could. Settlement holds the leases and the DSK
/// reservation until a terminal engine outcome; the engine refuses a
/// `keyer_set` drained after its deadline tick (as a held take's
/// `TAKE_PRIME_DEADLINE_TICKS`), so it never applies and leaves nothing
/// behind: no definition, no lease, no reservation, nothing persisted.
#[tokio::test(start_paused = true)]
async fn a_keyer_set_held_past_its_deadline_never_applies_and_leaves_nothing() {
    let doc = DOC.split("[[switcher.downstream_keyers]]").next().unwrap();
    let (_dir, path) = temp_boot(doc);
    let mut rig = build(&path, doc, false);
    let mut outcomes = rig.handle.outcomes();
    let verb = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx"}));
    let response = command(&rig, FULL, "late", &verb).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let op = body_json(response).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The clock does not drain it for longer than any wall-clock deadline.
    // Paused time: the sleep auto-advances once every task is idle, so the
    // settle task runs past any wall-clock deadline it might hold.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(
        *rig.leases.live.lock().unwrap(),
        vec!["keyer:main/dsk1:gfx".to_owned()],
        "the command can still apply: its leases are held"
    );
    assert!(
        rig.handle.clean_gate().clean_refused(),
        "the command can still apply: Clean stays excluded"
    );
    // The clock drains it only after its deadline tick: refused, never applied.
    let late = 2 + multiview_engine::switcher_control::KEYER_SET_DEADLINE_TICKS;
    tick(&mut rig.driver, late);
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(outcome.operation_id, op);
    assert!(!outcome.completed);
    assert_eq!(
        outcome.error.as_deref(),
        Some(multiview_engine::keyer::REFUSE_KEYER_SET_DEADLINE)
    );
    assert!(!rig.handle.keyer_definitions().unwrap().any());
    wait_for(|| rig.leases.live.lock().unwrap().is_empty()).await;
    wait_for(|| !rig.handle.clean_gate().clean_refused()).await;
    assert_eq!(
        persisted(&rig.state).switcher.unwrap().downstream_keyers,
        Vec::new(),
        "nothing persisted"
    );
}

/// Fill the engine mailbox so the next enqueue fails (503).
fn fill_mailbox(handle: &SwitcherControl) {
    use multiview_engine::switcher_control::{SwitcherCommand, SwitcherVerb};
    while handle
        .try_send(SwitcherCommand {
            operation_id: "fill".into(),
            scene_version: 1,
            verb: SwitcherVerb::KeyerTie {
                keyer: 0,
                tie: true,
            },
        })
        .is_ok()
    {}
}

/// Review H3(b): request A reserves and queues the first DSK; request B
/// (also defining a new keyer, no DSK committed yet) reserves too and fails
/// its enqueue (503). B's settlement must release only its own reservation:
/// a Clean take stays refused while A is queued, and after A applies the
/// committed bit keeps it refused.
#[tokio::test]
async fn a_failed_sibling_never_releases_a_queued_first_dsk_reservation() {
    let doc = DOC.split("[[switcher.downstream_keyers]]").next().unwrap();
    let (_dir, path) = temp_boot(doc);
    let mut rig = build(&path, doc, false);
    let mut outcomes = rig.handle.outcomes();
    let gate = rig.handle.clean_gate();
    let a = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx"}));
    assert_eq!(
        command(&rig, FULL, "a", &a).await.status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(gate.dsk_reservations(), 1, "A holds one reservation");
    fill_mailbox(&rig.handle);
    let b = body("keyer_set", json!({"keyer":"dsk2","fill":"gfx2"}));
    let (status, kind) = kind_of(command(&rig, FULL, "b", &b).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "engine-busy")
    );
    assert_eq!(gate.dsk_reservations(), 1, "B released only its own");
    // The Clean take the router would admit now: refused, A is queued.
    assert!(
        gate.try_reserve_clean().is_none(),
        "a Clean take passed while A was queued"
    );
    tick(&mut rig.driver, 1);
    let mine = std::iter::from_fn(|| outcomes.try_recv().ok())
        .map(|item| item.event)
        .find(|o| o.operation_id != "fill")
        .unwrap();
    assert!(mine.completed, "{mine:?}");
    assert!(rig.handle.keyer_definitions().unwrap().any());
    wait_for(|| gate.dsk_reservations() == 0).await;
    assert!(gate.defined(), "the committed DSK keeps Clean refused");
    assert!(gate.try_reserve_clean().is_none());
}

/// Review H3(a), both orders, deterministically at the one admission point:
/// (1) a Clean take admitted first (the router holds its reservation, the
/// routing snapshot not yet published) refuses the first DSK with 409 and
/// leaves nothing behind; (2) a first DSK admitted first (queued, not yet
/// applied) refuses every Clean take until it settles.
#[tokio::test]
async fn clean_and_the_first_dsk_never_both_win_in_either_order() {
    let doc = DOC.split("[[switcher.downstream_keyers]]").next().unwrap();
    // (1) Clean first. The rig's routing snapshot shows no Clean: only the
    // gate knows, as in the window before the router publishes.
    let (_dir, path) = temp_boot(doc);
    let rig = build(&path, doc, false);
    let gate = rig.handle.clean_gate();
    let clean = gate.try_reserve_clean().unwrap();
    let verb = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx"}));
    assert_eq!(
        body_json(plan(&rig, FULL, &verb).await).await["refusal"]["type"],
        "clean-unavailable-with-dsk"
    );
    let (status, kind) = kind_of(command(&rig, FULL, "dsk", &verb).await).await;
    assert_eq!(
        (status, kind.as_str()),
        (StatusCode::CONFLICT, "clean-unavailable-with-dsk")
    );
    assert_eq!(gate.dsk_reservations(), 0);
    assert!(rig.leases.acquired.lock().unwrap().is_empty());
    assert!(rig.state.idempotency.peek("dsk").is_none());
    drop(clean);
    // (2) The first DSK first: queued, not applied; Clean is refused.
    let (_dir2, path2) = temp_boot(doc);
    let mut rig = build(&path2, doc, false);
    let gate = rig.handle.clean_gate();
    assert_eq!(
        command(&rig, FULL, "dsk", &verb).await.status(),
        StatusCode::ACCEPTED
    );
    assert!(
        gate.try_reserve_clean().is_none(),
        "Clean while a DSK is queued"
    );
    tick(&mut rig.driver, 1);
    assert!(gate.try_reserve_clean().is_none(), "Clean after it applied");
}

async fn next_keyer(
    s: &mut SessionStream,
) -> Option<(Option<String>, multiview_events::SwitcherKeyer)> {
    loop {
        match tokio::time::timeout(Duration::from_millis(500), s.next_delta()).await {
            Ok(Ok(Some(frame))) => {
                if let Event::SwitcherKeyer(e) = frame.envelope.payload {
                    return Some((frame.envelope.id, e));
                }
            }
            Ok(Ok(None)) => {}
            _ => return None,
        }
    }
}

/// GET lists only the keyer rows the caller may read, redacts unreadable
/// sources, drops them from tally; `switcher.keyer` events are scoped and
/// redacted the same way (parity for a scoped principal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_and_events_filter_and_redact_alike_for_a_scoped_principal() {
    let doc = DOC.replace("fill = \"gfx\"", "fill = \"gfx\"\nkey = \"secret\"");
    let (_dir, path) = temp_boot(&doc);
    let mut rig = build(&path, &doc, false);
    let session = |objects: &[&str], program: bool| {
        SessionStream::new(rig.publisher.subscribe(), "s", None).with_scopes(
            Some(objects.iter().map(|o| (*o).to_owned()).collect()),
            Some(if program {
                vec!["program:main".into()]
            } else {
                vec![]
            }),
            None,
        )
    };
    let mut full = session(&["me1", "main/dsk1", "a", "b", "gfx", "gfx2"], true);
    let mut no_keyer = session(&["me1", "a", "b", "gfx", "gfx2"], true);
    let mut no_program = session(&["me1", "main/dsk1", "a", "b", "gfx"], false);
    let mut no_fill = session(&["me1", "main/dsk1", "a", "b", "gfx2"], true);
    let response = command(
        &rig,
        ADMIN,
        "on",
        &body("keyer_cut", json!({"keyer":"dsk1","on":true})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // The hook resolves keyers (and so the tally) on a composed frame only;
    // this rig drains without the hook, so tally is the background roles.
    tick(&mut rig.driver, 1);
    let (id, event) = next_keyer(&mut full).await.expect("a readable keyer event");
    assert_eq!(id.as_deref(), Some("main/dsk1"));
    assert_eq!(
        (event.fill.as_str(), event.key.as_deref()),
        ("gfx", Some("redacted"))
    );
    assert!(event.on_air);
    assert!(
        next_keyer(&mut no_keyer).await.is_none(),
        "keyer object scope filters"
    );
    assert!(
        next_keyer(&mut no_program).await.is_none(),
        "program scope filters"
    );
    let (_, redacted) = next_keyer(&mut no_fill).await.unwrap();
    assert_eq!(redacted.fill, "redacted");

    // GET parity for the same principals (FULL lacks `secret`).
    let get_for = |token: &'static str| {
        let router = rig.router.clone();
        async move { body_json(send(&router, get("/api/v1/switcher/me1", token)).await).await }
    };
    let full_get = get_for(FULL).await;
    let row = &full_get["keyers"][0];
    assert_eq!(row["id"], "dsk1");
    assert_eq!(row["family"], "dsk");
    assert_eq!(
        (row["fill"].as_str(), row["key"].as_str()),
        (Some(event.fill.as_str()), event.key.as_deref())
    );
    assert_eq!(row["on_air"], event.on_air);
    assert_eq!(row["type"], "linear");
    assert!(row["ramp"].is_null());
    assert_eq!(row["rate"], 15);
    assert_eq!(row["loss"], "drop_to_off_air");
    assert_eq!(full_get["keyers"].as_array().unwrap().len(), 1);
    let no_keyer_get = get_for(NO_KEYER).await;
    assert_eq!(no_keyer_get["keyers"], json!([]));
    let no_fill_get = get_for(NO_FILL).await;
    assert_eq!(no_fill_get["keyers"][0]["fill"], redacted.fill.as_str());
    // Tally carries only readable ids.
    let admin = get_for(ADMIN).await;
    for list in ["program", "preview"] {
        for id in admin["tally"][list].as_array().unwrap() {
            assert!(id.is_string());
        }
        for id in full_get["tally"][list].as_array().unwrap() {
            assert_ne!(id, "secret");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applied_definitions_persist_and_a_restart_comes_up_with_keyers_off() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    command(
        &rig,
        FULL,
        "on",
        &body("keyer_cut", json!({"keyer":"dsk1","on":true})),
    )
    .await;
    tick(&mut rig.driver, 1);
    outcomes.try_recv().unwrap();
    let set = body(
        "keyer_set",
        json!({"keyer":"dsk2","fill":"gfx2","fly":{"x":0,"y":0,"w":16,"h":16},
               "max_source":{"w":16,"h":16},"type":"luma","clip":0.25,"gain":2.0,
               "on_at_start":true}),
    );
    assert_eq!(
        command(&rig, FULL, "def2", &set).await.status(),
        StatusCode::ACCEPTED
    );
    assert!(
        persisted(&rig.state)
            .switcher
            .unwrap()
            .downstream_keyers
            .len()
            == 1,
        "nothing is persisted before the engine applied it"
    );
    tick(&mut rig.driver, 2);
    assert!(outcomes.try_recv().unwrap().event.completed);
    wait_for(|| {
        persisted(&rig.state)
            .switcher
            .unwrap()
            .downstream_keyers
            .len()
            == 2
    })
    .await;
    let config = persisted(&rig.state);
    config.validate().unwrap();
    let dsks = &config.switcher.as_ref().unwrap().downstream_keyers;
    assert_eq!(dsks[1].id, "dsk2");
    assert_eq!(dsks[1].z, 2);
    assert!((dsks[1].clip - 0.25).abs() < 1e-6);
    let text = toml::to_string(&config).unwrap();
    assert!(!text.contains("on_air"), "on-air state is never persisted");
    // Restart: the persisted document boots (the preflight accepts it) with
    // every keyer off except the on_at_start intent.
    let reloaded = MultiviewConfig::load_from_toml(&text).unwrap();
    reloaded.validate().unwrap();
    let (_dir2, path2) = temp_boot(&text);
    let restarted = build(&path2, &text, false);
    let snapshot = restarted.handle.snapshot().unwrap();
    assert!(snapshot.keyers.iter().flatten().all(|k| !k.on_air));
    let defs = restarted.handle.keyer_definitions().unwrap();
    assert!(!defs.defs[0].as_ref().unwrap().on_at_start);
    assert!(defs.defs[1].as_ref().unwrap().on_at_start);
    assert!(
        restarted
            .driver
            .keyers()
            .flags(1)
            .unwrap()
            .pending
            .is_some(),
        "on_at_start comes up as the boot intent"
    );
    assert!(restarted
        .driver
        .keyers()
        .flags(0)
        .unwrap()
        .pending
        .is_none());
}

/// A rail driver that never runs anything: the J1 test only reads what the
/// keyer demand set was last published with (the manager is never started).
struct NoRails;
impl multiview_engine::RailDriver<Nv12Image> for NoRails {
    fn store(&self, _id: &str) -> Option<Arc<TileStore<Nv12Image>>> {
        None
    }
    fn transition(
        &self,
        _id: &str,
        _from: multiview_engine::Rails,
        to: multiview_engine::Rails,
        _requested: Option<multiview_engine::RequestedSize>,
    ) -> Result<multiview_engine::Rails, multiview_engine::RailFailure> {
        Ok(to)
    }
    fn resolve(&self, _id: &str, asked: multiview_engine::Rails) -> multiview_engine::Rails {
        asked
    }
    fn progress(&self, _id: &str, _rails: multiview_engine::Rails) -> u64 {
        0
    }
}

/// Review J1: two applied `keyer_set`s settle concurrently. A (version N)
/// is paused in reconciliation just before it takes the config-mutation
/// lock, B (N+1) applies and settles in full, then A resumes and runs last.
/// Neither the durable document nor the keyer demand set may go back to N:
/// reconciliation reads the latest committed definitions INSIDE the lock. A
/// read moved before the lock happens while A is paused, sees N, and fails
/// this test (checked by hand, review J1 round 3). Deterministic: the
/// settlement seam, no sleeps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_settlement_never_overwrites_a_newer_definition() {
    use multiview_control::routes::switcher_keyers::settle_seam;
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let manager = multiview_engine::DemandManager::new(
        multiview_engine::SourceRegistry::new(),
        Arc::new(NoRails),
        multiview_engine::DemandConfig::default(),
    );
    let demand = manager.consumer_set(
        multiview_engine::ConsumerClass::Keyer,
        multiview_engine::Rails::VIDEO,
    );
    rig.handle.set_keyer_demand(demand.clone());
    let mut outcomes = rig.handle.outcomes();
    let define = |fill: &str| {
        body(
            "keyer_set",
            json!({"keyer":"dsk2","fill":fill,"fly":{"x":0,"y":0,"w":16,"h":16},
                   "max_source":{"w":16,"h":16}}),
        )
    };
    let op_of = |v: Value| v["operation_id"].as_str().unwrap().to_owned();
    // A defines dsk2 on gfx (version N) and pauses just before the lock.
    let a = op_of(body_json(command(&rig, FULL, "a", &define("gfx")).await).await);
    let pause_a = settle_seam::pause(&a);
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    pause_a.reached.await.unwrap();
    let n = rig.handle.keyer_definitions().unwrap().version;
    // B moves dsk2 to gfx2 (N + 1) and settles in full while A is paused.
    let b = op_of(body_json(command(&rig, FULL, "b", &define("gfx2")).await).await);
    let pause_b = settle_seam::pause(&b);
    pause_b.release.send(()).unwrap();
    tick(&mut rig.driver, 2);
    assert!(outcomes.try_recv().unwrap().event.completed);
    pause_b.done.await.unwrap();
    assert_eq!(rig.handle.keyer_definitions().unwrap().version, n + 1);
    // A resumes and finishes last.
    pause_a.release.send(()).unwrap();
    pause_a.done.await.unwrap();
    let dsk2_fill = |config: MultiviewConfig| {
        config
            .switcher
            .unwrap()
            .downstream_keyers
            .into_iter()
            .find(|d| d.id == "dsk2")
            .map(|d| d.fill)
    };
    assert_eq!(
        dsk2_fill(persisted(&rig.state)).as_deref(),
        Some("gfx2"),
        "the durable document went back to an older definition"
    );
    let items = demand.items();
    let dsk2: Vec<&str> = items
        .iter()
        .filter(|i| &*i.label == "main/dsk2")
        .map(|i| &*i.id)
        .collect();
    assert_eq!(dsk2, vec!["gfx2"], "the demand set went back to N");
}

/// Issue #464 regression (merge with K4): the on-air media-edit guard
/// (`switcher_bus_consumers` → `declared_keyers`) must use the committed
/// runtime keyer definitions `keyer_definitions()` (K4 `keyer_set` changes
/// them after boot), not only the boot adopted config. After `keyer_set`
/// repoints `dsk1`'s fill from `gfx` to `gfx2`, a media edit of `gfx2` is
/// refused 409 naming `keyer fill main/dsk1`, and a media edit of `gfx`
/// (the OLD fill) is no longer refused by the keyer guard.
#[tokio::test]
async fn keyer_set_repointing_a_fill_moves_the_media_edit_guard_to_the_new_fill() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();

    // dsk1 starts off-air, on `gfx`: no `allow` is needed to repoint it.
    assert!(!rig.handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    let change = body("keyer_set", json!({"keyer":"dsk1","fill":"gfx2"}));
    let accepted = command(&rig, FULL, "repoint", &change).await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    let defs = rig.handle.keyer_definitions().unwrap();
    assert_eq!(&*defs.defs[0].as_ref().unwrap().fill_id, "gfx2");

    // A media edit of the NEW fill (`gfx2`) is refused, naming the keyer.
    let current = send(&rig.router, get("/api/v1/sources/gfx2", FULL)).await;
    assert_eq!(current.status(), StatusCode::OK);
    let tag = etag(&current).unwrap();
    let resp = send(
        &rig.router,
        put_json(
            "/api/v1/sources/gfx2",
            FULL,
            Some(&tag),
            &json!({ "name": "gfx2", "body": {"id": "gfx2", "kind": "solid", "color": "#ff00ff"} }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let detail = body_json(resp).await["detail"].as_str().unwrap().to_owned();
    assert!(
        detail.contains("keyer fill main/dsk1"),
        "the refusal must name the keyer now protecting its NEW fill: {detail}"
    );

    // A media edit of the OLD fill (`gfx`) is no longer refused by the
    // keyer guard: the committed runtime definition moved on.
    let current = send(&rig.router, get("/api/v1/sources/gfx", FULL)).await;
    assert_eq!(current.status(), StatusCode::OK);
    let tag = etag(&current).unwrap();
    let resp = send(
        &rig.router,
        put_json(
            "/api/v1/sources/gfx",
            FULL,
            Some(&tag),
            &json!({ "name": "gfx", "body": {"id": "gfx", "kind": "solid", "color": "#00ff00"} }),
        ),
    )
    .await;
    assert_ne!(
        resp.status(),
        StatusCode::CONFLICT,
        "the old fill must no longer be guarded by the keyer: {:?}",
        body_json(resp).await
    );
}

/// K7 (#392): `keyer_set` accepts `type = "chroma"` with its reference
/// `color`, applies it at the tick as a chroma key, persists it, and GET
/// reports it; a chroma key without a colour, with a key source, or a stray
/// colour on a luma key are 422 `validation` with typed reasons.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chroma_keyer_set_applies_persists_and_refuses_bad_shapes() {
    let (_dir, path) = temp_boot(DOC);
    let mut rig = rig(&path);
    let mut outcomes = rig.handle.outcomes();
    for (extra, needle) in [
        (
            json!({"keyer":"dsk1","type":"chroma"}),
            "chroma-color-required",
        ),
        (
            json!({"keyer":"dsk1","type":"chroma","color":"#00b140","key":"gfx2"}),
            "chroma-key-source",
        ),
        (
            json!({"keyer":"dsk1","type":"chroma","color":"nope"}),
            "chroma-color",
        ),
        (
            json!({"keyer":"dsk1","type":"luma","color":"#00b140"}),
            "chroma-color-without-chroma",
        ),
    ] {
        let verb = body("keyer_set", extra.clone());
        let response = command(&rig, FULL, &format!("bad-{needle}"), &verb).await;
        let status = response.status();
        let problem = body_json(response).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{extra}: {problem}"
        );
        assert!(
            problem.to_string().contains(needle),
            "{extra}: {problem} lacks {needle}"
        );
    }
    let set = body(
        "keyer_set",
        json!({"keyer":"dsk1","type":"chroma","color":"#00b140","clip":0.05,"gain":8.0,
               "invert":true}),
    );
    let planned = body_json(plan(&rig, FULL, &set).await).await;
    assert_eq!(planned["can_take"], true, "{planned}");
    assert_eq!(
        command(&rig, FULL, "chroma", &set).await.status(),
        StatusCode::ACCEPTED
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().unwrap().event.completed);
    let defs = rig.handle.keyer_definitions().unwrap();
    let def = defs.defs[0].as_ref().unwrap();
    assert_eq!(
        def.params.kind,
        multiview_compositor::key::KeyKind::Chroma {
            reference: [0x00, 0xb1, 0x40],
            clip: 0.05,
            gain: 8.0,
        }
    );
    assert!(def.params.invert);
    wait_for(|| {
        persisted(&rig.state).switcher.unwrap().downstream_keyers[0]
            .color
            .as_deref()
            == Some("#00b140")
    })
    .await;
    let config = persisted(&rig.state);
    config.validate().unwrap();
    let text = toml::to_string(&config).unwrap();
    MultiviewConfig::load_from_toml(&text)
        .unwrap()
        .validate()
        .unwrap();
    let get = body_json(send(&rig.router, get("/api/v1/switcher/me1", FULL)).await).await;
    assert_eq!(get["keyers"][0]["type"], "chroma", "{get}");
}
