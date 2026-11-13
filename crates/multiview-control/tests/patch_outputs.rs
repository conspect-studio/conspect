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
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    OutputLive, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanPath, PlanRefusal,
    RoutedFeed, RoutingLive, SlotPlan, TakeSubmitError,
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
    keys.register(
        "patch-noaxis",
        "patch-noaxis-secret",
        scoped("patch-noaxis", &["output-1", "bars"], &[]),
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
    paths: HashMap<(String, RoutedFeed), (PlanPath, bool, u8)>,
    live: Option<RoutingLive>,
}

struct Fake {
    state: Mutex<FakeState>,
    _tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        Arc::new(Self {
            state: Mutex::new(FakeState::default()),
            _tx: tx,
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
        if *feed == RoutedFeed::Preview {
            return Err(PlanRefusal::SwitcherRequired);
        }
        Ok(PlanCost {
            needs_encoder: *feed != RoutedFeed::Program,
            sessions_in_use: 1,
            sessions_max: 6,
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
}

fn rig(path: &Path) -> Rig {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let fake = Fake::new();
    let seam: Arc<dyn OutputRouting> = fake.clone();
    let state = AppState::new(
        Arc::clone(&engine),
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
    .with_output_routing(seam);
    Rig {
        router: multiview_control::router(state),
        fake,
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
                },
            ),
            (
                OUT_B.to_owned(),
                OutputLive {
                    bus: "src:secret-cam".to_owned(),
                    since_tick: 5,
                    pending: Some(("src:bars".to_owned(), 3)),
                    degraded: true,
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
    assert!(rig.fake.takes().is_empty());
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
        (
            vec![video("output-0", json!({"layout": "NewsOne"}))],
            "unknown-source",
            "#306",
        ),
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
    assert_eq!(takes[0].feed, RoutedFeed::Source("bars".into()));
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
    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(other, 0, "every flood answer is 202 or 429");
    assert_eq!(accepted + limited, 2000);
    // The per-output limit is 2 per sliding second.
    let allowed = 2 * (elapsed.ceil() as usize + 1);
    assert!(accepted <= allowed, "{accepted} accepted in {elapsed:.2}s");
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
