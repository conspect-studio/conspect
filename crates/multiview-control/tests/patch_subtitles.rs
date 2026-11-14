//! #345 F2 — subtitle slots in `/api/v1/patch` against a fake engine seam:
//! plan and take, every typed refusal, BOLA and its ordering, the shared
//! take store (replay, 409 on another body, re-authorized replay), the
//! shared limiter, persistence of an applied re-patch and the `GET /patch`
//! subtitle list with redaction.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanRefusal, RoutedAudio,
    RoutedFeed, RoutingLive, SubtitleTakeError, SubtitleTakeRequest, TakeSubmitError,
};
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::{json, Value};
use support::{body_json, get, send, OPERATOR_TOKEN};

const DOC: &str = r##"
schema_version = 1
[canvas]
width = 640
height = 360
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "cam_a"
kind = "test"
captions = { mode = "auto" }
[[sources]]
id = "cam_b"
kind = "test"
captions = { mode = "auto" }
[[sources]]
id = "bars"
kind = "test"
[[sources]]
id = "secret-cam"
kind = "test"
captions = { mode = "auto" }
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam_a"
[[outputs]]
kind = "hls"
path = "/tmp/patch-subs/a/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs.slots.subtitle]]
language = "en"
name = "English"
source = "cam_a"
[[outputs.slots.subtitle]]
language = "es"
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
[[outputs]]
kind = "hls"
path = "/tmp/patch-subs/c/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs.slots.subtitle]]
language = "en"
source = "secret-cam"
"##;

const OUT_A: &str = "hls /tmp/patch-subs/a/index.m3u8";
const OUT_C: &str = "hls /tmp/patch-subs/c/index.m3u8";

/// output-0 on both axes; `cam_a`, `cam_b`, `bars`; not `secret-cam`.
const SUBS: &str = "subs-k.subs-secret";
/// output-0 on the object axis only.
const NO_AXIS: &str = "subs-noaxis.subs-noaxis-secret";
/// output-0 and output-2 on both axes; `cam_a` only.
const LIMITED: &str = "subs-lim.subs-lim-secret";

fn scoped(id: &str, objects: &[&str], outputs: &[&str]) -> Principal {
    Principal {
        key_id: id.to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(objects.iter().map(|s| (*s).to_owned()).collect()),
        scoped_output_ids: Some(outputs.iter().map(|s| (*s).to_owned()).collect()),
        scoped_discovery_domains: None,
    }
}

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    keys.register(
        "subs-k",
        "subs-secret",
        scoped(
            "subs-k",
            &["output-0", "output-9", "cam_a", "cam_b", "bars"],
            &["output-0", "output-9"],
        ),
    );
    keys.register(
        "subs-noaxis",
        "subs-noaxis-secret",
        scoped("subs-noaxis", &["output-0", "cam_a", "cam_b"], &[]),
    );
    keys.register(
        "subs-lim",
        "subs-lim-secret",
        scoped(
            "subs-lim",
            &["output-0", "output-2", "cam_a"],
            &["output-0", "output-2"],
        ),
    );
    keys
}

#[derive(Default)]
struct FakeState {
    subtitle_takes: Vec<SubtitleTakeRequest>,
    /// Live route tables by output config id (the engine's view).
    tables: std::collections::HashMap<String, Vec<Option<String>>>,
    busy: bool,
}

struct Fake {
    state: Mutex<FakeState>,
    /// When set, a take re-pointing any slot to `cam_b` waits (bounded) on
    /// it AFTER its swap: a forced "paused after the swap" schedule.
    hold_cam_b: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
    _tx: std::sync::mpsc::SyncSender<OutputOutcome>,
}

impl OutputRouting for Fake {
    fn plan(&self, _output: &str, _feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        Err(PlanRefusal::OutputKindUnroutable)
    }
    fn plan_audio(
        &self,
        _output: &str,
        _video: Option<&RoutedFeed>,
        _audio: Option<&RoutedAudio>,
    ) -> Result<multiview_control::output_routing::AudioPlanCost, PlanRefusal> {
        Err(PlanRefusal::OutputKindUnroutable)
    }
    fn try_take(&self, _take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        panic!("a subtitle patch never enqueues a video take");
    }
    fn try_take_subtitles(&self, take: SubtitleTakeRequest) -> Result<u64, SubtitleTakeError> {
        let mut state = self.state.lock().unwrap();
        if state.busy {
            return Err(SubtitleTakeError::Busy);
        }
        let Some(table) = state.tables.get_mut(&take.output) else {
            return Err(SubtitleTakeError::NotInRun);
        };
        for (slot, source) in &take.slots {
            table[*slot].clone_from(source);
        }
        let hold = take
            .slots
            .iter()
            .any(|(_, source)| source.as_deref() == Some("cam_b"));
        state.subtitle_takes.push(take);
        drop(state);
        if hold {
            if let Some(rx) = self.hold_cam_b.lock().unwrap().take() {
                // Bounded: under a correct serialization the other take
                // cannot finish while this one holds it.
                let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
            }
        }
        Ok(42)
    }
    fn subtitle_routes(&self, output: &str) -> Option<Vec<Option<String>>> {
        self.state.lock().unwrap().tables.get(output).cloned()
    }
    fn snapshot(&self) -> Option<RoutingLive> {
        None
    }
    fn subscribe_outcomes(&self) -> Option<Receiver<OutputOutcome>> {
        self.rx.lock().unwrap().take()
    }
}

struct Rig {
    router: axum::Router,
    fake: Arc<Fake>,
    keys: Arc<ApiKeyStore>,
    state: AppState,
}

impl Rig {
    fn takes(&self) -> Vec<SubtitleTakeRequest> {
        self.fake.state.lock().unwrap().subtitle_takes.clone()
    }
}

fn rig(path: &Path) -> Rig {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut tables = std::collections::HashMap::new();
    tables.insert(OUT_A.to_owned(), vec![Some("cam_a".to_owned()), None]);
    let fake = Arc::new(Fake {
        state: Mutex::new(FakeState {
            tables,
            ..FakeState::default()
        }),
        rx: Mutex::new(Some(rx)),
        _tx: tx,
        hold_cam_b: Mutex::new(None),
    });
    let seam: Arc<dyn OutputRouting> = fake.clone();
    let api_keys = Arc::new(keys());
    let state = AppState::new(
        Arc::clone(&engine),
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::clone(&api_keys),
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
    .with_output_routing(seam);
    Rig {
        router: multiview_control::router(state.clone()),
        fake,
        keys: api_keys,
        state,
    }
}

fn post(path: &str, token: &str, key: Option<&str>, body: &Value) -> Request<Body> {
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

fn sub(output: &str, slot: u32, source: Value) -> Value {
    json!({ "plane": "subtitle", "destination": {"output": output, "slot": slot}, "source": source })
}

fn patch(crosspoints: Vec<Value>) -> Value {
    json!({ "crosspoints": crosspoints })
}

async fn plan(rig: &Rig, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = send(&rig.router, post("/api/v1/patch/plan", token, None, &body)).await;
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn take(rig: &Rig, token: &str, key: &str, body: Value) -> (StatusCode, Value) {
    let resp = send(
        &rig.router,
        post("/api/v1/patch/take", token, Some(key), &body),
    )
    .await;
    let status = resp.status();
    (status, body_json(resp).await)
}

fn persisted_sources(rig: &Rig) -> Vec<Option<String>> {
    let config: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&rig.state).unwrap(),
    )
    .unwrap();
    config.outputs[0]
        .slots()
        .unwrap()
        .subtitle
        .iter()
        .map(|s| s.source.clone())
        .collect()
}

async fn wait_for<F: Fn() -> bool>(f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn plan_reports_each_subtitle_slot_and_notes_a_source_without_captions() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let (status, body) = plan(
        &rig,
        SUBS,
        patch(vec![
            sub("output-0", 1, json!({"source": "bars"})),
            sub("output-0", 0, json!("none")),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_take"], true, "{body}");
    let xps = body["crosspoints"].as_array().unwrap();
    assert_eq!(xps[0]["path"], "subtitle");
    assert_eq!(xps[0]["destination"]["slot"], 1);
    assert_eq!(xps[0]["note"], "source-has-no-captions", "{body}");
    assert!(xps[0]["refusal"].is_null());
    assert!(xps[1].get("note").is_none(), "{body}");
    // An unknown source is a plan refusal (the caller may read the id).
    let (status, body) = plan(
        &rig,
        OPERATOR_TOKEN,
        patch(vec![sub("output-0", 0, json!({"source": "ghost"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_take"], false);
    assert_eq!(body["crosspoints"][0]["refusal"]["type"], "unknown-source");
    assert!(rig.takes().is_empty(), "a plan changes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_repoints_the_slots_reports_applied_and_persists_the_sources() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let mut events = rig.state.engine.subscribe();
    let (status, body) = take(
        &rig,
        SUBS,
        "sub-1",
        patch(vec![
            sub("output-0", 0, json!({"source": "cam_b"})),
            sub("output-0", 1, json!({"source": "cam_a"})),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "accepted");
    let op = body["operation_id"].as_str().unwrap().to_owned();
    let takes = rig.takes();
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].output, OUT_A);
    assert_eq!(
        takes[0].slots,
        vec![(0, Some("cam_b".to_owned())), (1, Some("cam_a".to_owned()))]
    );
    assert_eq!(takes[0].principal, "subs-k");
    wait_for(|| {
        persisted_sources(&rig) == vec![Some("cam_b".to_owned()), Some("cam_a".to_owned())]
    })
    .await;
    // The outcome on `route.output`.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("route.output outcome")
            .unwrap();
        if let Event::RouteOutput(outcome) = event.event.as_ref() {
            assert_eq!(outcome.output, "output-0");
            assert_eq!(outcome.operation_id, op);
            assert_eq!(outcome.outcome, "subtitle:applied");
            assert_eq!(outcome.tick, 42);
            break;
        }
    }
    // Unpatch: "none" persists as an absent source.
    let (status, _) = take(
        &rig,
        SUBS,
        "sub-2",
        patch(vec![sub("output-0", 0, json!("none"))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_for(|| persisted_sources(&rig) == vec![None, Some("cam_a".to_owned())]).await;
}

#[tokio::test]
async fn subtitle_refusals_are_typed_422s_on_plan_and_take() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let cam = || json!({"source": "cam_a"});
    let cases: Vec<(Vec<Value>, &str, &str)> = vec![
        // Mixed with video.
        (
            vec![
                sub("output-0", 0, cam()),
                json!({ "plane": "video", "destination": {"output": "output-0", "slot": 0}, "source": cam() }),
            ],
            "unknown-slot",
            "#308",
        ),
        // Two outputs.
        (
            vec![sub("output-0", 0, cam()), sub("output-2", 0, cam())],
            "unknown-slot",
            "#308",
        ),
        // A slot twice.
        (
            vec![sub("output-0", 1, cam()), sub("output-0", 1, json!("none"))],
            "unknown-slot",
            "twice",
        ),
        // Beyond the declared slots.
        (
            vec![sub("output-0", 2, cam())],
            "unknown-slot",
            "declares 2 subtitle slot",
        ),
        // A kind without subtitles.
        (
            vec![sub("output-1", 0, cam())],
            "slot-medium-mismatch",
            "no subtitle slots",
        ),
        // A layout or bus has no subtitle essence.
        (
            vec![sub("output-0", 0, json!({"bus": "program"}))],
            "slot-medium-mismatch",
            "subtitle essence",
        ),
        (
            vec![sub("output-0", 0, json!({"layout": "NewsOne"}))],
            "slot-medium-mismatch",
            "subtitle essence",
        ),
        (
            vec![sub("output-0", 0, json!("follow"))],
            "unknown-source",
            "\"none\"",
        ),
    ];
    for (crosspoints, kind, detail) in cases {
        for path in ["/api/v1/patch/plan", "/api/v1/patch/take"] {
            let resp = send(
                &rig.router,
                post(
                    path,
                    OPERATOR_TOKEN,
                    Some("shape"),
                    &patch(crosspoints.clone()),
                ),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{path} {crosspoints:?}"
            );
            let problem = body_json(resp).await;
            assert!(
                problem["type"].as_str().unwrap().ends_with(kind),
                "{path}: {problem}"
            );
            assert!(
                problem["detail"].as_str().unwrap().contains(detail),
                "{path}: {problem}"
            );
        }
    }
    // An unknown source on take: the video path's `unknown-feed` mapping.
    let (status, problem) = take(
        &rig,
        OPERATOR_TOKEN,
        "ghost",
        patch(vec![sub("output-0", 0, json!({"source": "ghost"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(problem["type"].as_str().unwrap().ends_with("unknown-feed"));
    // A take without a key.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/patch/take",
            OPERATOR_TOKEN,
            None,
            &patch(vec![sub("output-0", 0, cam())]),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(rig.takes().is_empty(), "nothing reached the engine");
}

#[tokio::test]
async fn bola_is_checked_before_any_existence_answer() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = || patch(vec![sub("output-0", 0, json!({"source": "cam_b"}))]);
    // The output axis is denied: 403 on plan and take.
    assert_eq!(plan(&rig, NO_AXIS, body()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        take(&rig, NO_AXIS, "n", body()).await.0,
        StatusCode::FORBIDDEN
    );
    // Read on the source is denied: 403, even though the source exists.
    let secret = || patch(vec![sub("output-0", 0, json!({"source": "secret-cam"}))]);
    assert_eq!(plan(&rig, SUBS, secret()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        take(&rig, SUBS, "s", secret()).await.0,
        StatusCode::FORBIDDEN
    );
    // A source the caller may not read that does NOT exist: also 403.
    let ghost = patch(vec![sub("output-0", 0, json!({"source": "ghost"}))]);
    assert_eq!(plan(&rig, SUBS, ghost).await.0, StatusCode::FORBIDDEN);
    // An unknown output the caller has no grant for: 403, never 404.
    let unknown = patch(vec![sub("output-7", 0, json!({"source": "cam_a"}))]);
    assert_eq!(plan(&rig, SUBS, unknown).await.0, StatusCode::FORBIDDEN);
    // With the grant, an unknown output is 404 (the grant is the oracle gate).
    let granted = patch(vec![sub("output-9", 0, json!({"source": "cam_a"}))]);
    assert_eq!(plan(&rig, SUBS, granted).await.0, StatusCode::NOT_FOUND);
    // A denied end is 403 even when the shape is wrong (authz first).
    let bad_shape = patch(vec![
        sub("output-0", 0, json!({"source": "secret-cam"})),
        sub("output-0", 0, json!({"source": "secret-cam"})),
    ]);
    assert_eq!(plan(&rig, SUBS, bad_shape).await.0, StatusCode::FORBIDDEN);
    assert_eq!(rig.takes(), Vec::new());
}

#[tokio::test]
async fn a_key_replays_its_take_conflicts_on_another_body_and_is_reauthorized() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = || patch(vec![sub("output-0", 1, json!({"source": "cam_a"}))]);
    let (status, first) = take(&rig, LIMITED, "k", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, again) = take(&rig, LIMITED, "k", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["operation_id"], first["operation_id"]);
    assert_eq!(rig.takes().len(), 1, "a replay never re-applies");
    // The same key with another slot map: 409, nothing applied.
    let (status, problem) = take(
        &rig,
        LIMITED,
        "k",
        patch(vec![sub("output-0", 1, json!("none"))]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    // The same key with another output: 409 too.
    let (status, _) = take(
        &rig,
        LIMITED,
        "k",
        patch(vec![sub("output-2", 0, json!({"source": "cam_a"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rig.takes().len(), 1);
    // The key is in the slot-0 take namespace: a video alias take with it
    // is another take (409), not a fresh enqueue.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/output/take",
            LIMITED,
            Some("k"),
            &json!({ "target": {"kind": "output", "output": "output-0", "stream": "video"}, "feed": {"source": "cam_a"} }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    // Revoke the output axis: the replay is 403, no operation id.
    assert!(rig.keys.set_principal(
        "subs-lim",
        scoped(
            "subs-lim",
            &["output-0", "output-2", "cam_a"],
            &["output-2"]
        ),
    ));
    let (status, problem) = take(&rig, LIMITED, "k", body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert!(problem.get("operation_id").is_none());
}

#[tokio::test]
async fn subtitle_takes_share_the_output_and_principal_limits() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = |source: &str| patch(vec![sub("output-0", 0, json!({ "source": source }))]);
    assert_eq!(
        take(&rig, SUBS, "l1", body("cam_a")).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        take(&rig, SUBS, "l2", body("cam_b")).await.0,
        StatusCode::ACCEPTED
    );
    // Two per output per second: the third is 429 with Retry-After.
    let resp = send(
        &rig.router,
        post("/api/v1/patch/take", SUBS, Some("l3"), &body("cam_a")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(rig.takes().len(), 2);
    // A replay of an admitted key is still answered (no limiter charge).
    assert_eq!(
        take(&rig, SUBS, "l1", body("cam_a")).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(rig.takes().len(), 2);
}

#[tokio::test]
async fn between_runs_a_take_is_503_and_frees_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake.state.lock().unwrap().busy = true;
    let body = || patch(vec![sub("output-0", 0, json!({"source": "cam_b"}))]);
    let (status, problem) = take(&rig, SUBS, "busy", body()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert!(problem["type"].as_str().unwrap().ends_with("engine-busy"));
    rig.fake.state.lock().unwrap().busy = false;
    // The key was freed: the retry is a fresh take, applied.
    let (status, _) = take(&rig, SUBS, "busy", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(rig.takes().len(), 1);
}

#[tokio::test]
async fn get_lists_subtitle_slots_live_or_from_config_with_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake
        .state
        .lock()
        .unwrap()
        .tables
        .insert(OUT_A.to_owned(), vec![Some("secret-cam".to_owned()), None]);
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let outputs = all["outputs"].as_array().unwrap();
    let a = &outputs[0]["subtitle"];
    assert_eq!(a[0]["slot"], 0, "{all}");
    assert_eq!(a[0]["format"], "webvtt");
    assert_eq!(a[0]["language"], "en");
    assert_eq!(a[0]["name"], "English");
    assert_eq!(a[0]["live"], true);
    assert_eq!(a[0]["source"], json!({"source": "secret-cam"}));
    assert!(a[1]["source"].is_null());
    // The RTSP output has no subtitle slots.
    assert_eq!(outputs[1]["subtitle"], json!([]));
    // output-2 is not in the engine's tables: config, `live: false`.
    assert_eq!(outputs[2]["subtitle"][0]["live"], false);
    assert_eq!(
        outputs[2]["subtitle"][0]["source"],
        json!({"source": "secret-cam"})
    );
    // A key without Read on secret-cam sees it redacted (live and config).
    let limited = body_json(send(&rig.router, get("/api/v1/patch", LIMITED)).await).await;
    let outputs = limited["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 2, "{limited}");
    assert_eq!(
        outputs[0]["subtitle"][0]["source"],
        json!({"redacted": true})
    );
    assert_eq!(
        outputs[1]["subtitle"][0]["source"],
        json!({"redacted": true})
    );
}

/// Review COR-1 / AUZ-2 / INV-4: two concurrent takes on the same slot with a
/// forced inversion — take A (`cam_b`) swaps first and is then descheduled;
/// take B (`cam_a`) swaps last. Live state ends on B, so the accepted state
/// (what a restart starts from) must end on B too, and the outcomes are
/// published in swap order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_slot_takes_persist_the_last_swapped_source() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let (release_a, hold) = std::sync::mpsc::channel();
    *rig.fake.hold_cam_b.lock().unwrap() = Some(hold);
    let mut events = rig.state.engine.subscribe();
    let body = |source: &str| patch(vec![sub("output-0", 1, json!({ "source": source }))]);
    let router = rig.router.clone();
    let a = tokio::spawn(async move {
        let resp = send(
            &router,
            post(
                "/api/v1/patch/take",
                OPERATOR_TOKEN,
                Some("race-a"),
                &body("cam_b"),
            ),
        )
        .await;
        (resp.status(), body_json(resp).await)
    });
    // A has swapped (the fake recorded it) and is held.
    wait_for(|| rig.takes().len() == 1).await;
    let router = rig.router.clone();
    let b = tokio::spawn(async move {
        let resp = send(
            &router,
            post(
                "/api/v1/patch/take",
                OPERATOR_TOKEN,
                Some("race-b"),
                &body("cam_a"),
            ),
        )
        .await;
        (resp.status(), body_json(resp).await)
    });
    // Let B run to completion (persistence included) if nothing orders it
    // after A; then release A.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let _ = release_a.send(());
    let (status_a, body_a) = a.await.unwrap();
    let (status_b, body_b) = b.await.unwrap();
    assert_eq!(status_a, StatusCode::ACCEPTED, "{body_a}");
    assert_eq!(status_b, StatusCode::ACCEPTED, "{body_b}");
    let takes = rig.takes();
    assert_eq!(takes.len(), 2);
    let last = takes[1].slots[0].1.clone();
    assert_eq!(last.as_deref(), Some("cam_a"), "B swapped last");
    let live = rig.fake.state.lock().unwrap().tables[OUT_A].clone();
    assert_eq!(live[1], last);
    // Both outcomes published, in swap order.
    let mut ops = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while ops.len() < 2 {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("two route.output outcomes")
            .unwrap();
        if let Event::RouteOutput(outcome) = event.event.as_ref() {
            ops.push(outcome.operation_id.clone());
        }
    }
    assert_eq!(
        ops,
        vec![
            body_a["operation_id"].as_str().unwrap().to_owned(),
            body_b["operation_id"].as_str().unwrap().to_owned()
        ]
    );
    // The accepted state (a restart's start) agrees with the live route.
    assert_eq!(persisted_sources(&rig)[1], last);
    let control = rig.state.output_routing.as_ref().unwrap();
    wait_for(|| control.subtitle_persist_pending() == 0).await;
    assert_eq!(persisted_sources(&rig)[1], last);
}

/// Review AUZ-1 / INV-3: persistence is bounded. With the store blocked
/// (the config-mutation lock held), takes fill the bounded queue; the next
/// take is refused 503 BEFORE the engine is asked (its route unchanged, its
/// key free), and the pending work never exceeds the bound. Releasing the
/// store drains it and persists the applied sources.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_persistence_refuses_overload_without_touching_routes() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake
        .state
        .lock()
        .unwrap()
        .tables
        .insert(OUT_C.to_owned(), vec![Some("secret-cam".to_owned())]);
    let control = Arc::clone(rig.state.output_routing.as_ref().unwrap());
    control.set_subtitle_persist_capacity(2);
    let blocked = rig.state.lock_config_mutation().await;
    let (status, _) = take(
        &rig,
        OPERATOR_TOKEN,
        "o-1",
        patch(vec![sub("output-0", 0, json!({"source": "cam_b"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = take(
        &rig,
        OPERATOR_TOKEN,
        "o-2",
        patch(vec![sub("output-2", 0, json!({"source": "cam_a"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(control.subtitle_persist_pending(), 2);
    // Full: refused, the engine is not asked, the route is unchanged.
    let (status, problem) = take(
        &rig,
        OPERATOR_TOKEN,
        "o-3",
        patch(vec![sub("output-0", 1, json!({"source": "cam_a"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert!(problem["type"].as_str().unwrap().ends_with("engine-busy"));
    assert_eq!(
        rig.takes().len(),
        2,
        "the refused take never reached the engine"
    );
    assert_eq!(rig.fake.state.lock().unwrap().tables[OUT_A][1], None);
    assert_eq!(control.subtitle_persist_pending(), 2, "bounded");
    // Unblock: the queue drains and persists.
    drop(blocked);
    wait_for(|| control.subtitle_persist_pending() == 0).await;
    assert_eq!(persisted_sources(&rig)[0].as_deref(), Some("cam_b"));
    // The refused key was freed and its limiter slot refunded: once the
    // output's window allows, the same key is a fresh take.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, _) = take(
        &rig,
        OPERATOR_TOKEN,
        "o-3",
        patch(vec![sub("output-0", 1, json!({"source": "cam_a"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(rig.takes().len(), 3);
}

/// Review COR-2: a configured slot beyond the run's (shorter) live route
/// table is not live: it reports its configured source, `live: false`,
/// redacted for a caller without Read on it.
#[tokio::test]
async fn get_reports_a_slot_beyond_the_live_table_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    // output-2 declares one slot (secret-cam); the run's table has none.
    rig.fake
        .state
        .lock()
        .unwrap()
        .tables
        .insert(OUT_C.to_owned(), Vec::new());
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let slot = &all["outputs"][2]["subtitle"][0];
    assert_eq!(slot["live"], false, "{all}");
    assert_eq!(slot["source"], json!({"source": "secret-cam"}), "{all}");
    // Output-0's live slots stay live.
    assert_eq!(all["outputs"][0]["subtitle"][1]["live"], true);
    let limited = body_json(send(&rig.router, get("/api/v1/patch", LIMITED)).await).await;
    let slot = &limited["outputs"][1]["subtitle"][0];
    assert_eq!(slot["live"], false, "{limited}");
    assert_eq!(slot["source"], json!({"redacted": true}));
}

/// Review INV-1 (control side): once the run stopped (the engine has no
/// live table and refuses `Busy`), `GET /patch` reports the configured
/// sources `live: false` and a take is 503 `engine-busy` with no outcome.
#[tokio::test]
async fn after_the_run_stops_get_is_not_live_and_a_take_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    {
        let mut state = rig.fake.state.lock().unwrap();
        state.tables.clear();
        state.busy = true;
    }
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let a = &all["outputs"][0]["subtitle"];
    assert_eq!(a[0]["live"], false, "{all}");
    assert_eq!(a[0]["source"], json!({"source": "cam_a"}));
    let (status, problem) = take(
        &rig,
        OPERATOR_TOKEN,
        "stopped",
        patch(vec![sub("output-0", 0, json!({"source": "cam_b"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(rig.takes(), Vec::new());
    let control = rig.state.output_routing.as_ref().unwrap();
    assert_eq!(control.subtitle_persist_pending(), 0);
}
