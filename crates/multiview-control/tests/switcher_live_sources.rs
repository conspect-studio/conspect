//! Issue #464: on a switcher run, a media edit of a source that is on a bus
//! is refused with a `409` naming every switcher consumer, before anything is
//! stored or enqueued; a source off the buses edits live; a policy-only edit
//! of an on-bus source passes; and Start waits (bounded) for a source the
//! engine adopted live instead of answering "applies on restart".
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::BootModel;
use multiview_control::{
    Command, SourceDemandLease, SourceDemandPhase, SourceDemandProvider, SourceDemandView,
    StopRefusal,
};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::SwitcherControl;
use serde_json::json;
use support::{
    body_json, etag, get, harness_with, post_json, put_json, send, OPERATOR_TOKEN, SCOPED_TOKEN,
};

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
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "cam-a"
kind = "hls"
url = "http://[::1]:9/a.m3u8"
[[sources]]
id = "cam-b"
kind = "hls"
url = "http://[::1]:9/b.m3u8"
[[sources]]
id = "cam-c"
kind = "hls"
url = "http://[::1]:9/c.m3u8"
[[sources]]
id = "scoped-layout"
kind = "hls"
url = "http://[::1]:9/logo.m3u8"
[[cells]]
id = "pgm"
area = "a"
[cells.source]
input_id = "cam-a"
[[outputs]]
kind = "hls"
path = "/tmp/switcher-live-sources/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "cam-a"
preview_source = "cam-b"
[[switcher.downstream_keyers]]
id = "dsk1"
program = "main"
fill = "scoped-layout"
"##;

/// A demand provider whose catalogue gains `late` only once `ready` is set
/// (the hub catalogues a live add at the next frame boundary).
#[derive(Default)]
struct LateCatalogue {
    ready: AtomicBool,
}

impl SourceDemandProvider for LateCatalogue {
    fn status(&self, id: &str) -> SourceDemandView {
        SourceDemandView::idle(id)
    }
    fn active(&self) -> Vec<SourceDemandView> {
        Vec::new()
    }
    fn preview_lease(&self, _id: &str, _label: &str) -> Option<Box<dyn SourceDemandLease>> {
        None
    }
    fn touch_preview(&self, _id: &str) -> Option<SourceDemandPhase> {
        None
    }
    fn start_source(&self, id: &str) -> bool {
        id != "late" || self.ready.load(Ordering::Acquire)
    }
    fn stop_source(&self, _id: &str) -> Result<(), StopRefusal> {
        Ok(())
    }
}

struct Rig {
    h: support::Harness,
    demand: Arc<LateCatalogue>,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("cam-a"),
        SourceRef::new("cam-b"),
        vec![],
    ));
    // Publish the engine snapshot (Program cam-a, Preview cam-b).
    let tick = Tick {
        index: 0,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
    assert!(handle.snapshot().is_some());
    let dir = tempfile::tempdir().unwrap();
    let demand = Arc::new(LateCatalogue::default());
    let provider = Arc::clone(&demand);
    let path = dir.path().join("conspect.toml");
    let h = harness_with(move |state| {
        state
            .with_seeded_resources(seeded)
            .with_base_document(serde_json::to_value(&config).unwrap())
            .with_boot_model(Arc::new(BootModel::new(
                path,
                config.clone(),
                config,
                StartMode::Boot,
                false,
                None,
            )))
            .with_live_sources(multiview_control::LiveSourceCapability::synthetic_and_network())
            .with_source_demand(provider)
            .with_switcher(handle)
    });
    // The driver must outlive the test body only for the snapshot it published.
    drop(driver);
    Rig {
        h,
        demand,
        _dir: dir,
    }
}

async fn put_source(rig: &mut Rig, id: &str, body: serde_json::Value) -> axum::response::Response {
    put_source_as(rig, OPERATOR_TOKEN, id, body).await
}

async fn put_source_as(
    rig: &mut Rig,
    token: &str,
    id: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    let current = send(&rig.h.router, get(&format!("/api/v1/sources/{id}"), token)).await;
    assert_eq!(current.status(), StatusCode::OK, "GET {id}");
    let tag = etag(&current).unwrap();
    send(
        &rig.h.router,
        put_json(
            &format!("/api/v1/sources/{id}"),
            token,
            Some(&tag),
            &json!({ "name": id, "body": body }),
        ),
    )
    .await
}

fn upserts(rig: &mut Rig, id: &str) -> usize {
    rig.h
        .commands
        .try_drain()
        .iter()
        .filter(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == id))
        .count()
}

#[tokio::test]
async fn live_source_switcher_media_edit_on_a_bus_is_refused_naming_the_bus() {
    let mut rig = rig();
    for (id, consumer) in [
        ("cam-a", "program bus"),
        ("cam-b", "preview bus"),
        ("scoped-layout", "keyer fill main/dsk1"),
    ] {
        let resp = put_source(
            &mut rig,
            id,
            json!({"id": id, "kind": "hls", "url": "http://[::1]:9/edited.m3u8"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{id}");
        let detail = body_json(resp).await["detail"].as_str().unwrap().to_owned();
        assert!(detail.contains(consumer), "{id}: {detail}");
        assert_eq!(upserts(&mut rig, id), 0, "{id}: nothing enqueued");
        // Nothing was stored: the old URL is still the accepted document.
        let body = body_json(
            send(
                &rig.h.router,
                get(&format!("/api/v1/sources/{id}"), OPERATOR_TOKEN),
            )
            .await,
        )
        .await;
        assert!(
            !body["body"]["url"].as_str().unwrap().contains("edited"),
            "{id}: the refused edit was not stored: {body}"
        );
    }
}

#[tokio::test]
async fn live_source_switcher_edit_off_the_bus_and_policy_edit_on_the_bus_apply_live() {
    let mut rig = rig();
    let resp = put_source(
        &mut rig,
        "cam-c",
        json!({"id": "cam-c", "kind": "hls", "url": "http://[::1]:9/c2.m3u8"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
    assert_eq!(upserts(&mut rig, "cam-c"), 1);

    // An on-air source may still change policy (no producer restart).
    let resp = put_source(
        &mut rig,
        "cam-a",
        json!({"id": "cam-a", "kind": "hls", "url": "http://[::1]:9/a.m3u8", "activation": "on_demand"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn live_source_switcher_start_waits_for_a_live_adopted_source() {
    let mut rig = rig();
    let resp = send(
        &rig.h.router,
        post_json(
            "/api/v1/sources/late",
            OPERATOR_TOKEN,
            &json!({"name": "late", "body": {"id": "late", "kind": "hls", "url": "http://[::1]:9/late.m3u8"}}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers()["x-multiview-apply"], "live");
    assert_eq!(upserts(&mut rig, "late"), 1);
    // The hub catalogues it a little later.
    let demand = Arc::clone(&rig.demand);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        demand.ready.store(true, Ordering::Release);
    });
    let resp = send(
        &rig.h.router,
        post_json("/api/v1/sources/late/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "{:?}",
        body_json(resp).await
    );
}

/// BOLA: a key scoped to the edited source but not to the keyer object never
/// learns the keyer's id from the refusal.
#[tokio::test]
async fn live_source_switcher_on_bus_refusal_redacts_unreadable_keyers() {
    let mut rig = rig();
    let resp = put_source_as(
        &mut rig,
        SCOPED_TOKEN,
        "scoped-layout",
        json!({"id": "scoped-layout", "kind": "hls", "url": "http://[::1]:9/edited.m3u8"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let detail = body_json(resp).await["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("keyer fill restricted"), "{detail}");
    assert!(!detail.contains("dsk1"), "{detail}");
    assert_eq!(upserts(&mut rig, "scoped-layout"), 0);
}

// ---------------------------------------------------------------------------
// Windy VT (ship-1111): PVW select of a live-added `media_player` source.
// ---------------------------------------------------------------------------

/// A demand provider modelled on the binary's: an id is leasable only once it
/// is catalogued, and an alias of a player with no channel has a reason.
struct AliasDemand {
    catalogued: Vec<&'static str>,
    no_channel: Option<(&'static str, &'static str)>,
}

struct NoopLease;
impl SourceDemandLease for NoopLease {}

impl SourceDemandProvider for AliasDemand {
    fn status(&self, id: &str) -> SourceDemandView {
        SourceDemandView::idle(id)
    }
    fn active(&self) -> Vec<SourceDemandView> {
        Vec::new()
    }
    fn preview_lease(&self, id: &str, _label: &str) -> Option<Box<dyn SourceDemandLease>> {
        if self.catalogued.contains(&id) {
            let lease: Box<dyn SourceDemandLease> = Box::new(NoopLease);
            Some(lease)
        } else {
            None
        }
    }
    fn touch_preview(&self, _id: &str) -> Option<SourceDemandPhase> {
        None
    }
    fn select_refusal(&self, id: &str) -> Option<String> {
        self.no_channel
            .filter(|(alias, _)| *alias == id)
            .map(|(_, player)| format!("media player {player:?} has no channel in this run"))
    }
}

fn primed(
    id: &str,
) -> Arc<multiview_framestore::TileStore<multiview_compositor::pipeline::Nv12Image>> {
    let store = Arc::new(multiview_framestore::TileStore::with_defaults(
        id.to_owned(),
    ));
    store.publish(
        multiview_compositor::pipeline::Nv12Image::solid(
            64,
            64,
            90,
            128,
            128,
            multiview_core::color::ColorInfo::default().resolve_defaults(64, 64),
        )
        .unwrap(),
        MediaTime::ZERO,
    );
    store
}

fn vt_harness(
    demand: AliasDemand,
) -> (
    support::Harness,
    SwitcherControl,
    multiview_engine::switcher_control::SwitcherTickDriver,
) {
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("cam-a"),
        SourceRef::new("cam-b"),
        vec![],
    ));
    let scene = |name: &str, source: &str| {
        Arc::new(multiview_core::layout::Layout {
            name: name.into(),
            canvas: multiview_core::layout::Canvas {
                width: 64,
                height: 64,
                fps_num: 25,
                fps_den: 1,
            },
            cells: vec![multiview_core::layout::Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                source: Some(source.to_owned()),
                ..multiview_core::layout::Cell::default()
            }],
        })
    };
    handle
        .set_scenes(multiview_engine::switcher_control::ScenePair {
            version: 1,
            program: scene("program", "cam-a"),
            preview: scene("preview", "cam-b"),
        })
        .unwrap();
    let player = primed("vt-1");
    driver.set_prime_stores(
        [
            ("cam-a".to_owned(), primed("cam-a")),
            ("cam-b".to_owned(), primed("cam-b")),
            ("vt-1".to_owned(), Arc::clone(&player)),
            ("vt-1-src".to_owned(), player),
        ]
        .into_iter()
        .collect(),
    );
    let tick = Tick {
        index: 0,
        pts: MediaTime::ZERO,
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
    driver.publish_provider_status(multiview_engine::switcher_control::ProviderStatus {
        scene_version: 1,
        last_tick: 0,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    let store = multiview_control::InMemorySourceStore::new();
    for (id, body) in [
        ("cam-a", json!({"id": "cam-a", "kind": "solid"})),
        ("cam-b", json!({"id": "cam-b", "kind": "solid"})),
        (
            "vt-1-src",
            json!({"id": "vt-1-src", "kind": "media_player", "player": "vt-1"}),
        ),
        (
            "vt-ghost",
            json!({"id": "vt-ghost", "kind": "media_player", "player": "vt-9"}),
        ),
    ] {
        multiview_control::ResourceRepository::create(
            &store,
            id,
            multiview_control::ResourceInput {
                name: id.to_owned(),
                body,
            },
        )
        .unwrap();
    }
    let sources: Arc<dyn multiview_control::ResourceRepository> = Arc::new(store);
    let control = handle.clone();
    let h = harness_with(move |state| {
        state
            .with_sources_store(sources)
            .with_switcher(control)
            .with_source_demand(Arc::new(demand))
    });
    (h, handle, driver)
}

fn select(source: &str, key: &str) -> axum::http::Request<axum::body::Body> {
    let mut request = post_json(
        "/api/v1/switcher/commands/select_preview",
        OPERATOR_TOKEN,
        &json!({"me": "me1", "program": "main", "action": "select_preview", "source": source}),
    );
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    request
}

/// A live-added `media_player` source bound to a playing player is accepted
/// on PVW (202) and the engine applies it; nothing answers a bare Conflict.
#[tokio::test]
async fn vt_select_preview_of_a_media_player_source_is_accepted_and_applied() {
    let (h, handle, mut driver) = vt_harness(AliasDemand {
        catalogued: vec!["cam-a", "cam-b", "vt-1-src"],
        no_channel: None,
    });
    let resp = send(&h.router, select("vt-1-src", "pvw-vt")).await;
    let status = resp.status();
    assert_eq!(status, StatusCode::ACCEPTED, "{:?}", body_json(resp).await);
    let audit = body_json(send(&h.router, get("/api/v1/audit", support::ADMIN_TOKEN)).await).await;
    assert!(
        audit.to_string().contains("\"object_kind\":\"switcher\"")
            && audit.to_string().contains("\"source\":\"vt-1-src\""),
        "the accepted select is audited: {audit}"
    );
    let tick = Tick {
        index: 1,
        pts: MediaTime::from_nanos(40_000_000),
    };
    driver.drain_at_tick(tick);
    driver.commit_composed_tick(tick);
    assert_eq!(
        handle.snapshot().unwrap().preview.as_str(),
        "vt-1-src",
        "Preview is the VT"
    );
}

/// A select that cannot work says why: a `media_player` source whose player
/// has no channel is a typed 409 naming the player, a not-yet-registered
/// source a typed 409 that says to retry — never a bare "Conflict".
#[tokio::test]
async fn vt_select_preview_that_cannot_work_is_a_typed_problem_with_its_reason() {
    let (h, _handle, _driver) = vt_harness(AliasDemand {
        catalogued: vec!["cam-a", "cam-b"],
        no_channel: Some(("vt-ghost", "vt-9")),
    });
    let resp = send(&h.router, select("vt-ghost", "pvw-ghost")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["type"], "/problems/source-unavailable", "{body}");
    assert!(body["detail"].as_str().unwrap().contains("vt-9"), "{body}");

    let resp = send(&h.router, select("vt-1-src", "pvw-late")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["type"], "/problems/source-unavailable", "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("vt-1-src"),
        "{body}"
    );

    // BOLA: a scoped key not granted the source is refused before any reason.
    let mut request = post_json(
        "/api/v1/switcher/commands/select_preview",
        SCOPED_TOKEN,
        &json!({"me": "me1", "program": "main", "action": "select_preview", "source": "vt-ghost"}),
    );
    request
        .headers_mut()
        .insert("Idempotency-Key", "scoped".parse().unwrap());
    let resp = send(&h.router, request).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert!(!body.to_string().contains("vt-9"), "{body}");
}
