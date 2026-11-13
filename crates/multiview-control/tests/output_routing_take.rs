//! ADR-0108 slice 1 — output plan/take over HTTP against a fake engine seam:
//! authorization on both ends before existence, idempotent replay with the
//! recorded outcome, rate limits, engine-busy shed, persistence only after
//! `applied`, redaction, and `route.output` delivery per object.
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
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    OutputLive, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanRefusal, RoutedFeed,
    RoutingLive, TakeSubmitError,
};
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
    SessionStream,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_json, get, send, OPERATOR_TOKEN, VIEWER_TOKEN};

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
path = "/tmp/output-routing-take/a/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
kind = "hls"
path = "/tmp/output-routing-take/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

const OUT_B_CONFIG_ID: &str = "hls /tmp/output-routing-take/b/index.m3u8";

/// Scoped to output-1 and bars only (no Program scope, no M/E).
const SCOPED_ROUTER_TOKEN: &str = "route-scoped.route-scoped-secret";
/// Scoped to output-1 (object AND output axis) + the Program scope + me1 (may
/// route buses). Its output axis names output-1 explicitly: a key with an
/// output allowlist may route only the outputs on it (#329 F2).
const BUS_ROUTER_TOKEN: &str = "route-bus.route-bus-secret";
/// Confined on the OUTPUT axis only (`scoped_output_ids = ["output-1"]`, no
/// object list): ADR-W026's output-scoped key (#329 F2).
const OUTPUT_AXIS_TOKEN: &str = "route-outaxis.route-outaxis-secret";
/// Scoped to output-0 only.
const OTHER_OUTPUT_TOKEN: &str = "route-other.route-other-secret";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    let scoped = |id: &str, objects: &[&str], outputs: Option<Vec<String>>| Principal {
        key_id: id.to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(objects.iter().map(|s| (*s).to_owned()).collect()),
        scoped_output_ids: outputs,
        scoped_discovery_domains: None,
    };
    keys.register(
        "route-scoped",
        "route-scoped-secret",
        scoped("route-scoped", &["output-1", "bars"], None),
    );
    keys.register(
        "route-bus",
        "route-bus-secret",
        scoped(
            "route-bus",
            &["output-1", "me1"],
            Some(vec!["program:main".to_owned(), "output-1".to_owned()]),
        ),
    );
    keys.register(
        "route-outaxis",
        "route-outaxis-secret",
        Principal {
            key_id: "route-outaxis".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: Some(vec!["output-1".to_owned()]),
            scoped_discovery_domains: None,
        },
    );
    keys.register(
        "route-other",
        "route-other-secret",
        scoped("route-other", &["output-0", "bars"], None),
    );
    keys
}

/// A scriptable engine seam.
#[derive(Default)]
struct FakeState {
    takes: Vec<OutputTakeRequest>,
    busy: bool,
    needs_encoder: bool,
    in_use: usize,
    live: Option<RoutingLive>,
}

struct Fake {
    state: Mutex<FakeState>,
    tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        Arc::new(Self {
            state: Mutex::new(FakeState::default()),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
    fn takes(&self) -> Vec<OutputTakeRequest> {
        self.state.lock().unwrap().takes.clone()
    }
    fn emit(&self, operation_id: &str, output: &str, outcome: &str) {
        self.tx
            .send(OutputOutcome {
                operation_id: operation_id.to_owned(),
                output: output.to_owned(),
                tick: 77,
                outcome: outcome.to_owned(),
                feed: None,
                audio: None,
            })
            .unwrap();
    }
    /// Emit like the engine does: the outcome carries the output's feed.
    fn emit_with_feed(&self, operation_id: &str, output: &str, outcome: &str, feed: RoutedFeed) {
        self.tx
            .send(OutputOutcome {
                operation_id: operation_id.to_owned(),
                output: output.to_owned(),
                tick: 78,
                outcome: outcome.to_owned(),
                feed: Some(feed),
                audio: None,
            })
            .unwrap();
    }
}

impl OutputRouting for Fake {
    fn plan(&self, _output: &str, feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        if *feed == RoutedFeed::Preview {
            return Err(PlanRefusal::SwitcherRequired);
        }
        let s = self.state.lock().unwrap();
        Ok(PlanCost {
            needs_encoder: s.needs_encoder,
            sessions_in_use: s.in_use,
            sessions_max: 6,
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
    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        let mut s = self.state.lock().unwrap();
        if s.busy {
            return Err(TakeSubmitError::Busy);
        }
        s.takes.push(take);
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
    engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    state: AppState,
}

fn rig(path: &Path, with_routing: bool) -> Rig {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let fake = Fake::new();
    let mut state = AppState::new(
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
    )));
    if with_routing {
        let seam: Arc<dyn OutputRouting> = fake.clone();
        state = state.with_output_routing(seam);
    }
    Rig {
        router: multiview_control::router(state.clone()),
        fake,
        engine,
        state,
    }
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

fn body(output: &str, feed: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "target": {"kind": "output", "output": output, "stream": "video"},
        "feed": feed,
    })
}

fn take(rig: &Rig, token: &str, key: &str, output: &str, feed: serde_json::Value) -> Request<Body> {
    let _ = rig;
    post(
        "/api/v1/routing/output/take",
        token,
        Some(key),
        &body(output, feed),
    )
}

async fn wait_for<F: Fn() -> bool>(f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

fn persisted_feed(state: &AppState, output: &str) -> Option<serde_json::Value> {
    let config: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(state).unwrap(),
    )
    .unwrap();
    config
        .routing
        .as_ref()
        .and_then(|t| t.output.iter().find(|xp| xp.output == output))
        .and_then(|xp| xp.video.clone())
        .map(|v| serde_json::to_value(v).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_is_202_replays_with_its_outcome_and_persists_only_after_applied() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let src = serde_json::json!({"source": "bars"});
    let resp = send(
        &rig.router,
        take(&rig, OPERATOR_TOKEN, "k1", "output-1", src.clone()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let first = body_json(resp).await;
    assert_eq!(first["adopt"], "next_tick");
    let op = first["operation_id"].as_str().unwrap().to_owned();
    let takes = rig.fake.takes();
    assert_eq!(takes.len(), 1);
    assert_eq!(
        takes[0].output, OUT_B_CONFIG_ID,
        "API id mapped to the config id"
    );
    assert_eq!(takes[0].video, Some(RoutedFeed::Source("bars".into())));
    assert_eq!(takes[0].principal, "operator-key", "charged to the key id");
    // Replay before the outcome: same op id, no outcome, nothing re-enqueued.
    let replay = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "k1", "output-1", src.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(replay["operation_id"], op.as_str());
    assert!(replay.get("outcome").is_none());
    assert_eq!(rig.fake.takes().len(), 1);
    assert_eq!(
        persisted_feed(&rig.state, OUT_B_CONFIG_ID),
        None,
        "not before applied"
    );
    rig.fake.emit(&op, OUT_B_CONFIG_ID, "applied");
    wait_for(|| persisted_feed(&rig.state, OUT_B_CONFIG_ID).is_some()).await;
    assert_eq!(
        persisted_feed(&rig.state, OUT_B_CONFIG_ID),
        Some(src.clone())
    );
    // Replay after the outcome carries it.
    let replay = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "k1", "output-1", src),
        )
        .await,
    )
    .await;
    assert_eq!(replay["operation_id"], op.as_str());
    assert_eq!(replay["outcome"], "applied");
    assert_eq!(replay["tick"], 77);
    // Back to Program: the entry is removed on applied.
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "k2",
            "output-1",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    let op2 = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake.emit(&op2, OUT_B_CONFIG_ID, "applied");
    wait_for(|| persisted_feed(&rig.state, OUT_B_CONFIG_ID).is_none()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_take_leaves_the_config_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let mut events = SessionStream::new(rig.engine.subscribe(), "s", None);
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "r1",
            "output-1",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    let op = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake
        .emit(&op, OUT_B_CONFIG_ID, "refused:feed-not-fresh");
    // The event proves the relay processed the outcome before we look.
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(frame) = events.next_delta().await.unwrap() {
                if matches!(frame.envelope.payload, Event::RouteOutput(_)) {
                    return frame;
                }
            }
        }
    })
    .await
    .unwrap();
    match &frame.envelope.payload {
        Event::RouteOutput(e) => {
            assert_eq!(e.output, "output-1", "the event names the API id");
            assert_eq!(e.outcome, "refused:feed-not-fresh");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(frame.envelope.corr.as_deref(), Some(op.as_str()));
    assert_eq!(persisted_feed(&rig.state, OUT_B_CONFIG_ID), None);
}

#[tokio::test]
async fn authorization_runs_on_both_ends_before_existence() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let bars = || serde_json::json!({"source": "bars"});
    // Destination out of scope: 403 for an existing AND a missing output.
    for output in ["output-0", "no-such-output"] {
        for path in ["/api/v1/routing/output/take", "/api/v1/routing/plan"] {
            let resp = send(
                &rig.router,
                post(path, SCOPED_ROUTER_TOKEN, Some("a"), &body(output, bars())),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path} {output}");
        }
    }
    // Source out of scope: 403 for an existing AND a missing source.
    for source in ["secret-cam", "no-such-source"] {
        let resp = send(
            &rig.router,
            take(
                &rig,
                SCOPED_ROUTER_TOKEN,
                "b",
                "output-1",
                serde_json::json!({"source": source}),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{source}");
    }
    // A bus without the Program scope: 403; with Program + me1: accepted.
    let resp = send(
        &rig.router,
        take(
            &rig,
            SCOPED_ROUTER_TOKEN,
            "c",
            "output-1",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(
        &rig.router,
        take(
            &rig,
            BUS_ROUTER_TOKEN,
            "d",
            "output-1",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    // A viewer cannot take (role) but can plan.
    let resp = send(
        &rig.router,
        take(&rig, VIEWER_TOKEN, "e", "output-1", bars()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Nothing denied reached the engine, and no denied key was burned.
    assert_eq!(rig.fake.takes().len(), 1);
    let resp = send(
        &rig.router,
        take(&rig, SCOPED_ROUTER_TOKEN, "b", "output-1", bars()),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "key `b` was not reserved by the 403"
    );
    // In scope but missing: now existence answers.
    let resp = send(
        &rig.router,
        take(&rig, OPERATOR_TOKEN, "f", "no-such-output", bars()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "g",
            "output-0",
            serde_json::json!({"source": "nope"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_json(resp).await["type"]
        .as_str()
        .unwrap()
        .ends_with("unknown-feed"));
}

#[tokio::test]
async fn takes_are_rate_limited_per_output_and_per_principal() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let bars = || serde_json::json!({"source": "bars"});
    for i in 0..2 {
        let resp = send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, &format!("o{i}"), "output-1", bars()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }
    let resp = send(
        &rig.router,
        take(&rig, OPERATOR_TOKEN, "o2", "output-1", bars()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().get(header::RETRY_AFTER).is_some());
    // A replay of an accepted key is not a new take: never 429.
    let resp = send(
        &rig.router,
        take(&rig, OPERATOR_TOKEN, "o0", "output-1", bars()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(rig.fake.takes().len(), 2);
}

#[tokio::test]
async fn principal_limit_holds_across_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let limiter = &rig.state.output_routing.as_ref().unwrap().limiter;
    let now = std::time::Instant::now();
    for i in 0..10 {
        limiter
            .admit(&format!("x{i}"), "operator-key", now)
            .unwrap();
    }
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "q",
            "output-1",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rig.fake.takes().len(), 0);
}

#[tokio::test]
async fn a_full_mailbox_is_503_and_releases_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    rig.fake.state.lock().unwrap().busy = true;
    let req = || {
        take(
            &rig,
            OPERATOR_TOKEN,
            "busy",
            "output-1",
            serde_json::json!({"source": "bars"}),
        )
    };
    let resp = send(&rig.router, req()).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_json(resp).await["type"]
        .as_str()
        .unwrap()
        .ends_with("engine-busy"));
    rig.fake.state.lock().unwrap().busy = false;
    let resp = send(&rig.router, req()).await;
    let accepted = body_json(resp).await;
    assert!(
        accepted.get("outcome").is_none(),
        "a fresh take, not a replay: {accepted}"
    );
    assert_eq!(rig.fake.takes().len(), 1);
}

#[tokio::test]
async fn without_an_engine_seam_routes_answer_routing_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), false);
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "u",
            "output-1",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(body_json(resp).await["type"]
        .as_str()
        .unwrap()
        .ends_with("routing-unavailable"));
}

#[tokio::test]
async fn plan_reports_cost_and_redacts_sessions_for_scoped_principals() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    {
        let mut s = rig.fake.state.lock().unwrap();
        s.needs_encoder = true;
        s.in_use = 6;
    }
    let plan = |token: &str| {
        post(
            "/api/v1/routing/plan",
            token,
            None,
            &body("output-1", serde_json::json!({"source": "bars"})),
        )
    };
    let whole = body_json(send(&rig.router, plan(OPERATOR_TOKEN)).await).await;
    assert_eq!(whole["class"], "class1");
    assert_eq!(whole["cost"]["new_encoders"], 1);
    assert_eq!(whole["cost"]["encoder_sessions"], "6 of 6 in use");
    assert_eq!(whole["cost"]["fits"], false);
    assert_eq!(whole["refusals"][0]["type"], "capacity");
    assert!(whole["refusals"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("6 of 6"));
    let scoped = body_json(send(&rig.router, plan(SCOPED_ROUTER_TOKEN)).await).await;
    let text = scoped.to_string();
    assert!(scoped["cost"].get("encoder_sessions").is_none(), "{text}");
    assert_eq!(scoped["cost"]["fits"], false);
    assert_eq!(
        scoped["refusals"][0]["detail"],
        "No free encoder session for this output"
    );
    assert!(!text.contains("of 6"), "{text}");
    // The 409 capacity body is redacted the same way.
    let resp = send(
        &rig.router,
        take(
            &rig,
            SCOPED_ROUTER_TOKEN,
            "cap",
            "output-1",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let problem = body_json(resp).await;
    assert!(!problem.to_string().contains("in use"), "{problem}");
    assert!(problem.to_string().contains("No free encoder session"));
    // Preview without an M/E.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/plan",
            OPERATOR_TOKEN,
            None,
            &body("output-1", serde_json::json!({"bus": "preview"})),
        ),
    )
    .await;
    assert_eq!(
        body_json(resp).await["refusals"][0]["type"],
        "switcher-required"
    );
}

#[tokio::test]
async fn readback_carries_live_state_and_redacts_unreadable_pending_feeds() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let mut outputs = HashMap::new();
    outputs.insert(
        OUT_B_CONFIG_ID.to_owned(),
        OutputLive {
            bus: "src:bars".into(),
            since_tick: 900,
            pending: Some(("src:secret-cam".into(), 12)),
            degraded: false,
            audio: None,
            pending_audio: None,
            private_audio: false,
        },
    );
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs,
        encoders: 2,
        sessions_in_use: 1,
        sessions_max: 6,
        dropped_ticks: 0,
    });
    let whole = body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await;
    let row = whole["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["output"] == "output-1")
        .unwrap()
        .clone();
    assert_eq!(row["state"], "pending");
    assert_eq!(row["since_tick"], 900);
    assert_eq!(
        row["pending"]["video"],
        serde_json::json!({"source": "secret-cam"})
    );
    assert_eq!(row["pending"]["held_ticks"], 12);
    assert_eq!(whole["buses"]["encoder_sessions"], "1 of 6 in use");
    let scoped =
        body_json(send(&rig.router, get("/api/v1/routing", SCOPED_ROUTER_TOKEN)).await).await;
    let text = scoped.to_string();
    assert!(!text.contains("secret-cam"), "{text}");
    assert!(scoped["buses"].get("encoder_sessions").is_none(), "{text}");
    let rows = scoped["outputs"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the readable output: {text}");
    assert_eq!(
        rows[0]["pending"]["video"],
        serde_json::json!({"redacted": true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_output_reaches_only_principals_that_may_read_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let mut allowed = SessionStream::new(rig.engine.subscribe(), "a", None)
        .with_object_scope(Some(vec!["output-1".to_owned()]));
    let mut denied = SessionStream::new(rig.engine.subscribe(), "d", None)
        .with_object_scope(Some(vec!["output-0".to_owned()]));
    rig.fake.emit("", OUT_B_CONFIG_ID, "fallback:aux-failed");
    // A marker the denied session may read, so it has something to return.
    rig.engine
        .publish_event(Event::RouteOutput(multiview_events::RouteOutputEvent {
            output: "output-0".into(),
            operation_id: String::new(),
            tick: 1,
            outcome: "applied".into(),
        }));
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(f) = allowed.next_delta().await.unwrap() {
                return f;
            }
        }
    })
    .await
    .unwrap();
    match got.envelope.payload {
        Event::RouteOutput(e) => assert_eq!(e.output, "output-1"),
        other => panic!("{other:?}"),
    }
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(f) = denied.next_delta().await.unwrap() {
                return f;
            }
        }
    })
    .await
    .unwrap();
    match first.envelope.payload {
        Event::RouteOutput(e) => assert_eq!(e.output, "output-0", "output-1's event was filtered"),
        other => panic!("{other:?}"),
    }
}

/// Review D1: the take record lives as long as its reserved key. Fill the
/// records far past the old 1024 pending cap; an early key's replay (with a
/// different body) still answers its own operation as `pending` (never a new
/// `adopt`), nothing is enqueued twice, and its later `applied` persists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_early_reserved_key_survives_many_later_takes_and_still_persists() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let src = serde_json::json!({"source": "bars"});
    let first = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "early", "output-1", src.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(first["adopt"], "next_tick");
    assert_eq!(first["status"], "accepted");
    let op = first["operation_id"].as_str().unwrap().to_owned();
    // 1500 later accepted takes (past the old 1024 cap), recorded the way the
    // route records them: a fresh reservation in the take store.
    let control = rig.state.output_routing.clone().unwrap();
    fill_take_store(&control, "later", 1500);
    // Replay the early key with a different body.
    let replay = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "early",
            "output-1",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replay = body_json(replay).await;
    assert_eq!(replay["operation_id"], op.as_str());
    assert_eq!(replay["status"], "pending");
    assert!(replay.get("adopt").is_none(), "a replay never re-adopts");
    assert!(replay.get("outcome").is_none());
    assert_eq!(rig.fake.takes().len(), 1, "one enqueue total");
    // Its later applied persists, and the replay then carries the outcome.
    rig.fake.emit_with_feed(
        &op,
        OUT_B_CONFIG_ID,
        "applied",
        RoutedFeed::Source("bars".into()),
    );
    wait_for(|| persisted_feed(&rig.state, OUT_B_CONFIG_ID).is_some()).await;
    assert_eq!(
        persisted_feed(&rig.state, OUT_B_CONFIG_ID),
        Some(src.clone())
    );
    let replay = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "early", "output-1", src),
        )
        .await,
    )
    .await;
    assert_eq!(replay["operation_id"], op.as_str());
    assert_eq!(replay["status"], "settled");
    assert_eq!(replay["outcome"], "applied");
    assert_eq!(rig.fake.takes().len(), 1);
}

#[tokio::test]
async fn replays_for_other_bodies_never_apply_twice_and_take_requires_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/output/take",
            OPERATOR_TOKEN,
            None,
            &body("output-1", serde_json::json!({"source": "bars"})),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let _ = send(
        &rig.router,
        take(
            &rig,
            OTHER_OUTPUT_TOKEN,
            "z",
            "output-0",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    let _ = send(
        &rig.router,
        take(
            &rig,
            OTHER_OUTPUT_TOKEN,
            "z",
            "output-0",
            serde_json::json!({"source": "a"}),
        ),
    )
    .await;
    assert_eq!(rig.fake.takes().len(), 1);
}

/// Reserve `n` distinct takes in the take store the way the route does, each
/// charged to its own principal (so no per-principal cap is met).
fn fill_take_store(
    control: &multiview_control::output_routing::OutputRoutingControl,
    prefix: &str,
    n: usize,
) {
    for i in 0..n {
        let reserved = control.reserve_take(
            &format!("{prefix}-{i}"),
            &format!("{prefix}-principal-{i}"),
            multiview_control::output_routing::PendingTake {
                api_id: "output-0".to_owned(),
                feed: Some(RoutedFeed::Program),
                audio: None,
            },
        );
        assert!(
            matches!(
                reserved,
                multiview_control::output_routing::TakeReservation::Fresh(_)
            ),
            "fresh reservation {i}: {reserved:?}"
        );
    }
}

/// The scoped key the `IdempotencyKey` extractor derives for a take (the
/// route is nested under `/api/v1`, so the extractor sees the inner path).
fn scoped_take_key(key_id: &str, raw: &str) -> String {
    let path = "/routing/output/take";
    format!(
        "4:POST:{}:{key_id}:{}:{path}:{}:{raw}",
        key_id.len(),
        path.len(),
        raw.len()
    )
}

/// Review E1(a), revised by #329 F3: the take store fails closed at its cap
/// only when every record is PENDING — then one more distinct key is
/// `idempotency-capacity`, nothing is enqueued, and the oldest (pending) key
/// still replays its own operation. Once that oldest record is settled, the
/// next distinct key evicts it (oldest settled first) and is accepted; a
/// replay of the evicted key is then a new take (the documented trade-off).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_take_store_fails_closed_only_when_all_pending_and_evicts_the_oldest_settled() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let src = serde_json::json!({"source": "bars"});
    let first = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "oldest", "output-1", src.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(first["status"], "accepted");
    let op = first["operation_id"].as_str().unwrap().to_owned();
    let control = rig.state.output_routing.clone().unwrap();
    let cap = multiview_control::output_routing::MAX_TAKE_RECORDS;
    assert_eq!(cap, 4096);
    fill_take_store(&control, "filler", cap - 1);
    assert_eq!(control.records_len(), cap);
    let resp = send(
        &rig.router,
        take(&rig, OPERATOR_TOKEN, "one-more", "output-1", src.clone()),
    )
    .await;
    let status = resp.status();
    let problem = body_json(resp).await;
    assert!(
        problem["type"]
            .as_str()
            .is_some_and(|t| t.ends_with("idempotency-capacity")),
        "{status} {problem}"
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rig.fake.takes().len(), 1, "the refused key never enqueued");
    assert_eq!(control.records_len(), cap, "nothing evicted, nothing added");
    let replay = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "oldest", "output-1", src.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(replay["operation_id"], op.as_str());
    assert_eq!(
        replay["status"], "pending",
        "a pending record is never evicted"
    );
    // Settle the oldest: it becomes the eviction candidate.
    rig.fake.emit(&op, OUT_B_CONFIG_ID, "refused:capacity");
    wait_for(|| control.outcome_of(&op).is_some()).await;
    // Past the per-output rate window before the next fresh take.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let resp = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "one-more", "output-1", src.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(resp["status"], "accepted", "{resp}");
    assert_eq!(rig.fake.takes().len(), 2);
    assert_eq!(control.records_len(), cap, "one evicted, one added");
    assert!(
        control.outcome_of(&op).is_none(),
        "the oldest settled record was evicted"
    );
}

/// Review E1(b): another route releasing (or rebinding) the same scoped string
/// in the process-global idempotency store, as switcher / discovery / command
/// submission do, never changes a take's replay.
#[tokio::test]
async fn a_foreign_global_release_never_changes_a_take_replay() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let src = serde_json::json!({"source": "bars"});
    let first = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "shared", "output-1", src.clone()),
        )
        .await,
    )
    .await;
    let op = first["operation_id"].as_str().unwrap().to_owned();
    let scoped = scoped_take_key("operator-key", "shared");
    match rig.state.idempotency.reserve(Some(&scoped)) {
        multiview_control::concurrency::Reservation::Fresh(foreign)
        | multiview_control::concurrency::Reservation::Replay(foreign) => {
            rig.state.idempotency.release(Some(&scoped), &foreign);
        }
        multiview_control::concurrency::Reservation::Capacity => panic!("capacity"),
    }
    let replay = body_json(
        send(
            &rig.router,
            take(&rig, OPERATOR_TOKEN, "shared", "output-1", src),
        )
        .await,
    )
    .await;
    assert_eq!(replay["operation_id"], op.as_str(), "{replay}");
    assert_eq!(replay["status"], "pending");
    assert!(replay.get("adopt").is_none());
    assert_eq!(rig.fake.takes().len(), 1, "exactly one enqueue");
}

/// Review E1(c): the take key is scoped by principal (and method + path):
/// two principals with the same raw key get two separate takes.
#[tokio::test]
async fn two_principals_with_the_same_raw_key_get_separate_takes() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let src = serde_json::json!({"source": "bars"});
    let mut ops = Vec::new();
    for token in [OPERATOR_TOKEN, support::ADMIN_TOKEN] {
        let resp = body_json(
            send(
                &rig.router,
                take(&rig, token, "same", "output-1", src.clone()),
            )
            .await,
        )
        .await;
        assert_eq!(resp["status"], "accepted", "{resp}");
        ops.push(resp["operation_id"].as_str().unwrap().to_owned());
    }
    assert_ne!(ops[0], ops[1]);
    let takes = rig.fake.takes();
    assert_eq!(takes.len(), 2);
    assert_eq!(takes[0].principal, "operator-key");
    assert_eq!(takes[1].principal, "admin-key");
    // Each principal's replay answers its own operation.
    let replay = body_json(
        send(
            &rig.router,
            take(&rig, support::ADMIN_TOKEN, "same", "output-1", src),
        )
        .await,
    )
    .await;
    assert_eq!(replay["operation_id"], ops[1].as_str());
    assert_eq!(rig.fake.takes().len(), 2);
}

/// #329 F1(c): with a live snapshot whose bus differs from the persisted
/// feed (a refused boot take, a fallback), `video` and `tally` are the LIVE
/// bus and `configured_video` the persisted feed; each is redacted by its own
/// rule; without a snapshot the row is today's configured view.
#[allow(clippy::too_many_lines)] // One scenario: configured, then live readback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readback_reports_the_live_bus_and_the_configured_feed_separately() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let row = |state: &serde_json::Value| {
        state["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["output"] == "output-1")
            .cloned()
            .unwrap()
    };
    // Persist out1 = source bars (an applied take), no live snapshot yet.
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "p1",
            "output-1",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    let op = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake.emit_with_feed(
        &op,
        OUT_B_CONFIG_ID,
        "applied",
        RoutedFeed::Source("bars".into()),
    );
    wait_for(|| persisted_feed(&rig.state, OUT_B_CONFIG_ID).is_some()).await;
    let r = row(&body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await);
    assert_eq!(r["state"], "configured");
    assert_eq!(r["video"], serde_json::json!({"source": "bars"}));
    assert_eq!(r["configured_video"], serde_json::json!({"source": "bars"}));
    assert_eq!(r["tally"], "direct");
    // The engine is on Program for that output (e.g. after a restart whose
    // boot take was refused).
    let mut outputs = HashMap::new();
    outputs.insert(
        OUT_B_CONFIG_ID.to_owned(),
        OutputLive {
            bus: "program".into(),
            since_tick: 3,
            pending: None,
            degraded: false,
            audio: None,
            pending_audio: None,
            private_audio: false,
        },
    );
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs,
        encoders: 1,
        sessions_in_use: 0,
        sessions_max: 6,
        dropped_ticks: 0,
    });
    let r = row(&body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await);
    assert_eq!(r["state"], "on_air");
    assert_eq!(r["video"], serde_json::json!({"bus": "program"}), "{r}");
    assert_eq!(
        r["configured_video"],
        serde_json::json!({"source": "bars"}),
        "{r}"
    );
    assert_eq!(r["tally"], "program", "tally from the live bus: {r}");
    assert_eq!(r["effective_audio"], "program", "{r}");
    // A key whose output allowlist lacks `program:main` (so it may not read
    // the Program bus) but may read bars: the live Program bus is redacted,
    // the configured feed is shown.
    let r =
        row(&body_json(send(&rig.router, get("/api/v1/routing", OUTPUT_AXIS_TOKEN)).await).await);
    assert_eq!(r["video"], serde_json::json!({"redacted": true}), "{r}");
    assert_eq!(r["tally"], "redacted", "{r}");
    assert_eq!(
        r["configured_video"],
        serde_json::json!({"source": "bars"}),
        "{r}"
    );
    // And the reverse: on air a source the caller cannot read.
    rig.fake
        .state
        .lock()
        .unwrap()
        .live
        .as_mut()
        .unwrap()
        .outputs
        .get_mut(OUT_B_CONFIG_ID)
        .unwrap()
        .bus = "src:secret-cam".into();
    let scoped =
        body_json(send(&rig.router, get("/api/v1/routing", SCOPED_ROUTER_TOKEN)).await).await;
    assert!(!scoped.to_string().contains("secret-cam"), "{scoped}");
    let r = row(&scoped);
    assert_eq!(r["video"], serde_json::json!({"redacted": true}));
    assert_eq!(r["configured_video"], serde_json::json!({"source": "bars"}));
    let r = row(&body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await);
    assert_eq!(r["video"], serde_json::json!({"source": "secret-cam"}));
    assert_eq!(
        r["effective_audio"],
        serde_json::json!({"source": "secret-cam"})
    );
    assert_eq!(r["tally"], "direct");
}

/// #329 F2: a key confined by `scoped_output_ids` alone (no object list) can
/// plan/take, read and hear only its own outputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_scoped_key_routes_reads_and_hears_only_its_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let bars = || serde_json::json!({"source": "bars"});
    // The other output: 403 on plan AND take, nothing enqueued, key unburned.
    for path in ["/api/v1/routing/plan", "/api/v1/routing/output/take"] {
        let resp = send(
            &rig.router,
            post(
                path,
                OUTPUT_AXIS_TOKEN,
                Some("k"),
                &body("output-0", bars()),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path} output-0");
    }
    assert_eq!(rig.fake.takes().len(), 0);
    // Its own output: 200 plan, 202 take.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/routing/plan",
            OUTPUT_AXIS_TOKEN,
            None,
            &body("output-1", bars()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        &rig.router,
        take(&rig, OUTPUT_AXIS_TOKEN, "k", "output-1", bars()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(rig.fake.takes().len(), 1);
    // GET /routing lists only output-1.
    let state = body_json(send(&rig.router, get("/api/v1/routing", OUTPUT_AXIS_TOKEN)).await).await;
    let rows: Vec<&str> = state["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["output"].as_str().unwrap())
        .collect();
    assert_eq!(rows, vec!["output-1"], "{state}");
    // Realtime: the same session filter the WS/SSE handlers install for it.
    let mut session = SessionStream::new(rig.engine.subscribe(), "o", None).with_scopes(
        None,
        Some(vec!["output-1".to_owned()]),
        None,
    );
    for output in ["output-0", "output-1"] {
        rig.engine
            .publish_event(Event::RouteOutput(multiview_events::RouteOutputEvent {
                output: output.into(),
                operation_id: String::new(),
                tick: 1,
                outcome: "applied".into(),
            }));
    }
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(f) = session.next_delta().await.unwrap() {
                return f;
            }
        }
    })
    .await
    .unwrap();
    match got.envelope.payload {
        Event::RouteOutput(e) => assert_eq!(e.output, "output-1", "output-0 was filtered"),
        other => panic!("{other:?}"),
    }
}

struct Counters(Arc<multiview_engine::lateness::TickLateness>);
impl multiview_control::program_metrics::ProgramMetricsSource for Counters {
    fn read(&self) -> Option<multiview_control::program_metrics::ProgramCounters> {
        Some(multiview_control::program_metrics::ProgramCounters {
            lateness: self.0.snapshot(),
            frames_repeated: 3,
            phases: multiview_engine::lateness::PhasesSnapshot::default(),
        })
    }
}

#[tokio::test]
async fn program_metrics_are_whole_system_only() {
    let dir = tempfile::tempdir().unwrap();
    let plain = rig(&dir.path().join("boot.toml"), false);
    let resp = send(
        &plain.router,
        get("/api/v1/metrics/program", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let lateness = multiview_engine::lateness::TickLateness::shared();
    lateness.record(1_500_000, 0);
    let state = plain
        .state
        .clone()
        .with_program_metrics(Arc::new(Counters(Arc::clone(&lateness))));
    let router = multiview_control::router(state);
    let resp = send(&router, get("/api/v1/metrics/program", SCOPED_ROUTER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(send(&router, get("/api/v1/metrics/program", VIEWER_TOKEN)).await).await;
    assert_eq!(body["ticks_total"], 1);
    assert_eq!(body["frames_repeated_total"], 3);
    assert_eq!(body["lateness_p99_ms"], 2.0);
    assert_eq!(body["lateness_max_ms"], 1.5);
}

/// PR #352 review (HIGH): an engine-minted boot take — `boot:<id>` and the
/// audio-only `boot:audio:<id>` — is NEVER persisted, applied or not, so it
/// cannot rewrite or delete the operator's accepted `[[routing.output]]` row;
/// a normal applied take still persists. The accepted store itself is
/// checked (its revision and its row), not just a snapshot.
#[allow(clippy::too_many_lines)] // One scenario: an operator row, then both boot ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_boot_take_never_rewrites_the_accepted_row() {
    use multiview_control::output_routing::{is_boot_take, RoutedAudio};
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    let mut events = SessionStream::new(rig.engine.subscribe(), "s", None);
    // A normal applied take persists (out1 = bars).
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "n1",
            "output-1",
            serde_json::json!({"source": "bars"}),
        ),
    )
    .await;
    let op = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!is_boot_take(&op), "a client take is never a boot take");
    rig.fake.emit_with_feed(
        &op,
        OUT_B_CONFIG_ID,
        "applied",
        RoutedFeed::Source("bars".into()),
    );
    wait_for(|| persisted_feed(&rig.state, OUT_B_CONFIG_ID).is_some()).await;
    let accepted = || {
        rig.state
            .boot_model
            .as_ref()
            .unwrap()
            .accepted()
            .unwrap()
            .unwrap()
    };
    let before = accepted();
    let row_of = |d: &multiview_control::boot_model::DesiredState| {
        d.config
            .routing
            .as_ref()
            .and_then(|t| t.output.iter().find(|xp| xp.output == OUT_B_CONFIG_ID))
            .cloned()
    };
    let before_row = row_of(&before).expect("the operator's row");
    let boot_path = dir.path().join("boot.toml");
    let on_disk = || {
        multiview_control::boot_model::load_desired(&boot_path)
            .unwrap()
            .expect("desired.json")
    };
    assert_eq!(row_of(&on_disk()).as_ref(), Some(&before_row));
    // Boot outcomes whose feed/audio differ from the row: Program video
    // (would DELETE the row) and an audio-only `none` (would rewrite it).
    let boot_ops = [
        format!("boot:{OUT_B_CONFIG_ID}"),
        format!("boot:audio:{OUT_B_CONFIG_ID}"),
    ];
    for (op, audio) in boot_ops
        .iter()
        .zip([RoutedAudio::Follow, RoutedAudio::None])
    {
        assert!(is_boot_take(op), "{op}");
        rig.fake
            .tx
            .send(OutputOutcome {
                operation_id: op.clone(),
                output: OUT_B_CONFIG_ID.to_owned(),
                tick: 90,
                outcome: "applied".to_owned(),
                feed: Some(RoutedFeed::Program),
                audio: Some(audio),
            })
            .unwrap();
    }
    // The relay handles outcomes in order: both events prove both were seen.
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while seen.len() < boot_ops.len() {
            if let Some(frame) = events.next_delta().await.unwrap() {
                if let Event::RouteOutput(e) = &frame.envelope.payload {
                    if is_boot_take(&e.operation_id) {
                        seen.push(e.operation_id.clone());
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(seen, boot_ops);
    let after = accepted();
    assert_eq!(
        row_of(&after).as_ref(),
        Some(&before_row),
        "a boot outcome never rewrites the accepted row"
    );
    assert!(
        Arc::ptr_eq(&before, &after),
        "nothing was committed to the accepted store"
    );
    assert_eq!(
        row_of(&on_disk()).as_ref(),
        Some(&before_row),
        "the durable store keeps the operator's row"
    );
    assert_eq!(
        persisted_feed(&rig.state, OUT_B_CONFIG_ID),
        Some(serde_json::json!({"source": "bars"}))
    );
}

/// PR #352 review (HIGH, abuse): `video` (the live bus) and
/// `configured_video` (the persisted feed) are redacted each by its own rule
/// over HTTP: a key that may read the output and the Program bus but not the
/// persisted source sees the live bus and `configured_video` redacted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readback_redacts_live_and_configured_video_independently() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), true);
    // Persist out1 = secret-cam (an operator take, applied).
    let resp = send(
        &rig.router,
        take(
            &rig,
            OPERATOR_TOKEN,
            "s1",
            "output-1",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    let op = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake.emit_with_feed(
        &op,
        OUT_B_CONFIG_ID,
        "applied",
        RoutedFeed::Source("secret-cam".into()),
    );
    wait_for(|| persisted_feed(&rig.state, OUT_B_CONFIG_ID).is_some()).await;
    let mut outputs = HashMap::new();
    outputs.insert(
        OUT_B_CONFIG_ID.to_owned(),
        OutputLive {
            bus: "program".into(),
            since_tick: 3,
            pending: None,
            degraded: false,
            audio: None,
            pending_audio: None,
            private_audio: false,
        },
    );
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs,
        encoders: 1,
        sessions_in_use: 0,
        sessions_max: 6,
        dropped_ticks: 0,
    });
    let state = body_json(send(&rig.router, get("/api/v1/routing", BUS_ROUTER_TOKEN)).await).await;
    assert!(!state.to_string().contains("secret-cam"), "{state}");
    let r = state["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["output"] == "output-1")
        .cloned()
        .unwrap();
    assert_eq!(r["video"], serde_json::json!({"bus": "program"}), "{r}");
    assert_eq!(
        r["configured_video"],
        serde_json::json!({"redacted": true}),
        "{r}"
    );
    assert_eq!(r["tally"], "program", "{r}");
    assert_eq!(r["state"], "on_air", "{r}");
}

struct FakeFrontDoor;
impl multiview_control::frontdoor_metrics::FrontDoorMetricsSource for FakeFrontDoor {
    fn read(&self) -> Option<multiview_control::frontdoor_metrics::FrontDoorMetricsReport> {
        Some(
            multiview_control::frontdoor_metrics::FrontDoorMetricsReport {
                received: 42,
                to_quic: 10,
                to_rtc: 5,
                dropped_classify: 0,
                dropped_per_source: 0,
                dropped_global: 0,
                dropped_vn: 0,
                dropped_initial: 0,
                dropped_queue_full: 0,
                dropped_no_consumer: 0,
                recv_errors: 0,
                tracked_sources: 2,
                received_established_socket: 7,
                received_listener_socket: 35,
                steered_unverified: 0,
                steering_overflow: 0,
                steering_updates: 1,
                steering_update_errors: 0,
                steered_peers: 3,
                kernel_drops_listener: 0,
                kernel_drops_established: 0,
                dropped_stun_per_source: 0,
                dropped_stun_global: 0,
                dropped_established_peer: 0,
                quic_retries: 4,
                udp443_steering: multiview_control::system::Udp443Steering::ReuseportCbpf,
                nft_floor: multiview_control::system::NftFloorStatus::Present,
            },
        )
    }
}

#[tokio::test]
async fn frontdoor_metrics_are_whole_system_only_and_404_without_a_source() {
    let dir = tempfile::tempdir().unwrap();
    let plain = rig(&dir.path().join("boot.toml"), false);
    let resp = send(
        &plain.router,
        get("/api/v1/metrics/frontdoor", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let state = plain
        .state
        .clone()
        .with_frontdoor_metrics(Arc::new(FakeFrontDoor));
    let router = multiview_control::router(state);
    let resp = send(
        &router,
        get("/api/v1/metrics/frontdoor", SCOPED_ROUTER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let anonymous = Request::builder()
        .uri("/api/v1/metrics/frontdoor")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, anonymous).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let body = body_json(send(&router, get("/api/v1/metrics/frontdoor", VIEWER_TOKEN)).await).await;
    assert_eq!(body["received"], 42);
    assert_eq!(body["to_quic"], 10);
    assert_eq!(body["quic_retries"], 4);
    assert_eq!(body["udp443_steering"], "reuseport_cbpf");
    assert_eq!(body["nft_floor"], "present");
}
