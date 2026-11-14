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
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::SwitcherControl;
use multiview_core::time::MediaTime;
use serde_json::json;
use support::{body_json, etag, get, harness_with, post_json, put_json, send, OPERATOR_TOKEN};

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
id = "logo"
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
fill = "logo"
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
    let current = send(
        &rig.h.router,
        get(&format!("/api/v1/sources/{id}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(current.status(), StatusCode::OK, "GET {id}");
    let tag = etag(&current).unwrap();
    send(
        &rig.h.router,
        put_json(
            &format!("/api/v1/sources/{id}"),
            OPERATOR_TOKEN,
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
        ("logo", "keyer main/dsk1 fill"),
    ] {
        let resp = put_source(
            &mut rig,
            id,
            json!({"id": id, "kind": "hls", "url": "http://[::1]:9/edited.m3u8"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{id}");
        let detail = body_json(resp).await["detail"]
            .as_str()
            .unwrap()
            .to_owned();
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
