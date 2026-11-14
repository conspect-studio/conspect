//! A stored layout as an M/E source (#307 PR 2): `layout` on
//! `select_preview`/`select_program`, `{"layout": id}` on a mixer input, the
//! source-closure authorization (select, patch, CUT/AUTO of a layout on
//! Preview), admissibility (nested layout/bus, translucent, foreign canvas,
//! unknown), the registry cap (409 `capacity`), plan registers nothing, the
//! edit and delete guards, and the 10 s linger sweep.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::auth::{ApiKeyStore, Principal, Role};
use multiview_control::me_layouts::{
    MeLayoutRefusal, MeLayoutRegistry, MeLayoutSources, MAX_ME_LAYOUT_SOURCES, ME_LAYOUT_LINGER,
};
use multiview_control::output_routing::LayoutBody;
use multiview_control::{command_bus, AppState, InMemoryRepository, LayoutInput};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherControl, SwitcherTickDriver,
};
use multiview_engine::switcher_inputs::MixerInputs;
use multiview_engine::EnginePublisher;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::{json, Value};
use support::{body_json, delete_if_match, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN};

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
id = "secret"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "/tmp/switcher-layout-source/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// me1 + Program scope + sources a, b + layouts wall, secret-wall (NOT the
/// source `secret`).
const LIMITED: &str = "me-limited.me-limited-secret";
/// Like LIMITED, but without the M/E object or the Program scope.
const NO_ME: &str = "me-none.me-none-secret";

fn keys() -> ApiKeyStore {
    let mut keys = support::seeded_keys();
    let scoped = |id: &str, objects: &[&str], program: bool| Principal {
        key_id: id.to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(objects.iter().map(|s| (*s).to_owned()).collect()),
        scoped_output_ids: Some(if program {
            vec!["program:main".to_owned()]
        } else {
            vec![]
        }),
        scoped_discovery_domains: None,
    };
    keys.register(
        "me-limited",
        "me-limited-secret",
        scoped(
            "me-limited",
            &["me1", "a", "b", "wall", "secret-wall"],
            true,
        ),
    );
    keys.register(
        "me-none",
        "me-none-secret",
        scoped("me-none", &["a", "b", "secret", "wall"], false),
    );
    keys
}

/// The engine side, recorded: the registered bodies by id.
#[derive(Default)]
struct FakeSources {
    bodies: Mutex<BTreeMap<String, LayoutBody>>,
}

impl MeLayoutSources for FakeSources {
    fn register(&self, body: LayoutBody) -> Result<(), MeLayoutRefusal> {
        let mut bodies = self.bodies.lock().unwrap();
        if !bodies.contains_key(&body.0.id) && bodies.len() >= MAX_ME_LAYOUT_SOURCES {
            return Err(MeLayoutRefusal::Capacity);
        }
        bodies.insert(body.0.id.clone(), body);
        Ok(())
    }

    fn unregister(&self, id: &str) -> bool {
        self.bodies.lock().unwrap().remove(id).is_some()
    }

    fn registered(&self) -> Vec<String> {
        self.bodies.lock().unwrap().keys().cloned().collect()
    }
}

fn shared(sources: &Arc<FakeSources>) -> Arc<dyn MeLayoutSources> {
    let shared: Arc<dyn MeLayoutSources> = sources.clone();
    shared
}

fn frame() -> Nv12Image {
    Nv12Image::solid(
        64,
        64,
        128,
        128,
        128,
        ColorInfo::default().resolve_defaults(64, 64),
    )
    .unwrap()
}

fn store(id: &str) -> Arc<TileStore<Nv12Image>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
    store.publish(frame(), MediaTime::ZERO);
    store
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

fn layout_body(cells: &[(&str, f32)], width: u32) -> Value {
    let cells: Vec<Value> = cells
        .iter()
        .enumerate()
        .map(|(i, (source, opacity))| {
            json!({
                "id": format!("c{i}"),
                "rect": {"x": 0.0, "y": 0.0, "w": 0.5, "h": 0.5},
                "opacity": opacity,
                "source": {"input_id": source},
            })
        })
        .collect();
    json!({
        "canvas": {"width": width, "height": 64, "fps": "25/1"},
        "layout": {"kind": "absolute"},
        "cells": cells,
    })
}

struct Rig {
    router: axum::Router,
    state: AppState,
    handle: SwitcherControl,
    driver: SwitcherTickDriver,
    sources: Arc<FakeSources>,
    next_tick: u64,
}

impl Rig {
    fn new(inputs: bool) -> Self {
        let config = MultiviewConfig::load_from_toml(DOC).unwrap();
        let seeded = multiview_control::seed_resources(&config).unwrap();
        let (sender, _receiver) = command_bus(64);
        let (program, preview) = if inputs {
            (SourceRef::input(1), SourceRef::input(2))
        } else {
            (SourceRef::new("a"), SourceRef::new("b"))
        };
        let (handle, mut driver) =
            SwitcherControl::new(SwitcherState::new(program, preview, vec![]));
        driver.set_prime_stores(
            ["a", "b", "secret"]
                .into_iter()
                .map(|id| (id.to_owned(), store(id)))
                .collect(),
        );
        if inputs {
            driver.set_inputs(
                MixerInputs::new([
                    (1, None, Some(SourceRef::new("a"))),
                    (2, None, Some(SourceRef::new("b"))),
                    (3, None, None),
                    (4, None, None),
                ])
                .unwrap(),
            );
        }
        handle
            .set_scenes(ScenePair {
                version: 1,
                program: scene("a"),
                preview: scene("b"),
            })
            .unwrap();
        // The real frame hook advertises AUTO; this rig drives the driver.
        driver.advertise_auto();
        driver.publish_provider_status(ProviderStatus {
            scene_version: 1,
            last_tick: 0,
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        });
        let sources = Arc::new(FakeSources::default());
        let repository = Arc::new(InMemoryRepository::new());
        let state = AppState::new(
            Arc::new(EnginePublisher::new(256)),
            sender,
            repository,
            Arc::new(keys()),
        )
        .with_seeded_resources(seeded)
        .with_base_document(serde_json::to_value(&config).unwrap())
        .with_switcher(handle.clone())
        .with_me_layouts(shared(&sources));
        for (id, body) in [
            ("wall", layout_body(&[("a", 1.0), ("b", 1.0)], 64)),
            (
                "secret-wall",
                layout_body(&[("a", 1.0), ("secret", 1.0)], 64),
            ),
            (
                "nested",
                layout_body(&[("a", 1.0), ("layout:wall", 1.0)], 64),
            ),
            ("buses", layout_body(&[("bus:program", 1.0)], 64)),
            ("glass", layout_body(&[("a", 0.5)], 64)),
            ("wide", layout_body(&[("a", 1.0)], 128)),
            ("w1", layout_body(&[("a", 1.0)], 64)),
            ("w2", layout_body(&[("a", 1.0)], 64)),
            ("w3", layout_body(&[("a", 1.0)], 64)),
            ("w4", layout_body(&[("a", 1.0)], 64)),
        ] {
            state
                .repository
                .create_layout(
                    id,
                    LayoutInput {
                        name: id.to_owned(),
                        body,
                    },
                )
                .unwrap();
        }
        let mut rig = Self {
            router: multiview_control::router(state.clone()),
            state,
            handle,
            driver,
            sources,
            next_tick: 0,
        };
        rig.tick();
        rig
    }

    /// One engine tick, with every registered layout's store primed (what
    /// the frame hook's adoption plus the aux compose provide in a run).
    fn tick(&mut self) {
        let registered: Vec<(String, u64)> = self
            .sources
            .bodies
            .lock()
            .unwrap()
            .iter()
            .map(|(id, body)| (id.clone(), body.0.revision))
            .collect();
        for (id, revision) in registered {
            let key = format!("layout:{id}");
            self.driver.insert_prime_store(&key, store(&key), revision);
        }
        let tick = Tick {
            index: self.next_tick,
            pts: MediaTime::ZERO,
        };
        self.next_tick += 1;
        self.driver.drain_at_tick(tick);
        self.driver.commit_composed_tick(tick);
    }

    fn registered(&self) -> Vec<String> {
        self.state.me_layouts.as_ref().unwrap().ids()
    }
}

fn keyed(mut request: Request<Body>, key: &str) -> Request<Body> {
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    request
}

fn select(action: &str, layout: &str) -> Value {
    json!({ "me": "me1", "program": "main", "action": action, "layout": layout })
}

async fn submit(rig: &Rig, token: &str, key: &str, body: Value) -> axum::response::Response {
    let action = body["action"].as_str().unwrap().to_owned();
    send(
        &rig.router,
        keyed(
            post_json(&format!("/api/v1/switcher/commands/{action}"), token, &body),
            key,
        ),
    )
    .await
}

async fn problem_type(response: axum::response::Response) -> String {
    let body = body_json(response).await;
    body["type"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn a_layout_select_registers_it_and_puts_it_on_preview() {
    let mut rig = Rig::new(false);
    // A plan resolves it but registers nothing.
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/switcher/plan",
            OPERATOR_TOKEN,
            &select("select_preview", "wall"),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(rig.registered().is_empty(), "plan registers nothing");
    let response = submit(&rig, OPERATOR_TOKEN, "k1", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(rig.registered(), vec!["wall".to_owned()]);
    assert_eq!(rig.sources.registered(), vec!["wall".to_owned()]);
    rig.tick();
    assert_eq!(
        rig.handle.snapshot().unwrap().preview.as_str(),
        "layout:wall"
    );
    // `source: "layout:wall"` is the same request.
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "k2",
        json!({"me": "me1", "program": "main", "action": "select_program", "source": "layout:wall"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    assert_eq!(
        rig.handle.snapshot().unwrap().program.as_str(),
        "layout:wall"
    );
    // Exactly one of source/input/layout; layout on CUT is refused.
    let both = json!({"me": "me1", "program": "main", "action": "select_preview", "layout": "wall", "source": "a"});
    assert_eq!(
        submit(&rig, OPERATOR_TOKEN, "k3", both).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let cut = json!({"me": "me1", "program": "main", "action": "cut", "layout": "wall"});
    assert_eq!(
        submit(&rig, OPERATOR_TOKEN, "k4", cut).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn the_source_closure_is_authorized_and_a_refusal_burns_no_key() {
    let rig = Rig::new(false);
    // Read on the layout, not on every source it binds: one generic 403.
    for path in [
        "/api/v1/switcher/plan",
        "/api/v1/switcher/commands/select_preview",
    ] {
        let response = send(
            &rig.router,
            keyed(
                post_json(path, LIMITED, &select("select_preview", "secret-wall")),
                "kc",
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert!(rig.registered().is_empty(), "nothing registered");
    // The key is still fresh for the principal.
    let response = submit(&rig, LIMITED, "kc", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // Without the M/E or Program scope: 403 before anything.
    let response = submit(&rig, NO_ME, "kn", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn inadmissible_layouts_are_typed_422s() {
    let rig = Rig::new(false);
    for (id, kind, why) in [
        (
            "nested",
            "layout-not-admissible",
            "nested layouts and buses",
        ),
        ("buses", "layout-not-admissible", "nested layouts and buses"),
        ("glass", "layout-not-admissible", "translucent"),
        ("wide", "layout-not-admissible", "canvas"),
        ("ghost", "unknown-source", "does not exist"),
    ] {
        let response = submit(&rig, OPERATOR_TOKEN, id, select("select_preview", id)).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{id}");
        let problem = body_json(response).await;
        assert!(
            problem["type"].as_str().unwrap().ends_with(kind),
            "{id}: {problem}"
        );
        assert!(
            problem["detail"].as_str().unwrap().contains(why),
            "{id}: {problem}"
        );
    }
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "nested2",
        select("select_preview", "nested"),
    )
    .await;
    let detail = body_json(response).await["detail"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(detail.contains("nested layouts and buses"), "{detail}");
    assert_eq!(rig.registered(), Vec::<String>::new());
}

#[tokio::test]
async fn a_fifth_distinct_layout_is_refused_with_capacity() {
    let rig = Rig::new(false);
    for id in ["w1", "w2", "w3", "w4"] {
        let response = submit(&rig, OPERATOR_TOKEN, id, select("select_preview", id)).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{id}");
    }
    let response = submit(&rig, OPERATOR_TOKEN, "w5", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(problem_type(response).await.ends_with("capacity"));
    assert_eq!(rig.registered().len(), MAX_ME_LAYOUT_SOURCES);
    // The refused key was released: the same key is fresh again.
    assert!(rig.state.me_layouts.as_ref().unwrap().remove("w1"));
    let response = submit(&rig, OPERATOR_TOKEN, "w5", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn cut_of_a_layout_on_preview_needs_its_closure() {
    let mut rig = Rig::new(false);
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "pv",
        select("select_preview", "secret-wall"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    assert_eq!(
        rig.handle.snapshot().unwrap().preview.as_str(),
        "layout:secret-wall"
    );
    let cut = json!({"me": "me1", "program": "main", "action": "cut"});
    let response = submit(&rig, LIMITED, "cut", cut.clone()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let auto = json!({"me": "me1", "program": "main", "action": "auto", "duration_frames": 5});
    let response = send(
        &rig.router,
        post_json("/api/v1/switcher/plan", LIMITED, &auto),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A caller with the whole closure takes it.
    let response = submit(&rig, OPERATOR_TOKEN, "cut", cut).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    assert_eq!(
        rig.handle.snapshot().unwrap().program.as_str(),
        "layout:secret-wall"
    );
}

fn patch_xp(input: u16, source: Value) -> Value {
    json!({ "crosspoints": [
        { "plane": "video", "destination": {"mixer": "me1", "input": input}, "source": source }
    ], "on_air": "protect" })
}

#[tokio::test]
async fn a_layout_patches_to_a_mixer_input_and_a_bus_does_not() {
    let mut rig = Rig::new(true);
    // Not on an input yet: a layout select is refused like a source.
    let response = submit(&rig, OPERATOR_TOKEN, "s0", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(problem_type(response).await.ends_with("not-on-an-input"));
    assert_eq!(rig.registered(), Vec::<String>::new());
    // Plan: planned, nothing registered.
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/patch/plan",
            OPERATOR_TOKEN,
            &patch_xp(3, json!({"layout": "wall"})),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["can_take"], true);
    assert_eq!(rig.registered(), Vec::<String>::new());
    // Take: registered, enqueued, applied.
    let response = send(
        &rig.router,
        keyed(
            post_json(
                "/api/v1/patch/take",
                OPERATOR_TOKEN,
                &patch_xp(3, json!({"layout": "wall"})),
            ),
            "p1",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(rig.registered(), vec!["wall".to_owned()]);
    rig.tick();
    let table = rig.handle.inputs().unwrap();
    let input = table.inputs.iter().find(|i| i.number == 3).unwrap();
    assert_eq!(input.source.as_ref().unwrap().as_str(), "layout:wall");
    // Now a select by layout resolves to input 3.
    let response = submit(&rig, OPERATOR_TOKEN, "s1", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    assert_eq!(rig.handle.snapshot().unwrap().preview_input, Some(3));
    // A bus on a mixer input stays refused; the closure applies to a patch.
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/patch/plan",
            OPERATOR_TOKEN,
            &patch_xp(4, json!({"bus": "program"})),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(problem_type(response).await.ends_with("unknown-source"));
    let response = send(
        &rig.router,
        keyed(
            post_json(
                "/api/v1/patch/take",
                LIMITED,
                &patch_xp(4, json!({"layout": "secret-wall"})),
            ),
            "p2",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An inadmissible layout on an input is a typed 422.
    let response = send(
        &rig.router,
        post_json(
            "/api/v1/patch/plan",
            OPERATOR_TOKEN,
            &patch_xp(4, json!({"layout": "nested"})),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(problem_type(response)
        .await
        .ends_with("layout-not-admissible"));
    assert_eq!(rig.registered(), vec!["wall".to_owned()]);
}

#[tokio::test]
async fn editing_a_registered_layout_needs_its_new_closure_and_stays_admissible() {
    let rig = Rig::new(false);
    let response = submit(&rig, OPERATOR_TOKEN, "e0", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let edit = |body: Value| json!({ "name": "wall", "body": body });
    // The new body shows `secret`, which LIMITED may not read.
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/layouts/wall",
            LIMITED,
            Some("W/\"1\""),
            &edit(layout_body(&[("a", 1.0), ("secret", 1.0)], 64)),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A nested layout is refused before the commit.
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/layouts/wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &edit(layout_body(&[("layout:w1", 1.0)], 64)),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(problem_type(response)
        .await
        .ends_with("layout-not-admissible"));
    // An admissible edit commits and swaps the registered body.
    let before = rig.sources.bodies.lock().unwrap()["wall"].0.revision;
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/layouts/wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &edit(layout_body(&[("b", 1.0)], 64)),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bodies = rig.sources.bodies.lock().unwrap();
    assert!(bodies["wall"].0.revision > before, "body swapped");
    assert_eq!(bodies["wall"].0.layout.cells.len(), 1);
}

#[tokio::test]
async fn deleting_a_layout_on_the_me_is_refused() {
    let mut rig = Rig::new(false);
    let response = submit(&rig, OPERATOR_TOKEN, "d0", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    let response = send(
        &rig.router,
        delete_if_match("/api/v1/layouts/wall", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(problem_type(response).await.ends_with("layout-on-air"));
    assert_eq!(rig.registered(), vec!["wall".to_owned()]);
    // Off the M/E but still registered (lingering): still refused (#307
    // review A3); once the linger sweep released it, the delete commits.
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "d1",
        json!({"me": "me1", "program": "main", "action": "select_preview", "source": "b"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    let response = send(
        &rig.router,
        delete_if_match("/api/v1/layouts/wall", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(rig.registered(), vec!["wall".to_owned()]);
    let swept = rig
        .state
        .me_layouts
        .as_ref()
        .unwrap()
        .sweep(|_| false, Instant::now() + ME_LAYOUT_LINGER);
    assert_eq!(swept, 1);
    let response = send(
        &rig.router,
        delete_if_match("/api/v1/layouts/wall", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(rig.registered(), Vec::<String>::new());
    assert_eq!(rig.sources.registered(), Vec::<String>::new());
}

fn routed(id: &str) -> LayoutBody {
    LayoutBody(Arc::new(multiview_control::output_routing::RoutedLayout {
        id: id.to_owned(),
        layout: (*scene("a")).clone(),
        slates: vec![multiview_config::FailoverSlate::Black],
        audio: multiview_config::LayoutAudio::default(),
        overlays: multiview_config::LayoutOverlays::none(),
        revision: 1,
    }))
}

#[test]
fn the_sweep_drops_an_unreferenced_entry_after_the_linger() {
    let sources = Arc::new(FakeSources::default());
    let registry = MeLayoutRegistry::new(shared(&sources));
    let t0 = Instant::now();
    registry.ensure(routed("on"), None, t0).unwrap();
    registry.ensure(routed("off"), None, t0).unwrap();
    let on_air = |id: &str| id == "on";
    let just_short = t0 + ME_LAYOUT_LINGER.saturating_sub(Duration::from_millis(1));
    assert_eq!(registry.sweep(on_air, just_short), 0, "not yet");
    assert_eq!(registry.sweep(on_air, t0 + ME_LAYOUT_LINGER), 1);
    assert_eq!(registry.ids(), vec!["on".to_owned()]);
    assert_eq!(sources.registered(), vec!["on".to_owned()]);
    // A referenced entry stays however long it is on air.
    assert_eq!(registry.sweep(on_air, t0 + ME_LAYOUT_LINGER * 6), 0);
    // Once released it lingers from its last reference.
    let released = t0 + ME_LAYOUT_LINGER * 6;
    assert_eq!(
        registry.sweep(|_| false, released + Duration::from_secs(9)),
        0
    );
    assert_eq!(registry.sweep(|_| false, released + ME_LAYOUT_LINGER), 1);
    assert_eq!(registry.ids(), Vec::<String>::new());
}

fn routed_rev(id: &str, revision: u64) -> LayoutBody {
    LayoutBody(Arc::new(multiview_control::output_routing::RoutedLayout {
        revision,
        ..(*routed(id).0).clone()
    }))
}

/// Live demand leases (acquired minus released), for lease ownership.
#[derive(Default)]
struct CountingDemand {
    live: Arc<std::sync::atomic::AtomicUsize>,
}

struct CountedLease(Arc<std::sync::atomic::AtomicUsize>);

impl multiview_control::SourceDemandLease for CountedLease {}

impl Drop for CountedLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl multiview_control::SourceDemandProvider for CountingDemand {
    fn status(&self, id: &str) -> multiview_control::SourceDemandView {
        multiview_control::SourceDemandView::idle(id)
    }
    fn active(&self) -> Vec<multiview_control::SourceDemandView> {
        Vec::new()
    }
    fn preview_lease(
        &self,
        _id: &str,
        _label: &str,
    ) -> Option<Box<dyn multiview_control::SourceDemandLease>> {
        self.live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(Box::new(CountedLease(Arc::clone(&self.live))))
    }
    fn touch_preview(&self, _id: &str) -> Option<multiview_control::SourceDemandPhase> {
        None
    }
}

/// An engine side whose `unregister` parks on a barrier pair once armed:
/// `entered` (the removal is inside the adapter) then `release`.
struct BlockingSources {
    inner: FakeSources,
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

impl BlockingSources {
    fn new() -> Self {
        Self {
            inner: FakeSources::default(),
            armed: std::sync::atomic::AtomicBool::new(false),
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        }
    }
}

impl MeLayoutSources for BlockingSources {
    fn register(&self, body: LayoutBody) -> Result<(), MeLayoutRefusal> {
        self.inner.register(body)
    }
    fn unregister(&self, id: &str) -> bool {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.wait();
            self.release.wait();
        }
        self.inner.unregister(id)
    }
    fn registered(&self) -> Vec<String> {
        self.inner.registered()
    }
}

/// #307 review A2/M1/I3: a removal (sweep or remove) blocked inside the
/// engine unregister cannot interleave with an `ensure` of the same id; the
/// fresh registration lands after it, stays registered on both sides and
/// owns exactly its own leases.
fn removal_cannot_unregister_a_fresh_registration(sweep: bool) {
    let sources = Arc::new(BlockingSources::new());
    let engine: Arc<dyn MeLayoutSources> = sources.clone();
    let registry = Arc::new(MeLayoutRegistry::new(engine));
    let counting = CountingDemand::default();
    let live = Arc::clone(&counting.live);
    let demand: multiview_control::SharedSourceDemand = Arc::new(counting);
    let t0 = Instant::now();
    registry
        .ensure(routed_rev("x", 10), Some(&demand), t0)
        .unwrap();
    assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 1);
    sources
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let remover = {
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || {
            if sweep {
                assert_eq!(registry.sweep(|_| false, t0 + ME_LAYOUT_LINGER), 1);
            } else {
                assert!(registry.remove("x"));
            }
        })
    };
    // The removal is now parked inside the adapter's unregister.
    sources.entered.wait();
    let ensured = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second = {
        let registry = Arc::clone(&registry);
        let demand = Arc::clone(&demand);
        let ensured = Arc::clone(&ensured);
        std::thread::spawn(move || {
            registry
                .ensure(routed_rev("x", 20), Some(&demand), Instant::now())
                .unwrap();
            ensured.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !ensured.load(std::sync::atomic::Ordering::SeqCst),
        "ensure interleaved with a removal in progress"
    );
    sources.release.wait();
    remover.join().unwrap();
    second.join().unwrap();
    assert_eq!(registry.ids(), vec!["x".to_owned()]);
    assert_eq!(sources.registered(), vec!["x".to_owned()]);
    assert_eq!(sources.inner.bodies.lock().unwrap()["x"].0.revision, 20);
    // The old entry's lease was released; the fresh one holds exactly its own.
    assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(registry.remove("x"));
    assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(sources.registered(), Vec::<String>::new());
}

#[test]
fn a_sweep_cannot_unregister_a_concurrent_fresh_registration() {
    removal_cannot_unregister_a_fresh_registration(true);
}

#[test]
fn a_remove_cannot_unregister_a_concurrent_fresh_registration() {
    removal_cannot_unregister_a_fresh_registration(false);
}

/// #307 review A3/M3: a body read before a committed edit is refused even
/// when the layout was not registered (first registration), and a body read
/// after the commit registers.
#[tokio::test]
async fn an_old_read_is_refused_after_an_edit_commits() {
    let rig = Rig::new(false);
    let registry = Arc::clone(rig.state.me_layouts.as_ref().unwrap());
    let old = routed_rev(
        "wall",
        multiview_control::output_routing::next_layout_revision(),
    );
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/layouts/wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "wall", "body": layout_body(&[("b", 1.0)], 64) }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(registry.fence("wall") > old.0.revision);
    assert_eq!(
        registry.ensure(old, None, Instant::now()),
        Err(multiview_control::me_layouts::MeLayoutError::Changed)
    );
    assert!(
        rig.sources.registered().is_empty(),
        "the old body never registers"
    );
    let fresh = routed_rev(
        "wall",
        multiview_control::output_routing::next_layout_revision(),
    );
    registry.ensure(fresh, None, Instant::now()).unwrap();
    assert_eq!(rig.sources.registered(), vec!["wall".to_owned()]);
}

/// While an edit is committing (its fence guard alive), even a newer read
/// is refused; once it completes only a read after it registers.
#[test]
fn a_committing_edit_refuses_every_registration() {
    let sources = Arc::new(FakeSources::default());
    let registry = MeLayoutRegistry::new(shared(&sources));
    let fence = registry.begin_edit("x");
    assert!(!fence.registered());
    let during = routed_rev(
        "x",
        multiview_control::output_routing::next_layout_revision(),
    );
    assert_eq!(
        registry.ensure(during.clone(), None, Instant::now()),
        Err(multiview_control::me_layouts::MeLayoutError::Changed)
    );
    drop(fence);
    assert_eq!(
        registry.ensure(during, None, Instant::now()),
        Err(multiview_control::me_layouts::MeLayoutError::Changed),
        "read during the commit stays refused"
    );
    let after = routed_rev(
        "x",
        multiview_control::output_routing::next_layout_revision(),
    );
    registry.ensure(after, None, Instant::now()).unwrap();
    assert_eq!(registry.ids(), vec!["x".to_owned()]);
}

/// #307 review A3: a body read before a delete never registers afterwards.
#[tokio::test]
async fn an_old_read_is_refused_after_a_delete() {
    let rig = Rig::new(false);
    let registry = Arc::clone(rig.state.me_layouts.as_ref().unwrap());
    let old = routed_rev(
        "w1",
        multiview_control::output_routing::next_layout_revision(),
    );
    let response = send(
        &rig.router,
        delete_if_match("/api/v1/layouts/w1", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        registry.ensure(old, None, Instant::now()),
        Err(multiview_control::me_layouts::MeLayoutError::Changed)
    );
    assert_eq!(rig.sources.registered(), Vec::<String>::new());
    // Through HTTP the layout is simply gone now.
    let response = submit(&rig, OPERATOR_TOKEN, "gone", select("select_preview", "w1")).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// #307 review A3: a registered layout cannot be deleted even when nothing
/// references it any more (it lingers): 409 `layout-on-air`, nothing changes.
#[tokio::test]
async fn deleting_a_registered_but_unreferenced_layout_is_refused() {
    let mut rig = Rig::new(false);
    let response = submit(&rig, OPERATOR_TOKEN, "r0", select("select_preview", "wall")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "r1",
        json!({"me": "me1", "program": "main", "action": "select_preview", "source": "b"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    assert_ne!(
        rig.handle.snapshot().unwrap().preview.as_str(),
        "layout:wall",
        "off the M/E"
    );
    let response = send(
        &rig.router,
        delete_if_match("/api/v1/layouts/wall", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem = body_json(response).await;
    assert!(problem["type"].as_str().unwrap().ends_with("layout-on-air"));
    assert!(problem["detail"]
        .as_str()
        .unwrap()
        .contains("retry after 10 s"));
    assert_eq!(rig.registered(), vec!["wall".to_owned()]);
    // Still in the library.
    let response = send(
        &rig.router,
        support::get("/api/v1/layouts/wall", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // After the linger sweep it deletes.
    let registry = rig.state.me_layouts.as_ref().unwrap();
    assert_eq!(
        registry.sweep(|_| false, Instant::now() + ME_LAYOUT_LINGER),
        1
    );
    let response = send(
        &rig.router,
        delete_if_match("/api/v1/layouts/wall", ADMIN_TOKEN, Some("W/\"1\"")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// The engine's outcome for operation `op` (from a subscription taken before
/// the request).
fn outcome_of(
    outcomes: &mut multiview_engine::EventSubscription<
        multiview_engine::switcher_control::SwitcherOutcome,
    >,
    op: &str,
) -> multiview_engine::switcher_control::SwitcherOutcome {
    while let Ok(event) = outcomes.try_recv() {
        if event.event.operation_id == op {
            return (*event.event).clone();
        }
    }
    panic!("no outcome for {op}");
}

async fn accepted_op(response: axum::response::Response) -> String {
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    body_json(response).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// #307 review A1: an unrestricted operator queues `secret-wall` on Preview;
/// a scoped operator (no Read on `secret`) CUTs before the tick. The CUT was
/// authorized for the old Preview, so the engine refuses it.
#[tokio::test]
async fn a_scoped_cut_cannot_take_a_preview_queued_after_its_authorization() {
    let mut rig = Rig::new(false);
    let mut outcomes = rig.handle.outcomes();
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "q1",
        select("select_preview", "secret-wall"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let cut = json!({"me": "me1", "program": "main", "action": "cut"});
    let op = accepted_op(submit(&rig, LIMITED, "q2", cut).await).await;
    rig.tick();
    let outcome = outcome_of(&mut outcomes, &op);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("selection-changed")),
        "{outcome:?}"
    );
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "a");
}

/// #307 review A1: a select of a numbered input authorized for its old patch
/// is refused when a re-patch queued before it lands first.
#[tokio::test]
async fn an_input_select_after_a_queued_repatch_is_refused() {
    let mut rig = Rig::new(true);
    let mut outcomes = rig.handle.outcomes();
    let patched = send(
        &rig.router,
        keyed(
            post_json(
                "/api/v1/patch/take",
                OPERATOR_TOKEN,
                &patch_xp(3, json!({"source": "b"})),
            ),
            "rp0",
        ),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::ACCEPTED);
    rig.tick();
    // Queue a re-patch of input 3 to `secret`, then (before the tick) a
    // scoped select of input 3, authorized against its patch `b`.
    let repatch = send(
        &rig.router,
        keyed(
            post_json(
                "/api/v1/patch/take",
                OPERATOR_TOKEN,
                &patch_xp(3, json!({"source": "secret"})),
            ),
            "rp1",
        ),
    )
    .await;
    assert_eq!(repatch.status(), StatusCode::ACCEPTED);
    let pgm = json!({"me": "me1", "program": "main", "action": "select_program", "input": 3});
    let op = accepted_op(submit(&rig, LIMITED, "rp2", pgm).await).await;
    rig.tick();
    let outcome = outcome_of(&mut outcomes, &op);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("selection-changed")),
        "{outcome:?}"
    );
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "a");
}

/// #307 review A1: a CUT of `layout:wall` authorized for its registered body
/// is refused when the body is edited (and adopted) before it executes.
#[tokio::test]
async fn a_cut_of_a_layout_edited_after_authorization_is_refused() {
    let mut rig = Rig::new(false);
    let mut outcomes = rig.handle.outcomes();
    let response = submit(
        &rig,
        OPERATOR_TOKEN,
        "be0",
        select("select_preview", "wall"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rig.tick();
    let cut = json!({"me": "me1", "program": "main", "action": "cut"});
    let op = accepted_op(submit(&rig, LIMITED, "be1", cut.clone()).await).await;
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/layouts/wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "wall", "body": layout_body(&[("b", 1.0)], 64) }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    rig.tick();
    let outcome = outcome_of(&mut outcomes, &op);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("selection-changed")),
        "{outcome:?}"
    );
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "a");
    // Re-planned against the edited body, the CUT takes it.
    let op = accepted_op(submit(&rig, LIMITED, "be2", cut).await).await;
    rig.tick();
    assert_eq!(outcome_of(&mut outcomes, &op).error, None);
    assert_eq!(
        rig.handle.snapshot().unwrap().program.as_str(),
        "layout:wall"
    );
}

/// #307 review A4: a scoped patch of `layout:wall` is authorized for the
/// body `ensure` registered; an edit committed after that (before the tick)
/// to a body the scoped caller cannot read makes the engine refuse the patch
/// `selection-changed`, the input unchanged.
#[tokio::test]
async fn a_patch_of_a_layout_edited_after_authorization_is_refused() {
    let mut rig = Rig::new(true);
    let mut outcomes = rig.handle.outcomes();
    let op = accepted_op(
        send(
            &rig.router,
            keyed(
                post_json(
                    "/api/v1/patch/take",
                    LIMITED,
                    &patch_xp(3, json!({"layout": "wall"})),
                ),
                "pe0",
            ),
        )
        .await,
    )
    .await;
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/layouts/wall",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "wall", "body": layout_body(&[("a", 1.0), ("secret", 1.0)], 64) }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    rig.tick();
    let outcome = outcome_of(&mut outcomes, &op);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("selection-changed")),
        "{outcome:?}"
    );
    let table = rig.handle.inputs().unwrap();
    let input = table.inputs.iter().find(|i| i.number == 3).unwrap();
    assert!(input.source.is_none(), "input 3 unchanged: {input:?}");
    // Re-planned: the scoped caller may not read the new closure.
    let response = send(
        &rig.router,
        keyed(
            post_json(
                "/api/v1/patch/take",
                LIMITED,
                &patch_xp(3, json!({"layout": "wall"})),
            ),
            "pe1",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An unrestricted operator's patch of the current body applies.
    let op = accepted_op(
        send(
            &rig.router,
            keyed(
                post_json(
                    "/api/v1/patch/take",
                    OPERATOR_TOKEN,
                    &patch_xp(3, json!({"layout": "wall"})),
                ),
                "pe2",
            ),
        )
        .await,
    )
    .await;
    rig.tick();
    assert_eq!(outcome_of(&mut outcomes, &op).error, None);
    let table = rig.handle.inputs().unwrap();
    let input = table.inputs.iter().find(|i| i.number == 3).unwrap();
    assert_eq!(input.source.as_ref().unwrap().as_str(), "layout:wall");
}
