//! ADR-0111 C2: a server-side Cast session (ADR-M011) plays a cast stream
//! when the engine runs a cast output: it LAUNCHes the configured receiver
//! app, LOADs the absolute `https` token URL (fMP4, LIVE), owns its stream
//! (stop revokes it, a revoked stream ends the session), publishes the
//! player state, and never exposes the token (answer, list, audit, events,
//! logs). Driven through the real router with a fake cast engine, a fake
//! routing seam and a scripted CASTV2 channel (socket-free).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::cast_engine::{CastEngine, CastEngineError, CastOutputStatus};
use multiview_control::devices::cast::media::{CastDelivery, CastMediaBase};
use multiview_control::devices::cast::protocol::{
    CastAppId, CastFrame, NS_MEDIA, NS_RECEIVER, PLATFORM_RECEIVER_ID, SENDER_ID,
};
use multiview_control::devices::cast::runtime::CastSessionFactory;
use multiview_control::devices::cast::session::{
    CastSessionConfig, ScriptedChannel, ScriptedConnector, ScriptedInbound, SentFrames,
};
use multiview_control::devices::cast::store::redact_cast_url;
use multiview_control::devices::DevicePollerRegistry;
use multiview_control::output_routing::{
    AudioPlanCost, OutputLive, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost,
    PlanRefusal, RoutedAudio, RoutedFeed, RoutingLive, TakeSubmitError,
};
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

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
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
kind = "cast"
id = "cast-1"
codec = "h264"
"##;

/// Operator confined to output-0 and source `a`, without the Program scope.
const NO_PROGRAM: &str = "cs-np.cs-np-secret";
const BASE: &str = "https://engine.example.org";
const APP: &str = "8E05190C";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    keys.register(
        "cs-np",
        "cs-np-secret",
        Principal {
            key_id: "cs-np".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(vec!["output-0".to_owned(), "a".to_owned()]),
            scoped_output_ids: Some(vec!["output-0".to_owned()]),
            scoped_discovery_domains: None,
        },
    );
    keys
}

/// One always-ready cast output over a real directory.
struct FakeCast {
    dir: PathBuf,
    active: Mutex<bool>,
}

impl CastEngine for FakeCast {
    fn outputs(&self) -> Vec<CastOutputStatus> {
        let active = *self.active.lock().unwrap();
        vec![CastOutputStatus {
            id: "cast-1".to_owned(),
            dir: self.dir.clone(),
            active,
            ready: active,
            generation: 1,
            video_segments: if active { 9 } else { 0 },
            dropped_packets: 0,
            last_error: None,
            width: 640,
            height: 360,
            fps_num: 25,
            fps_den: 1,
        }]
    }
    fn activate(&self, _id: &str, _preroll: bool) -> Result<u64, CastEngineError> {
        *self.active.lock().unwrap() = true;
        Ok(1)
    }
    fn deactivate(&self, _id: &str) -> Result<(), CastEngineError> {
        *self.active.lock().unwrap() = false;
        Ok(())
    }
}

/// A routing seam whose cast output shows Program; takes apply at once.
struct FakeRouting {
    live: Mutex<RoutingLive>,
    tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
}

impl OutputRouting for FakeRouting {
    fn plan(&self, _output: &str, _feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        Ok(PlanCost {
            needs_encoder: false,
            sessions_in_use: 0,
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
        let bus = take.video.as_ref().unwrap().bus_name();
        self.live
            .lock()
            .unwrap()
            .outputs
            .get_mut(&take.output)
            .unwrap()
            .bus = bus;
        let _ = self.tx.send(OutputOutcome {
            operation_id: take.operation_id,
            output: take.output,
            tick: 1,
            outcome: "applied".to_owned(),
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

fn routing() -> Arc<FakeRouting> {
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    let mut live = RoutingLive::default();
    live.outputs.insert(
        "cast-1".to_owned(),
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
    Arc::new(FakeRouting {
        live: Mutex::new(live),
        tx,
        rx: Mutex::new(Some(rx)),
    })
}

fn from_device(namespace: &str, payload: serde_json::Value) -> CastFrame {
    CastFrame {
        namespace: namespace.to_owned(),
        source: PLATFORM_RECEIVER_ID.to_owned(),
        destination: SENDER_ID.to_owned(),
        payload: payload.to_string(),
    }
}

/// A device that runs `app_id` once launched, then reports `player_state`.
fn device_script(
    app_id: &str,
    player_state: &str,
    idle_reason: Option<&str>,
) -> Vec<ScriptedInbound> {
    let mut entry = serde_json::json!({ "mediaSessionId": 1, "playerState": player_state });
    if let Some(reason) = idle_reason {
        entry["idleReason"] = serde_json::Value::from(reason);
    }
    vec![
        ScriptedInbound::Frame(from_device(
            NS_RECEIVER,
            serde_json::json!({
                "type": "RECEIVER_STATUS",
                "requestId": 1,
                "status": { "applications": [{
                    "appId": app_id,
                    "sessionId": "s-1",
                    "transportId": "t-1",
                    "displayName": "Receiver"
                }] }
            }),
        )),
        ScriptedInbound::Frame(from_device(
            NS_MEDIA,
            serde_json::json!({ "type": "MEDIA_STATUS", "requestId": 0, "status": [entry] }),
        )),
        ScriptedInbound::Hang,
    ]
}

struct Rig {
    router: axum::Router,
    state: AppState,
    sent: SentFrames,
    _dir: tempfile::TempDir,
}

/// The rig: one cast output, the given `cast_media_base` (`None` = unset),
/// the registered receiver app id, and a device that runs `device_app`.
fn rig_with(base: Option<&str>, app_id: Option<&str>, device_app: &str, state_token: &str) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cast-1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("master.m3u8"), "#EXTM3U\n").unwrap();
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, _receiver) = command_bus(64);
    let (channel, sent) =
        ScriptedChannel::new(device_script(device_app, state_token, Some("ERROR")));
    let factory = CastSessionFactory::new(
        Arc::new(ScriptedConnector::new(vec![channel])),
        Arc::new(CastDelivery::new()),
        CastSessionConfig::test_fast(),
    );
    let mut state = AppState::new(
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
        let seam: Arc<dyn OutputRouting> = routing();
        seam
    })
    .with_cast_engine(Arc::new(FakeCast {
        dir,
        active: Mutex::new(false),
    }))
    .with_cast_app_id(app_id.map(str::to_owned))
    .with_device_pollers(Arc::new(DevicePollerRegistry::with_factory(Arc::new(
        factory,
    ))));
    if let Some(base) = base {
        let base = CastMediaBase::parse(base).unwrap();
        state = state.with_cast_delivery(Arc::new(CastDelivery::new().with_base(base)));
    }
    Rig {
        router: multiview_control::router(state.clone()),
        state,
        sent,
        _dir: tmp,
    }
}

fn rig() -> Rig {
    rig_with(Some(BASE), Some(APP), APP, "PLAYING")
}

fn start_body(feed: Option<serde_json::Value>) -> serde_json::Value {
    let mut body = serde_json::json!({ "address": "[2001:db8::20]:8009", "name": "Lounge TV" });
    if let Some(feed) = feed {
        body["feed"] = feed;
    }
    body
}

async fn start(rig: &Rig, token: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = send(
        &rig.router,
        support::post_json("/api/v1/cast/sessions", token, &body),
    )
    .await;
    let status = response.status();
    (status, body_json(response).await)
}

fn delete(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// The frames the actor sent so far, as JSON payloads.
fn sent_payloads(sent: &SentFrames) -> Vec<(String, serde_json::Value)> {
    sent.lock()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f.namespace.clone(),
                serde_json::from_str(&f.payload).unwrap(),
            )
        })
        .collect()
}

/// Wait (bounded) until the actor sent a frame of `kind`.
async fn wait_for_frame(sent: &SentFrames, kind: &str) -> serde_json::Value {
    for _ in 0..200 {
        if let Some((_, body)) = sent_payloads(sent)
            .into_iter()
            .find(|(_, b)| b["type"] == kind)
        {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the actor never sent {kind}");
}

/// The single stream's token, read through the registry as the owner would.
fn token_of_only_stream(state: &AppState) -> String {
    let streams = state.cast_streams.list();
    assert_eq!(streams.len(), 1, "exactly one stream");
    state.cast_streams.get(&streams[0].id).unwrap().1
}

#[tokio::test]
async fn a_session_launches_the_configured_receiver_and_loads_the_absolute_token_url() {
    let rig = rig();
    let (status, doc) = start(
        &rig,
        OPERATOR_TOKEN,
        start_body(Some(serde_json::json!({"source": "a"}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let token = token_of_only_stream(&rig.state);
    let launch = wait_for_frame(&rig.sent, "LAUNCH").await;
    assert_eq!(launch["appId"], APP, "the registered receiver is launched");
    let load = wait_for_frame(&rig.sent, "LOAD").await;
    let media = &load["media"];
    let url = format!("{BASE}/cast/{token}/master.m3u8");
    assert_eq!(media["contentId"], url.as_str());
    assert_eq!(media["contentUrl"], url.as_str());
    assert_eq!(media["contentType"], "application/vnd.apple.mpegurl");
    assert_eq!(media["streamType"], "LIVE");
    assert_eq!(media["hlsSegmentFormat"], "fmp4");
    assert_eq!(media["hlsVideoSegmentFormat"], "fmp4");
    assert_eq!(media["customData"]["title"], "Lounge TV");
    // The answer carries the redacted URL and the owned stream.
    assert_eq!(
        doc["media_url"],
        format!("{BASE}/cast/<redacted>/master.m3u8").as_str()
    );
    assert_eq!(doc["output"], "output-0");
    let stream_id = doc["stream_id"].as_str().unwrap().to_owned();
    assert_eq!(rig.state.cast_streams.list()[0].id, stream_id);
    assert_eq!(rig.state.cast_streams.list()[0].owner, "operator-key");
    // The player state the receiver reports is published.
    let id = doc["id"].as_str().unwrap().to_owned();
    let mut seen = serde_json::Value::Null;
    for _ in 0..200 {
        let r = send(
            &rig.router,
            support::get(&format!("/api/v1/cast/sessions/{id}"), VIEWER_TOKEN),
        )
        .await;
        seen = body_json(r).await;
        if seen["player_state"] == "playing" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen["player_state"], "playing", "{seen}");
    assert!(
        seen.get("idle_reason").is_none(),
        "no reason while playing: {seen}"
    );
}

#[tokio::test]
async fn without_a_registered_receiver_the_default_media_receiver_is_launched() {
    let rig = rig_with(Some(BASE), None, "CC1AD845", "BUFFERING");
    let (status, doc) = start(&rig, OPERATOR_TOKEN, start_body(None)).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    let launch = wait_for_frame(&rig.sent, "LAUNCH").await;
    assert_eq!(launch["appId"], "CC1AD845");
    // The launched default receiver is recognised: the LOAD follows.
    let load = wait_for_frame(&rig.sent, "LOAD").await;
    assert_eq!(load["media"]["streamType"], "LIVE");
}

#[tokio::test]
async fn an_idle_receiver_publishes_its_reason() {
    let rig = rig_with(Some(BASE), Some(APP), APP, "IDLE");
    let (_, doc) = start(&rig, OPERATOR_TOKEN, start_body(None)).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    let mut seen = serde_json::Value::Null;
    for _ in 0..200 {
        let r = send(
            &rig.router,
            support::get(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN),
        )
        .await;
        seen = body_json(r).await;
        if seen["player_state"] == "idle" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen["player_state"], "idle", "{seen}");
    assert_eq!(seen["idle_reason"], "ERROR");
}

#[tokio::test]
async fn no_absolute_https_base_is_refused_before_any_stream_exists() {
    for base in [None, Some("http://[2001:db8::1]:8080")] {
        let rig = rig_with(base, Some(APP), APP, "PLAYING");
        let (status, doc) = start(&rig, OPERATOR_TOKEN, start_body(None)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{doc}");
        assert!(
            doc["type"]
                .as_str()
                .unwrap()
                .ends_with("cast-no-public-base"),
            "{doc}"
        );
        assert!(doc["detail"].as_str().unwrap().contains("cast_media_base"));
        assert!(rig.state.cast_streams.list().is_empty(), "no stream minted");
        assert!(rig.state.cast_sessions.list().is_empty(), "no session");
        assert!(rig.sent.lock().unwrap().is_empty(), "nothing dialled");
    }
}

#[tokio::test]
async fn the_session_stream_uses_the_stream_routes_authorization() {
    let rig = rig();
    // A viewer cannot start a session at all.
    let (status, _) = start(&rig, VIEWER_TOKEN, start_body(None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Program (the default feed) needs the Program scope: refused like
    // POST /cast/streams refuses it, before any stream or dial.
    let (status, doc) = start(&rig, NO_PROGRAM, start_body(None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{doc}");
    let r = send(
        &rig.router,
        support::post_json("/api/v1/cast/streams", NO_PROGRAM, &serde_json::json!({})),
    )
    .await;
    let status = r.status();
    let direct = body_json(r).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{direct}");
    assert!(rig.state.cast_streams.list().is_empty());
    assert!(rig.sent.lock().unwrap().is_empty());
    // A source it may read is allowed.
    let (status, doc) = start(
        &rig,
        NO_PROGRAM,
        start_body(Some(serde_json::json!({"source": "a"}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    assert_eq!(rig.state.cast_streams.list()[0].owner, "cs-np");
}

#[tokio::test]
async fn stopping_the_session_revokes_its_stream() {
    let rig = rig();
    let (_, doc) = start(&rig, OPERATOR_TOKEN, start_body(None)).await;
    let token = token_of_only_stream(&rig.state);
    let _ = wait_for_frame(&rig.sent, "LOAD").await;
    let id = doc["id"].as_str().unwrap();
    let r = send(
        &rig.router,
        delete(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let stop = wait_for_frame(&rig.sent, "STOP").await;
    assert_eq!(stop["sessionId"], "s-1", "the receiver is told to stop");
    assert!(
        rig.state.cast_streams.list().is_empty(),
        "the stream is revoked"
    );
    let r = send(
        &rig.router,
        Request::builder()
            .uri(format!("/cast/{token}/master.m3u8"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::NOT_FOUND,
        "the token is dead at once"
    );
}

#[tokio::test]
async fn a_revoked_stream_ends_its_session_and_a_stream_session_cannot_be_saved() {
    let rig = rig();
    let (_, doc) = start(&rig, OPERATOR_TOKEN, start_body(None)).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    let stream = doc["stream_id"].as_str().unwrap().to_owned();
    let _ = wait_for_frame(&rig.sent, "LOAD").await;
    let r = send(
        &rig.router,
        support::post_json(
            &format!("/api/v1/cast/sessions/{id}/save"),
            OPERATOR_TOKEN,
            &serde_json::json!({"device_id": "tv-1"}),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::CONFLICT,
        "a temporary stream is not a device"
    );
    let r = send(
        &rig.router,
        delete(&format!("/api/v1/cast/streams/{stream}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = send(
        &rig.router,
        support::get(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::NOT_FOUND,
        "the session ended with its stream"
    );
    let _ = wait_for_frame(&rig.sent, "STOP").await;
}

#[test]
fn redaction_keeps_origin_and_file_only() {
    assert_eq!(
        redact_cast_url("https://e.example.org/cast/AbC-_123/master.m3u8"),
        "https://e.example.org/cast/<redacted>/master.m3u8"
    );
    assert_eq!(
        redact_cast_url("http://192.0.2.7:8080/hls/out/a.m3u8"),
        "http://192.0.2.7:8080/hls/out/a.m3u8"
    );
    assert_eq!(CastAppId::parse("8e05190c").unwrap().as_str(), "8E05190C");
    assert!(CastAppId::parse("8E05190").is_none());
    assert!(CastAppId::parse("8E05190G").is_none());
    assert_eq!(CastAppId::default().as_str(), "CC1AD845");
}

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
fn the_session_token_never_reaches_an_answer_a_list_the_audit_an_event_or_a_log() {
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
        let rig = rig_with(Some(BASE), Some(APP), APP, "IDLE");
        let mut events = rig.state.engine.subscribe();
        let (status, doc) = start(&rig, OPERATOR_TOKEN, start_body(None)).await;
        assert_eq!(status, StatusCode::CREATED, "{doc}");
        let token = token_of_only_stream(&rig.state);
        let load = wait_for_frame(&rig.sent, "LOAD").await;
        assert!(
            load.to_string().contains(&token),
            "only the LOAD carries it"
        );
        let id = doc["id"].as_str().unwrap().to_owned();
        let mut answers = doc.to_string();
        for path in [
            "/api/v1/cast/sessions".to_owned(),
            format!("/api/v1/cast/sessions/{id}"),
        ] {
            let r = send(&rig.router, support::get(&path, OPERATOR_TOKEN)).await;
            answers.push_str(&body_json(r).await.to_string());
        }
        let r = send(
            &rig.router,
            delete(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(!answers.contains(&token), "token in an answer");
        assert!(answers.contains("/cast/<redacted>/master.m3u8"));
        let audit = serde_json::to_string(&rig.state.audit.list(None).unwrap()).unwrap();
        assert!(!audit.contains(&token), "token in the audit log");
        assert!(
            audit.contains("<redacted>"),
            "the session start is audited redacted"
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !format!("{:?}", event.event).contains(&token),
                "token in an event"
            );
        }
        let logs = capture.0.lock().unwrap().clone();
        assert!(!logs.contains(&token), "token in a log line");
    });
}
