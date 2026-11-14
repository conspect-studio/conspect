//! #342 B2 — output slots in `/api/v1/patch` against a fake engine seam:
//! the GET listing (per-object omission, redaction), the plan's §12.4 path
//! and cost, typed refusals, BOLA on plan/take, the shared take machinery
//! with `/routing/output/take` (one idempotency store, one limiter), and a
//! take flood that never reaches the engine beyond the limiter.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    OutputLive, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanPath, PlanRefusal,
    RoutedAudio, RoutedFeed, RoutingLive, SlotPlan, TakeSubmitError,
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
id = "a"
kind = "test"
[[sources]]
id = "bars"
kind = "test"
[[sources]]
id = "secret-cam"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "/tmp/patch-outputs/a/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
kind = "hls"
path = "/tmp/patch-outputs/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.format]
width = 360
height = 640
fit = { mode = "cover" }
[[outputs.slots.audio]]
language = "eng"
"##;

const OUT_A: &str = "hls /tmp/patch-outputs/a/index.m3u8";
const OUT_B: &str = "hls /tmp/patch-outputs/b/index.m3u8";

/// output-1 on both axes, bars only (no secret-cam, no Program scope).
const OUT1: &str = "patch-out1.patch-out1-secret";
/// output-1 on the object axis only: no output-axis grant.
const NO_OUTPUT_AXIS: &str = "patch-noaxis.patch-noaxis-secret";
/// output-1 on both axes, bars and the track name `eng`, not `secret-cam`.
const TRACK_ONLY: &str = "patch-track.patch-track-secret";
/// A second principal with output-0 + output-1 on both axes and bars.
const OTHER: &str = "patch-other.patch-other-secret";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    let scoped = |id: &str, objects: &[&str], outputs: &[&str]| Principal {
        key_id: id.to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(objects.iter().map(|s| (*s).to_owned()).collect()),
        scoped_output_ids: Some(outputs.iter().map(|s| (*s).to_owned()).collect()),
        scoped_discovery_domains: None,
    };
    keys.register(
        "patch-out1",
        "patch-out1-secret",
        scoped("patch-out1", &["output-1", "bars"], &["output-1"]),
    );
    // The layouts `NewsOne`/`Secret` and the source `a`, but not `secret-cam`.
    keys.register(
        "patch-lay",
        "patch-lay-secret",
        scoped(
            "patch-lay",
            // `output-9` does not exist (review T-A1's oracle probe).
            &["output-1", "output-9", "Secret", "NewsOne", "a", "bars"],
            &["output-1", "output-9"],
        ),
    );
    keys.register(
        "patch-noaxis",
        "patch-noaxis-secret",
        scoped("patch-noaxis", &["output-1", "bars"], &[]),
    );
    keys.register(
        "patch-track",
        "patch-track-secret",
        scoped("patch-track", &["output-1", "bars", "eng"], &["output-1"]),
    );
    keys.register(
        "patch-other",
        "patch-other-secret",
        scoped(
            "patch-other",
            &["output-0", "output-1", "bars"],
            &["output-0", "output-1"],
        ),
    );
    keys
}

#[derive(Default)]
struct FakeState {
    takes: Vec<OutputTakeRequest>,
    reformats: Vec<multiview_control::output_routing::OutputReformatRequest>,
    /// When set, the fake engine's reset mailbox is full (review X2).
    reformat_busy: bool,
    paths: HashMap<(String, RoutedFeed), (PlanPath, bool, u8)>,
    live: Option<RoutingLive>,
}

struct Fake {
    state: Mutex<FakeState>,
    /// Race seam: when armed, every take-time plan waits here, so two
    /// concurrent takes both pass the replay peek before either reserves.
    rendezvous: Mutex<Option<Arc<std::sync::atomic::AtomicUsize>>>,
    tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        Arc::new(Self {
            state: Mutex::new(FakeState::default()),
            rendezvous: Mutex::new(None),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
    fn takes(&self) -> Vec<OutputTakeRequest> {
        self.state.lock().unwrap().takes.clone()
    }
    fn set_path(
        &self,
        output: &str,
        feed: RoutedFeed,
        path: PlanPath,
        encoder: bool,
        converts: u8,
    ) {
        self.state
            .lock()
            .unwrap()
            .paths
            .insert((output.to_owned(), feed), (path, encoder, converts));
    }
}

impl OutputRouting for Fake {
    fn plan(&self, _output: &str, feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        let rendezvous = self.rendezvous.lock().unwrap().clone();
        if let Some(arrived) = rendezvous {
            // Wait (bounded: a request refused before the plan never hangs
            // the other) until both concurrent takes are here.
            arrived.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while arrived.load(std::sync::atomic::Ordering::SeqCst) < 2
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        if *feed == RoutedFeed::Preview {
            return Err(PlanRefusal::SwitcherRequired);
        }
        Ok(PlanCost {
            needs_encoder: *feed != RoutedFeed::Program,
            sessions_in_use: 1,
            sessions_max: 6,
            needs_aux_canvas: matches!(feed, RoutedFeed::Layout(_)),
        })
    }
    fn plan_slot(&self, output: &str, feed: &RoutedFeed) -> Result<SlotPlan, PlanRefusal> {
        let cost = self.plan(output, feed)?;
        let found = self
            .state
            .lock()
            .unwrap()
            .paths
            .get(&(output.to_owned(), feed.clone()))
            .copied();
        let (path, needs_encoder, converts) =
            found.unwrap_or((PlanPath::P1, cost.needs_encoder, 0));
        Ok(SlotPlan {
            path,
            cost: PlanCost {
                needs_encoder,
                ..cost
            },
            converts,
        })
    }
    fn plan_audio(
        &self,
        _output: &str,
        _video: Option<&RoutedFeed>,
        _audio: Option<&multiview_control::output_routing::RoutedAudio>,
    ) -> Result<multiview_control::output_routing::AudioPlanCost, PlanRefusal> {
        Ok(multiview_control::output_routing::AudioPlanCost::default())
    }
    fn try_reformat(
        &self,
        request: multiview_control::output_routing::OutputReformatRequest,
    ) -> Result<(), multiview_control::output_routing::ReformatSubmitError> {
        // The fake engine resets HLS outputs live, anything else needs a
        // restart (the real router decides by the binding's container).
        if !matches!(request.config, multiview_config::Output::Hls { .. }) {
            return Err(multiview_control::output_routing::ReformatSubmitError::RestartRequired);
        }
        let mut state = self.state.lock().unwrap();
        if state.reformat_busy {
            return Err(multiview_control::output_routing::ReformatSubmitError::Busy);
        }
        state.reformats.push(request);
        Ok(())
    }
    fn current_path(&self, output: &str) -> Option<PlanPath> {
        (output == OUT_B).then_some(PlanPath::P3)
    }
    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        self.state.lock().unwrap().takes.push(take);
        Ok(())
    }
    fn snapshot(&self) -> Option<RoutingLive> {
        self.state.lock().unwrap().live.clone()
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

fn rig(path: &Path) -> Rig {
    rig_with(path, DOC)
}

fn rig_with(path: &Path, doc: &str) -> Rig {
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let fake = Fake::new();
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

fn video(output: &str, source: Value) -> Value {
    json!({ "plane": "video", "destination": {"output": output, "slot": 0}, "source": source })
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

fn alias(output: &str, feed: Value) -> Value {
    json!({ "target": {"kind": "output", "output": output, "stream": "video"}, "feed": feed })
}

#[tokio::test]
async fn get_lists_output_slots_with_redaction_and_per_object_omission() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs: HashMap::from([
            (
                OUT_A.to_owned(),
                OutputLive {
                    bus: "program".to_owned(),
                    since_tick: 1,
                    pending: None,
                    degraded: false,
                    audio: None,
                    pending_audio: None,
                    private_audio: false,
                },
            ),
            (
                OUT_B.to_owned(),
                OutputLive {
                    bus: "src:secret-cam".to_owned(),
                    since_tick: 5,
                    pending: Some(("src:bars".to_owned(), 3)),
                    degraded: true,
                    audio: None,
                    pending_audio: None,
                    private_audio: false,
                },
            ),
        ]),
        ..RoutingLive::default()
    });
    // A whole-system operator sees both outputs, with format and slots.
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let outputs = all["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 2, "{all}");
    assert_eq!(outputs[0]["output"], "output-0");
    assert!(outputs[0]["format"].is_null(), "no format = the canvas");
    assert_eq!(outputs[0]["video"][0]["source"], json!({"bus": "program"}));
    assert_eq!(outputs[0]["video"][0]["slot"], 0);
    assert_eq!(outputs[0]["audio"][0]["source"], "follow");
    let b = &outputs[1];
    assert_eq!(b["format"]["width"], 360);
    assert_eq!(b["format"]["fit"]["mode"], "cover");
    assert_eq!(b["video"][0]["source"], json!({"source": "secret-cam"}));
    assert_eq!(b["video"][0]["path"], "P3");
    assert_eq!(b["video"][0]["degraded"], true);
    assert_eq!(
        b["video"][0]["pending"]["source"],
        json!({"source": "bars"})
    );
    assert_eq!(b["audio"][0]["language"], "eng");
    // A key with output-1 on both axes sees only output-1; the source it
    // cannot read is redacted, the pending one it can read is shown.
    let scoped = body_json(send(&rig.router, get("/api/v1/patch", OUT1)).await).await;
    let outputs = scoped["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 1, "{scoped}");
    assert_eq!(outputs[0]["output"], "output-1");
    assert_eq!(outputs[0]["video"][0]["source"], json!({"redacted": true}));
    assert_eq!(
        outputs[0]["video"][0]["pending"]["source"],
        json!({"source": "bars"})
    );
    // Without the output-axis grant the output is not listed at all.
    let none = body_json(send(&rig.router, get("/api/v1/patch", NO_OUTPUT_AXIS)).await).await;
    assert_eq!(none["outputs"], json!([]), "{none}");
}

#[tokio::test]
async fn bola_without_the_output_grant_is_403_on_plan_and_take() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = patch(vec![video("output-1", json!({"source": "bars"}))]);
    let (status, _) = plan(&rig, NO_OUTPUT_AXIS, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = take(&rig, NO_OUTPUT_AXIS, "k", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An unknown output the caller has no grant for is 403 too (no oracle).
    let body = patch(vec![video("output-9", json!({"source": "bars"}))]);
    let (status, _) = plan(&rig, OUT1, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A source the caller cannot read: 403 before existence.
    let body = patch(vec![video("output-1", json!({"source": "secret-cam"}))]);
    let (status, _) = take(&rig, OUT1, "k2", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(rig.fake.takes().len(), 0, "nothing reached the engine");
}

#[tokio::test]
async fn plan_reports_the_path_and_cost() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake.set_path(
        OUT_A,
        RoutedFeed::Source("bars".into()),
        PlanPath::P2,
        true,
        0,
    );
    rig.fake.set_path(
        OUT_B,
        RoutedFeed::Source("bars".into()),
        PlanPath::P3,
        true,
        1,
    );
    let (status, p1) = plan(
        &rig,
        OPERATOR_TOKEN,
        patch(vec![video("output-0", json!({"bus": "program"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{p1}");
    assert_eq!(p1["class"], "class1");
    assert_eq!(p1["can_take"], true);
    let xp = &p1["crosspoints"][0];
    assert_eq!(xp["path"], "P1");
    assert_eq!(xp["cost"]["new_encoders"], 0);
    assert_eq!(xp["cost"]["converts"], 0);
    assert_eq!(xp["cost"]["sessions_in_use"], 1);
    assert_eq!(xp["cost"]["sessions_max"], 6);
    assert_eq!(xp["cost"]["fits"], true);
    let (_, p2) = plan(
        &rig,
        OPERATOR_TOKEN,
        patch(vec![video("output-0", json!({"source": "bars"}))]),
    )
    .await;
    assert_eq!(p2["crosspoints"][0]["path"], "P2", "{p2}");
    assert_eq!(p2["crosspoints"][0]["cost"]["new_encoders"], 1);
    assert_eq!(p2["crosspoints"][0]["cost"]["converts"], 0);
    // A cover output with audio slot 0 follow: the video is P3 + 1 convert,
    // the audio rides it.
    let (_, p3) = plan(
        &rig,
        OPERATOR_TOKEN,
        patch(vec![
            video("output-1", json!({"source": "bars"})),
            json!({ "plane": "audio", "destination": {"output": "output-1", "slot": 0}, "source": "follow" }),
        ]),
    )
    .await;
    assert_eq!(p3["crosspoints"][0]["path"], "P3", "{p3}");
    assert_eq!(p3["crosspoints"][0]["cost"]["converts"], 1);
    assert_eq!(p3["crosspoints"][1]["path"], "follow");
    // A scoped principal never sees session counts.
    let (_, scoped) = plan(
        &rig,
        OUT1,
        patch(vec![video("output-1", json!({"source": "bars"}))]),
    )
    .await;
    assert!(
        scoped["crosspoints"][0]["cost"]
            .get("sessions_in_use")
            .is_none(),
        "{scoped}"
    );
    // A plan refusal is reported, not thrown: Preview without an M/E.
    let (status, refused) = plan(
        &rig,
        OPERATOR_TOKEN,
        patch(vec![video("output-0", json!({"bus": "preview"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(refused["can_take"], false);
    assert_eq!(
        refused["crosspoints"][0]["refusal"]["type"],
        "switcher-required"
    );
}

#[tokio::test]
async fn unsupported_output_slots_are_typed_422s() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let bars = || json!({"source": "bars"});
    let cases = [
        (
            vec![
                json!({ "plane": "video", "destination": {"output": "output-0", "slot": 1}, "source": bars() }),
            ],
            "unknown-slot",
            "video slot 1",
        ),
        (
            vec![
                json!({ "plane": "subtitle", "destination": {"output": "output-0", "slot": 0}, "source": bars() }),
            ],
            "slot-medium-mismatch",
            "#345",
        ),
        (
            vec![
                json!({ "plane": "audio", "destination": {"output": "output-0", "slot": 1}, "source": bars() }),
            ],
            "slot-medium-mismatch",
            "fixed by the output's configuration",
        ),
        // A layout on video slot 0 is routable since #368 (see
        // `a_layout_patches_onto_output_video_slot_0`); an unknown one is an
        // unknown feed.
        (
            vec![video("output-0", bars()), video("output-1", bars())],
            "unknown-slot",
            "#308",
        ),
        (
            vec![
                video("output-0", bars()),
                json!({ "plane": "video", "destination": {"mixer": "me1", "input": 4}, "source": bars() }),
            ],
            "unknown-slot",
            "#308",
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
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
            let problem = body_json(resp).await;
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
    assert!(rig.fake.takes().is_empty(), "nothing reached the engine");
}

#[tokio::test]
async fn the_patch_take_and_the_alias_share_one_idempotency_store_and_limiter() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    // A patch take is 202 {operation_id, status: accepted}.
    let (status, first) = take(
        &rig,
        OPERATOR_TOKEN,
        "shared-key",
        patch(vec![video("output-0", json!({"source": "bars"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(first["status"], "accepted");
    let op = first["operation_id"].as_str().unwrap().to_owned();
    let takes = rig.fake.takes();
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].output, OUT_A);
    assert_eq!(takes[0].video, Some(RoutedFeed::Source("bars".into())));
    // The same key on the alias replays the same operation; nothing new.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/output/take",
            OPERATOR_TOKEN,
            Some("shared-key"),
            &alias("output-0", json!({"source": "bars"})),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let replay = body_json(resp).await;
    assert_eq!(replay["operation_id"], op.as_str(), "{replay}");
    assert_eq!(replay["status"], "pending");
    assert_eq!(rig.fake.takes().len(), 1, "a replay never re-enqueues");
    // And back: a key first used on the alias replays on /patch/take.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/output/take",
            OPERATOR_TOKEN,
            Some("alias-first"),
            &alias("output-1", json!({"source": "bars"})),
        ),
    )
    .await;
    let alias_op = body_json(resp).await["operation_id"].clone();
    let (status, replay) = take(
        &rig,
        OPERATOR_TOKEN,
        "alias-first",
        patch(vec![video("output-1", json!({"source": "bars"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(replay["operation_id"], alias_op);
    assert_eq!(rig.fake.takes().len(), 2);
    // One per-output limiter: output-0 has had one take (2/s): one more on
    // the alias, then the patch take is 429.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/output/take",
            OPERATOR_TOKEN,
            Some("o0-second"),
            &alias("output-0", json!({"bus": "program"})),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let (status, _) = take(
        &rig,
        OPERATOR_TOKEN,
        "o0-third",
        patch(vec![video("output-0", json!({"bus": "program"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rig.fake.takes().len(), 3);
}

/// Chaos (§12.10 "take flood"): 2,000 patch takes at one output within about
/// a second are almost all 429; the engine mailbox never sees more than the
/// limiter allows (2 per output per second); another principal's admissible
/// take on another output still gets 202.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_flood_is_429_and_never_starves_another_principal() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Arc::new(rig(&dir.path().join("boot.toml")));
    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for worker in 0..8 {
        let rig = Arc::clone(&rig);
        tasks.push(tokio::spawn(async move {
            let mut counts = (0usize, 0usize, 0usize);
            for i in 0..250 {
                let (status, _) = take(
                    &rig,
                    OPERATOR_TOKEN,
                    &format!("flood-{worker}-{i}"),
                    patch(vec![video("output-1", json!({"source": "bars"}))]),
                )
                .await;
                match status {
                    StatusCode::ACCEPTED => counts.0 += 1,
                    StatusCode::TOO_MANY_REQUESTS => counts.1 += 1,
                    _ => counts.2 += 1,
                }
            }
            counts
        }));
    }
    // Another principal, another output, mid-flood.
    let (status, body) = take(
        &rig,
        OTHER,
        "other-1",
        patch(vec![video("output-0", json!({"source": "bars"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let mut accepted = 0;
    let mut limited = 0;
    let mut other = 0;
    for task in tasks {
        let (a, l, o) = task.await.unwrap();
        accepted += a;
        limited += l;
        other += o;
    }
    let elapsed = started.elapsed();
    assert_eq!(other, 0, "every flood answer is 202 or 429");
    assert_eq!(accepted + limited, 2000);
    // The per-output limit is 2 per sliding second.
    let seconds = usize::try_from(elapsed.as_secs()).unwrap() + 1;
    let allowed = 2 * (seconds + 1);
    assert!(accepted <= allowed, "{accepted} accepted in {elapsed:?}");
    let flood_takes = rig
        .fake
        .takes()
        .iter()
        .filter(|t| t.principal == "operator-key")
        .count();
    assert_eq!(
        flood_takes, accepted,
        "only admitted takes reach the mailbox"
    );
    assert!(limited >= 2000 - allowed);
    assert!(rig
        .fake
        .takes()
        .iter()
        .any(|t| t.principal == "patch-other" && t.output == OUT_A));
}

async fn alias_take(rig: &Rig, token: &str, key: &str, output: &str, feed: Value) -> StatusCode {
    send(
        &rig.router,
        post(
            "/api/v1/routing/output/take",
            token,
            Some(key),
            &alias(output, feed),
        ),
    )
    .await
    .status()
}

/// Review A1: a take key is bound to its (output, feed) across both routes:
/// the same key with another output or feed is 409 on either route and
/// enqueues nothing; the same body still replays its first operation.
#[tokio::test]
async fn a_reused_key_with_another_body_is_409_on_both_routes() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let (status, first) = take(
        &rig,
        OPERATOR_TOKEN,
        "bound",
        patch(vec![video("output-0", json!({"source": "bars"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // Another output, another feed, on the alias and on the patch route.
    for (output, feed) in [
        ("output-1", json!({"source": "bars"})),
        ("output-0", json!({"source": "a"})),
        ("output-1", json!({"bus": "program"})),
    ] {
        assert_eq!(
            alias_take(&rig, OPERATOR_TOKEN, "bound", output, feed.clone()).await,
            StatusCode::CONFLICT,
            "alias {output} {feed}"
        );
        let (status, problem) = take(
            &rig,
            OPERATOR_TOKEN,
            "bound",
            patch(vec![video(output, feed.clone())]),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "patch {output} {feed}");
        assert!(
            problem["detail"].as_str().unwrap().contains("reused"),
            "{problem}"
        );
    }
    assert_eq!(rig.fake.takes().len(), 1, "zero extra enqueues");
    // The same body replays on both routes.
    let (status, replay) = take(
        &rig,
        OPERATOR_TOKEN,
        "bound",
        patch(vec![video("output-0", json!({"source": "bars"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(replay["operation_id"], first["operation_id"]);
    assert_eq!(
        alias_take(
            &rig,
            OPERATOR_TOKEN,
            "bound",
            "output-0",
            json!({"source": "bars"})
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert_eq!(rig.fake.takes().len(), 1);
}

/// Review A1/A2: a replay is re-authorized under the principal's CURRENT
/// grants on both routes: a key accepted while the principal held the output
/// axis is 403 on either route once that grant is gone, and nothing (no op
/// id, no outcome) is returned.
#[tokio::test]
async fn a_replay_is_reauthorized_and_403_once_the_output_axis_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = || patch(vec![video("output-1", json!({"source": "bars"}))]);
    let (status, _) = take(&rig, OUT1, "k", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // Revoke the output axis (the object grant stays).
    assert!(rig.keys.set_principal(
        "patch-out1",
        Principal {
            key_id: "patch-out1".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(vec!["output-1".to_owned(), "bars".to_owned()]),
            scoped_output_ids: Some(vec![]),
            scoped_discovery_domains: None,
        },
    ));
    let (status, problem) = take(&rig, OUT1, "k", body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert!(problem.get("operation_id").is_none());
    assert_eq!(
        alias_take(&rig, OUT1, "k", "output-1", json!({"source": "bars"})).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(rig.fake.takes().len(), 1);
}

/// Review A2: the alias requires the output axis: an object grant without it
/// is 403 on `/routing/output/take` (and plan), before existence.
#[tokio::test]
async fn the_alias_requires_the_output_axis() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    for output in ["output-1", "no-such-output"] {
        assert_eq!(
            alias_take(&rig, NO_OUTPUT_AXIS, "x", output, json!({"source": "bars"})).await,
            StatusCode::FORBIDDEN,
            "{output}"
        );
        let resp = send(
            &rig.router,
            post(
                "/api/v1/routing/plan",
                NO_OUTPUT_AXIS,
                None,
                &alias(output, json!({"source": "bars"})),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "plan {output}");
    }
    // The same object grant WITH the output axis is accepted.
    assert_eq!(
        alias_take(&rig, OUT1, "y", "output-1", json!({"source": "bars"})).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(rig.fake.takes().len(), 1);
}

/// Review A3: one principal flooding MANY outputs is capped by the
/// per-principal limit, not just the per-output one: over 12 outputs (12 x
/// 2/s = 24/s of per-output room) at most 10 takes per started second reach
/// the engine; every other answer is 429.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_principal_flooding_many_outputs_is_capped_at_ten_per_second() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = DOC.to_owned();
    for i in 0..10 {
        let _ = write!(
            doc,
            "[[outputs]]\nkind = \"hls\"\npath = \"/tmp/patch-outputs/m{i}/index.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n"
        );
    }
    let rig = rig_with(&dir.path().join("boot.toml"), &doc);
    let started = std::time::Instant::now();
    let mut accepted = 0usize;
    let mut limited = 0usize;
    for i in 0..1200 {
        let output = format!("output-{}", i % 12);
        let (status, _) = take(
            &rig,
            OPERATOR_TOKEN,
            &format!("many-{i}"),
            patch(vec![video(&output, json!({"bus": "program"}))]),
        )
        .await;
        match status {
            StatusCode::ACCEPTED => accepted += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected {other}"),
        }
    }
    let seconds = usize::try_from(started.elapsed().as_secs()).unwrap() + 1;
    assert_eq!(accepted + limited, 1200);
    assert!(accepted >= 10, "the first window admits ten");
    assert!(
        accepted <= 10 * (seconds + 1),
        "{accepted} admitted in {seconds}s: the principal cap is 10/s"
    );
    assert_eq!(
        rig.fake.takes().len(),
        accepted,
        "only admitted takes reach the mailbox"
    );
    let outputs: std::collections::HashSet<_> =
        rig.fake.takes().iter().map(|t| t.output.clone()).collect();
    assert!(
        outputs.len() >= 5,
        "takes spread over many outputs: {outputs:?}"
    );
}

/// Review M3: audio slot 0 reports the output's ACCEPTED audio feed exactly
/// as `GET /routing` projects it (default `follow`), redacted for a caller
/// who may not read it; audio slots i > 0 keep their configured track.
#[tokio::test]
async fn get_reports_the_accepted_audio_feed_of_slot_zero() {
    let dir = tempfile::tempdir().unwrap();
    let doc = format!(
        "{DOC}\n[[routing.output]]\noutput = \"{OUT_B}\"\nprogram = \"main\"\naudio = {{ source = \"secret-cam\" }}\n"
    );
    let rig = rig_with(&dir.path().join("boot.toml"), &doc);
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let outputs = all["outputs"].as_array().unwrap();
    assert_eq!(outputs[0]["audio"][0]["source"], "follow", "{all}");
    assert_eq!(
        outputs[1]["audio"][0]["source"],
        json!({"source": "secret-cam"}),
        "{all}"
    );
    // The routing readback agrees for the same caller.
    let routing = body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await;
    let row = routing["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["output"] == "output-1")
        .cloned()
        .unwrap();
    assert_eq!(row["audio"], outputs[1]["audio"][0]["source"], "{routing}");
    // A caller who may not read secret-cam sees it redacted.
    let scoped = body_json(send(&rig.router, get("/api/v1/patch", OUT1)).await).await;
    assert_eq!(
        scoped["outputs"][0]["audio"][0]["source"],
        json!({"redacted": true}),
        "{scoped}"
    );
}

/// Review A1-r2: the body binding is atomic with the reservation. Two
/// concurrent requests of one principal with the SAME key and two DIFFERENT
/// authorized bodies (one per route, other outputs) are released together
/// through a barrier, 50 times: every iteration gives exactly one 202 and
/// one 409, enqueues exactly one take, and the 202's operation id is that
/// take's — the loser never receives the winner's operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_reuse_of_one_key_with_two_bodies_is_one_202_and_one_409() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = DOC.to_owned();
    for i in 0..18 {
        let _ = write!(
            doc,
            "[[outputs]]\nkind = \"hls\"\npath = \"/tmp/patch-outputs/r{i}/index.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n"
        );
    }
    let rig = Arc::new(rig_with(&dir.path().join("boot.toml"), &doc));
    for iteration in 0..50usize {
        // Two fresh outputs per iteration (20 outputs, cycled), so neither
        // per-output limit is hit; the per-principal limit (10/s) is waited
        // out below.
        let a = format!("output-{}", (2 * iteration) % 20);
        let b = format!("output-{}", (2 * iteration + 1) % 20);
        let key = format!("race-{iteration}");
        let before = rig.fake.takes().len();
        // Each request runs on its own OS thread and runtime, released
        // together by a barrier; both meet again inside the take-time plan
        // gate (the fake's rendezvous), i.e. AFTER the replay peek and BEFORE
        // the reservation: the race the store's own lock must decide.
        *rig.fake.rendezvous.lock().unwrap() =
            Some(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let run = |request: Request<Body>| {
            let (router, barrier) = (rig.router.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                barrier.wait();
                runtime.block_on(async move {
                    let resp = send(&router, request).await;
                    let status = resp.status();
                    (status, body_json(resp).await)
                })
            })
        };
        let patch_side = run(post(
            "/api/v1/patch/take",
            OPERATOR_TOKEN,
            Some(&key),
            &patch(vec![video(&a, json!({"source": "bars"}))]),
        ));
        let alias_side = run(post(
            "/api/v1/routing/output/take",
            OPERATOR_TOKEN,
            Some(&key),
            &alias(&b, json!({"bus": "program"})),
        ));
        let (p_status, p_body) = patch_side.join().unwrap();
        let (a_status, a_body) = alias_side.join().unwrap();
        let arrived = rig.fake.rendezvous.lock().unwrap().take();
        assert_eq!(
            arrived.map(|a| a.load(std::sync::atomic::Ordering::SeqCst)),
            Some(2),
            "iteration {iteration}: both requests passed the replay peek"
        );
        let mut statuses = [p_status, a_status];
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::ACCEPTED, StatusCode::CONFLICT],
            "iteration {iteration}: {p_body} {a_body}"
        );
        let takes = rig.fake.takes();
        assert_eq!(
            takes.len(),
            before + 1,
            "iteration {iteration}: one enqueue"
        );
        let winner = if p_status == StatusCode::ACCEPTED {
            &p_body
        } else {
            &a_body
        };
        let loser = if p_status == StatusCode::ACCEPTED {
            &a_body
        } else {
            &p_body
        };
        assert_eq!(
            winner["operation_id"].as_str().unwrap(),
            takes[before].operation_id,
            "iteration {iteration}: the 202 names the enqueued take"
        );
        assert!(loser.get("operation_id").is_none(), "{loser}");
        // The enqueued take is the winner's body.
        let expected = if p_status == StatusCode::ACCEPTED {
            &a
        } else {
            &b
        };
        assert_eq!(
            takes[before].output,
            format!("hls /tmp/patch-outputs/{}/index.m3u8", api_path(expected)),
            "iteration {iteration}"
        );
        // Stay well under the per-principal limit (10/s): two requests a
        // round, about four a second.
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    }
}

/// The config path segment of a test output API id (`output-0` = a,
/// `output-1` = b, `output-N` = r{N-2}).
fn api_path(api_id: &str) -> String {
    let n: usize = api_id.trim_start_matches("output-").parse().unwrap();
    match n {
        0 => "a".to_owned(),
        1 => "b".to_owned(),
        n => format!("r{}", n - 2),
    }
}

/// Review M6: a `tracks` audio feed whose route input the caller may not
/// read is `{redacted: true}` on BOTH `GET /patch` (audio slot 0) and
/// `GET /routing` (the row's `audio`): one rule, so the endpoints never
/// disagree. A caller who may read the input sees the tracks on both.
#[tokio::test]
async fn a_track_feed_with_an_unreadable_route_input_is_redacted_on_both_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let doc = format!(
        "{DOC}\n[audio]\nsample_rate_hz = 48000\n[[audio.routes]]\ninput_id = \"secret-cam\"\nchannels = {{ kind = \"stereo\" }}\ntarget_track = \"eng\"\n\n[[routing.output]]\noutput = \"{OUT_B}\"\nprogram = \"main\"\naudio = {{ tracks = [\"eng\"] }}\n"
    );
    let rig = rig_with(&dir.path().join("boot.toml"), &doc);
    // TRACK_ONLY: output-1 on both axes, bars and the track name `eng` —
    // but not the track's route input `secret-cam`.
    let track_token = TRACK_ONLY;
    let routing_audio = |body: &Value| {
        body["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["output"] == "output-1")
            .map(|r| r["audio"].clone())
            .unwrap()
    };
    let patch_audio = |body: &Value| {
        body["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["output"] == "output-1")
            .map(|o| o["audio"][0]["source"].clone())
            .unwrap()
    };
    // The scoped caller: redacted on both.
    let patch = body_json(send(&rig.router, get("/api/v1/patch", track_token)).await).await;
    let routing = body_json(send(&rig.router, get("/api/v1/routing", track_token)).await).await;
    assert_eq!(patch_audio(&patch), json!({"redacted": true}), "{patch}");
    assert_eq!(
        routing_audio(&routing),
        json!({"redacted": true}),
        "{routing}"
    );
    // The whole-system operator: the tracks on both, identically.
    let patch = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let routing = body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await;
    assert_eq!(patch_audio(&patch), json!({"tracks": ["eng"]}), "{patch}");
    assert_eq!(patch_audio(&patch), routing_audio(&routing));
}

fn put_output(token: &str, id: &str, etag: &str, name: &str, body: &Value) -> Request<Body> {
    let mut request = post(
        &format!("/api/v1/outputs/{id}"),
        token,
        None,
        &json!({"name": name, "body": body}),
    );
    *request.method_mut() = axum::http::Method::PUT;
    request
        .headers_mut()
        .insert(header::IF_MATCH, etag.parse().unwrap());
    request
}

async fn current_output(rig: &Rig, id: &str) -> (String, String, Value) {
    let resp = send(
        &rig.router,
        get(&format!("/api/v1/outputs/{id}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = body_json(resp).await;
    (
        etag,
        body["name"].as_str().unwrap().to_owned(),
        body["body"].clone(),
    )
}

/// #342 B3: a PUT changing ONLY an HLS output's format (and video bitrate)
/// is committed and applied live: `x-multiview-apply: live`, the reset note,
/// the operation id, and one reformat with the new config reaches the
/// engine; any other change (a codec change) keeps the restart answer and
/// starts no reset.
#[tokio::test]
async fn a_format_only_put_resets_the_output_live_and_a_codec_change_is_restart() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let (etag, name, mut body) = current_output(&rig, "output-1").await;
    body["format"] = json!({"width": 720, "height": 1280, "fit": {"mode": "cover"}});
    body["slots"]["video"] = json!([{"bitrate_kbps": 3000}]);
    let resp = send(
        &rig.router,
        put_output(OPERATOR_TOKEN, "output-1", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
    assert_eq!(
        resp.headers()["x-multiview-apply-note"],
        "resets this output (about 1 s); Program and other outputs are not affected",
        "a static note, never the raw id (review X3)"
    );
    let op = resp.headers()["x-multiview-operation-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let reformats = rig.fake.state.lock().unwrap().reformats.clone();
    assert_eq!(reformats.len(), 1);
    assert_eq!(reformats[0].operation_id, op);
    assert_eq!(reformats[0].output, OUT_B);
    assert_eq!(reformats[0].principal, "operator-key");
    let format = reformats[0].config.format().unwrap();
    assert_eq!((format.width, format.height), (720, 1280));
    // The commit is the store: GET returns the new format.
    let (etag, name, stored) = current_output(&rig, "output-1").await;
    assert_eq!(stored["format"]["width"], 720);
    // A codec change: restart, no reset.
    let mut codec = stored.clone();
    codec["codec"] = json!("h264");
    let resp = send(
        &rig.router,
        put_output(OPERATOR_TOKEN, "output-1", &etag, &name, &codec),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-multiview-apply"], "restart");
    assert!(resp.headers().get("x-multiview-operation-id").is_none());
    assert_eq!(rig.fake.state.lock().unwrap().reformats.len(), 1);
}

/// #342 B3: the live reset is authorized on the output axis too (BOLA): a
/// key with the output object but without the output-axis grant is 403 and
/// nothing is committed or reset.
#[tokio::test]
async fn a_live_reset_needs_the_output_axis() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let (etag, name, mut body) = current_output(&rig, "output-1").await;
    body["format"]["width"] = json!(720);
    let resp = send(
        &rig.router,
        put_output(NO_OUTPUT_AXIS, "output-1", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(rig.fake.state.lock().unwrap().reformats.len(), 0);
    let (after, _, stored) = current_output(&rig, "output-1").await;
    assert_eq!(after, etag, "nothing committed");
    assert_eq!(stored["format"]["width"], 360);
    // With the output axis it is applied live.
    let resp = send(
        &rig.router,
        put_output(OUT1, "output-1", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
}

/// #342 B3: live resets are rate-limited per output with the take limiter
/// (one reset = one take: 2/s per output): the third reset within a second
/// is 429 with Retry-After and is neither committed nor sent.
#[tokio::test]
async fn live_resets_are_rate_limited_like_takes() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    for (i, width) in [720, 1080, 540].into_iter().enumerate() {
        let (etag, name, mut body) = current_output(&rig, "output-1").await;
        body["format"]["width"] = json!(width);
        let resp = send(
            &rig.router,
            put_output(OPERATOR_TOKEN, "output-1", &etag, &name, &body),
        )
        .await;
        if i < 2 {
            assert_eq!(resp.status(), StatusCode::OK, "reset {i}");
            assert_eq!(resp.headers()["x-multiview-apply"], "live");
        } else {
            assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(resp.headers().get(header::RETRY_AFTER).is_some());
            let (after, _, stored) = current_output(&rig, "output-1").await;
            assert_eq!(after, etag, "a limited reset commits nothing");
            assert_eq!(stored["format"]["width"], 1080);
        }
    }
    assert_eq!(rig.fake.state.lock().unwrap().reformats.len(), 2);
}

/// Review X2: a live reset the engine cannot take (its mailbox is full) is
/// 503 `engine-busy`, the store is rolled back to the running body (GET
/// shows the old format), and the rate-limit slot is refunded (two more
/// resets in the same second still pass).
#[tokio::test]
async fn a_busy_engine_rolls_the_reset_back_and_charges_no_rate_limit() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake.state.lock().unwrap().reformat_busy = true;
    let (etag, name, mut body) = current_output(&rig, "output-1").await;
    body["format"]["width"] = json!(720);
    let resp = send(
        &rig.router,
        put_output(OPERATOR_TOKEN, "output-1", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_json(resp).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap_or_default()
            .contains("engine-busy"),
        "{problem}"
    );
    let (_, _, stored) = current_output(&rig, "output-1").await;
    assert_eq!(
        stored["format"]["width"], 360,
        "rolled back to the running body"
    );
    assert_eq!(rig.fake.state.lock().unwrap().reformats.len(), 0);
    // The refused attempt charged nothing: two resets still pass (2/s).
    rig.fake.state.lock().unwrap().reformat_busy = false;
    for width in [720, 1080] {
        let (etag, name, mut body) = current_output(&rig, "output-1").await;
        body["format"]["width"] = json!(width);
        let resp = send(
            &rig.router,
            put_output(OPERATOR_TOKEN, "output-1", &etag, &name, &body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "{width}");
        assert_eq!(resp.headers()["x-multiview-apply"], "live");
    }
    assert_eq!(rig.fake.state.lock().unwrap().reformats.len(), 2);
}

/// Review M-4: a PUT whose format/bitrate differ only RAW (an explicit
/// default bitrate for an absent one) but not EFFECTIVELY is a no-op: it is
/// committed and answered applied, with no reset started and no operation id.
#[tokio::test]
async fn an_effectively_equal_format_put_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let (etag, name, mut body) = current_output(&rig, "output-1").await;
    assert!(
        body.pointer("/slots/video/0/bitrate_kbps").is_none(),
        "the fixture has no explicit bitrate: {body}"
    );
    body["slots"]["video"] = json!([{"bitrate_kbps": 4000}]);
    let resp = send(
        &rig.router,
        put_output(OPERATOR_TOKEN, "output-1", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
    assert!(resp.headers().get("x-multiview-operation-id").is_none());
    assert_eq!(
        rig.fake.state.lock().unwrap().reformats.len(),
        0,
        "no reset"
    );
    let (_, _, stored) = current_output(&rig, "output-1").await;
    assert_eq!(
        stored["slots"]["video"][0]["bitrate_kbps"], 4000,
        "committed"
    );
    // An explicit canvas-size, default-fit format on an output without one
    // equals the absent format: also a no-op.
    let (etag, name, mut body) = current_output(&rig, "output-0").await;
    assert!(body.get("format").is_none(), "{body}");
    body["format"] = json!({"width": 640, "height": 360});
    let resp = send(
        &rig.router,
        put_output(OPERATOR_TOKEN, "output-0", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
    assert!(resp.headers().get("x-multiview-operation-id").is_none());
    assert_eq!(
        rig.fake.state.lock().unwrap().reformats.len(),
        0,
        "no reset"
    );
    // A real change on it is a reset.
    let (etag, name, mut body) = current_output(&rig, "output-0").await;
    body["format"] = json!({"width": 320, "height": 180});
    let resp = send(
        &rig.router,
        put_output(OPERATOR_TOKEN, "output-0", &etag, &name, &body),
    )
    .await;
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
    assert!(resp.headers().get("x-multiview-operation-id").is_some());
    assert_eq!(rig.fake.state.lock().unwrap().reformats.len(), 1);
}

/// Store a layout (two cells: `a` and `right`) in the repository.
fn store_layout(rig: &Rig, id: &str, right: &str) {
    rig.state
        .repository
        .create_layout(
            id,
            multiview_control::repository::LayoutInput {
                name: id.to_owned(),
                body: json!({
                    "canvas": {"width": 640, "height": 360, "fps": "30000/1001"},
                    "layout": {"kind": "grid", "columns": ["1fr", "1fr"], "rows": ["1fr"], "areas": ["a b"]},
                    "cells": [
                        {"id": "l", "area": "a", "source": {"input_id": "a"}},
                        {"id": "r", "area": "b", "source": {"input_id": right}},
                    ],
                }),
            },
        )
        .unwrap();
}

/// #342 B2 x #368: `{layout}` on an output's video slot 0 goes through the
/// shared slot-0 take (the alias's machinery): it plans (+1 encoder), takes
/// with the resolved layout body, and replays; an unknown layout is
/// `unknown-source`, and a principal that may read the layout but not every
/// source it shows is a bare 403 (plan and take), with nothing enqueued.
#[tokio::test]
async fn a_layout_patches_onto_output_video_slot_0() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "bars");
    store_layout(&rig, "Secret", "secret-cam");
    let body = patch(vec![video("output-0", json!({"layout": "NewsOne"}))]);
    let (status, plan_body) = plan(&rig, OPERATOR_TOKEN, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{plan_body}");
    assert_eq!(plan_body["can_take"], true, "{plan_body}");
    let (status, first) = take(&rig, OPERATOR_TOKEN, "lay-1", body.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    {
        let takes = rig.fake.takes();
        assert_eq!(takes.len(), 1);
        assert_eq!(
            takes[0].video,
            Some(RoutedFeed::Layout("NewsOne".to_owned()))
        );
        let layout = takes[0]
            .layout
            .as_ref()
            .expect("the resolved layout body rides the take");
        assert_eq!(layout.0.id, "NewsOne");
    }
    // A replay of the same key answers the same operation, never re-enqueues.
    let (status, again) = take(&rig, OPERATOR_TOKEN, "lay-1", body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["operation_id"], first["operation_id"]);
    assert_eq!(rig.fake.takes().len(), 1);
    // Unknown layout: unknown-source in the plan, a typed 422 on take.
    let unknown = patch(vec![video("output-0", json!({"layout": "Nope"}))]);
    let (status, plan_body) = plan(&rig, OPERATOR_TOKEN, unknown.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(plan_body["can_take"], false, "{plan_body}");
    assert_eq!(
        plan_body["crosspoints"][0]["refusal"]["type"],
        "unknown-source"
    );
    let (status, _) = take(&rig, OPERATOR_TOKEN, "lay-2", unknown).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // `patch-lay` may read the layout `Secret` and output-1, but not the
    // `secret-cam` source the layout shows (§6.3 source closure).
    let token = "patch-lay.patch-lay-secret";
    let secret = patch(vec![video("output-1", json!({"layout": "Secret"}))]);
    let (status, _) = plan(&rig, token, secret.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the layout's closure is authorized"
    );
    let (status, _) = take(&rig, token, "lay-3", secret).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(rig.fake.takes().len(), 1, "nothing more reached the engine");
}

/// #342 B2 x #368: a replayed layout take is re-authorized on the layout's
/// whole source closure under the CURRENT grants (the shared slot-0 key):
/// once a source the layout shows is revoked, the replay of the very same
/// key is a 403 on `/patch/take` and on the alias, and nothing re-enqueues.
#[tokio::test]
async fn a_layout_replay_reauthorizes_its_source_closure() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "bars");
    let token = "patch-lay.patch-lay-secret";
    let body = || patch(vec![video("output-1", json!({"layout": "NewsOne"}))]);
    let (status, first) = take(&rig, token, "k", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    // Revoke `bars` (shown by the layout); the layout object stays.
    assert!(rig.keys.set_principal(
        "patch-lay",
        Principal {
            key_id: "patch-lay".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(vec![
                "output-1".to_owned(),
                "NewsOne".to_owned(),
                "a".to_owned(),
            ]),
            scoped_output_ids: Some(vec!["output-1".to_owned()]),
            scoped_discovery_domains: None,
        },
    ));
    let (status, problem) = take(&rig, token, "k", body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert!(problem.get("operation_id").is_none());
    assert_eq!(
        alias_take(&rig, token, "k", "output-1", json!({"layout": "NewsOne"})).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(rig.fake.takes().len(), 1);
}

/// Review T-A1: a `{layout}` feed's whole source closure is authorized with
/// the other end checks, BEFORE any output lookup: a principal holding the
/// output axis and the layout id but not a source the layout shows gets the
/// SAME generic 403 for an existing and a nonexistent output, on
/// `/patch/plan`, `/patch/take` and the alias (take and plan) — never a
/// 404-vs-403 existence oracle. Nothing reaches the engine.
#[tokio::test]
async fn a_denied_layout_closure_is_403_before_the_output_lookup_on_every_route() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "Secret", "secret-cam");
    let token = "patch-lay.patch-lay-secret";
    let mut answers = Vec::new();
    for output in ["output-1", "output-9"] {
        let body = patch(vec![video(output, json!({"layout": "Secret"}))]);
        let (plan_status, plan_body) = plan(&rig, token, body.clone()).await;
        let (take_status, take_body) = take(&rig, token, &format!("ta1-{output}"), body).await;
        let alias_take_status = alias_take(
            &rig,
            token,
            &format!("ta1a-{output}"),
            output,
            json!({"layout": "Secret"}),
        )
        .await;
        let alias_plan = send(
            &rig.router,
            post(
                "/api/v1/routing/plan",
                token,
                None,
                &alias(output, json!({"layout": "Secret"})),
            ),
        )
        .await;
        let alias_plan_status = alias_plan.status();
        let alias_plan_body = body_json(alias_plan).await;
        for (route, status) in [
            ("patch/plan", plan_status),
            ("patch/take", take_status),
            ("alias take", alias_take_status),
            ("alias plan", alias_plan_status),
        ] {
            assert_eq!(status, StatusCode::FORBIDDEN, "{route} on {output}");
        }
        answers.push((
            plan_body["detail"].clone(),
            take_body["detail"].clone(),
            alias_plan_body["detail"].clone(),
        ));
    }
    assert_eq!(
        answers[0], answers[1],
        "identical answers: no existence oracle"
    );
    assert!(rig.fake.takes().is_empty(), "nothing reached the engine");
}

fn bars() -> Value {
    json!({"source": "bars"})
}

fn audio_follow(output: &str) -> Value {
    json!({ "plane": "audio", "destination": {"output": output, "slot": 0}, "source": "follow" })
}

/// The persisted `[[routing.output]]` row of an output's config id.
fn persisted_row(rig: &Rig, output: &str) -> Option<Value> {
    let config: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&rig.state).unwrap(),
    )
    .unwrap();
    config
        .routing
        .as_ref()
        .and_then(|t| t.output.iter().find(|xp| xp.output == output))
        .map(|xp| serde_json::to_value(xp).unwrap())
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

/// Review T-M1: audio slot 0 `"follow"` rides the patch take as the alias's
/// `audio: "follow"`. An output with an explicit audio choice
/// (`secret-cam`), patched `{video, audio follow}`: the engine take request
/// carries `Follow`, and once applied the persisted row's audio is follow
/// (the explicit choice is gone). Without audio slot 0 the take keeps the
/// output's choice (`audio: None`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_slot_zero_follow_rides_the_take_and_persists_follow() {
    let dir = tempfile::tempdir().unwrap();
    let doc = format!(
        "{DOC}\n[[routing.output]]\noutput = \"{OUT_B}\"\nprogram = \"main\"\naudio = {{ source = \"secret-cam\" }}\n"
    );
    let rig = rig_with(&dir.path().join("boot.toml"), &doc);
    assert_eq!(
        persisted_row(&rig, OUT_B).unwrap()["audio"],
        json!({"source": "secret-cam"})
    );
    // No audio slot: the output's choice is kept.
    let (status, _) = take(
        &rig,
        OPERATOR_TOKEN,
        "keep",
        patch(vec![video("output-1", bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(rig.fake.takes()[0].audio, None, "omitted: keep the choice");
    // Audio slot 0 follow: the engine take carries Follow.
    let (status, accepted) = take(
        &rig,
        OPERATOR_TOKEN,
        "follow",
        patch(vec![video("output-1", bars()), audio_follow("output-1")]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let takes = rig.fake.takes();
    assert_eq!(takes[1].audio, Some(RoutedAudio::Follow));
    rig.fake
        .tx
        .send(OutputOutcome {
            operation_id: accepted["operation_id"].as_str().unwrap().to_owned(),
            output: OUT_B.into(),
            tick: 7,
            outcome: "applied".into(),
            feed: Some(RoutedFeed::Source("bars".into())),
            audio: Some(RoutedAudio::Follow),
        })
        .unwrap();
    wait_for(|| {
        persisted_row(&rig, OUT_B).is_some_and(|r| r["video"] == json!({"source": "bars"}))
    })
    .await;
    let row = persisted_row(&rig, OUT_B).unwrap();
    assert!(
        row.get("audio").is_none_or(|a| a == "follow"),
        "the explicit choice is replaced by follow: {row}"
    );
}

/// Review T-M1 addendum: an explicit audio follow is the SAME take on both
/// slot-0 routes, so one Idempotency-Key replays across them in both
/// directions (same operation id, never a 409 and never a second enqueue).
#[tokio::test]
async fn explicit_follow_replays_across_the_patch_and_the_alias() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let alias_follow = |output: &str| {
        json!({
            "target": {"kind": "output", "output": output, "stream": "video"},
            "feed": bars(),
            "audio": "follow",
        })
    };
    let alias_send = |key: &'static str| {
        let body = alias_follow("output-1");
        let router = rig.router.clone();
        async move {
            let resp = send(
                &router,
                post(
                    "/api/v1/routing/output/take",
                    OPERATOR_TOKEN,
                    Some(key),
                    &body,
                ),
            )
            .await;
            (resp.status(), body_json(resp).await)
        }
    };
    let patch_body = || patch(vec![video("output-1", bars()), audio_follow("output-1")]);
    // Patch first, then the alias with the same key.
    let (status, first) = take(&rig, OPERATOR_TOKEN, "x1", patch_body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, again) = alias_send("x1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["operation_id"], first["operation_id"]);
    // Alias first, then the patch with the same key.
    let (status, first) = alias_send("x2").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, again) = take(&rig, OPERATOR_TOKEN, "x2", patch_body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["operation_id"], first["operation_id"]);
    assert_eq!(rig.fake.takes().len(), 2, "one enqueue per key");
}

/// Review T-M2: with no live readback (the engine reports no snapshot),
/// `GET /patch` never invents Program: video slot 0 is `live: false` with a
/// null `source` and `path`; once a readback exists it is `live: true` with
/// the readback's feed.
#[tokio::test]
async fn get_without_a_readback_reports_the_live_state_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake.state.lock().unwrap().live = None;
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    for output in all["outputs"].as_array().unwrap() {
        let slot = &output["video"][0];
        assert_eq!(slot["live"], false, "{all}");
        assert!(slot["source"].is_null(), "never an invented program: {all}");
        assert!(slot["path"].is_null(), "{all}");
        assert!(slot["pending"].is_null(), "{all}");
    }
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs: HashMap::from([(
            OUT_A.to_owned(),
            OutputLive {
                bus: "src:bars".to_owned(),
                since_tick: 1,
                pending: None,
                degraded: false,
                audio: None,
                pending_audio: None,
                private_audio: false,
            },
        )]),
        ..RoutingLive::default()
    });
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    let a = &all["outputs"][0]["video"][0];
    assert_eq!(a["live"], true, "{all}");
    assert_eq!(a["source"], json!({"source": "bars"}), "{all}");
    let b = &all["outputs"][1]["video"][0];
    assert_eq!(b["live"], false, "an output without a readback: {all}");
    assert!(b["source"].is_null(), "{all}");
}

/// A repository that counts `get_layout` calls and delegates everything.
struct CountingRepository {
    inner: Arc<dyn multiview_control::repository::Repository>,
    gets: std::sync::atomic::AtomicUsize,
    lists: std::sync::atomic::AtomicUsize,
}

impl multiview_control::repository::Repository for CountingRepository {
    fn list_layouts(
        &self,
    ) -> multiview_control::error::ControlResult<Vec<multiview_control::repository::VersionedLayout>>
    {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.list_layouts()
    }
    fn get_layout(
        &self,
        id: &str,
    ) -> multiview_control::error::ControlResult<multiview_control::repository::VersionedLayout>
    {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get_layout(id)
    }
    fn create_layout(
        &self,
        id: &str,
        input: multiview_control::repository::LayoutInput,
    ) -> multiview_control::error::ControlResult<multiview_control::repository::VersionedLayout>
    {
        self.inner.create_layout(id, input)
    }
    fn update_layout(
        &self,
        id: &str,
        input: multiview_control::repository::LayoutInput,
    ) -> multiview_control::error::ControlResult<multiview_control::repository::VersionedLayout>
    {
        self.inner.update_layout(id, input)
    }
    fn delete_layout(&self, id: &str) -> multiview_control::error::ControlResult<()> {
        self.inner.delete_layout(id)
    }
}

/// A rig whose layout repository counts its reads.
fn counting_rig(path: &Path) -> (Rig, Arc<CountingRepository>) {
    let mut rig = rig(path);
    let counting = Arc::new(CountingRepository {
        inner: Arc::clone(&rig.state.repository),
        gets: std::sync::atomic::AtomicUsize::new(0),
        lists: std::sync::atomic::AtomicUsize::new(0),
    });
    let repository: Arc<dyn multiview_control::repository::Repository> = counting.clone();
    rig.state.repository = repository;
    rig.router = multiview_control::router(rig.state.clone());
    (rig, counting)
}

/// Review T-I1: a `{layout}` request reads the stored layout exactly ONCE
/// (off the async worker), and that one read feeds the closure authorization,
/// the resolve and a replay's re-authorization — on `/patch/plan`,
/// `/patch/take` (fresh and replayed), the alias take and the alias plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layout_request_reads_the_stored_layout_once() {
    let dir = tempfile::tempdir().unwrap();
    let (rig, counting) = counting_rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "bars");
    let gets = || counting.gets.load(std::sync::atomic::Ordering::SeqCst);
    let body = || patch(vec![video("output-0", json!({"layout": "NewsOne"}))]);
    let mut before = gets();
    let mut once = |what: &str| {
        let now = gets();
        assert_eq!(now - before, 1, "{what}: one read per request");
        before = now;
    };
    let (status, _) = plan(&rig, OPERATOR_TOKEN, body()).await;
    assert_eq!(status, StatusCode::OK);
    once("patch plan");
    let (status, first) = take(&rig, OPERATOR_TOKEN, "once", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    once("patch take");
    let (status, again) = take(&rig, OPERATOR_TOKEN, "once", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["operation_id"], first["operation_id"]);
    once("patch take replay");
    let status = alias_take(
        &rig,
        OPERATOR_TOKEN,
        "once-alias",
        "output-0",
        json!({"layout": "NewsOne"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    once("alias take");
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/plan",
            OPERATOR_TOKEN,
            None,
            &alias("output-0", json!({"layout": "NewsOne"})),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    once("alias plan");
    // A request without a layout never reads one.
    let (status, _) = plan(&rig, OPERATOR_TOKEN, patch(vec![video("output-0", bars())])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(gets(), before, "no layout, no read");
}

/// Review T-I1: the layout reads are admitted by a process-wide semaphore
/// (`try_acquire`): with every permit held, a `{layout}` request on any
/// slot-0 route is 503 `engine-busy` with a generic body and the repository
/// is never called; once a permit frees, the same request proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layout_reads_without_a_free_permit_are_503_and_never_touch_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let (rig, counting) = counting_rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "bars");
    let gets = || counting.gets.load(std::sync::atomic::Ordering::SeqCst);
    let held = Arc::clone(&rig.state.layout_reads)
        .try_acquire_many_owned(
            u32::try_from(multiview_control::routes::output_routing::LAYOUT_READ_PERMITS).unwrap(),
        )
        .unwrap();
    let body = || patch(vec![video("output-0", json!({"layout": "NewsOne"}))]);
    let (status, problem) = plan(&rig, OPERATOR_TOKEN, body()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert!(
        problem["type"].as_str().unwrap().ends_with("engine-busy"),
        "{problem}"
    );
    assert!(
        !problem.to_string().contains("NewsOne") && !problem.to_string().contains("output-0"),
        "generic: {problem}"
    );
    let (status, _) = take(&rig, OPERATOR_TOKEN, "busy", body()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let status = alias_take(
        &rig,
        OPERATOR_TOKEN,
        "busy-a",
        "output-0",
        json!({"layout": "NewsOne"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(gets(), 0, "the repository was never called");
    assert_eq!(rig.fake.takes().len(), 0, "nothing reached the engine");
    drop(held);
    let (status, _) = take(&rig, OPERATOR_TOKEN, "busy", body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "proceeds once a permit frees");
    assert_eq!(gets(), 1);
}

/// Review T-I2: `GET /patch` never lists the layout library on the async
/// handler. It reads only the layouts the live readback names for outputs the
/// caller may see (each once, through the bounded off-handler read); with no
/// layout on air it reads none; and a layout feed is still redacted for a
/// caller who cannot read one of its sources.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_patch_reads_only_the_layouts_on_air_and_never_lists_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let (rig, counting) = counting_rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "bars");
    store_layout(&rig, "Other", "bars");
    let gets = || counting.gets.load(std::sync::atomic::Ordering::SeqCst);
    let lists = || counting.lists.load(std::sync::atomic::Ordering::SeqCst);
    // No layout on air: no read at all.
    let resp = send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!((gets(), lists()), (0, 0), "no layout on air, no read");
    // A layout on air (and pending on another output): one read each.
    let live = |bus: &str, pending: Option<&str>| OutputLive {
        bus: bus.to_owned(),
        since_tick: 1,
        pending: pending.map(|p| (p.to_owned(), 3)),
        degraded: false,
        audio: None,
        pending_audio: None,
        private_audio: false,
    };
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs: HashMap::from([
            (OUT_A.to_owned(), live("aux:NewsOne", None)),
            (OUT_B.to_owned(), live("src:bars", Some("aux:NewsOne"))),
        ]),
        ..RoutingLive::default()
    });
    let all = body_json(send(&rig.router, get("/api/v1/patch", OPERATOR_TOKEN)).await).await;
    assert_eq!(
        all["outputs"][0]["video"][0]["source"],
        json!({"layout": "NewsOne"}),
        "{all}"
    );
    assert_eq!(gets(), 1, "the one layout on air is read once");
    assert_eq!(lists(), 0, "the library is never listed");
}

/// #383 x B2: a layout take through `/patch` (the shared slot-0 path and its
/// one bounded layout read) carries #383's resolved body: the layout's
/// `audio` attribute and a resolution revision taken BEFORE the read, so an
/// edit committed after it (a higher revision) supersedes it in the engine.
#[tokio::test]
async fn a_patch_layout_take_carries_the_audio_attribute_and_a_pre_read_revision() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "bars");
    let before = multiview_control::output_routing::next_layout_revision();
    let (status, accepted) = take(
        &rig,
        OPERATOR_TOKEN,
        "rev",
        patch(vec![video("output-0", json!({"layout": "NewsOne"}))]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let after = multiview_control::output_routing::next_layout_revision();
    let takes = rig.fake.takes();
    let body = &takes[0]
        .layout
        .as_ref()
        .expect("the resolved body rides the take")
        .0;
    assert_eq!(body.audio, multiview_config::LayoutAudio::default());
    assert!(
        body.revision > before && body.revision < after,
        "taken during this request ({before} < {} < {after})",
        body.revision
    );
}

/// Review R2-M1: an identical PUT (the body unchanged) is a no-op: committed
/// and answered `apply: live` with no operation id; no reset is started and
/// no restart is announced.
#[tokio::test]
async fn an_identical_put_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    for id in ["output-0", "output-1"] {
        let (etag, name, body) = current_output(&rig, id).await;
        let resp = send(
            &rig.router,
            put_output(OPERATOR_TOKEN, id, &etag, &name, &body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
        assert_eq!(resp.headers()["x-multiview-apply"], "live", "{id}");
        assert!(
            resp.headers().get("x-multiview-operation-id").is_none(),
            "{id}"
        );
    }
    assert_eq!(
        rig.fake.state.lock().unwrap().reformats.len(),
        0,
        "no reset"
    );
}
