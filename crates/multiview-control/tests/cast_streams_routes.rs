//! ADR-0111 C1: cast stream tokens over HTTP against a fake cast engine and a
//! fake output routing seam: token shape and storage, expiry, revocation,
//! caps, BOLA on every id, feed selection and the output lifecycle, the
//! public media/report routes and the receiver app.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::cast_engine::{CastEngine, CastEngineError, CastOutputStatus};
use multiview_control::cast_streams::CastStreams;
use multiview_control::output_routing::{
    AudioPlanCost, OutputLive, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost,
    PlanRefusal, RoutedAudio, RoutedFeed, RoutingLive, TakeSubmitError,
};
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_bytes, body_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN};

const DOC: &str = r##"
schema_version = 1
[canvas]
width = 640
height = 360
fps = "25/1"
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
id = "secret-cam"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
kind = "cast"
id = "cast-1"
codec = "h264"
[[outputs]]
kind = "cast"
id = "cast-2"
codec = "h264"
"##;

/// Operator confined to output-0 (cast-1) and source `a`, with Program.
const SCOPED: &str = "cs-scoped.cs-scoped-secret";
/// Operator confined to output-1 (cast-2) and source `a`, no Program scope.
const SCOPED_B: &str = "cs-other.cs-other-secret";
/// A second unrestricted operator.
const OPERATOR_2: &str = "cs-op2.cs-op2-secret";
/// A third unrestricted operator.
const FRESH: &str = "cs-fresh.cs-fresh-secret";

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
        "cs-scoped",
        "cs-scoped-secret",
        scoped(
            "cs-scoped",
            &["output-0", "a", "me1"],
            &["output-0", "program:main"],
        ),
    );
    keys.register(
        "cs-other",
        "cs-other-secret",
        scoped("cs-other", &["output-1", "a"], &["output-1"]),
    );
    for id in ["cs-op2", "cs-fresh"] {
        keys.register(
            id,
            &format!("{id}-secret"),
            Principal {
                key_id: id.to_owned(),
                role: Role::Operator,
                scoped_object_ids: None,
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
        );
    }
    keys
}

/// A fake cast engine over real directories.
struct FakeCast {
    dirs: HashMap<String, PathBuf>,
    state: Mutex<HashMap<String, (bool, u64)>>,
    activations: Mutex<Vec<(String, bool)>>,
    deactivations: Mutex<Vec<String>>,
    error: Mutex<Option<String>>,
}

impl CastEngine for FakeCast {
    fn outputs(&self) -> Vec<CastOutputStatus> {
        let state = self.state.lock().unwrap();
        let error = self.error.lock().unwrap().clone();
        ["cast-1", "cast-2"]
            .iter()
            .map(|id| {
                let (active, generation) = state.get(*id).copied().unwrap_or((false, 0));
                CastOutputStatus {
                    id: (*id).to_owned(),
                    dir: self.dirs[*id].clone(),
                    active,
                    ready: active,
                    generation,
                    video_segments: if active { 9 } else { 0 },
                    dropped_packets: 0,
                    last_error: error.clone(),
                    width: 640,
                    height: 360,
                    fps_num: 25,
                    fps_den: 1,
                }
            })
            .collect()
    }
    fn activate(&self, id: &str, preroll: bool) -> Result<u64, CastEngineError> {
        if let Some(e) = self.error.lock().unwrap().clone() {
            return Err(CastEngineError::Unavailable(e));
        }
        self.activations
            .lock()
            .unwrap()
            .push((id.to_owned(), preroll));
        self.state.lock().unwrap().insert(id.to_owned(), (true, 1));
        Ok(1)
    }
    fn deactivate(&self, id: &str) -> Result<(), CastEngineError> {
        self.deactivations.lock().unwrap().push(id.to_owned());
        self.state.lock().unwrap().insert(id.to_owned(), (false, 0));
        Ok(())
    }
}

/// A fake routing seam: a take applies at once (readback + `applied`
/// outcome) unless `refuse` names an outcome.
struct FakeRouting {
    live: Mutex<RoutingLive>,
    takes: Mutex<Vec<OutputTakeRequest>>,
    refuse: Mutex<Option<String>>,
    no_encoder: Mutex<bool>,
    tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
}

impl FakeRouting {
    fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let mut live = RoutingLive::default();
        for id in ["cast-1", "cast-2"] {
            live.outputs.insert(
                id.to_owned(),
                OutputLive {
                    bus: "program".to_owned(),
                    since_tick: 0,
                    pending: None,
                    degraded: false,
                    audio: None,
                    pending_audio: None,
                    private_audio: false,
                },
            );
        }
        Arc::new(Self {
            live: Mutex::new(live),
            takes: Mutex::new(Vec::new()),
            refuse: Mutex::new(None),
            no_encoder: Mutex::new(false),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
    fn takes(&self) -> Vec<OutputTakeRequest> {
        self.takes.lock().unwrap().clone()
    }
    fn bus(&self, id: &str) -> String {
        self.live.lock().unwrap().outputs[id].bus.clone()
    }
}

impl OutputRouting for FakeRouting {
    fn plan(&self, _output: &str, _feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        Ok(PlanCost {
            needs_encoder: *self.no_encoder.lock().unwrap(),
            sessions_in_use: 4,
            sessions_max: 4,
            needs_aux_canvas: false,
        })
    }
    fn plan_audio(
        &self,
        _o: &str,
        _v: Option<&RoutedFeed>,
        _a: Option<&RoutedAudio>,
    ) -> Result<AudioPlanCost, PlanRefusal> {
        Ok(AudioPlanCost::default())
    }
    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        self.takes.lock().unwrap().push(take.clone());
        let refused = self.refuse.lock().unwrap().clone();
        let outcome = if let Some(r) = refused {
            r
        } else {
            {
                let bus = take.video.as_ref().unwrap().bus_name();
                self.live
                    .lock()
                    .unwrap()
                    .outputs
                    .get_mut(&take.output)
                    .unwrap()
                    .bus = bus;
                "applied".to_owned()
            }
        };
        let _ = self.tx.send(OutputOutcome {
            operation_id: take.operation_id,
            output: take.output,
            tick: 1,
            outcome,
            feed: take.video,
            audio: None,
        });
        Ok(())
    }
    fn snapshot(&self) -> Option<RoutingLive> {
        Some(self.live.lock().unwrap().clone())
    }
    fn subscribe_outcomes(&self) -> Option<Receiver<OutputOutcome>> {
        self.rx.lock().unwrap().take()
    }
}

struct Rig {
    router: axum::Router,
    cast: Arc<FakeCast>,
    routing: Arc<FakeRouting>,
    now: Arc<AtomicU64>,
    state: AppState,
    dir: tempfile::TempDir,
}

impl Rig {
    fn advance(&self, d: Duration) {
        self.now
            .fetch_add(u64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

fn write_stream(dir: &Path, tag: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("master.m3u8"), format!("#EXTM3U\n#{tag}\n")).unwrap();
    std::fs::write(dir.join("seg-v-1-0.m4s"), vec![7u8; 1000]).unwrap();
    std::fs::write(dir.join("init-v-1-0.mp4"), b"init").unwrap();
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let mut dirs = HashMap::new();
    for id in ["cast-1", "cast-2"] {
        let dir = tmp.path().join(id);
        write_stream(&dir, id);
        dirs.insert(id.to_owned(), dir);
    }
    std::fs::write(tmp.path().join("secret.m3u8"), "#EXTM3U\n#secret\n").unwrap();
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let cast = Arc::new(FakeCast {
        dirs,
        state: Mutex::new(HashMap::new()),
        activations: Mutex::new(Vec::new()),
        deactivations: Mutex::new(Vec::new()),
        error: Mutex::new(None),
    });
    let routing = FakeRouting::new();
    let now = Arc::new(AtomicU64::new(1_000_000));
    let clock_now = Arc::clone(&now);
    let streams = Arc::new(CastStreams::with_clock(Arc::new(move || {
        clock_now.load(Ordering::SeqCst)
    })));
    let state = AppState::new(
        Arc::clone(&engine),
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        tmp.path().join("multiview.toml"),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )))
    .with_output_routing({
        let seam: Arc<dyn OutputRouting> = routing.clone();
        seam
    })
    .with_cast_engine(cast.clone())
    .with_cast_streams(streams)
    .with_cast_app_id(Some("1A2B3C4D".to_owned()));
    Rig {
        router: multiview_control::router(state.clone()),
        cast,
        routing,
        now,
        state,
        dir: tmp,
    }
}

fn post(path: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    support::post_json(path, token, body)
}

fn delete(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn public(method: &str, path: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(path)
}

async fn create(
    rig: &Rig,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = send(&rig.router, post("/api/v1/cast/streams", token, &body)).await;
    let status = response.status();
    let bytes = body_bytes(response).await;
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

fn token_of(doc: &serde_json::Value) -> String {
    let path = doc["path"].as_str().unwrap();
    path.strip_prefix("/cast/")
        .unwrap()
        .strip_suffix("/master.m3u8")
        .unwrap()
        .to_owned()
}

async fn fetch(rig: &Rig, path: &str) -> (StatusCode, Vec<u8>) {
    let response = send(
        &rig.router,
        public("GET", path).body(Body::empty()).unwrap(),
    )
    .await;
    let status = response.status();
    (status, body_bytes(response).await)
}

#[tokio::test]
async fn create_answers_a_43_char_token_path_and_serves_the_output() {
    let rig = rig();
    let (status, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    assert_eq!(token.len(), 43);
    assert!(token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    assert_eq!(doc["output"], "output-0");
    assert_eq!(doc["feed"], serde_json::json!({"bus": "program"}));
    assert_eq!(doc["content_type"], "application/vnd.apple.mpegurl");
    assert_eq!(doc["hls_segment_format"], "fmp4");
    assert_eq!(doc["hls_video_segment_format"], "fmp4");
    assert_eq!(doc["ready"], true);
    assert_eq!(doc["segments"], 9);
    // Program already on air: no take, pre-rolled activation.
    assert_eq!(rig.routing.takes(), Vec::new());
    assert_eq!(
        *rig.cast.activations.lock().unwrap(),
        vec![("cast-1".to_owned(), true)]
    );
    let (status, body) = fetch(&rig, &format!("/cast/{token}/master.m3u8")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("#cast-1"));
    // The registry holds no plaintext.
    let dump = format!("{:?}", rig.state.cast_streams);
    assert!(!dump.contains(&token));
}

#[tokio::test]
async fn pending_and_idle_expiry_and_immediate_revoke() {
    let rig = rig();
    let (_, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let token = token_of(&doc);
    rig.advance(Duration::from_secs(91));
    let (status, pending_body) = fetch(&rig, &format!("/cast/{token}/master.m3u8")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "pending expired after 90 s");

    let (_, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let token = token_of(&doc);
    for _ in 0..3 {
        rig.advance(Duration::from_secs(55));
        assert_eq!(
            fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
            StatusCode::OK
        );
    }
    rig.advance(Duration::from_secs(61));
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::NOT_FOUND,
        "idle expiry"
    );

    let (_, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let token = token_of(&doc);
    let id = doc["id"].as_str().unwrap();
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    let response = send(
        &rig.router,
        delete(&format!("/api/v1/cast/streams/{id}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (status, revoked_body) = fetch(&rig, &format!("/cast/{token}/master.m3u8")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "revoked at once");
    let report = send(
        &rig.router,
        public("POST", &format!("/cast/{token}/report"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        report.status(),
        StatusCode::NOT_FOUND,
        "report 404 on revoke"
    );
    // Identical bodies: unknown, expired, revoked, bad file.
    let (_, unknown_body) = fetch(&rig, &format!("/cast/{}/master.m3u8", "A".repeat(43))).await;
    let (_, live) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let (_, bad_file) = fetch(&rig, &format!("/cast/{}/x.ts", token_of(&live))).await;
    assert_eq!(pending_body, revoked_body);
    assert_eq!(pending_body, unknown_body);
    assert_eq!(pending_body, bad_file);
}

#[tokio::test]
async fn bola_scoped_principals_and_owners() {
    let rig = rig();
    // A key confined to output-1 cannot use output-0 ...
    let (status, _) = create(&rig, SCOPED_B, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // ... nor Program (no Program scope), nor a source it may not read.
    let (status, _) = create(&rig, SCOPED_B, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = create(
        &rig,
        SCOPED,
        serde_json::json!({"feed": {"source": "secret-cam"}}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Viewer cannot create.
    let (status, _) = create(&rig, VIEWER_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An output that shows a feed the caller cannot read: no token.
    let (status, _) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"output": "output-1", "feed": {"source": "secret-cam"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(rig.routing.bus("cast-2"), "src:secret-cam");
    let (status, _) = create(&rig, SCOPED_B, serde_json::json!({"output": "output-1"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "output-1 shows secret-cam");

    let (status, mine) = create(&rig, SCOPED, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    let id = mine["id"].as_str().unwrap();
    // Another principal: 404 on GET and DELETE, absent from its list.
    let other = send(
        &rig.router,
        support::get(&format!("/api/v1/cast/streams/{id}"), OPERATOR_2),
    )
    .await;
    assert_eq!(other.status(), StatusCode::NOT_FOUND);
    let other = send(
        &rig.router,
        delete(&format!("/api/v1/cast/streams/{id}"), OPERATOR_2),
    )
    .await;
    assert_eq!(other.status(), StatusCode::NOT_FOUND);
    let list = body_json(
        send(
            &rig.router,
            support::get("/api/v1/cast/streams", OPERATOR_2),
        )
        .await,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    // The owner sees its path; an Admin sees the stream without the path.
    let own = body_json(
        send(
            &rig.router,
            support::get(&format!("/api/v1/cast/streams/{id}"), SCOPED),
        )
        .await,
    )
    .await;
    assert_eq!(own["path"], mine["path"]);
    let admin = send(
        &rig.router,
        support::get(&format!("/api/v1/cast/streams/{id}"), ADMIN_TOKEN),
    )
    .await;
    assert_eq!(admin.status(), StatusCode::OK);
    assert!(body_json(admin).await.get("path").is_none());
    // Lists never carry a path or a token.
    let list = body_bytes(
        send(
            &rig.router,
            support::get("/api/v1/cast/streams", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let text = String::from_utf8(list).unwrap();
    assert!(!text.contains("\"path\"") && !text.contains(&token_of(&mine)));
    // Config lists only the outputs the caller may use.
    let config =
        body_json(send(&rig.router, support::get("/api/v1/cast/config", SCOPED)).await).await;
    assert_eq!(config["outputs"].as_array().unwrap().len(), 1);
    assert_eq!(config["receiver_app_id"], "1A2B3C4D");
    assert_eq!(config["receiver_path"], "/cast/receiver/");
    // No token in the audit log.
    let audit = rig.state.audit.list(None).unwrap();
    let audit = serde_json::to_string(&audit).unwrap();
    assert!(!audit.contains(&token_of(&mine)));
}

#[tokio::test]
async fn lifecycle_share_take_linger_and_take_back() {
    let rig = rig();
    let feed = serde_json::json!({"feed": {"source": "a"}});
    let (status, first) = create(&rig, OPERATOR_TOKEN, feed.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    // The first free output is taken (cast: op id), then activated without
    // pre-roll (the buffered packets are the old picture).
    let takes = rig.routing.takes();
    assert_eq!(takes.len(), 1);
    assert!(takes[0].operation_id.starts_with("cast:"));
    assert_eq!(takes[0].output, "cast-1");
    assert_eq!(
        *rig.cast.activations.lock().unwrap(),
        vec![("cast-1".to_owned(), false)]
    );
    // A second stream of the same feed shares the output: no take.
    let (_, second) = create(&rig, OPERATOR_2, feed.clone()).await;
    assert_eq!(second["output"], first["output"]);
    assert_eq!(rig.routing.takes().len(), 1);
    // A different feed goes to the free output.
    let (_, program) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    assert_eq!(program["output"], "output-1");
    // Every output busy with another feed: cast-capacity.
    let (status, doc) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"feed": {"bus": "clean"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(doc["type"], "/problems/cast-capacity");
    // End both streams on cast-1: linger, then deactivate + take back.
    for doc in [&first, &second] {
        let token = if doc == &first {
            OPERATOR_TOKEN
        } else {
            OPERATOR_2
        };
        let id = doc["id"].as_str().unwrap();
        let r = send(
            &rig.router,
            delete(&format!("/api/v1/cast/streams/{id}"), token),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
    }
    assert!(rig.cast.deactivations.lock().unwrap().is_empty(), "lingers");
    rig.advance(Duration::from_secs(10));
    let _ = send(
        &rig.router,
        support::get("/api/v1/cast/streams", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(
        *rig.cast.deactivations.lock().unwrap(),
        vec!["cast-1".to_owned()]
    );
    let takes = rig.routing.takes();
    let back = takes.last().unwrap();
    assert!(back.operation_id.starts_with("cast:"));
    assert_eq!(back.video, Some(RoutedFeed::Program));
    assert_eq!(rig.routing.bus("cast-1"), "program");
    // A cast take is never persisted (no routing row written).
    let desired = multiview_control::boot_model::current_desired_config(&rig.state).unwrap();
    assert!(
        desired
            .get("routing")
            .is_none_or(|r| r.get("output").is_none()),
        "{desired}"
    );
}

#[tokio::test]
async fn refusals_pass_through_and_release_the_stream() {
    let rig = rig();
    *rig.routing.refuse.lock().unwrap() = Some("refused:feed-not-fresh".to_owned());
    let (status, doc) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(doc["type"], "/problems/refused-feed-not-fresh");
    let list = body_json(
        send(
            &rig.router,
            support::get("/api/v1/cast/streams", OPERATOR_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    *rig.routing.refuse.lock().unwrap() = None;
    *rig.routing.no_encoder.lock().unwrap() = true;
    rig.advance(Duration::from_secs(4));
    let (status, doc) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(doc["type"], "/problems/cast-no-encoder");
    let (status, doc) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"feed": {"source": "nope"}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{doc}");
    rig.advance(Duration::from_secs(4));
    // Engine not usable: cast-unavailable with its text.
    *rig.cast.error.lock().unwrap() = Some("the cast directory is not writable".to_owned());
    let (status, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(doc["type"], "/problems/cast-unavailable");
    let (status, doc) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"output": "output-0"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(doc["detail"], "the cast directory is not writable");
}

#[tokio::test]
async fn no_engine_is_cast_unavailable() {
    let rig = rig();
    let mut state = rig.state.clone();
    state.cast_engine = None;
    let router = multiview_control::router(state);
    let response = send(
        &router,
        post(
            "/api/v1/cast/streams",
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(response).await["type"],
        "/problems/cast-unavailable"
    );
}

#[tokio::test]
async fn caps_and_creation_rate() {
    let rig = rig();
    for i in 0..8 {
        if i % 4 == 0 && i > 0 {
            rig.advance(Duration::from_secs(4));
        }
        let (status, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{i}: {doc}");
    }
    rig.advance(Duration::from_secs(4));
    let (status, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "9th per principal");
    assert_eq!(doc["type"], "/problems/cast-capacity");
    // Rate: a burst of 4, then 429.
    for _ in 0..4 {
        let _ = create(&rig, ADMIN_TOKEN, serde_json::json!({})).await;
    }
    let response = send(
        &rig.router,
        post("/api/v1/cast/streams", ADMIN_TOKEN, &serde_json::json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn the_33rd_stream_is_cast_capacity() {
    let rig = rig();
    let owners = [OPERATOR_TOKEN, OPERATOR_2, ADMIN_TOKEN, SCOPED];
    for round in 0..2 {
        for owner in owners {
            for _ in 0..4 {
                let (status, doc) = create(&rig, owner, serde_json::json!({})).await;
                assert_eq!(status, StatusCode::CREATED, "{round} {doc}");
            }
        }
        rig.advance(Duration::from_secs(5));
    }
    let list = body_json(
        send(
            &rig.router,
            support::get("/api/v1/cast/streams", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 32);
    let (status, doc) = create(&rig, FRESH, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{doc}");
    assert_eq!(doc["type"], "/problems/cast-capacity");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one walk through the whole media contract
async fn media_types_cache_cors_range_head_and_scope() {
    let rig = rig();
    let (_, a) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let (_, b) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(b["output"], "output-1");
    let ta = token_of(&a);
    let get = |path: String, origin: bool| {
        let mut r = public("GET", &path);
        if origin {
            r = r.header(header::ORIGIN, "https://cast.example");
        }
        r.body(Body::empty()).unwrap()
    };
    let r = send(&rig.router, get(format!("/cast/{ta}/master.m3u8"), true)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let h = r.headers();
    assert_eq!(h[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
    // ABUSE-2: the token surface overrides the public-HLS cache tiers.
    assert_eq!(h[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        h[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://cast.example"
    );
    assert_eq!(h[header::VARY], "Origin");
    assert_eq!(
        h[header::ACCESS_CONTROL_EXPOSE_HEADERS],
        "Content-Length, Content-Range"
    );
    let r = send(&rig.router, get(format!("/cast/{ta}/seg-v-1-0.m4s"), true)).await;
    assert_eq!(r.headers()[header::CONTENT_TYPE], "video/iso.segment");
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    let r = send(
        &rig.router,
        get(format!("/cast/{ta}/init-v-1-0.mp4"), false),
    )
    .await;
    assert_eq!(r.headers()[header::CONTENT_TYPE], "video/mp4");
    // Range => 206; HEAD => length, no body.
    let r = send(
        &rig.router,
        public("GET", &format!("/cast/{ta}/seg-v-1-0.m4s"))
            .header(header::RANGE, "bytes=0-99")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(r).await.len(), 100);
    let r = send(
        &rig.router,
        public("HEAD", &format!("/cast/{ta}/seg-v-1-0.m4s"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_LENGTH], "1000");
    // 404 carries CORS too; OPTIONS preflight.
    let r = send(
        &rig.router,
        get(format!("/cast/{}/master.m3u8", "B".repeat(43)), true),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://cast.example"
    );
    let r = send(
        &rig.router,
        public("OPTIONS", &format!("/cast/{ta}/master.m3u8"))
            .header(header::ORIGIN, "https://cast.example")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        r.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
        "GET, HEAD, OPTIONS"
    );
    assert_eq!(
        r.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
        "Range, Content-Type, Accept-Encoding"
    );
    // Token A serves output A only.
    let (_, body) = fetch(&rig, &format!("/cast/{ta}/master.m3u8")).await;
    assert!(String::from_utf8(body).unwrap().contains("#cast-1"));
    // Traversal, encoded traversal, absolute paths, symlink escape: 404.
    std::os::unix::fs::symlink(
        rig.dir.path().join("secret.m3u8"),
        rig.dir.path().join("cast-1").join("escape.m3u8"),
    )
    .unwrap();
    for path in [
        format!("/cast/{ta}/../secret.m3u8"),
        format!("/cast/{ta}/%2e%2e%2fsecret.m3u8"),
        format!("/cast/{ta}/..%2Fsecret.m3u8"),
        format!("/cast/{ta}/%2Fetc%2Fpasswd.m3u8"),
        format!("/cast/{ta}/escape.m3u8"),
        format!("/cast/{ta}/cast-2/master.m3u8"),
        "/cast/x".to_owned(),
        "/cast/".to_owned(),
    ] {
        let (status, body) = fetch(&rig, &path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body, b"not found\n", "{path}");
    }
    // Fetch counters (no token) on the owner's GET.
    let id = a["id"].as_str().unwrap();
    let doc = body_json(
        send(
            &rig.router,
            support::get(&format!("/api/v1/cast/streams/{id}"), OPERATOR_TOKEN),
        )
        .await,
    )
    .await;
    assert!(doc["fetches"]["playlists"].as_u64().unwrap() >= 2);
    assert!(doc["fetches"]["segments"].as_u64().unwrap() >= 3);
}

fn report_req(token: &str, body: Vec<u8>) -> Request<Body> {
    public("POST", &format!("/cast/{token}/report"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "https://receiver.example")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn receiver_reports_size_rate_and_owner_view() {
    let rig = rig();
    let (_, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let token = token_of(&doc);
    let report =
        serde_json::json!({"v": 1, "state": "playing", "error": null, "stats": {"width": 640}});
    let r = send(
        &rig.router,
        report_req(&token, serde_json::to_vec(&report).unwrap()),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://receiver.example"
    );
    let r = send(&rig.router, report_req(&token, b"{}".to_vec())).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS, "1/s per token");
    rig.advance(Duration::from_secs(1));
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(2100));
    let r = send(&rig.router, report_req(&token, big.into_bytes())).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let r = send(&rig.router, report_req(&token, b"[1,2]".to_vec())).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = send(&rig.router, report_req(&"C".repeat(43), b"{}".to_vec())).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let preflight = send(
        &rig.router,
        public("OPTIONS", &format!("/cast/{token}/report"))
            .header(header::ORIGIN, "https://receiver.example")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
        "POST, OPTIONS"
    );
    let id = doc["id"].as_str().unwrap();
    let doc = body_json(
        send(
            &rig.router,
            support::get(&format!("/api/v1/cast/streams/{id}"), OPERATOR_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(doc["last_report"]["report"]["state"], "playing");
}

#[tokio::test]
async fn receiver_app_routes_headers_and_precedence() {
    let rig = rig();
    for (path, ty) in [
        ("/cast/receiver/", "text/html; charset=utf-8"),
        ("/cast/receiver/index.html", "text/html; charset=utf-8"),
        (
            "/cast/receiver/receiver.js",
            "text/javascript; charset=utf-8",
        ),
        ("/cast/receiver/receiver.css", "text/css; charset=utf-8"),
    ] {
        let r = send(
            &rig.router,
            public("GET", path)
                .header(header::USER_AGENT, "Mozilla/5.0 CrKey/1.56.500000")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            r.status(),
            StatusCode::OK,
            "{path}: precedence over /cast/{{token}}/{{file}}"
        );
        let h = r.headers();
        assert_eq!(h[header::CONTENT_TYPE], ty);
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(
            h[header::CONTENT_SECURITY_POLICY],
            multiview_control::routes::cast_public::RECEIVER_CSP
        );
        assert_eq!(
            h[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'; script-src 'self' https://www.gstatic.com https://ajax.googleapis.com; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' https: blob:; connect-src 'self' https: ws://localhost:8008; font-src 'self' https://fonts.gstatic.com; worker-src blob:"
        );
    }
    let r = send(
        &rig.router,
        public("GET", "/cast/receiver").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(r.headers()[header::LOCATION], "/cast/receiver/");
    let config = body_json(
        send(
            &rig.router,
            support::get("/api/v1/cast/config", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(config["receiver_last_fetch"]["cast_device"], true);
    // The embedded receiver is byte-identical to its source.
    let js = body_bytes(
        send(
            &rig.router,
            public("GET", "/cast/receiver/receiver.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert_eq!(js, include_bytes!("../src/cast_receiver/receiver.js"));
}

/// Captures every tracing event and span field as text.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap()
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn the_token_never_reaches_a_log_line_an_event_or_the_audit_log() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let rig = rig();
        let mut events = rig.state.engine.subscribe();
        let (_, doc) = create(
            &rig,
            OPERATOR_TOKEN,
            serde_json::json!({"feed": {"source": "a"}}),
        )
        .await;
        let token = token_of(&doc);
        for path in ["master.m3u8", "nope.m3u8", "seg-v-1-0.m4s"] {
            let _ = fetch(&rig, &format!("/cast/{token}/{path}")).await;
        }
        let _ = send(
            &rig.router,
            report_req(&token, b"{\"state\":\"error\"}".to_vec()),
        )
        .await;
        let id = doc["id"].as_str().unwrap();
        let _ = send(
            &rig.router,
            delete(&format!("/api/v1/cast/streams/{id}"), OPERATOR_TOKEN),
        )
        .await;
        rig.advance(Duration::from_secs(11));
        let _ = send(
            &rig.router,
            support::get("/api/v1/cast/streams", OPERATOR_TOKEN),
        )
        .await;
        let logs = capture.0.lock().unwrap().clone();
        assert!(!logs.contains(&token), "token in logs");
        let audit = serde_json::to_string(&rig.state.audit.list(None).unwrap()).unwrap();
        assert!(!audit.contains(&token), "token in audit");
        assert!(
            audit.contains("cast_stream"),
            "creation and deletion are audited"
        );
        while let Ok(event) = events.try_recv() {
            let text = format!("{:?}", event.event);
            assert!(!text.contains(&token), "token in an event");
        }
    });
}

#[tokio::test]
async fn the_hosted_receiver_origin_is_reflected_everywhere() {
    // The hosted receiver (another origin) plays any engine: its Origin is
    // reflected on the media route, the report POST and both preflights,
    // like any Origin.
    let rig = rig();
    let (_, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let token = token_of(&doc);
    let hosted = "https://cast.conspect.studio";
    for (method, path) in [
        ("OPTIONS", format!("/cast/{token}/report")),
        ("OPTIONS", format!("/cast/{token}/master.m3u8")),
        ("GET", format!("/cast/{token}/master.m3u8")),
        ("GET", format!("/cast/{token}/seg-v-1-0.m4s")),
        ("GET", format!("/cast/{}/master.m3u8", "D".repeat(43))),
    ] {
        let r = send(
            &rig.router,
            public(method, &path)
                .header(header::ORIGIN, hosted)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            hosted,
            "{method} {path}"
        );
        assert_eq!(r.headers()[header::VARY], "Origin");
    }
    let r = send(
        &rig.router,
        public("POST", &format!("/cast/{token}/report"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, hosted)
            .body(Body::from("{\"v\":1,\"state\":\"playing\"}"))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], hosted);
}

fn route_take(token: &str, key: &str, output: &str, feed: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/routing/output/take")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", key)
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "target": {"kind": "output", "output": output, "stream": "video"},
                "feed": feed,
            }))
            .unwrap(),
        ))
        .unwrap()
}

/// ABUSE-1: a principal scoped to source `a` mints a token while output-0
/// shows `a`; another operator takes output-0 to `secret-cam` through the
/// ordinary routing take. The token dies BEFORE the new feed is published:
/// its next playlist and segment requests are the generic 404, and its
/// owner's GET says why.
#[tokio::test]
async fn a_take_to_a_feed_the_owner_may_not_read_revokes_its_token() {
    let rig = rig();
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    let id = doc["id"].as_str().unwrap().to_owned();
    assert_eq!(rig.routing.bus("cast-1"), "src:a");
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    let takes_before = rig.routing.takes().len();
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-1-take",
            "output-0",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(rig.routing.takes().len(), takes_before + 1);
    assert_eq!(rig.routing.bus("cast-1"), "src:secret-cam");
    for file in ["master.m3u8", "seg-v-1-0.m4s", "init-v-1-0.mp4"] {
        let (status, body) = fetch(&rig, &format!("/cast/{token}/{file}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{file}");
        assert_eq!(body, b"not found\n", "{file}");
    }
    // The owner learns why; another principal still sees a plain 404.
    let own = send(
        &rig.router,
        support::get(&format!("/api/v1/cast/streams/{id}"), SCOPED),
    )
    .await;
    assert_eq!(own.status(), StatusCode::NOT_FOUND);
    let own = body_json(own).await;
    assert_eq!(own["type"], "/problems/cast-stream-revoked", "{own}");
    assert!(own["detail"].as_str().unwrap().contains("may not read"));
    let other = body_json(
        send(
            &rig.router,
            support::get(&format!("/api/v1/cast/streams/{id}"), OPERATOR_2),
        )
        .await,
    )
    .await;
    assert_eq!(other["type"], "/problems/not-found", "{other}");
    // Audited (without the token).
    let audit = serde_json::to_string(&rig.state.audit.list(None).unwrap()).unwrap();
    assert!(audit.contains(&id) && audit.contains("may not read"));
    assert!(!audit.contains(&token));
}

/// ABUSE-1, the positive case: an owner authorized for both feeds keeps its
/// URL across the take (ADR-0111), while a co-viewer of the same output who
/// is not authorized for the new feed loses its own.
#[tokio::test]
async fn a_take_keeps_tokens_whose_owner_may_read_the_new_feed() {
    let rig = rig();
    let (status, kept) = create(
        &rig,
        OPERATOR_2,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{kept}");
    let (status, lost) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{lost}");
    assert_eq!(lost["output"], "output-0");
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-1-keep",
            "output-0",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let kept_token = token_of(&kept);
    for file in ["master.m3u8", "seg-v-1-0.m4s"] {
        assert_eq!(
            fetch(&rig, &format!("/cast/{kept_token}/{file}")).await.0,
            StatusCode::OK,
            "{file}"
        );
    }
    let lost_token = token_of(&lost);
    assert_eq!(
        fetch(&rig, &format!("/cast/{lost_token}/master.m3u8"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // Taking it back to a feed both may read revokes nothing more.
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-1-back",
            "output-0",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(
        fetch(&rig, &format!("/cast/{kept_token}/master.m3u8"))
            .await
            .0,
        StatusCode::OK
    );
}

/// ABUSE-1: an owner whose key is revoked (it no longer resolves) loses its
/// token on the next feed change, even one to a feed anyone may read.
#[tokio::test]
async fn a_take_revokes_tokens_whose_owner_no_longer_resolves() {
    let rig = rig();
    let (status, doc) = create(
        &rig,
        FRESH,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    assert!(rig.state.api_keys.revoke("cs-fresh"));
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-1-owner",
            "output-0",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let token = token_of(&doc);
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::NOT_FOUND
    );
}

/// ABUSE-1 backstop: a feed change no take path saw (an engine fallback
/// carried on the outcome relay) re-checks the output's tokens too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_fallback_to_a_feed_the_owner_may_not_read_revokes_its_token() {
    let rig = rig();
    // cs-other may read output-1 and source `a`, but not Program.
    let (status, doc) = create(
        &rig,
        SCOPED_B,
        serde_json::json!({"output": "output-1", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    rig.routing
        .tx
        .send(OutputOutcome {
            operation_id: String::new(),
            output: "cast-2".to_owned(),
            tick: 9,
            outcome: "fallback:aux-failed".to_owned(),
            feed: Some(RoutedFeed::Program),
            audio: None,
        })
        .unwrap();
    for _ in 0..200 {
        if fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0 == StatusCode::NOT_FOUND {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the token survived a fallback to a feed its owner may not read");
}

/// ABUSE-2: every token-route response — playlist, segment, init, HEAD,
/// Range (206), 404 and report — is `Cache-Control: no-store`, so no cache
/// can answer for a token after it expired or was revoked. CORS unchanged.
#[tokio::test]
async fn token_routes_are_no_store_on_every_response() {
    let rig = rig();
    let (_, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let token = token_of(&doc);
    let req = |method: &str, file: &str, range: bool| {
        let mut r = public(method, &format!("/cast/{token}/{file}"))
            .header(header::ORIGIN, "https://cast.example");
        if range {
            r = r.header(header::RANGE, "bytes=0-99");
        }
        r.body(Body::empty()).unwrap()
    };
    for (method, file, range, status) in [
        ("GET", "master.m3u8", false, StatusCode::OK),
        ("HEAD", "master.m3u8", false, StatusCode::OK),
        ("GET", "seg-v-1-0.m4s", false, StatusCode::OK),
        ("HEAD", "seg-v-1-0.m4s", false, StatusCode::OK),
        ("GET", "seg-v-1-0.m4s", true, StatusCode::PARTIAL_CONTENT),
        ("GET", "init-v-1-0.mp4", false, StatusCode::OK),
        ("GET", "seg-v-9-9.m4s", false, StatusCode::NOT_FOUND),
        ("OPTIONS", "master.m3u8", false, StatusCode::NO_CONTENT),
    ] {
        let r = send(&rig.router, req(method, file, range)).await;
        assert_eq!(r.status(), status, "{method} {file} {range}");
        assert_eq!(
            r.headers()[header::CACHE_CONTROL],
            "no-store",
            "{method} {file} {range}"
        );
        assert_eq!(
            r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://cast.example"
        );
    }
    let r = send(&rig.router, report_req(&token, b"{}".to_vec())).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    let dead = "C".repeat(43);
    let r = send(
        &rig.router,
        public("GET", &format!("/cast/{dead}/seg-v-1-0.m4s"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
}

/// ABUSE-2: ordinary public `/hls/` mounts keep the ADR-0032 cache tiers.
#[tokio::test]
async fn public_hls_keeps_its_cache_policy() {
    use tower::ServiceExt;
    let tmp = tempfile::tempdir().unwrap();
    write_stream(tmp.path(), "hls");
    let router = multiview_output::hls::http::hls_router(tmp.path());
    let get = |path: &str| Request::builder().uri(path).body(Body::empty()).unwrap();
    let r = router.clone().oneshot(get("/master.m3u8")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers()[header::CACHE_CONTROL],
        "max-age=1, must-revalidate"
    );
    let r = router.oneshot(get("/seg-v-1-0.m4s")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
}

/// ABUSE-1: the cast API's own take seam (a new stream's take, the take
/// back to Program) re-checks the output's live tokens before it enqueues.
#[tokio::test]
async fn the_cast_api_take_seam_rechecks_live_tokens() {
    let rig = rig();
    let (status, doc) = create(
        &rig,
        SCOPED_B,
        serde_json::json!({"output": "output-1", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    multiview_control::cast_streams::lifecycle::submit_take(
        &rig.state,
        &multiview_control::cast_streams::lifecycle::cast_operation_id(),
        "cast-2",
        &RoutedFeed::Program,
        None,
        "cast",
    )
    .unwrap();
    assert_eq!(rig.routing.bus("cast-2"), "program");
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::NOT_FOUND
    );
}
