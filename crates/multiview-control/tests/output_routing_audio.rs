//! ADR-0108 slice 2 part 3 — the explicit audio feed of an output take over
//! HTTP against a fake engine seam: `stream: audio` (audio-only) and a
//! combined video + audio body reach the seam as typed fields; audio-feed
//! authorization (source, program, tracks + their route input) runs before
//! existence; `unknown-feed`, `audio-unsupported` (no slot, two tracks); the
//! plan's audio cost; and persistence of an applied audio-only take.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    AudioPlanCost, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanRefusal,
    RoutedAudio, RoutedFeed, RoutingLive, TakeSubmitError,
};
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
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
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "secret-cam"
channels = { kind = "stereo" }
target_track = "eng"
[[audio.routes]]
input_id = "bars"
channels = { kind = "stereo" }
target_track = "deu"
[[outputs]]
kind = "hls"
path = "/tmp/output-routing-audio/a/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
kind = "hls"
path = "/tmp/output-routing-audio/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

const OUT_B: &str = "hls /tmp/output-routing-audio/b/index.m3u8";

/// output-1 + bars only (no Program scope).
const SCOPED: &str = "aud-scoped.aud-scoped-secret";
/// output-1 + the track name `eng`, but not its route input `secret-cam`.
const TRACK_ONLY: &str = "aud-track.aud-track-secret";
/// output-1 + `eng` + `secret-cam`.
const TRACK_AND_INPUT: &str = "aud-both.aud-both-secret";
/// output-1 (object AND output axis) + Program scope + me1. Its output axis
/// names output-1 explicitly: a key with an output allowlist may route only
/// the outputs on it (#329 F2).
const BUS: &str = "aud-bus.aud-bus-secret";

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
        "aud-scoped",
        "aud-scoped-secret",
        scoped("aud-scoped", &["output-1", "bars"], None),
    );
    keys.register(
        "aud-track",
        "aud-track-secret",
        scoped("aud-track", &["output-1", "eng"], None),
    );
    keys.register(
        "aud-both",
        "aud-both-secret",
        scoped("aud-both", &["output-1", "eng", "secret-cam"], None),
    );
    keys.register(
        "aud-bus",
        "aud-bus-secret",
        scoped(
            "aud-bus",
            &["output-1", "me1"],
            Some(vec!["program:main".to_owned(), "output-1".to_owned()]),
        ),
    );
    keys
}

#[derive(Default)]
struct FakeState {
    takes: Vec<OutputTakeRequest>,
    /// The output has no program-audio slot.
    no_audio_slot: bool,
    /// The output's kind is not routable (`RunOutput.routable: false`).
    unroutable: bool,
    /// The audio label the output carries now (the plan compares it).
    carried: String,
    private_audio: bool,
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
            state: Mutex::new(FakeState {
                carried: "program".into(),
                ..FakeState::default()
            }),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
    fn takes(&self) -> Vec<OutputTakeRequest> {
        self.state.lock().unwrap().takes.clone()
    }
}

impl OutputRouting for Fake {
    fn plan(&self, _output: &str, _feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        Ok(PlanCost {
            needs_encoder: false,
            sessions_in_use: 0,
            sessions_max: 6,
            needs_aux_canvas: false,
        })
    }
    fn plan_audio(
        &self,
        _output: &str,
        _video: Option<&RoutedFeed>,
        audio: Option<&RoutedAudio>,
    ) -> Result<AudioPlanCost, PlanRefusal> {
        let s = self.state.lock().unwrap();
        if s.unroutable {
            return Err(PlanRefusal::OutputKindUnroutable);
        }
        if s.no_audio_slot && audio.is_some_and(|a| *a != RoutedAudio::Follow) {
            return Err(PlanRefusal::AudioUnsupported);
        }
        let changes = audio.is_some_and(|a| a.label() != s.carried);
        Ok(AudioPlanCost {
            needs_encoder: changes && !s.private_audio,
        })
    }
    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        let mut s = self.state.lock().unwrap();
        if s.unroutable {
            return Err(TakeSubmitError::Unroutable);
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
    state: AppState,
}

fn rig(path: &Path, doc: &str) -> Rig {
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let fake = Fake::new();
    let seam: Arc<dyn OutputRouting> = fake.clone();
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
    .with_output_routing(seam);
    Rig {
        router: multiview_control::router(state.clone()),
        fake,
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

fn audio_body(output: &str, audio: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "target": {"kind": "output", "output": output, "stream": "audio"},
        "audio": audio,
    })
}

/// A take; the per-output limit (2/s, tested elsewhere) is waited out so a
/// test can issue several takes on one output.
async fn take(
    rig: &Rig,
    token: &str,
    key: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    loop {
        let resp = send(
            &rig.router,
            post("/api/v1/routing/output/take", token, Some(key), &body),
        )
        .await;
        if resp.status() != StatusCode::TOO_MANY_REQUESTS {
            return resp;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn plan(rig: &Rig, token: &str, body: serde_json::Value) -> serde_json::Value {
    let resp = send(
        &rig.router,
        post("/api/v1/routing/plan", token, None, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

async fn problem_type(resp: axum::response::Response) -> (StatusCode, String) {
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body["type"].as_str().unwrap_or_default().to_owned())
}

fn persisted_row(state: &AppState, output: &str) -> Option<serde_json::Value> {
    let config: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(state).unwrap(),
    )
    .unwrap();
    config
        .routing
        .as_ref()
        .and_then(|t| t.output.iter().find(|xp| xp.output == output))
        .map(|xp| serde_json::to_value(xp).unwrap())
}

#[tokio::test]
async fn an_audio_only_take_reaches_the_seam_with_no_video() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    let resp = take(
        &rig,
        OPERATOR_TOKEN,
        "a1",
        audio_body("output-1", serde_json::json!({"source": "bars"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let takes = rig.fake.takes();
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].output, OUT_B);
    assert_eq!(takes[0].video, None, "the video is untouched");
    assert_eq!(takes[0].audio, Some(RoutedAudio::Source("bars".into())));
    // A track resolves to its route input.
    let resp = take(
        &rig,
        OPERATOR_TOKEN,
        "a2",
        audio_body("output-1", serde_json::json!({"tracks": ["eng"]})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        rig.fake.takes()[1].audio,
        Some(RoutedAudio::Track {
            name: "eng".into(),
            input: "secret-cam".into()
        })
    );
}

#[tokio::test]
async fn a_combined_body_carries_video_and_audio_and_slice_one_bodies_still_work() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    let body = serde_json::json!({
        "target": {"kind": "output", "output": "output-1", "stream": "video"},
        "feed": {"source": "bars"},
        "audio": "none",
    });
    assert_eq!(
        take(&rig, OPERATOR_TOKEN, "c1", body).await.status(),
        StatusCode::ACCEPTED
    );
    let t = &rig.fake.takes()[0];
    assert_eq!(t.video, Some(RoutedFeed::Source("bars".into())));
    assert_eq!(t.audio, Some(RoutedAudio::None));
    // Slice-1 body: `audio: "follow"`.
    let body = serde_json::json!({
        "target": {"kind": "output", "output": "output-1", "stream": "video"},
        "feed": {"bus": "program"},
        "audio": "follow",
    });
    assert_eq!(
        take(&rig, OPERATOR_TOKEN, "c2", body).await.status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(rig.fake.takes()[1].audio, Some(RoutedAudio::Follow));
    // Mismatched shapes are 422 and never reach the seam.
    for (key, body) in [
        (
            "m1",
            serde_json::json!({"target": {"kind": "output", "output": "output-1", "stream": "audio"}, "feed": {"source": "bars"}, "audio": "none"}),
        ),
        (
            "m2",
            serde_json::json!({"target": {"kind": "output", "output": "output-1", "stream": "audio"}}),
        ),
        (
            "m3",
            serde_json::json!({"target": {"kind": "output", "output": "output-1", "stream": "video"}, "audio": "none"}),
        ),
    ] {
        let resp = take(&rig, OPERATOR_TOKEN, key, body).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{key}");
    }
    assert_eq!(rig.fake.takes().len(), 2);
}

#[tokio::test]
async fn audio_feed_authorization_runs_before_existence() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    // A source the principal cannot read, existing or not: 403 on both routes.
    for source in ["secret-cam", "no-such-source"] {
        let body = audio_body("output-1", serde_json::json!({"source": source}));
        let resp = take(&rig, SCOPED, "s", body.clone()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{source}");
        let resp = send(
            &rig.router,
            post("/api/v1/routing/plan", SCOPED, None, &body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "plan {source}");
    }
    // The program mix needs the Program scope + me1.
    let program = audio_body("output-1", serde_json::json!("program"));
    assert_eq!(
        take(&rig, SCOPED, "p1", program.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        take(&rig, BUS, "p2", program).await.status(),
        StatusCode::ACCEPTED
    );
    // A track needs its name AND its route input; an unknown name is 403
    // without the grant on that name.
    let eng = audio_body("output-1", serde_json::json!({"tracks": ["eng"]}));
    assert_eq!(
        take(&rig, TRACK_ONLY, "t1", eng.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    let unknown = audio_body("output-1", serde_json::json!({"tracks": ["fra"]}));
    assert_eq!(
        take(&rig, TRACK_ONLY, "t2", unknown).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        take(&rig, TRACK_AND_INPUT, "t3", eng).await.status(),
        StatusCode::ACCEPTED
    );
    // `none` and `follow` need nothing beyond the output.
    let none = audio_body("output-1", serde_json::json!("none"));
    assert_eq!(
        take(&rig, SCOPED, "n1", none).await.status(),
        StatusCode::ACCEPTED
    );
    // The denied key `s` was not burned.
    let bars = audio_body("output-1", serde_json::json!({"source": "bars"}));
    assert_eq!(
        take(&rig, SCOPED, "s", bars).await.status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(rig.fake.takes().len(), 4);
}

#[tokio::test]
async fn unknown_audio_feeds_and_capability_refusals_are_typed() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    let (status, kind) = problem_type(
        take(
            &rig,
            OPERATOR_TOKEN,
            "u1",
            audio_body("output-1", serde_json::json!({"source": "nope"})),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(kind.ends_with("unknown-feed"), "{kind}");
    let (status, kind) = problem_type(
        take(
            &rig,
            OPERATOR_TOKEN,
            "u2",
            audio_body("output-1", serde_json::json!({"tracks": ["fra"]})),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(kind.ends_with("unknown-feed"), "{kind}");
    // Two tracks: the output carries one routed audio feed.
    let two = audio_body("output-1", serde_json::json!({"tracks": ["eng", "deu"]}));
    let resp = take(&rig, OPERATOR_TOKEN, "u3", two.clone()).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert!(problem["type"]
        .as_str()
        .unwrap()
        .ends_with("audio-unsupported"));
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("choose one track"),
        "{problem}"
    );
    let planned = plan(&rig, OPERATOR_TOKEN, two).await;
    assert_eq!(planned["refusals"][0]["type"], "audio-unsupported");
    // No program-audio slot: 409 at take, a refusal at plan.
    rig.fake.state.lock().unwrap().no_audio_slot = true;
    let src = audio_body("output-1", serde_json::json!({"source": "bars"}));
    let resp = take(&rig, OPERATOR_TOKEN, "u4", src.clone()).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let problem = body_json(resp).await;
    assert!(problem["type"]
        .as_str()
        .unwrap()
        .ends_with("audio-unsupported"));
    assert_eq!(
        problem["detail"],
        "This output cannot carry a routed audio feed"
    );
    let planned = plan(&rig, OPERATOR_TOKEN, src).await;
    assert_eq!(planned["refusals"][0]["type"], "audio-unsupported");
    assert!(
        rig.fake.takes().is_empty(),
        "nothing refused reached the engine"
    );
}

#[tokio::test]
async fn plan_reports_one_audio_encode_then_none_once_carried() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    let src = audio_body("output-1", serde_json::json!({"source": "bars"}));
    let planned = plan(&rig, OPERATOR_TOKEN, src.clone()).await;
    assert_eq!(planned["cost"]["audio_encoders"], 1);
    assert_eq!(
        planned["cost"]["audio_cost"],
        "+1 audio encode (transient, CPU)"
    );
    assert_eq!(planned["refusals"], serde_json::json!([]));
    rig.fake.state.lock().unwrap().carried = "src:bars".into();
    let planned = plan(&rig, OPERATOR_TOKEN, src).await;
    assert_eq!(planned["cost"]["audio_encoders"], 0);
    assert!(planned["cost"].get("audio_cost").is_none(), "{planned}");
    // A running private bus absorbs a change.
    rig.fake.state.lock().unwrap().private_audio = true;
    let planned = plan(
        &rig,
        OPERATOR_TOKEN,
        audio_body("output-1", serde_json::json!("none")),
    )
    .await;
    assert_eq!(planned["cost"]["audio_encoders"], 0);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_audio_only_take_persists_audio_and_keeps_the_video() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    // First a video take to bars, applied.
    let video = serde_json::json!({
        "target": {"kind": "output", "output": "output-1", "stream": "video"},
        "feed": {"source": "bars"},
    });
    let op = body_json(take(&rig, OPERATOR_TOKEN, "v1", video).await).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake
        .tx
        .send(OutputOutcome {
            operation_id: op,
            output: OUT_B.into(),
            tick: 5,
            outcome: "applied".into(),
            feed: Some(RoutedFeed::Source("bars".into())),
            audio: Some(RoutedAudio::Follow),
        })
        .unwrap();
    wait_for(|| persisted_row(&rig.state, OUT_B).is_some()).await;
    // Then an audio-only take; the engine reports the output's (unchanged)
    // bus with the outcome, as it does for every outcome.
    let resp = take(
        &rig,
        OPERATOR_TOKEN,
        "a1",
        audio_body("output-1", serde_json::json!("none")),
    )
    .await;
    let op = body_json(resp).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake
        .tx
        .send(OutputOutcome {
            operation_id: op,
            output: OUT_B.into(),
            tick: 9,
            outcome: "applied".into(),
            feed: Some(RoutedFeed::Source("bars".into())),
            audio: Some(RoutedAudio::None),
        })
        .unwrap();
    wait_for(|| persisted_row(&rig.state, OUT_B).is_some_and(|r| r["audio"] == "none")).await;
    let row = persisted_row(&rig.state, OUT_B).unwrap();
    assert_eq!(row["video"], serde_json::json!({"source": "bars"}), "{row}");
    // Back to Program video and follow audio: the row is removed.
    let back = serde_json::json!({
        "target": {"kind": "output", "output": "output-1", "stream": "video"},
        "feed": {"bus": "program"},
        "audio": "follow",
    });
    let op = body_json(take(&rig, OPERATOR_TOKEN, "b1", back).await).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.fake
        .tx
        .send(OutputOutcome {
            operation_id: op,
            output: OUT_B.into(),
            tick: 12,
            outcome: "applied".into(),
            feed: Some(RoutedFeed::Program),
            audio: Some(RoutedAudio::Follow),
        })
        .unwrap();
    wait_for(|| persisted_row(&rig.state, OUT_B).is_none()).await;
}

#[tokio::test]
async fn readback_effective_audio_is_the_live_audio_redacted_per_object() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    let mut outputs = std::collections::HashMap::new();
    outputs.insert(
        OUT_B.to_owned(),
        multiview_control::output_routing::OutputLive {
            bus: "program".into(),
            since_tick: 1,
            pending: Some(("program".into(), 3)),
            degraded: false,
            audio: Some("src:secret-cam".into()),
            pending_audio: Some("track:eng".into()),
            private_audio: true,
        },
    );
    rig.fake.state.lock().unwrap().live = Some(RoutingLive {
        outputs,
        encoders: 1,
        sessions_in_use: 0,
        sessions_max: 6,
        dropped_ticks: 0,
    });
    let row = |body: serde_json::Value| {
        body["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["output"] == "output-1")
            .cloned()
            .unwrap()
    };
    let whole =
        row(body_json(send(&rig.router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await);
    assert_eq!(
        whole["effective_audio"],
        serde_json::json!({"source": "secret-cam"})
    );
    assert_eq!(
        whole["pending"]["audio"],
        serde_json::json!({"tracks": ["eng"]})
    );
    // Program scope (reads the Program video) but not secret-cam nor `eng`.
    let bus = row(body_json(send(&rig.router, get("/api/v1/routing", BUS)).await).await);
    let text = bus.to_string();
    assert!(
        !text.contains("secret-cam") && !text.contains("eng"),
        "{text}"
    );
    assert_eq!(
        bus["effective_audio"],
        serde_json::json!({"redacted": true})
    );
    assert_eq!(
        bus["pending"]["audio"],
        serde_json::json!({"redacted": true})
    );
    // A principal granted secret-cam and `eng` sees both.
    let both =
        row(body_json(send(&rig.router, get("/api/v1/routing", TRACK_AND_INPUT)).await).await);
    assert_eq!(
        both["effective_audio"],
        serde_json::json!({"source": "secret-cam"})
    );
    assert_eq!(
        both["pending"]["audio"],
        serde_json::json!({"tracks": ["eng"]})
    );
    // The track name alone is not enough: its route input is secret-cam.
    let track = row(body_json(send(&rig.router, get("/api/v1/routing", TRACK_ONLY)).await).await);
    assert_eq!(
        track["pending"]["audio"],
        serde_json::json!({"redacted": true}),
        "{track}"
    );
}

/// PR #352 review (MEDIUM, media): an audio-only take on an output whose kind
/// the engine does not route is `409 output-kind-unroutable` from the gate,
/// before any reservation or enqueue (the key is not burned).
#[tokio::test]
async fn an_audio_take_on_an_unroutable_output_is_409_before_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    rig.fake.state.lock().unwrap().unroutable = true;
    let src = audio_body("output-1", serde_json::json!({"source": "bars"}));
    let (status, kind) = problem_type(take(&rig, OPERATOR_TOKEN, "k1", src.clone()).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(kind.ends_with("output-kind-unroutable"), "{kind}");
    assert!(rig.fake.takes().is_empty(), "nothing reached the engine");
    let routing = rig.state.output_routing.as_ref().unwrap();
    assert_eq!(routing.records_len(), 0, "refused before reserve");
    let planned = plan(&rig, OPERATOR_TOKEN, src.clone()).await;
    assert_eq!(planned["refusals"][0]["type"], "output-kind-unroutable");
    // Once routable, the same key is a fresh take.
    rig.fake.state.lock().unwrap().unroutable = false;
    assert_eq!(
        take(&rig, OPERATOR_TOKEN, "k1", src).await.status(),
        StatusCode::ACCEPTED
    );
}

/// #342 B2 review A1 with explicit audio: a take key is bound to the WHOLE
/// take — output, video and audio. The same key with the same video but
/// another audio (or an audio-only take where a video take was recorded) is
/// 409 and enqueues nothing; the identical body replays its first operation.
#[tokio::test]
async fn a_reused_take_key_is_bound_to_its_audio_too() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"), DOC);
    let combined = |audio: serde_json::Value| {
        serde_json::json!({
            "target": {"kind": "output", "output": "output-1", "stream": "video"},
            "feed": {"source": "bars"},
            "audio": audio,
        })
    };
    let first = take(
        &rig,
        OPERATOR_TOKEN,
        "bound",
        combined(serde_json::json!("none")),
    )
    .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let op = body_json(first).await["operation_id"].clone();
    for body in [
        combined(serde_json::json!("program")),
        combined(serde_json::json!({"tracks": ["deu"]})),
        serde_json::json!({
            "target": {"kind": "output", "output": "output-1", "stream": "video"},
            "feed": {"source": "bars"},
        }),
        audio_body("output-1", serde_json::json!("none")),
    ] {
        let resp = take(&rig, OPERATOR_TOKEN, "bound", body.clone()).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{body}");
    }
    assert_eq!(rig.fake.takes().len(), 1, "a reused key never enqueues");
    let replay = take(
        &rig,
        OPERATOR_TOKEN,
        "bound",
        combined(serde_json::json!("none")),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(replay).await["operation_id"], op);
    assert_eq!(rig.fake.takes().len(), 1);
}
