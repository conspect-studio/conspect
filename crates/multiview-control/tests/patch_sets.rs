//! #308 E2 — atomic multi-destination patch sets on `POST /patch/plan|take`
//! and patch salvos, against the real switcher control (its mailbox and
//! armed slot) and a fake output-routing seam: one `PatchArmed` + one
//! `try_take_set` per set, authorization of every member before anything,
//! the set's own idempotency namespace, the limits counted once per
//! principal and once per destination, the encoder-delta admission, the
//! second-enqueue rollback, the `patch.take` aggregate with per-destination
//! delivery, patch salvos, and a take flood (chaos).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

mod support;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::output_routing::{
    OutputOutcome, OutputRouting, OutputTakeRequest, OutputTakeSet, PlanCost, PlanRefusal,
    PrincipalSessions, RoutedFeed, RoutingLive, TakeSubmitError,
};
use multiview_control::realtime::SessionStream;
use multiview_control::{boot_model::BootModel, command_bus, AppState, InMemoryRepository};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::patch_set::{ArmState, SetRefusal};
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherControl, SwitcherTickDriver,
};
use multiview_engine::switcher_inputs::MixerInputs;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::{json, Value};
use support::{body_json, send, ADMIN_TOKEN, OPERATOR_TOKEN};

/// Outputs in the rig's document (32: one 64-crosspoint set of video +
/// audio follow on every output).
const OUTPUTS: usize = 32;

fn doc() -> String {
    let mut doc = String::from(
        r##"
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
id = "abc-news"
kind = "bars"
[[sources]]
id = "vt1"
kind = "bars"
[[sources]]
id = "bars"
kind = "bars"
[[sources]]
id = "secret"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "abc-news"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "abc-news"
preview_source = "vt1"
program_input = 1
preview_input = 2
[[switcher.mix_effects.inputs]]
number = 1
video = { source = "abc-news" }
[[switcher.mix_effects.inputs]]
number = 2
video = { source = "vt1" }
[[switcher.mix_effects.inputs]]
number = 3
video = { source = "secret" }
[[switcher.mix_effects.inputs]]
number = 4
[[switcher.mix_effects.inputs]]
number = 5
"##,
    );
    for i in 0..OUTPUTS {
        write!(
            doc,
            "[[outputs]]\nkind = \"hls\"\npath = \"/tmp/patch-sets/{i}/index.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n"
        )
        .unwrap();
    }
    doc
}

fn config_id(i: usize) -> String {
    format!("hls /tmp/patch-sets/{i}/index.m3u8")
}

/// me1 + Program scope, output-0 and output-1 on both axes, bars, the layout
/// `NewsOne` (which shows `secret`), the salvos `news` and `wide` — not
/// output-2, not `secret`.
const LIMITED: &str = "set-limited.set-limited-secret";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    keys.register(
        "set-limited",
        "set-limited-secret",
        Principal {
            key_id: "set-limited".to_owned(),
            role: Role::Operator,
            scoped_object_ids: Some(
                [
                    "me1", "output-0", "output-1", "bars", "abc-news", "vt1", "NewsOne", "news",
                    "wide",
                ]
                .map(str::to_owned)
                .to_vec(),
            ),
            scoped_output_ids: Some(
                ["output-0", "output-1", "program:main"]
                    .map(str::to_owned)
                    .to_vec(),
            ),
            scoped_discovery_domains: None,
        },
    );
    keys
}

#[derive(Default)]
struct FakeState {
    sets: Vec<OutputTakeSet>,
    takes: Vec<OutputTakeRequest>,
    /// Every set offered, admitted or not (the busy path's arm).
    offered: Vec<OutputTakeSet>,
    /// The set mailbox is full.
    busy: bool,
    /// Encoder sessions held per principal (cap 2).
    held: HashMap<String, usize>,
    /// The fake engine's bounded set mailbox (never drained).
    mailbox_cap: usize,
}

struct Fake {
    state: Mutex<FakeState>,
    /// Held so the outcome channel stays open for the relay.
    _tx: SyncSender<OutputOutcome>,
    rx: Mutex<Option<Receiver<OutputOutcome>>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        Arc::new(Self {
            state: Mutex::new(FakeState {
                mailbox_cap: 1024,
                ..FakeState::default()
            }),
            _tx: tx,
            rx: Mutex::new(Some(rx)),
        })
    }
    fn sets(&self) -> Vec<OutputTakeSet> {
        self.state.lock().unwrap().sets.clone()
    }
}

impl OutputRouting for Fake {
    fn plan(&self, _output: &str, feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        Ok(PlanCost {
            needs_encoder: *feed != RoutedFeed::Program,
            sessions_in_use: 1,
            sessions_max: 6,
            needs_aux_canvas: false,
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
        self.state.lock().unwrap().takes.push(take);
        Ok(())
    }
    fn try_take_set(&self, set: OutputTakeSet) -> Result<(), TakeSubmitError> {
        let mut state = self.state.lock().unwrap();
        state.offered.push(set.clone());
        if state.busy || state.sets.len() >= state.mailbox_cap {
            return Err(TakeSubmitError::Busy);
        }
        state.sets.push(set);
        Ok(())
    }
    fn principal_sessions(&self, principal: &str) -> Option<PrincipalSessions> {
        let held = self
            .state
            .lock()
            .unwrap()
            .held
            .get(principal)
            .copied()
            .unwrap_or(0);
        Some(PrincipalSessions { held, max: 2 })
    }
    /// The engine's encoder-sharing identity: the feed and the output's
    /// encoder settings. Every output has the same format; output-2 alone
    /// has its own encoder settings (a bit rate of its own).
    fn encoder_identity(&self, output: &str, feed: &RoutedFeed) -> Option<String> {
        let settings = if output == config_id(2) { "s1" } else { "s0" };
        Some(format!("{}|{settings}", feed.bus_name()))
    }
    fn snapshot(&self) -> Option<RoutingLive> {
        None
    }
    fn subscribe_outcomes(&self) -> Option<Receiver<OutputOutcome>> {
        self.rx.lock().unwrap().take()
    }
}

fn stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["abc-news", "vt1", "bars", "secret"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
            store.publish(
                Nv12Image::solid(
                    64,
                    64,
                    128,
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
    fake: Arc<Fake>,
    engine: Arc<EnginePublisher<multiview_control::EngineStateSnapshot, Event>>,
}

fn tick(driver: &mut SwitcherTickDriver, index: u64) {
    let tick = Tick {
        index,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
}

fn rig(path: &Path) -> Rig {
    let doc = doc();
    std::fs::write(path, &doc).unwrap();
    let config = MultiviewConfig::load_from_toml(&doc).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let engine = Arc::new(EnginePublisher::new(4096));
    let (sender, _receiver) = command_bus(64);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::input(1),
        SourceRef::input(2),
        vec![],
    ));
    driver.set_prime_stores(stores());
    driver.set_inputs(
        MixerInputs::new([
            (1, None, Some(SourceRef::new("abc-news"))),
            (2, None, Some(SourceRef::new("vt1"))),
            (3, None, Some(SourceRef::new("secret"))),
            (4, None, None),
            (5, None, None),
        ])
        .unwrap(),
    );
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("abc-news"),
            preview: scene("vt1"),
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
    .with_switcher(handle.clone())
    .with_output_routing(seam);
    Rig {
        router: multiview_control::router(state.clone()),
        state,
        handle,
        driver,
        fake,
        engine,
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

fn mixer(input: u16, source: &str) -> Value {
    json!({ "plane": "video", "destination": {"mixer": "me1", "input": input}, "source": {"source": source} })
}

fn video(output: usize, source: Value) -> Value {
    json!({ "plane": "video", "destination": {"output": format!("output-{output}"), "slot": 0}, "source": source })
}

fn follow(output: usize) -> Value {
    json!({ "plane": "audio", "destination": {"output": format!("output-{output}"), "slot": 0}, "source": "follow" })
}

fn bars() -> Value {
    json!({"source": "bars"})
}

fn set(crosspoints: Vec<Value>) -> Value {
    json!({ "crosspoints": crosspoints })
}

async fn take(rig: &Rig, token: &str, key: &str, body: &Value) -> (StatusCode, Value) {
    let resp = send(
        &rig.router,
        post("/api/v1/patch/take", token, Some(key), body),
    )
    .await;
    let status = resp.status();
    let bytes = support::body_bytes(resp).await;
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)));
    (status, value)
}

fn store_layout(rig: &Rig, id: &str, right: &str) {
    rig.state
        .repository
        .create_layout(
            id,
            multiview_control::repository::LayoutInput {
                name: id.to_owned(),
                body: json!({
                    "canvas": {"width": 64, "height": 64, "fps": "25/1"},
                    "layout": {"kind": "grid", "columns": ["1fr", "1fr"], "rows": ["1fr"], "areas": ["a b"]},
                    "cells": [
                        {"id": "l", "area": "a", "source": {"input_id": "bars"}},
                        {"id": "r", "area": "b", "source": {"input_id": right}},
                    ],
                }),
            },
        )
        .unwrap();
}

/// A mixed set (mixer input + two outputs, one with audio follow) is one
/// switcher `PatchArmed` and one `try_take_set` with every output member,
/// sharing one arm; the plan reports every member and the set's cost.
#[tokio::test]
async fn a_mixed_set_is_one_armed_patch_and_one_router_set() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(&dir.path().join("boot.toml"));
    let body = set(vec![
        video(0, bars()),
        mixer(4, "bars"),
        video(1, bars()),
        follow(1),
    ]);
    let resp = send(
        &rig.router,
        post("/api/v1/patch/plan", OPERATOR_TOKEN, None, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;
    assert_eq!(plan["crosspoints"].as_array().unwrap().len(), 4, "{plan}");
    assert_eq!(
        plan["crosspoints"][1]["path"], "mixer-input",
        "request order"
    );
    assert_eq!(plan["crosspoints"][3]["path"], "follow");
    assert_eq!(plan["can_take"], true, "{plan}");
    // Two members on the same (feed, format) share one new encoder.
    assert_eq!(plan["set"]["new_encoders"], 1, "{plan}");
    assert!(rig.fake.sets().is_empty(), "a plan changes nothing");

    let (status, accepted) = take(&rig, OPERATOR_TOKEN, "mixed", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let op = accepted["operation_id"].as_str().unwrap().to_owned();
    let sets = rig.fake.sets();
    assert_eq!(sets.len(), 1, "one router set");
    let routed = &sets[0];
    assert_eq!(routed.operation_id, op);
    assert_eq!(routed.principal, "operator-key");
    let outputs: Vec<&str> = routed.members.iter().map(|m| m.output.as_str()).collect();
    assert_eq!(outputs, [config_id(0), config_id(1)]);
    assert!(routed.members[0].audio.is_none());
    assert_eq!(
        routed.members[1].audio,
        Some(multiview_control::output_routing::RoutedAudio::Follow)
    );
    assert!(
        rig.fake.state.lock().unwrap().takes.is_empty(),
        "no single take"
    );
    // The switcher holds the same arm: one drain validates and primes it.
    let arm = routed.arm.clone().expect("a mixer half arms the set");
    assert_eq!(arm.state(), ArmState::Pending);
    tick(&mut rig.driver, 1);
    assert_eq!(
        arm.state(),
        ArmState::SwitcherReady,
        "PatchArmed reached the switcher"
    );
    assert_eq!(rig.state.patch_sets.pending_len(), 1);
}

/// Every member's both ends are authorized before anything: one member's
/// destination, source or layout closure the caller may not read is 403,
/// nothing reaches either engine half, and the key is not burned.
#[tokio::test]
async fn any_member_403_enqueues_nothing_and_burns_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(&dir.path().join("boot.toml"));
    store_layout(&rig, "NewsOne", "secret");
    let mut outcomes = rig.handle.outcomes();
    for body in [
        // A destination: output-2 is not granted.
        set(vec![video(0, bars()), video(2, bars())]),
        // A source: `secret`.
        set(vec![
            video(0, bars()),
            video(1, json!({"source": "secret"})),
        ]),
        // A mixer member's source.
        set(vec![video(0, bars()), mixer(4, "secret")]),
        // A layout whose closure shows `secret`.
        set(vec![
            video(0, json!({"layout": "NewsOne"})),
            video(1, bars()),
        ]),
    ] {
        let (status, problem) = take(&rig, LIMITED, "k", &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body} → {problem}");
        let resp = send(
            &rig.router,
            post("/api/v1/patch/plan", LIMITED, None, &body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "plan of {body}");
    }
    assert!(
        rig.fake.state.lock().unwrap().offered.is_empty(),
        "nothing offered"
    );
    tick(&mut rig.driver, 1);
    assert!(outcomes.try_recv().is_err(), "nothing reached the switcher");
    // The same key is still fresh: an authorized set under it is admitted.
    let (status, body) = take(
        &rig,
        LIMITED,
        "k",
        &set(vec![video(0, bars()), mixer(4, "bars"), video(1, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(rig.fake.sets().len(), 1);
}

#[tokio::test]
async fn duplicate_destinations_and_oversize_sets_are_422() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let too_many: Vec<Value> = (0..OUTPUTS)
        .flat_map(|i| [video(i, bars()), follow(i)])
        .chain([mixer(4, "bars")])
        .collect();
    assert_eq!(too_many.len(), 65);
    for (body, kind) in [
        (
            set(vec![video(0, bars()), video(1, bars()), video(0, bars())]),
            "unknown-slot",
        ),
        (
            set(vec![mixer(4, "bars"), video(0, bars()), mixer(4, "vt1")]),
            "unknown-slot",
        ),
        (
            set(vec![
                video(0, bars()),
                follow(0),
                follow(0),
                video(1, bars()),
            ]),
            "unknown-slot",
        ),
        (set(too_many), ""),
    ] {
        for path in ["/api/v1/patch/plan", "/api/v1/patch/take"] {
            let resp = send(&rig.router, post(path, OPERATOR_TOKEN, Some("dup"), &body)).await;
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
            let problem = body_json(resp).await;
            assert!(
                problem["type"].as_str().unwrap().ends_with(kind),
                "{problem}"
            );
        }
    }
    assert!(rig.fake.state.lock().unwrap().offered.is_empty());
}

/// More than four distinct layouts in one set is refused before any read.
#[tokio::test]
async fn at_most_four_layouts_are_read_per_set() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = set((0..5)
        .map(|i| video(i, json!({"layout": format!("L{i}")})))
        .collect());
    let resp = send(
        &rig.router,
        post("/api/v1/patch/take", OPERATOR_TOKEN, Some("l"), &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .ends_with("unknown-source"),
        "{problem}"
    );
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("too many layouts"),
        "{problem}"
    );
}

/// One request is one take for the principal and one per destination: a
/// 64-crosspoint set does not multiply either budget; the 11th set in one
/// second is 429; another principal is unaffected.
#[tokio::test]
async fn the_limiter_counts_a_set_once_per_principal_and_per_destination() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let everything: Vec<Value> = (0..OUTPUTS)
        .flat_map(|i| [video(i, bars()), follow(i)])
        .collect();
    assert_eq!(everything.len(), 64);
    let (status, body) = take(&rig, OPERATOR_TOKEN, "all", &set(everything)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(rig.fake.sets()[0].members.len(), OUTPUTS);
    // Nine more sets, each output used at most twice in the window.
    for i in 0..9 {
        let (status, body) = take(
            &rig,
            OPERATOR_TOKEN,
            &format!("pair-{i}"),
            &set(vec![video(2 * i, bars()), video(2 * i + 1, bars())]),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "set {i}: {body}");
    }
    // The 11th set of this principal: its destinations are used once only,
    // so only the per-principal limit (10/s) refuses it.
    let (status, _) = take(
        &rig,
        OPERATOR_TOKEN,
        "eleventh",
        &set(vec![video(30, bars()), video(31, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rig.fake.sets().len(), 10, "the 429 enqueued nothing");
    let (status, body) = take(
        &rig,
        ADMIN_TOKEN,
        "admin",
        &set(vec![video(30, bars()), video(31, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

/// #308 review B3: a single-output take (`POST /patch/take` with one
/// output, which runs the slot-0 alias machinery) and a two-output set both
/// charge output-0 against the SAME per-output limiter
/// (`control.limiter`, `TAKES_PER_OUTPUT_PER_SEC = 2`): alternating the two
/// paths on the same output and principal within one second cannot exceed
/// that single per-output limit, nor the per-principal limit — the set path
/// no longer has its own independent budget for output members. The
/// over-limit request is 429 with nothing enqueued on either engine seam.
#[tokio::test]
async fn alternating_single_takes_and_sets_share_the_per_output_and_per_principal_limits() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));

    // 1st charge on output-0: a single-output take (the slot-0 alias path,
    // `control.limiter` directly).
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "single-1",
        &set(vec![video(0, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(rig.fake.state.lock().unwrap().takes.len(), 1);

    // 2nd charge on output-0: a two-output SET including output-0 (the set
    // path, now also `control.limiter` for its output members). Before the
    // B3 fix this used an independent `patch_limiter` budget and would have
    // been wrongly admitted on its own separate counter even after output-0
    // already held 2 single-output charges.
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "set-1",
        &set(vec![video(0, bars()), video(5, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(rig.fake.sets().len(), 1, "{body}");

    // output-0 is now at TAKES_PER_OUTPUT_PER_SEC (2) in the ONE shared
    // limiter: a 3rd touch of output-0, in either shape, is 429 and enqueues
    // nothing on either engine seam.
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "single-2",
        &set(vec![video(0, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(body["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("too many"));
    assert_eq!(
        rig.fake.state.lock().unwrap().takes.len(),
        1,
        "the 429 enqueued no single take"
    );
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "set-2",
        &set(vec![video(0, bars()), video(6, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        rig.fake.sets().len(),
        1,
        "the 429 enqueued no set: output-0's shared budget, not an independent one"
    );

    // A fresh output (output-7) still has room: the per-output limit is
    // per-destination, not global.
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "single-3",
        &set(vec![video(7, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    // The per-principal limit (10/s) is ALSO one shared counter across both
    // paths: it already holds 3 charges from above (single take on
    // output-0, the set, single take on output-7). 7 more single takes on
    // distinct, never-before-touched outputs reach the 10/s cap; the 11th
    // touch (any shape) is 429 with nothing enqueued, even though every
    // individual output involved is still under its own 2/s limit.
    for i in 8..15 {
        let (status, body) = take(
            &rig,
            OPERATOR_TOKEN,
            &format!("single-fill-{i}"),
            &set(vec![video(i, bars())]),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "output-{i}: {body}");
    }
    let takes_before = rig.fake.state.lock().unwrap().takes.len();
    let sets_before = rig.fake.sets().len();
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "eleventh-touch",
        &set(vec![video(20, bars()), video(21, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        rig.fake.state.lock().unwrap().takes.len(),
        takes_before,
        "nothing enqueued on the over-limit request"
    );
    assert_eq!(
        rig.fake.sets().len(),
        sets_before,
        "nothing enqueued on the over-limit request"
    );

    // Existing flood/limiter tests (`the_limiter_counts_a_set_once_per_...`,
    // `takes_are_rate_limited_per_output_and_per_principal`,
    // `a_set_flood_is_429_and_never_starves_another_principal`) keep passing
    // unmodified: this test only adds the cross-path regression.
}

/// A principal holding its cap is refused `capacity` for a set that opens
/// new encoders, before anything is enqueued; another principal's set is
/// admitted (the fake records the principal each set is charged to).
#[tokio::test]
async fn a_set_over_the_principal_cap_is_409_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake
        .state
        .lock()
        .unwrap()
        .held
        .insert("operator-key".to_owned(), 2);
    let body = set(vec![video(0, bars()), video(1, json!({"source": "vt1"}))]);
    let (status, problem) = take(&rig, OPERATOR_TOKEN, "cap", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["type"].as_str().unwrap().ends_with("capacity"),
        "{problem}"
    );
    assert!(
        rig.fake.state.lock().unwrap().offered.is_empty(),
        "nothing enqueued"
    );
    assert_eq!(rig.state.patch_sets.records_len(), 0, "no key burned");
    // A set needing no new encoder (Program) still fits.
    let program = set(vec![
        video(2, json!({"bus": "program"})),
        video(3, json!({"bus": "program"})),
    ]);
    let (status, accepted) = take(&rig, OPERATOR_TOKEN, "program", &program).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    // The plan says so too.
    let resp = send(
        &rig.router,
        post("/api/v1/patch/plan", OPERATOR_TOKEN, None, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;
    assert_eq!(plan["set"]["new_encoders"], 2, "{plan}");
    assert_eq!(plan["set"]["fits"], false, "{plan}");
    assert_eq!(plan["can_take"], false, "{plan}");
    // Another principal with room is admitted.
    let (status, body) = take(&rig, ADMIN_TOKEN, "cap", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let principals: Vec<String> = rig
        .fake
        .sets()
        .iter()
        .map(|s| s.principal.clone())
        .collect();
    assert_eq!(principals, ["operator-key", "admin-key"]);
}

/// A replay of the same body (any member order) answers the first 202; the
/// same key with another body is 409 `conflict`; neither enqueues again.
#[tokio::test]
async fn a_set_key_replays_its_body_and_refuses_another() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let body = set(vec![mixer(4, "bars"), video(0, bars()), video(1, bars())]);
    let (status, first) = take(&rig, OPERATOR_TOKEN, "replay", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let reordered = set(vec![video(1, bars()), mixer(4, "bars"), video(0, bars())]);
    let (status, again) = take(&rig, OPERATOR_TOKEN, "replay", &reordered).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["operation_id"], first["operation_id"]);
    let other = set(vec![mixer(4, "bars"), video(0, bars()), video(2, bars())]);
    let (status, problem) = take(&rig, OPERATOR_TOKEN, "replay", &other).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["type"].as_str().unwrap().ends_with("conflict"),
        "{problem}"
    );
    assert_eq!(
        rig.fake.state.lock().unwrap().offered.len(),
        1,
        "enqueued once"
    );
    // A set key never replays a single-output take, and vice versa.
    let single = set(vec![video(0, bars())]);
    let (status, alone) = take(&rig, OPERATOR_TOKEN, "replay", &single).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{alone}");
    assert_ne!(alone["operation_id"], first["operation_id"]);
    assert_eq!(rig.fake.state.lock().unwrap().takes.len(), 1);
}

/// When the router's mailbox refuses the set after the switcher accepted
/// its armed half, the arm is refused (the switcher drops the half and
/// reports it refused), the key is released and the answer is 503.
#[tokio::test]
async fn a_failed_second_enqueue_refuses_the_arm_and_releases_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(&dir.path().join("boot.toml"));
    let mut outcomes = rig.handle.outcomes();
    rig.fake.state.lock().unwrap().busy = true;
    let body = set(vec![mixer(4, "bars"), video(0, bars())]);
    let (status, problem) = take(&rig, OPERATOR_TOKEN, "busy", &body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    let offered = rig.fake.state.lock().unwrap().offered.clone();
    let arm = offered[0].arm.clone().unwrap();
    assert_eq!(arm.state(), ArmState::Refused);
    assert_eq!(arm.reason(), Some(SetRefusal::Busy));
    assert_eq!(rig.state.patch_sets.records_len(), 0, "key released");
    assert_eq!(rig.state.patch_sets.pending_len(), 0);
    // The switcher drops its half on the next drain, never applying it.
    tick(&mut rig.driver, 1);
    let dropped = outcomes.try_recv().unwrap().event;
    assert!(!dropped.completed, "{dropped:?}");
    assert_eq!(dropped.operation_id, offered[0].operation_id);
    let input4 = rig
        .handle
        .inputs()
        .unwrap()
        .inputs
        .iter()
        .find(|i| i.number == 4)
        .and_then(|i| i.source.clone());
    assert_eq!(input4, None, "nothing applied");
    // The key is fresh again: a retry is admitted under a new operation.
    rig.fake.state.lock().unwrap().busy = false;
    let (status, body) = take(&rig, OPERATOR_TOKEN, "busy", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_ne!(
        body["operation_id"].as_str(),
        Some(offered[0].operation_id.as_str())
    );
}

/// Wait (bounded) until the aggregator task has settled every set: counts,
/// not timings.
async fn settled(rig: &Rig) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while rig.state.patch_sets.pending_len() > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn patch_takes_until_marker(
    stream: &mut SessionStream,
) -> Vec<multiview_events::PatchTakeEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut seen = Vec::new();
        loop {
            let Some(frame) = stream.next_delta().await.unwrap() else {
                continue;
            };
            match frame.envelope.payload {
                Event::PatchTake(e) => seen.push(e),
                Event::RouteOutput(e) if e.operation_id == "marker" => return seen,
                _ => {}
            }
        }
    })
    .await
    .unwrap()
}

fn route(op: &str, output: &str, tick: u64, outcome: &str) -> Event {
    Event::RouteOutput(multiview_events::RouteOutputEvent {
        output: output.into(),
        operation_id: op.into(),
        tick,
        outcome: outcome.into(),
    })
}

fn switcher(op: &str, tick: u64, error: Option<&str>) -> Event {
    Event::SwitcherOperation(multiview_events::SwitcherOperation {
        me: "me1".into(),
        program: "main".into(),
        tick,
        operation_id: op.into(),
        completed: error.is_none(),
        error: error.map(str::to_owned),
    })
}

/// `patch.take` is published once per destination when every member is
/// terminal: `applied` when all applied at one tick, else `refused` with
/// the first cause; a principal receives only its own destinations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_take_aggregates_and_is_delivered_per_destination() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let mut whole = SessionStream::new(rig.engine.subscribe(), "w", None);
    let mut scoped = SessionStream::new(rig.engine.subscribe(), "s", None).with_scopes(
        Some(vec!["output-0".to_owned()]),
        Some(vec!["output-0".to_owned()]),
        None,
    );
    let body = set(vec![mixer(4, "bars"), video(0, bars()), video(1, bars())]);
    let (status, accepted) = take(&rig, OPERATOR_TOKEN, "agg", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let op = accepted["operation_id"].as_str().unwrap().to_owned();
    // Every member applied at tick 7.
    rig.engine
        .publish_event(route(&op, "output-0", 7, "applied"));
    rig.engine.publish_event(switcher(&op, 7, None));
    rig.engine
        .publish_event(route(&op, "output-1", 7, "applied"));
    // A refused set: the first cause wins.
    let refused_body = set(vec![video(0, bars()), video(1, bars())]);
    let (status, refused) = take(&rig, ADMIN_TOKEN, "agg2", &refused_body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{refused}");
    let op2 = refused["operation_id"].as_str().unwrap().to_owned();
    rig.engine
        .publish_event(route(&op2, "output-1", 9, "refused:unroutable"));
    rig.engine
        .publish_event(route(&op2, "output-0", 9, "refused:feed-not-fresh"));
    settled(&rig).await;
    rig.engine
        .publish_event(route("marker", "output-0", 10, "applied"));

    let all = patch_takes_until_marker(&mut whole).await;
    let first: Vec<_> = all.iter().filter(|e| e.operation_id == op).collect();
    assert_eq!(first.len(), 3, "one per destination: {all:?}");
    assert!(first
        .iter()
        .all(|e| e.outcome == "applied" && e.tick == 7 && e.refusal.is_none()));
    assert!(first
        .iter()
        .any(|e| e.destination.mixer.as_deref() == Some("me1")));
    let second: Vec<_> = all.iter().filter(|e| e.operation_id == op2).collect();
    assert_eq!(second.len(), 2, "{all:?}");
    assert!(second
        .iter()
        .all(|e| e.outcome == "refused" && e.refusal.as_deref() == Some("slot-medium-mismatch")));

    let mine = patch_takes_until_marker(&mut scoped).await;
    assert_eq!(mine.len(), 2, "only output-0's deliveries: {mine:?}");
    assert!(mine
        .iter()
        .all(|e| e.destination.output.as_deref() == Some("output-0")));
    assert_eq!(rig.state.patch_sets.pending_len(), 0, "settled");
}

/// A set without a final outcome is published `refused` / `deadline`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_without_an_outcome_is_refused_at_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    let mut whole = SessionStream::new(rig.engine.subscribe(), "w", None);
    let (status, accepted) = take(
        &rig,
        OPERATOR_TOKEN,
        "late",
        &set(vec![video(0, bars()), video(1, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    rig.engine.publish_event(route(
        accepted["operation_id"].as_str().unwrap(),
        "output-0",
        3,
        "applied",
    ));
    assert_eq!(
        multiview_control::patch_set::expire_sets(&rig.state, std::time::Instant::now()),
        0,
        "not yet"
    );
    let later = std::time::Instant::now() + multiview_control::patch_set::SET_OUTCOME_DEADLINE;
    assert_eq!(
        multiview_control::patch_set::expire_sets(&rig.state, later),
        2
    );
    rig.engine
        .publish_event(route("marker", "output-0", 10, "applied"));
    let all = patch_takes_until_marker(&mut whole).await;
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all.iter().all(|e| e.refusal.as_deref() == Some("deadline")));
}

fn put_salvo(token: &str, id: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/api/v1/salvos/{id}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

/// A patch salvo: PUT validates and authorizes every member (a principal
/// cannot store what it could not take), `plan` returns the patch plan and
/// `take` runs the set path in its own key namespace.
#[tokio::test]
async fn a_patch_salvo_is_authorized_at_store_time_and_taken_as_a_set() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(&dir.path().join("boot.toml"));
    let patch = vec![
        mixer(4, "bars"),
        video(0, bars()),
        video(1, bars()),
        follow(1),
    ];
    let salvo = json!({ "id": "news", "patch": patch });
    // A member the caller may not take (output-2) is 403, nothing stored.
    let denied = json!({ "id": "news", "patch": [video(0, bars()), video(2, bars())] });
    let resp = send(&rig.router, put_salvo(LIMITED, "news", &denied)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Unknown outputs, sources and mixer inputs, duplicates and > 64 are 422.
    for bad in [
        json!({ "id": "news", "patch": [video(0, bars()), video(99, bars())] }),
        json!({ "id": "news", "patch": [video(0, json!({"source": "ghost"})), video(1, bars())] }),
        json!({ "id": "news", "patch": [mixer(9, "bars"), video(1, bars())] }),
        json!({ "id": "news", "patch": [video(0, bars()), video(0, bars())] }),
        json!({ "id": "news", "patch": (0..65).map(|i| video(i % OUTPUTS, bars())).collect::<Vec<_>>() }),
    ] {
        let resp = send(&rig.router, put_salvo(OPERATOR_TOKEN, "news", &bad)).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let resp = send(&rig.router, get_salvo("news")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "nothing was stored");
    let resp = send(&rig.router, put_salvo(LIMITED, "news", &salvo)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let stored = body_json(resp).await;
    assert_eq!(stored["patch"].as_array().unwrap().len(), 4, "{stored}");
    assert_eq!(
        stored["patch"][3]["source"], "follow",
        "the patch API's JSON shape"
    );

    let resp = send(
        &rig.router,
        post("/api/v1/salvos/news/plan", OPERATOR_TOKEN, None, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;
    assert_eq!(plan["crosspoints"].as_array().unwrap().len(), 4, "{plan}");
    assert!(rig.fake.sets().is_empty());

    let resp = send(
        &rig.router,
        post(
            "/api/v1/salvos/news/take",
            OPERATOR_TOKEN,
            Some("s1"),
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let first = body_json(resp).await;
    assert_eq!(first["status"], "accepted");
    assert!(
        first.get("recall_operation_id").is_none(),
        "no legacy recall"
    );
    let sets = rig.fake.sets();
    assert_eq!(sets.len(), 1);
    assert_eq!(
        sets[0].operation_id,
        first["operation_id"].as_str().unwrap()
    );
    assert_eq!(sets[0].members.len(), 2);
    let arm = sets[0].arm.clone().unwrap();
    tick(&mut rig.driver, 1);
    assert_eq!(arm.state(), ArmState::SwitcherReady);
    // A replay answers the first operation; the same raw key on
    // `/patch/take` is another namespace.
    let resp = send(
        &rig.router,
        post(
            "/api/v1/salvos/news/take",
            OPERATOR_TOKEN,
            Some("s1"),
            &json!({}),
        ),
    )
    .await;
    assert_eq!(body_json(resp).await["operation_id"], first["operation_id"]);
    assert_eq!(rig.fake.sets().len(), 1, "not enqueued again");
    // A principal that lost a member's grant cannot take the stored salvo.
    let resp = send(
        &rig.router,
        post("/api/v1/salvos/news/take", LIMITED, Some("s2"), &json!({})),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "LIMITED may take what it stored"
    );
    let narrower = json!({ "id": "wide", "patch": [video(0, bars()), video(2, bars())] });
    let resp = send(&rig.router, put_salvo(OPERATOR_TOKEN, "wide", &narrower)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = send(
        &rig.router,
        post("/api/v1/salvos/wide/take", LIMITED, Some("s3"), &json!({})),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "re-authorized at take"
    );
}

fn get_salvo(id: &str) -> Request<Body> {
    support::get(&format!("/api/v1/salvos/{id}"), OPERATOR_TOKEN)
}

/// Chaos: 2,000 set takes from one principal in a loop. Almost all are 429
/// (the limiter, never the engine, sheds them); nothing else is answered;
/// the engine mailboxes never exceed their bounds (no 503); the set store
/// stays bounded; another principal's take still gets 202.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_flood_is_429_and_never_starves_another_principal() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Arc::new(rig(&dir.path().join("boot.toml")));
    let started = std::time::Instant::now();
    let flood = {
        let rig = Arc::clone(&rig);
        tokio::spawn(async move {
            let body = set(vec![video(0, bars()), video(1, bars()), follow(1)]);
            let (mut accepted, mut limited, mut other) = (0usize, 0usize, Vec::new());
            for i in 0..2000 {
                let (status, problem) =
                    take(&rig, OPERATOR_TOKEN, &format!("flood-{i}"), &body).await;
                match status {
                    StatusCode::ACCEPTED => accepted += 1,
                    StatusCode::TOO_MANY_REQUESTS => limited += 1,
                    _ => other.push((status, problem)),
                }
            }
            (accepted, limited, other)
        })
    };
    // Another principal, other destinations, mid-flood.
    let (status, body) = take(
        &rig,
        ADMIN_TOKEN,
        "other",
        &set(vec![video(2, bars()), video(3, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (accepted, limited, other) = flood.await.unwrap();
    let elapsed = started.elapsed();
    assert!(
        other.is_empty(),
        "every flood answer is 202 or 429: {other:?}"
    );
    assert_eq!(accepted + limited, 2000);
    // 2 per destination per sliding second.
    let seconds = usize::try_from(elapsed.as_secs()).unwrap() + 1;
    let allowed = 2 * (seconds + 1);
    assert!(accepted <= allowed, "{accepted} accepted in {elapsed:?}");
    assert!(limited >= 2000 - allowed);
    assert!(accepted >= 1, "the first set is admitted");
    let state = rig.fake.state.lock().unwrap();
    let flood_sets = state
        .sets
        .iter()
        .filter(|s| s.principal == "operator-key")
        .count();
    assert_eq!(flood_sets, accepted, "only admitted sets reach the router");
    assert!(
        state.sets.len() <= state.mailbox_cap,
        "the mailbox bound held"
    );
    assert!(state.sets.iter().any(|s| s.principal == "admin-key"));
    drop(state);
    assert!(rig.state.patch_sets.pending_len() <= multiview_control::patch_set::MAX_PENDING_SETS);
    let (outputs, principals) = rig.state.patch_limiter.tracked();
    assert!(
        outputs <= 8 && principals <= 2,
        "limiter state bounded: {outputs}/{principals}"
    );
}

/// The set's encoder delta dedupes by the engine's COMPLETE sharing
/// identity, not (feed, format) alone: two outputs on the same feed and
/// format but different encoder settings need two encoders. A principal
/// with one session left is told `fits: false` and the take is 409
/// `capacity` with nothing enqueued and no key burned; the same feed on two
/// outputs with equal settings shares one encoder and is admitted.
#[tokio::test]
async fn different_encoder_settings_never_share_one_new_encoder() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));
    rig.fake
        .state
        .lock()
        .unwrap()
        .held
        .insert("operator-key".to_owned(), 1);
    let body = set(vec![video(1, bars()), video(2, bars())]);
    let resp = send(
        &rig.router,
        post("/api/v1/patch/plan", OPERATOR_TOKEN, None, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;
    assert_eq!(plan["set"]["new_encoders"], 2, "{plan}");
    assert_eq!(plan["set"]["fits"], false, "{plan}");
    assert_eq!(plan["can_take"], false, "{plan}");
    let (status, problem) = take(&rig, OPERATOR_TOKEN, "settings", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["type"].as_str().unwrap().ends_with("capacity"),
        "{problem}"
    );
    assert!(
        rig.fake.state.lock().unwrap().offered.is_empty(),
        "nothing enqueued"
    );
    assert_eq!(rig.state.patch_sets.records_len(), 0, "no key burned");
    assert_eq!(rig.state.patch_sets.pending_len(), 0);
    // Equal settings (output-0 and output-1): one shared encoder, fits.
    let shared = set(vec![video(0, bars()), video(1, bars())]);
    let resp = send(
        &rig.router,
        post("/api/v1/patch/plan", OPERATOR_TOKEN, None, &shared),
    )
    .await;
    let plan = body_json(resp).await;
    assert_eq!(plan["set"]["new_encoders"], 1, "{plan}");
    assert_eq!(plan["set"]["fits"], true, "{plan}");
    let (status, accepted) = take(&rig, OPERATOR_TOKEN, "shared", &shared).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
}

/// #308 review A7: a set's limiter charge is refunded on EVERY unsuccessful
/// exit after admission — 409 `capacity` (the encoder delta does not fit),
/// 503 `engine-busy` (the router's own mailbox refuses the enqueue after
/// every control-plane gate passed) — and a replay of an earlier 202 never
/// charges the limiter at all (it answers before `TakeLimiter::admit_across`
/// runs). None of them narrow output-0's or the principal's per-second
/// budget: two 409s and one 503, all touching output-0, still leave BOTH of
/// output-0's `TAKES_PER_OUTPUT_PER_SEC = 2` slots free for two fresh
/// single-output takes in the same second.
#[tokio::test]
async fn refused_and_replayed_sets_refund_their_limiter_charge() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(&dir.path().join("boot.toml"));

    // The principal already holds its encoder cap (2 of 2): a set needing
    // ANY new encoder is refused 409 `capacity` AFTER `admit_across` has
    // already charged output-0 and output-1 (and the principal window).
    rig.fake
        .state
        .lock()
        .unwrap()
        .held
        .insert("operator-key".to_owned(), 2);
    let capacity_body = set(vec![video(0, bars()), video(1, json!({"source": "vt1"}))]);
    for i in 0..2 {
        let (status, problem) = take(
            &rig,
            OPERATOR_TOKEN,
            &format!("refund-cap-{i}"),
            &capacity_body,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{problem}");
        assert!(
            problem["type"].as_str().unwrap().ends_with("capacity"),
            "{problem}"
        );
    }
    assert!(
        rig.fake.state.lock().unwrap().offered.is_empty(),
        "a 409 capacity set never reaches the engine"
    );

    // Clear the cap, then make the router's OWN mailbox refuse: a set that
    // passes every control-plane gate (including the encoder delta) but
    // whose `try_take_set` the router refuses (503 `engine-busy`) must ALSO
    // refund — it reached the engine's door but never landed.
    rig.fake.state.lock().unwrap().held.remove("operator-key");
    rig.fake.state.lock().unwrap().busy = true;
    let busy_body = set(vec![video(0, bars()), video(5, bars())]);
    let (status, problem) = take(&rig, OPERATOR_TOKEN, "refund-busy", &busy_body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    rig.fake.state.lock().unwrap().busy = false;
    assert_eq!(
        rig.state.patch_sets.records_len(),
        0,
        "the busy set's key was released, nothing left charged"
    );

    // An admitted set on DIFFERENT outputs (not output-0), then a replay of
    // its own key: the replay answers from the store before `admit_across`
    // runs at all, so it never re-charges any limiter either.
    let admitted_body = set(vec![video(7, bars()), video(8, bars())]);
    let (status, first) = take(&rig, OPERATOR_TOKEN, "refund-replay", &admitted_body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, replay) = take(&rig, OPERATOR_TOKEN, "refund-replay", &admitted_body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replay}");
    assert_eq!(replay["operation_id"], first["operation_id"], "a replay");

    // Despite two 409s and one 503 all touching output-0 (three charges
    // that would have exhausted and overrun its 2/s window had any of them
    // leaked), output-0 still admits two FRESH single-output takes in the
    // same second — proving every one of those charges was given back.
    for i in 0..2 {
        let (status, body) = take(
            &rig,
            OPERATOR_TOKEN,
            &format!("refund-single-{i}"),
            &set(vec![video(0, bars())]),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "single take {i}: {body}");
    }
    // Both of output-0's slots are now genuinely spent (not still open from
    // an under-counted refund): a third touch in the same second is 429.
    let (status, body) = take(
        &rig,
        OPERATOR_TOKEN,
        "refund-single-overflow",
        &set(vec![video(0, bars())]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}
