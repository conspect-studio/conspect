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
    PlanRefusal, RoutedAudio, RoutedFeed, RoutingLive, SubtitleTakeError, SubtitleTakeRequest,
    TakeSubmitError,
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
    /// Video segments an active output reports (default 9).
    segments: Mutex<HashMap<String, u64>>,
    /// The engine status is unavailable (no outputs reported).
    hidden: std::sync::atomic::AtomicBool,
    /// The outputs report `disabled` (#418 review r7).
    disabled: std::sync::atomic::AtomicBool,
}

impl FakeCast {
    /// Set the video segments an active `id` reports.
    fn set_segments(&self, id: &str, n: u64) {
        self.segments.lock().unwrap().insert(id.to_owned(), n);
    }
    /// Move active `id` to file generation `generation` (a new packager).
    fn set_generation(&self, id: &str, generation: u64) {
        self.state
            .lock()
            .unwrap()
            .insert(id.to_owned(), (true, generation));
    }
}

impl CastEngine for FakeCast {
    fn outputs(&self) -> Vec<CastOutputStatus> {
        if self.hidden.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let segments = self.segments.lock().unwrap().clone();
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
                    video_segments: if active {
                        segments.get(*id).copied().unwrap_or(9)
                    } else {
                        0
                    },
                    dropped_packets: 0,
                    last_error: error.clone(),
                    disabled: self.disabled.load(Ordering::SeqCst),
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
        // Idempotent while active: the running generation.
        let mut state = self.state.lock().unwrap();
        let entry = state.entry(id.to_owned()).or_insert((false, 0));
        if !entry.0 {
            *entry = (true, 1);
        }
        Ok(entry.1)
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
    /// The plan answers this refusal (a DSK defined refuses Clean, K2a).
    plan_refusal: Mutex<Option<PlanRefusal>>,
    tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
    /// The live subtitle route table per output (#418).
    subtitles: Mutex<HashMap<String, Vec<Option<String>>>>,
    /// Run inside a subtitle take, before the table is swapped.
    on_subtitle_take: SubtitleHook,
}

type SubtitleHook = Mutex<Option<Box<dyn Fn() + Send + Sync>>>;

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
            plan_refusal: Mutex::new(None),
            tx,
            rx: Mutex::new(Some(rx)),
            subtitles: Mutex::new(HashMap::new()),
            on_subtitle_take: Mutex::new(None),
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
        if let Some(refusal) = *self.plan_refusal.lock().unwrap() {
            return Err(refusal);
        }
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
    fn try_take_subtitles(&self, take: SubtitleTakeRequest) -> Result<u64, SubtitleTakeError> {
        if let Some(hook) = self.on_subtitle_take.lock().unwrap().as_ref() {
            hook();
        }
        let mut tables = self.subtitles.lock().unwrap();
        let Some(table) = tables.get_mut(&take.output) else {
            return Err(SubtitleTakeError::NotInRun);
        };
        for (slot, source) in take.slots {
            table[slot] = source;
        }
        Ok(1)
    }
    fn subtitle_routes(&self, output: &str) -> Option<Vec<Option<String>>> {
        self.subtitles.lock().unwrap().get(output).cloned()
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
    rig_with(|state| state)
}

/// [`rig`] with the `AppState` adjusted by `adjust` (e.g. JWT enabled).
fn rig_with(adjust: impl FnOnce(AppState) -> AppState) -> Rig {
    rig_doc(DOC, adjust)
}

/// [`rig_with`] over the configuration `doc`.
fn rig_doc(doc: &str, adjust: impl FnOnce(AppState) -> AppState) -> Rig {
    rig_mode(doc, Mode::Modeled, adjust)
}

/// How the rig's control plane is bound to the run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// A config-file run: the start document and a boot model.
    Modeled,
    /// A store-only run (no boot model) given its start document.
    StoreOnly,
    /// A store-only run with no start document at all (no run view).
    Bare,
}

/// [`rig_doc`] in `mode`.
fn rig_mode(doc: &str, mode: Mode, adjust: impl FnOnce(AppState) -> AppState) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let mut dirs = HashMap::new();
    for id in ["cast-1", "cast-2"] {
        let dir = tmp.path().join(id);
        write_stream(&dir, id);
        dirs.insert(id.to_owned(), dir);
    }
    std::fs::write(tmp.path().join("secret.m3u8"), "#EXTM3U\n#secret\n").unwrap();
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
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
        segments: Mutex::new(HashMap::new()),
        hidden: std::sync::atomic::AtomicBool::new(false),
        disabled: std::sync::atomic::AtomicBool::new(false),
    });
    let routing = FakeRouting::new();
    let now = Arc::new(AtomicU64::new(1_000_000));
    let clock_now = Arc::clone(&now);
    let streams = Arc::new(CastStreams::with_clock(Arc::new(move || {
        clock_now.load(Ordering::SeqCst)
    })));
    let mut state = AppState::new(
        Arc::clone(&engine),
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys()),
    )
    .with_seeded_resources(seeded);
    if mode != Mode::Bare {
        state = state.with_base_document(serde_json::to_value(&config).unwrap());
    }
    if mode == Mode::Modeled {
        state = state.with_boot_model(Arc::new(BootModel::new(
            tmp.path().join("multiview.toml"),
            config.clone(),
            config,
            StartMode::Boot,
            false,
            None,
        )));
    }
    let state = state
        .with_output_routing({
            let seam: Arc<dyn OutputRouting> = routing.clone();
            seam
        })
        .with_cast_engine(cast.clone())
        .with_cast_streams(streams)
        .with_cast_app_id(Some("1A2B3C4D".to_owned()));
    let state = adjust(state);
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

/// ADR-0113 K2a: a cast of Clean while a downstream keyer is defined is
/// refused exactly as a take is (409 `clean-unavailable-with-dsk`), and no
/// stream or take is left behind.
#[tokio::test]
async fn a_cast_of_clean_with_a_dsk_defined_is_refused() {
    let rig = rig();
    *rig.routing.plan_refusal.lock().unwrap() = Some(PlanRefusal::CleanUnavailableWithDsk);
    let (status, doc) = create(
        &rig,
        OPERATOR_TOKEN,
        serde_json::json!({"feed": {"bus": "clean"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{doc}");
    assert_eq!(doc["type"], "/problems/clean-unavailable-with-dsk");
    assert!(rig.routing.takes().is_empty(), "no take was submitted");
    let list = body_json(
        send(
            &rig.router,
            support::get("/api/v1/cast/streams", OPERATOR_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 0, "no stream was created");
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

/// #418: a `WebVTT` subtitle segment of the stream is served as `text/vtt`
/// under the same token checks and counted as a segment fetch; another
/// subtitle extension is not served.
#[tokio::test]
async fn webvtt_segments_are_served_as_text_vtt_and_counted_as_segments() {
    let rig = rig();
    let (_, a) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    let ta = token_of(&a);
    let dir = rig.dir.path().join("cast-1");
    std::fs::write(dir.join("sub0-1-0.vtt"), "WEBVTT\n\n").unwrap();
    std::fs::write(dir.join("sub0-1-0.srt"), "1\n").unwrap();
    let r = send(
        &rig.router,
        public("GET", &format!("/cast/{ta}/sub0-1-0.vtt"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/vtt");
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(body_bytes(r).await, b"WEBVTT\n\n");
    let (status, body) = fetch(&rig, &format!("/cast/{ta}/sub0-1-0.srt")).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::NOT_FOUND, &b"not found\n"[..])
    );
    let (status, _) = fetch(&rig, &format!("/cast/{}/sub0-1-0.vtt", "B".repeat(43))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let id = a["id"].as_str().unwrap();
    let doc = body_json(
        send(
            &rig.router,
            support::get(&format!("/api/v1/cast/streams/{id}"), OPERATOR_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(doc["fetches"]["segments"], 1, "{doc}");
    assert_eq!(doc["fetches"]["playlists"], 0, "{doc}");
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

/// Every response a token gets for the playlist, a segment and the init.
async fn statuses(rig: &Rig, token: &str) -> Vec<StatusCode> {
    let mut out = Vec::new();
    for file in ["master.m3u8", "seg-v-1-0.m4s", "init-v-1-0.mp4"] {
        out.push(fetch(rig, &format!("/cast/{token}/{file}")).await.0);
    }
    out
}

/// ABUSE-1 round 2, create racing a take: the creation authorized its owner
/// for what output-0 carried and claimed its token; a take to a feed the
/// owner may not read lands AFTER that snapshot and BEFORE the token is
/// armed. The token is never servable (not during the take, not after) and
/// the creation is refused: no token is ever minted against a stale feed.
#[tokio::test]
async fn a_create_racing_a_take_never_serves_the_new_feed() {
    let rig = rig();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (state, observed) = (rig.state.clone(), Arc::clone(&seen));
    rig.state
        .cast_streams
        .set_before_arm(Some(Arc::new(move || {
            let streams = &state.cast_streams;
            observed.lock().unwrap().push(streams.servable_on("cast-1"));
            multiview_control::cast_streams::lifecycle::submit_take(
                &state,
                &multiview_control::cast_streams::lifecycle::cast_operation_id(),
                "cast-1",
                &RoutedFeed::Source("secret-cam".to_owned()),
                None,
                "cs-op2",
            )
            .unwrap();
            observed.lock().unwrap().push(streams.servable_on("cast-1"));
        })));
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(rig.routing.bus("cast-1"), "src:secret-cam");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![0, 0],
        "servable during the take"
    );
    assert_eq!(status, StatusCode::FORBIDDEN, "{doc}");
    assert!(doc.get("path").is_none(), "{doc}");
    assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
    // Nothing of this owner is left to serve after the take either.
    let list =
        body_json(send(&rig.router, support::get("/api/v1/cast/streams", SCOPED)).await).await;
    assert_eq!(list, serde_json::json!([]), "{list}");
}

/// ABUSE-1 round 2, the benign race: a take to a feed the owner MAY read
/// lands between snapshot and arming; the creation re-checks at the new
/// generation and succeeds, and its token plays.
#[tokio::test]
async fn a_create_racing_a_readable_take_rechecks_and_plays() {
    let rig = rig();
    let state = rig.state.clone();
    rig.state
        .cast_streams
        .set_before_arm(Some(Arc::new(move || {
            multiview_control::cast_streams::lifecycle::submit_take(
                &state,
                &multiview_control::cast_streams::lifecycle::cast_operation_id(),
                "cast-1",
                &RoutedFeed::Program,
                None,
                "cs-op2",
            )
            .unwrap();
        })));
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    assert_eq!(statuses(&rig, &token).await, vec![StatusCode::OK; 3]);
}

/// ABUSE-1 round 2, fallback before relay: the engine moves output-1 to
/// Program (a fallback) and its outcome has NOT reached the control plane.
/// The first media request already sees the new generation: the owner who
/// may not read Program gets the generic 404 on every file, from the first
/// request on, while a co-viewer who may read it keeps playing (its token
/// was re-authorized synchronously on the media path). The late outcome
/// changes neither.
#[tokio::test]
async fn a_fallback_before_its_relay_is_never_served_to_an_unauthorized_owner() {
    let rig = rig();
    let (status, lost) = create(
        &rig,
        SCOPED_B,
        serde_json::json!({"output": "output-1", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{lost}");
    let (status, kept) = create(&rig, OPERATOR_2, serde_json::json!({"output": "output-1"})).await;
    assert_eq!(status, StatusCode::CREATED, "{kept}");
    let (lost, kept) = (token_of(&lost), token_of(&kept));
    assert_eq!(statuses(&rig, &lost).await, vec![StatusCode::OK; 3]);
    let before = rig.state.cast_streams.generation("cast-2").unwrap();
    // The engine falls back; only its readback says so.
    rig.routing
        .live
        .lock()
        .unwrap()
        .outputs
        .get_mut("cast-2")
        .unwrap()
        .bus = "program".to_owned();
    assert_eq!(statuses(&rig, &lost).await, vec![StatusCode::NOT_FOUND; 3]);
    assert!(rig.state.cast_streams.generation("cast-2").unwrap() > before);
    assert_eq!(statuses(&rig, &kept).await, vec![StatusCode::OK; 3]);
    // The outcome arrives late: nothing changes.
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
    for _ in 0..20 {
        tokio::task::yield_now().await;
        assert_eq!(statuses(&rig, &lost).await, vec![StatusCode::NOT_FOUND; 3]);
        assert_eq!(statuses(&rig, &kept).await, vec![StatusCode::OK; 3]);
    }
}

/// ABUSE-1 round 2, the positive case: an owner authorized for every feed
/// keeps playing across takes (each moves the generation; the re-check moves
/// its token forward in the same critical section), every response 200,
/// with the outcomes relayed or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_authorized_owner_keeps_playing_across_takes() {
    let rig = rig();
    let (status, doc) = create(
        &rig,
        OPERATOR_2,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    let mut last = rig.state.cast_streams.generation("cast-1").unwrap();
    // One routing take (the per-output take limiter admits one), then the
    // cast API's own take seam: both commit through the same gate.
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "keep-0",
            "output-0",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    for feed in [
        None,
        Some(RoutedFeed::Program),
        Some(RoutedFeed::Source("a".to_owned())),
    ] {
        if let Some(feed) = feed {
            multiview_control::cast_streams::lifecycle::submit_take(
                &rig.state,
                &multiview_control::cast_streams::lifecycle::cast_operation_id(),
                "cast-1",
                &feed,
                None,
                "cs-op2",
            )
            .unwrap();
        }
        let now = rig.state.cast_streams.generation("cast-1").unwrap();
        assert!(now > last, "the take moved the generation");
        last = now;
        for _ in 0..5 {
            assert_eq!(statuses(&rig, &token).await, vec![StatusCode::OK; 3]);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

const JWT_SECRET: &[u8] = b"cast-jwt-secret-shared-with-as";
const JWT_ISS: &str = "https://auth.cast.example";
const JWT_AUD: &str = "multiview";

fn jwt_rig() -> Rig {
    rig_with(|state| {
        state
            .with_jwt(
                Arc::new(multiview_control::JwtValidator::new_hs256(
                    JWT_SECRET.to_vec(),
                    JWT_ISS,
                    JWT_AUD,
                )),
                JWT_AUD,
            )
            .with_ack_clock(Arc::new(|| {
                multiview_core::time::MediaTime::from_nanos(1_800_000_000_000_000_000)
            }))
    })
}

fn jwt_for(sub: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use hmac::{KeyInit, Mac};
    let b64 = |v: &serde_json::Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
    let input = format!(
        "{}.{}",
        b64(&serde_json::json!({"alg": "HS256", "typ": "JWT"})),
        b64(&serde_json::json!({
            "iss": JWT_ISS, "sub": sub, "aud": [JWT_AUD],
            "exp": 1_900_000_000i64, "iat": 1_700_000_000i64,
            "x-nmos-api": {"version": "1.0", "access": {"multiview": "write"}}
        })),
    );
    let mut mac = <hmac::Hmac<sha2::Sha256> as KeyInit>::new_from_slice(JWT_SECRET).unwrap();
    mac.update(input.as_bytes());
    format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

/// ABUSE-3: with JWT validation configured, an unscoped NATIVE API key that
/// created a stream and is then revoked does not come back as a "JWT
/// owner": its token dies on the next take and on the periodic re-check.
#[tokio::test]
async fn a_revoked_native_key_never_resolves_as_a_jwt_owner() {
    let rig = jwt_rig();
    let (status, doc) = create(&rig, FRESH, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let (status, other) = create(&rig, OPERATOR_2, serde_json::json!({"output": "output-1"})).await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    assert_eq!(
        statuses(&rig, &token_of(&other)).await,
        vec![StatusCode::OK; 3]
    );
    assert!(rig.state.api_keys.revoke("cs-fresh"));
    assert!(rig.state.api_keys.revoke("cs-op2"));
    // A take on output-0 revokes the first …
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-3-take",
            "output-0",
            serde_json::json!({"bus": "program"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(
        statuses(&rig, &token_of(&doc)).await,
        vec![StatusCode::NOT_FOUND; 3]
    );
    // … and the periodic re-check (no take) the second.
    let other = token_of(&other);
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    assert_eq!(statuses(&rig, &other).await, vec![StatusCode::NOT_FOUND; 3]);
}

/// ABUSE-3, the positive case: a genuine JWT-authenticated owner (not in the
/// key store) keeps its token across a take and the periodic re-check.
#[tokio::test]
async fn a_genuine_jwt_owner_keeps_its_token() {
    let rig = jwt_rig();
    let jwt = jwt_for("jwt-caster");
    let (status, doc) = create(&rig, &jwt, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-3-jwt",
            "output-0",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(statuses(&rig, &token).await, vec![StatusCode::OK; 3]);
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    assert_eq!(statuses(&rig, &token).await, vec![StatusCode::OK; 3]);
}

/// A `POST /api/v1/cast/streams` whose JSON body is held back until
/// `release` fires; `polled` fires when axum first reads it (every header
/// extractor, the `Principal` included, has run by then).
fn held_create(
    token: &str,
    body: serde_json::Value,
    polled: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> Request<Body> {
    let bytes = serde_json::to_vec(&body).unwrap();
    let stream = async_stream::stream! {
        let _ = polled.send(());
        let _ = release.await;
        yield Ok::<_, std::io::Error>(bytes);
    };
    Request::builder()
        .method("POST")
        .uri("/api/v1/cast/streams")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// ABUSE-3 round 3: provenance comes from the authentication that
/// succeeded, never from a later re-verification. A native key passes
/// `Principal` extraction, is revoked while its request body is still in
/// flight, and the handler then runs with JWT configured: the stream is
/// never recorded as a JWT owner, so it fails closed (refused, or revoked
/// on the next re-check) instead of living on with the revoked key's grants.
#[tokio::test]
async fn a_key_revoked_mid_request_is_never_recorded_as_a_jwt_owner() {
    let rig = jwt_rig();
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let request = held_create(
        FRESH,
        serde_json::json!({"output": "output-0"}),
        polled_tx,
        release_rx,
    );
    let router = rig.router.clone();
    let call = tokio::spawn(async move { send(&router, request).await });
    polled_rx.await.unwrap();
    assert!(rig.state.api_keys.revoke("cs-fresh"));
    release_tx.send(()).unwrap();
    let response = call.await.unwrap();
    let status = response.status();
    let doc: serde_json::Value =
        serde_json::from_slice(&body_bytes(response).await).unwrap_or_default();
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    if status == StatusCode::CREATED {
        assert_eq!(
            statuses(&rig, &token_of(&doc)).await,
            vec![StatusCode::NOT_FOUND; 3],
            "a revoked native key resurrected as a JWT owner"
        );
    }
    assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0, "{doc}");
}

/// The request seam of ABUSE-1 round 3: pause the next `/cast/<token>/…`
/// media request after its authorization and before the file is opened.
/// Returns (paused, resume).
fn pause_media(
    rig: &Rig,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    rig.state
        .cast_streams
        .set_before_open(Some((paused_tx, resume_rx)));
    (paused_rx, resume_tx)
}

/// Overwrite output `id`'s files the way the packager does (a new file
/// renamed over the name) with bytes tagged `tag`.
fn republish(rig: &Rig, id: &str, tag: &str) {
    let dir = rig.dir.path().join(id);
    for (name, bytes) in [
        ("master.m3u8", format!("#EXTM3U\n#{tag}\n").into_bytes()),
        ("seg-v-1-0.m4s", tag.as_bytes().repeat(100)),
        ("init-v-1-0.mp4", tag.as_bytes().to_vec()),
    ] {
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, bytes).unwrap();
        std::fs::rename(&tmp, dir.join(name)).unwrap();
    }
}

/// ABUSE-1 round 3, request in flight: a media request passed its
/// authorization and is paused before it opens the file; a take to a feed
/// its owner may not read revokes the owner and the output publishes the
/// restricted media. Resumed, the request answers the generic 404 and never
/// a byte of the restricted feed, for playlists, segments and init.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_in_flight_across_a_revoking_take_never_serves_its_bytes() {
    for file in ["master.m3u8", "seg-v-1-0.m4s", "init-v-1-0.mp4"] {
        let rig = rig();
        let (status, doc) = create(
            &rig,
            SCOPED,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{doc}");
        let token = token_of(&doc);
        let (paused, resume) = pause_media(&rig);
        let router = rig.router.clone();
        let path = format!("/cast/{token}/{file}");
        let call = tokio::spawn(async move {
            let response = send(&router, public("GET", &path).body(Body::empty()).unwrap()).await;
            (response.status(), body_bytes(response).await)
        });
        paused.await.unwrap();
        let r = send(
            &rig.router,
            route_take(
                OPERATOR_TOKEN,
                "abuse-1-in-flight",
                "output-0",
                serde_json::json!({"source": "secret-cam"}),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        republish(&rig, "cast-1", "RESTRICTED");
        resume.send(()).unwrap();
        let (status, body) = call.await.unwrap();
        assert_eq!(status, StatusCode::NOT_FOUND, "{file}");
        assert_eq!(body, b"not found\n", "{file}");
    }
}

/// ABUSE-1 round 3, the positive case: an owner authorized for the new
/// feed, paused the same way across the same take, is served (the new
/// media, which it may read).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_authorized_request_in_flight_across_a_take_is_served() {
    for file in ["master.m3u8", "seg-v-1-0.m4s", "init-v-1-0.mp4"] {
        let rig = rig();
        let (status, doc) = create(
            &rig,
            OPERATOR_2,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{doc}");
        let token = token_of(&doc);
        let (paused, resume) = pause_media(&rig);
        let router = rig.router.clone();
        let path = format!("/cast/{token}/{file}");
        let call = tokio::spawn(async move {
            let response = send(&router, public("GET", &path).body(Body::empty()).unwrap()).await;
            (response.status(), body_bytes(response).await)
        });
        paused.await.unwrap();
        let r = send(
            &rig.router,
            route_take(
                OPERATOR_TOKEN,
                "abuse-1-in-flight-ok",
                "output-0",
                serde_json::json!({"source": "secret-cam"}),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        republish(&rig, "cast-1", "NEWFEED");
        resume.send(()).unwrap();
        let (status, body) = call.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{file}");
        assert!(
            String::from_utf8_lossy(&body).contains("NEWFEED"),
            "{file}: {body:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// #418 review ABUSE-1 / ABUSE-2: a cast stream carries its output's subtitle
// slots and discrete audio tracks, so its token needs Read on them too.
// ---------------------------------------------------------------------------

/// [`DOC`] with cast-1 (output-0) carrying `slots` (TOML appended to its
/// table) and `extra` appended to the document.
fn doc_with(slots: &str, extra: &str) -> String {
    let anchor = "id = \"cast-1\"\ncodec = \"h264\"\n";
    assert!(DOC.contains(anchor));
    format!(
        "{}{extra}",
        DOC.replacen(anchor, &format!("{anchor}{slots}"), 1)
    )
}

/// cast-1 with one subtitle slot patched to `source`.
fn subtitle_doc(source: &str) -> String {
    doc_with(
        &format!(
            "[[outputs.slots.subtitle]]\nformat = \"webvtt\"\nlanguage = \"en\"\nsource = \"{source}\"\n"
        ),
        "",
    )
}

/// A discrete track `secret-trk` routed from `secret-cam`.
const SECRET_TRACK: &str = r#"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "secret-cam"
channels = { kind = "stereo" }
target_track = "secret-trk"
"#;

fn subtitle_take(token: &str, key: &str, source: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/patch/take")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", key)
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"crosspoints": [{
                "plane": "subtitle",
                "destination": {"output": "output-0", "slot": 0},
                "source": source,
            }]}))
            .unwrap(),
        ))
        .unwrap()
}

/// ABUSE-1: an owner who may not read the subtitle slot's source gets no
/// token — from the accepted config, and from the engine's live route table
/// when it differs (the readback wins).
#[tokio::test]
async fn create_is_refused_when_the_owner_may_not_read_a_subtitle_source() {
    let rig = rig_doc(&subtitle_doc("secret-cam"), |s| s);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{doc}");
    assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
    // An owner who may read it is served.
    let (status, doc) = create(
        &rig,
        OPERATOR_2,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");

    // Config says `a` (readable), the run's live table says `secret-cam`.
    let rig = rig_doc(&subtitle_doc("a"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("secret-cam".to_owned())]);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{doc}");
}

/// ABUSE-1: a subtitle re-patch through `/patch/take` to a source the
/// owner may not read revokes the token BEFORE the engine swaps the route
/// table (no cue of it can be published to it); the owner's GET says why.
#[tokio::test]
async fn a_subtitle_repatch_to_an_unreadable_source_revokes_before_the_swap() {
    let rig = rig_doc(&subtitle_doc("a"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("a".to_owned())]);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    let id = doc["id"].as_str().unwrap().to_owned();
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (state, observed) = (rig.state.clone(), Arc::clone(&seen));
    *rig.routing.on_subtitle_take.lock().unwrap() = Some(Box::new(move || {
        observed
            .lock()
            .unwrap()
            .push(state.cast_streams.servable_on("cast-1"));
    }));
    let r = send(
        &rig.router,
        subtitle_take(
            OPERATOR_TOKEN,
            "abuse-1-subs",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(*seen.lock().unwrap(), vec![0], "servable at the swap");
    assert_eq!(
        rig.routing.subtitles.lock().unwrap()["cast-1"],
        vec![Some("secret-cam".to_owned())]
    );
    for file in ["master.m3u8", "seg-v-1-0.m4s"] {
        let (status, body) = fetch(&rig, &format!("/cast/{token}/{file}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{file}");
        assert_eq!(body, b"not found\n", "{file}");
    }
    let own = body_json(
        send(
            &rig.router,
            support::get(&format!("/api/v1/cast/streams/{id}"), SCOPED),
        )
        .await,
    )
    .await;
    assert_eq!(own["type"], "/problems/cast-stream-revoked", "{own}");
}

/// ABUSE-1, the positive case: an owner who may read the new subtitle
/// source keeps playing across the re-patch, while a co-viewer who may not
/// loses its token.
#[tokio::test]
async fn a_subtitle_repatch_keeps_tokens_whose_owner_may_read_the_new_source() {
    let rig = rig_doc(&subtitle_doc("a"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("a".to_owned())]);
    let (status, doc) = create(
        &rig,
        OPERATOR_2,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let keeps = token_of(&doc);
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let loses = token_of(&doc);
    let generation = rig.state.cast_streams.generation("cast-1").unwrap();
    let r = send(
        &rig.router,
        subtitle_take(
            OPERATOR_TOKEN,
            "abuse-1-subs-ok",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert!(rig.state.cast_streams.generation("cast-1").unwrap() > generation);
    assert_eq!(
        fetch(&rig, &format!("/cast/{keeps}/master.m3u8")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        fetch(&rig, &format!("/cast/{loses}/master.m3u8")).await.0,
        StatusCode::NOT_FOUND
    );
    // Unpatching (`none`) needs nothing: the remaining token keeps playing.
    let r = send(
        &rig.router,
        subtitle_take(
            OPERATOR_TOKEN,
            "abuse-1-subs-none",
            serde_json::json!("none"),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(
        fetch(&rig, &format!("/cast/{keeps}/master.m3u8")).await.0,
        StatusCode::OK
    );
}

/// ABUSE-1: a subtitle route the engine swapped without a commit (seen
/// only on its readback) is re-authorized by the media barrier before
/// anything is served, even though the picture did not change.
#[tokio::test]
async fn the_media_barrier_rechecks_a_subtitle_route_seen_on_the_readback() {
    let rig = rig_doc(&subtitle_doc("a"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("a".to_owned())]);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("secret-cam".to_owned())]);
    let (status, body) = fetch(&rig, &format!("/cast/{token}/sub0-1-0.vtt")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, b"not found\n");
    assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
}

/// ABUSE-1: a media request in flight (authorized, paused before opening
/// its file) across a revoking subtitle re-patch never serves its bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_in_flight_across_a_revoking_subtitle_repatch_is_not_served() {
    let rig = rig_doc(&subtitle_doc("a"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("a".to_owned())]);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    let (paused, resume) = pause_media(&rig);
    let router = rig.router.clone();
    let path = format!("/cast/{token}/master.m3u8");
    let call = tokio::spawn(async move {
        let response = send(&router, public("GET", &path).body(Body::empty()).unwrap()).await;
        (response.status(), body_bytes(response).await)
    });
    paused.await.unwrap();
    let r = send(
        &rig.router,
        subtitle_take(
            OPERATOR_TOKEN,
            "abuse-1-subs-flight",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    resume.send(()).unwrap();
    let (status, body) = call.await.unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, b"not found\n");
}

/// ABUSE-2: a discrete track the stream packages (an explicit audio slot,
/// or `auto`) needs Read on the track and its route input; the routed audio
/// (`follow`) is not the whole rendition set.
#[tokio::test]
async fn create_is_refused_when_the_owner_may_not_read_a_carried_audio_track() {
    let slots = "[[outputs.slots.audio]]\n[[outputs.slots.audio]]\ntrack = \"secret-trk\"\n";
    let auto = "audio = { mode = \"auto\" }\n";
    for carried in [slots, auto] {
        let rig = rig_doc(&doc_with(carried, SECRET_TRACK), |s| s);
        let (status, doc) = create(
            &rig,
            SCOPED,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{carried}: {doc}");
        assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
        let (status, doc) = create(
            &rig,
            OPERATOR_2,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{carried}: {doc}");
        let token = token_of(&doc);
        assert_eq!(
            fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
            StatusCode::OK
        );
    }
    // A program-only cast output declaring no track selection is unaffected.
    let rig = rig_doc(&doc_with("", SECRET_TRACK), |s| s);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
}

/// ABUSE-2: a video take on an output carrying a private track keeps
/// requiring it (the closure is complete on every re-check).
#[tokio::test]
async fn a_take_rechecks_the_carried_audio_tracks() {
    let slots = "[[outputs.slots.audio]]\n[[outputs.slots.audio]]\ntrack = \"secret-trk\"\n";
    let rig = rig_doc(&doc_with(slots, SECRET_TRACK), |s| s);
    let (status, doc) = create(
        &rig,
        OPERATOR_2,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    let r = send(
        &rig.router,
        route_take(
            OPERATOR_TOKEN,
            "abuse-2-take",
            "output-0",
            serde_json::json!({"source": "secret-cam"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
}

// ---------------------------------------------------------------------------
// #418 review ABUSE-1-R2 / ABUSE-2-R2: an output PUT that changes a cast
// output's subtitle or audio slots is restart-pending, while the running
// sink keeps packaging the run's renditions. The token closure covers both.
// ---------------------------------------------------------------------------

/// PUT output-0 (cast-1) with its body edited by `edit` (restart-pending:
/// the run keeps what it started with).
async fn put_cast_1(rig: &Rig, edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) {
    let r = send(
        &rig.router,
        support::get("/api/v1/outputs/output-0", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let etag = r.headers()[header::ETAG].to_str().unwrap().to_owned();
    let current = body_json(r).await;
    let mut body = current["body"].as_object().unwrap().clone();
    edit(&mut body);
    let r = send(
        &rig.router,
        support::put_json(
            "/api/v1/outputs/output-0",
            ADMIN_TOKEN,
            Some(&etag),
            &serde_json::json!({"name": current["name"], "body": body}),
        ),
    )
    .await;
    let status = r.status();
    assert_eq!(status, StatusCode::OK, "{}", body_json(r).await);
}

/// Drop the output's slots (subtitle and audio) and its audio selection.
fn drop_slots(body: &mut serde_json::Map<String, serde_json::Value>) {
    body.remove("slots");
    body.remove("audio");
}

/// ABUSE-1-R2: a restart-pending PUT removing a private subtitle slot does
/// not hide it — whether the run reports its live table or not.
#[tokio::test]
async fn a_restart_pending_subtitle_slot_removal_still_needs_its_source() {
    for live in [false, true] {
        let rig = rig_doc(&subtitle_doc("secret-cam"), |s| s);
        if live {
            rig.routing
                .subtitles
                .lock()
                .unwrap()
                .insert("cast-1".to_owned(), vec![Some("secret-cam".to_owned())]);
        }
        put_cast_1(&rig, drop_slots).await;
        let (status, doc) = create(
            &rig,
            SCOPED,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "live={live}: {doc}");
        assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
        let (status, doc) = create(
            &rig,
            OPERATOR_2,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "live={live}: {doc}");
    }
}

/// ABUSE-1-R2: the media barrier re-authorizes against the COMPLETE live
/// table even when the accepted output no longer declares the slot: a live
/// swap to a private source after a restart-pending removal revokes.
#[tokio::test]
async fn the_media_barrier_revokes_on_a_live_slot_the_accepted_config_removed() {
    let rig = rig_doc(&subtitle_doc("a"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("a".to_owned())]);
    let (status, doc) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    put_cast_1(&rig, drop_slots).await;
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("secret-cam".to_owned())]);
    let (status, body) = fetch(&rig, &format!("/cast/{token}/sub0-1-0.vtt")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, b"not found\n");
    assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
}

/// ABUSE-2-R2: a restart-pending PUT making the output program-only does
/// not hide the private discrete track the running sink still packages —
/// explicit `slots.audio` and `auto` selection alike.
#[tokio::test]
async fn a_restart_pending_audio_change_still_needs_the_running_tracks() {
    let slots = "[[outputs.slots.audio]]\n[[outputs.slots.audio]]\ntrack = \"secret-trk\"\n";
    let auto = "audio = { mode = \"auto\" }\n";
    for carried in [slots, auto] {
        let rig = rig_doc(&doc_with(carried, SECRET_TRACK), |s| s);
        put_cast_1(&rig, drop_slots).await;
        let (status, doc) = create(
            &rig,
            SCOPED,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{carried}: {doc}");
        assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
        let (status, doc) = create(
            &rig,
            OPERATOR_2,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{carried}: {doc}");
    }
}

/// ABUSE-2-R2 / ABUSE-1-R2: an existing token whose owner loses Read on a
/// private track or subtitle source the RUN still publishes is revoked by
/// the re-check (and the media barrier then refuses it), although the
/// accepted config no longer declares the dependency.
#[tokio::test]
async fn a_recheck_revokes_on_running_dependencies_the_accepted_config_dropped() {
    let slots = "[[outputs.slots.audio]]\n[[outputs.slots.audio]]\ntrack = \"secret-trk\"\n";
    let docs = [
        doc_with(slots, SECRET_TRACK),
        doc_with("audio = { mode = \"auto\" }\n", SECRET_TRACK),
        subtitle_doc("secret-cam"),
    ];
    for doc in &docs {
        let rig = rig_doc(doc, |s| s);
        let (status, created) = create(
            &rig,
            FRESH,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let token = token_of(&created);
        put_cast_1(&rig, drop_slots).await;
        assert!(rig.state.api_keys.set_principal(
            "cs-fresh",
            Principal {
                key_id: "cs-fresh".to_owned(),
                role: Role::Operator,
                scoped_object_ids: Some(vec!["output-0".to_owned(), "a".to_owned()]),
                scoped_output_ids: Some(vec!["output-0".to_owned(), "program:main".to_owned()]),
                scoped_discovery_domains: None,
            },
        ));
        multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
        let (status, body) = fetch(&rig, &format!("/cast/{token}/master.m3u8")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{doc}");
        assert_eq!(body, b"not found\n");
    }
}

// ---------------------------------------------------------------------------
// #418 review ABUSE-STORE-R3: a store-only run (no boot model) keeps a
// run-bound view of what its cast outputs package — the start document
// snapshotted when the cast engine attaches — so a restart-pending output
// PUT cannot hide a running dependency; without any run view the closure
// fails closed.
// ---------------------------------------------------------------------------

/// The carried-dependency documents: explicit `slots.audio` track, `auto`
/// selection, a private subtitle slot.
fn store_only_docs() -> [String; 3] {
    let slots = "[[outputs.slots.audio]]\n[[outputs.slots.audio]]\ntrack = \"secret-trk\"\n";
    [
        doc_with(slots, SECRET_TRACK),
        doc_with("audio = { mode = \"auto\" }\n", SECRET_TRACK),
        subtitle_doc("secret-cam"),
    ]
}

/// ABUSE-STORE-R3: in a store-only run, a restart-pending PUT dropping a
/// private explicit track, the `auto` selection or a subtitle slot before
/// the first token does not let a principal without Read on it create one
/// (no live subtitle table: the run's slot sources stand in, never empty).
#[tokio::test]
async fn a_store_only_restart_pending_removal_still_needs_the_running_dependencies() {
    for doc in &store_only_docs() {
        let rig = rig_mode(doc, Mode::StoreOnly, |s| s);
        assert!(rig.state.boot_model.is_none());
        assert!(rig.state.cast_run.is_some());
        put_cast_1(&rig, drop_slots).await;
        let (status, created) = create(
            &rig,
            SCOPED,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{doc}: {created}");
        assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
        let (status, created) = create(
            &rig,
            OPERATOR_2,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{doc}: {created}");
    }
}

/// ABUSE-STORE-R3: in a store-only run, an existing token whose owner loses
/// Read on a dependency the RUN still packages is revoked by the re-check
/// and then refused at the media barrier, although a restart-pending PUT
/// dropped it from the accepted stores.
#[tokio::test]
async fn a_store_only_recheck_revokes_on_running_dependencies_the_put_dropped() {
    for doc in &store_only_docs() {
        let rig = rig_mode(doc, Mode::StoreOnly, |s| s);
        let (status, created) = create(
            &rig,
            FRESH,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let token = token_of(&created);
        put_cast_1(&rig, drop_slots).await;
        assert_eq!(
            fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
            StatusCode::OK,
            "{doc}"
        );
        assert!(rig.state.api_keys.set_principal(
            "cs-fresh",
            Principal {
                key_id: "cs-fresh".to_owned(),
                role: Role::Operator,
                scoped_object_ids: Some(vec!["output-0".to_owned(), "a".to_owned()]),
                scoped_output_ids: Some(vec!["output-0".to_owned(), "program:main".to_owned()]),
                scoped_discovery_domains: None,
            },
        ));
        multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
        let (status, body) = fetch(&rig, &format!("/cast/{token}/master.m3u8")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{doc}");
        assert_eq!(body, b"not found\n");
        assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
    }
}

/// ABUSE-STORE-R3: in a store-only run, a live subtitle swap to a private
/// source after the restart-pending removal is caught by the media barrier.
#[tokio::test]
async fn a_store_only_media_barrier_revokes_on_a_live_slot_the_put_removed() {
    let rig = rig_mode(&subtitle_doc("a"), Mode::StoreOnly, |s| s);
    let (status, created) = create(
        &rig,
        SCOPED,
        serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = token_of(&created);
    put_cast_1(&rig, drop_slots).await;
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("secret-cam".to_owned())]);
    let (status, body) = fetch(&rig, &format!("/cast/{token}/sub0-1-0.vtt")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, b"not found\n");
    assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
}

/// ABUSE-STORE-R3: with no run-bound view at all (no start document, no
/// boot model) what the cast output packages cannot be established:
/// creation fails closed, even for an unscoped operator.
#[tokio::test]
async fn without_a_run_view_cast_creation_fails_closed() {
    let rig = rig_mode(&subtitle_doc("a"), Mode::Bare, |s| s);
    assert!(rig.state.cast_run.is_none());
    for key in [SCOPED, OPERATOR_2] {
        let (status, created) = create(
            &rig,
            key,
            serde_json::json!({"output": "output-0", "feed": {"source": "a"}}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{created}");
        assert_eq!(rig.state.cast_streams.servable_on("cast-1"), 0);
    }
}

// ---------------------------------------------------------------------------
// #418 review ABUSE-HISTORY-R4: the media of a superseded feed (retained
// segments and `.vtt` files, listed or not) stays servable for a while after
// a take or a subtitle re-patch, so the gate keeps that feed's grants for
// `MEDIA_RETENTION_MS` after an outcome settles a newer one.
// ---------------------------------------------------------------------------

/// The accepted subtitle slot sources of cast-1.
fn accepted_subtitles(rig: &Rig) -> Vec<Option<String>> {
    let config: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&rig.state).unwrap(),
    )
    .unwrap();
    config
        .outputs
        .iter()
        .find(|o| o.id() == "cast-1")
        .and_then(|o| o.slots())
        .map(|s| s.subtitle.iter().map(|s| s.source.clone()).collect())
        .unwrap_or_default()
}

/// Wait until the relay has handled the outcome of operation `op`.
async fn settled(events: &mut multiview_engine::EventSubscription<Event>, op: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the take's outcome is relayed")
            .unwrap();
        if let Event::RouteOutput(outcome) = event.event.as_ref() {
            if outcome.operation_id == op {
                return;
            }
        }
    }
}

/// A take of output-0 to `feed` by an operator, waited until settled.
async fn take_settled(rig: &Rig, key: &str, feed: serde_json::Value) {
    let mut events = rig.state.engine.subscribe();
    let r = send(
        &rig.router,
        route_take(OPERATOR_TOKEN, key, "output-0", feed),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let op = body_json(r).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    settled(&mut events, &op).await;
}

/// A private-to-public subtitle re-patch (secret-cam → a) of output-0 on
/// a rig whose run samples secret-cam, persisted, then an ordinary take of
/// output-0 to source `a` whose outcome settles: the subtitle commit is gone
/// from the gate, but secret-cam's cues stay servable.
async fn repatched_then_settled(rig: &Rig) {
    let r = send(
        &rig.router,
        subtitle_take(
            OPERATOR_TOKEN,
            "history-subs",
            serde_json::json!({"source": "a"}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    for _ in 0..500 {
        if accepted_subtitles(rig) == vec![Some("a".to_owned())] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(accepted_subtitles(rig), vec![Some("a".to_owned())]);
    assert_eq!(
        rig.routing.subtitles.lock().unwrap()["cast-1"],
        vec![Some("a".to_owned())]
    );
    take_settled(rig, "history-take", serde_json::json!({"source": "a"})).await;
    assert_eq!(rig.routing.bus("cast-1"), "src:a");
}

fn subtitle_history_rig() -> Rig {
    let rig = rig_doc(&subtitle_doc("secret-cam"), |s| s);
    rig.routing
        .subtitles
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), vec![Some("secret-cam".to_owned())]);
    rig
}

/// Keep `token` live across `total` of clock time (playlist fetches every
/// 30 s refresh its idle expiry).
async fn advance_keeping(rig: &Rig, token: &str, total: Duration) {
    let step = Duration::from_secs(30);
    let mut left = total;
    while !left.is_zero() {
        let d = left.min(step);
        rig.advance(d);
        left -= d;
        assert_eq!(
            fetch(rig, &format!("/cast/{token}/master.m3u8")).await.0,
            StatusCode::OK
        );
    }
}

fn retention() -> Duration {
    Duration::from_millis(multiview_control::cast_streams::MEDIA_RETENTION_MS)
}

/// ABUSE-HISTORY-R4 (the reviewer's scenario): an administrator's stream
/// plays output-0 with private subtitle source secret-cam; an operator
/// re-patches the subtitles to public `a` and takes the output to source
/// `a`, and that outcome settles. A new principal allowed the output, Program
/// and `a` but not secret-cam is refused while secret-cam's `.vtt` files
/// may still be served, then admitted once the retention passed. The
/// administrator's own token is never revoked by the history.
#[tokio::test]
async fn a_new_token_is_refused_while_a_superseded_private_subtitle_source_stays_servable() {
    let rig = subtitle_history_rig();
    let (status, admin) =
        create(&rig, ADMIN_TOKEN, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{admin}");
    let admin = token_of(&admin);
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "secret-cam on air: {doc}");
    repatched_then_settled(&rig).await;
    assert_eq!(
        fetch(&rig, &format!("/cast/{admin}/master.m3u8")).await.0,
        StatusCode::OK
    );
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "retained secret-cam cues: {doc}"
    );
    advance_keeping(
        &rig,
        &admin,
        retention().checked_sub(Duration::from_secs(1)).unwrap(),
    )
    .await;
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "just before the retention: {doc}"
    );
    advance_keeping(&rig, &admin, Duration::from_secs(1)).await;
    // The sink is stalled (9 segments, as when superseded): the retention
    // alone never expires the history (ABUSE-HISTORY-R4, round 5).
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "stalled sink: {doc}");
    advance_keeping(&rig, &admin, Duration::from_secs(600)).await;
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "still stalled: {doc}");
    // 20 more segments: the oldest retained file may still be of that era.
    rig.cast.set_segments("cast-1", 9 + 20);
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "one file left: {doc}");
    // 21: every file of the superseded era is retired.
    rig.cast.set_segments("cast-1", 9 + 21);
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "sink replaced it: {doc}");
    let scoped = token_of(&doc);
    assert_eq!(
        fetch(&rig, &format!("/cast/{scoped}/master.m3u8")).await.0,
        StatusCode::OK
    );
}

/// ABUSE-HISTORY-R4, the video case: after a take from private
/// `secret-cam` to public `a` settles, its retained segments may still be
/// served: a principal without secret-cam gets no token until the retention
/// passed.
#[tokio::test]
async fn a_new_token_is_refused_while_a_superseded_private_video_feed_stays_servable() {
    let rig = rig();
    let (status, admin) = create(
        &rig,
        ADMIN_TOKEN,
        serde_json::json!({"output": "output-0", "feed": {"source": "secret-cam"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{admin}");
    let admin = token_of(&admin);
    assert_eq!(rig.routing.bus("cast-1"), "src:secret-cam");
    take_settled(&rig, "history-video", serde_json::json!({"source": "a"})).await;
    assert_eq!(rig.routing.bus("cast-1"), "src:a");
    assert_eq!(
        fetch(&rig, &format!("/cast/{admin}/master.m3u8")).await.0,
        StatusCode::OK
    );
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "retained secret-cam segments: {doc}"
    );
    advance_keeping(&rig, &admin, retention()).await;
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "stalled sink: {doc}");
    rig.cast.set_segments("cast-1", 9 + 21);
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "retention over, sink replaced: {doc}"
    );
}

/// ABUSE-HISTORY-R4: an existing token whose owner is re-scoped off the
/// superseded private source while its media is still servable is revoked
/// by the re-check (under the CURRENT grants, history included).
#[tokio::test]
async fn a_recheck_within_the_retention_revokes_an_owner_who_lost_the_superseded_source() {
    let rig = subtitle_history_rig();
    let (status, created) = create(&rig, FRESH, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = token_of(&created);
    repatched_then_settled(&rig).await;
    // The take and the re-check under unchanged grants keep it.
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    assert_eq!(
        fetch(&rig, &format!("/cast/{token}/master.m3u8")).await.0,
        StatusCode::OK
    );
    assert!(rig.state.api_keys.set_principal(
        "cs-fresh",
        Principal {
            key_id: "cs-fresh".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(vec![
                "output-0".to_owned(),
                "a".to_owned(),
                "me1".to_owned(),
            ]),
            scoped_output_ids: Some(vec!["output-0".to_owned(), "program:main".to_owned()]),
            scoped_discovery_domains: None,
        },
    ));
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    let (status, body) = fetch(&rig, &format!("/cast/{token}/master.m3u8")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, b"not found\n");
}

/// ABUSE-HISTORY-R4 (round 5): a new file generation (a new activation or
/// packager: new names, the old ones purged and never served) retires the
/// superseded media once the retention elapsed, however few segments.
#[tokio::test]
async fn a_new_sink_generation_expires_the_history_after_the_retention() {
    let rig = subtitle_history_rig();
    let (status, admin) =
        create(&rig, ADMIN_TOKEN, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{admin}");
    let admin = token_of(&admin);
    repatched_then_settled(&rig).await;
    write_generation(&rig, "cast-1", 2);
    rig.cast.set_generation("cast-1", 2);
    rig.cast.set_segments("cast-1", 1);
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "within the retention: {doc}");
    advance_keeping(&rig, &admin, retention()).await;
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "new generation: {doc}");
    assert_eq!(rig.state.cast_streams.history_len("cast-1"), 0);
}

/// ABUSE-HISTORY-R4 (round 5): with the engine status unavailable nothing
/// superseded is forgotten (fail closed), even when the sink did replace
/// it; once the status is back and proves it, the history goes.
#[tokio::test]
async fn an_unavailable_sink_status_keeps_the_history() {
    let rig = subtitle_history_rig();
    let (status, admin) =
        create(&rig, ADMIN_TOKEN, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{admin}");
    let admin = token_of(&admin);
    repatched_then_settled(&rig).await;
    let kept = rig.state.cast_streams.history_len("cast-1");
    assert!(kept > 0);
    advance_keeping(&rig, &admin, retention()).await;
    rig.cast.set_segments("cast-1", 9 + 21);
    rig.cast.hidden.store(true, Ordering::SeqCst);
    multiview_control::cast_streams::lifecycle::sweep_now(&rig.state);
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    assert_eq!(
        rig.state.cast_streams.history_len("cast-1"),
        kept,
        "status unavailable: kept"
    );
    rig.cast.hidden.store(false, Ordering::SeqCst);
    multiview_control::cast_streams::lifecycle::sweep_now(&rig.state);
    assert_eq!(rig.state.cast_streams.history_len("cast-1"), 0);
}

/// Write a minimal stream of file generation `generation` into `id`'s dir.
fn write_generation(rig: &Rig, id: &str, generation: u64) {
    let dir = rig.dir.path().join(id);
    std::fs::write(dir.join(format!("seg-v-{generation}-0.m4s")), b"new-seg").unwrap();
    std::fs::write(dir.join(format!("init-v-{generation}-0.mp4")), b"new-init").unwrap();
    std::fs::write(
        dir.join(format!("sub0-{generation}-0.vtt")),
        "WEBVTT\n\nnew\n",
    )
    .unwrap();
}

/// ABUSE-HISTORY-R4 (round 5), defence in depth: a file of another
/// generation than the output's current one (left by a best-effort purge)
/// is the generic 404 even for a valid token; the current one is served,
/// playlists always.
#[tokio::test]
async fn a_stale_generation_file_is_never_served() {
    let rig = rig();
    let (status, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    std::fs::write(
        rig.dir.path().join("cast-1").join("sub0-1-0.vtt"),
        "WEBVTT\n\nold\n",
    )
    .unwrap();
    for file in ["seg-v-1-0.m4s", "init-v-1-0.mp4", "sub0-1-0.vtt"] {
        assert_eq!(
            fetch(&rig, &format!("/cast/{token}/{file}")).await.0,
            StatusCode::OK,
            "{file}"
        );
    }
    write_generation(&rig, "cast-1", 2);
    rig.cast.set_generation("cast-1", 2);
    for file in ["seg-v-1-0.m4s", "init-v-1-0.mp4", "sub0-1-0.vtt"] {
        let (status, body) = fetch(&rig, &format!("/cast/{token}/{file}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "stale {file}");
        assert_eq!(body, b"not found\n");
    }
    for file in [
        "seg-v-2-0.m4s",
        "init-v-2-0.mp4",
        "sub0-2-0.vtt",
        "master.m3u8",
    ] {
        assert_eq!(
            fetch(&rig, &format!("/cast/{token}/{file}")).await.0,
            StatusCode::OK,
            "{file}"
        );
    }
}

/// #418 review r7: a disabled output (its stream files could not be
/// removed) serves nothing, not even to a valid token at the current
/// generation: every media path is the generic 404.
#[tokio::test]
async fn a_disabled_output_serves_nothing_to_a_valid_token() {
    let rig = rig();
    let (status, doc) = create(&rig, OPERATOR_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of(&doc);
    for file in ["master.m3u8", "seg-v-1-0.m4s", "init-v-1-0.mp4"] {
        assert_eq!(
            fetch(&rig, &format!("/cast/{token}/{file}")).await.0,
            StatusCode::OK,
            "{file}"
        );
    }
    // Even while the status still reads active (the gate deactivates on
    // disable, but the flag alone suffices).
    rig.cast.disabled.store(true, Ordering::SeqCst);
    for file in ["master.m3u8", "seg-v-1-0.m4s", "init-v-1-0.mp4"] {
        let (status, body) = fetch(&rig, &format!("/cast/{token}/{file}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{file}");
        assert_eq!(body, b"not found\n");
    }
}

/// #418 review r7: while the output is disabled, a generation change (or
/// an inactive status) never expires the superseded grants: the old
/// generation's files may be left on disk.
#[tokio::test]
async fn a_generation_change_on_a_disabled_output_keeps_the_history() {
    let rig = subtitle_history_rig();
    let (status, admin) =
        create(&rig, ADMIN_TOKEN, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::CREATED, "{admin}");
    let admin = token_of(&admin);
    repatched_then_settled(&rig).await;
    let kept = rig.state.cast_streams.history_len("cast-1");
    assert!(kept > 0);
    advance_keeping(&rig, &admin, retention()).await;
    rig.cast.disabled.store(true, Ordering::SeqCst);
    rig.cast.set_generation("cast-1", 2);
    rig.cast.set_segments("cast-1", 9 + 21);
    multiview_control::cast_streams::lifecycle::sweep_now(&rig.state);
    multiview_control::cast_streams::lifecycle::recheck_now(&rig.state);
    assert_eq!(
        rig.state.cast_streams.history_len("cast-1"),
        kept,
        "disabled: kept"
    );
    let (status, doc) = create(&rig, SCOPED, serde_json::json!({"output": "output-0"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "disabled: {doc}");
    rig.cast
        .state
        .lock()
        .unwrap()
        .insert("cast-1".to_owned(), (false, 3));
    multiview_control::cast_streams::lifecycle::sweep_now(&rig.state);
    assert_eq!(
        rig.state.cast_streams.history_len("cast-1"),
        kept,
        "disabled and inactive: kept"
    );
}
