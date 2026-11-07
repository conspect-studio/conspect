//! Catalogue-linked sources go through the durable write path: `follow = auto`
//! survives a restart (committed through `commit_resource` into the boot file), an
//! HLS `user_agent`/`referrer`-only edit is media (never the metadata-only path),
//! and an `origin`-only edit may take it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]
mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_desired, BootModel};
use multiview_control::catalogue::sync::{BoxFuture, FetchOutcome, Fetcher};
use multiview_control::catalogue::{probe, Catalogue};
use multiview_control::{command_bus, AppState, EngineStateSnapshot, InMemoryRepository};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::{json, Value};
use support::{body_json, etag, get, post_json, put_json, send, ADMIN_TOKEN};

const BOOT: &str = r##"schema_version = 1
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
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/catalogue-durable.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

struct NoNet;
impl Fetcher for NoNet {
    fn fetch<'a>(
        &'a self,
        _: &'a str,
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchOutcome, String>> {
        Box::pin(async { Err("offline".to_owned()) })
    }
}

struct Never;
impl probe::ProbeRunner for Never {
    fn run(&self, _: String, _: probe::ProbeRequest) -> BoxFuture<'_, probe::ProbeResult> {
        Box::pin(std::future::pending())
    }
}

fn bundle(ua: Option<&str>) -> Value {
    json!({
        "streams": [
            { "channel": "A.us", "feed": "SD", "title": "A", "url": "https://a/1.m3u8", "quality": "720p", "labels": [], "user_agent": ua, "referrer": null }
        ],
        "channels": [ { "id": "A.us", "name": "A", "country": "US", "categories": ["news"] } ],
        "feeds": [ { "channel": "A.us", "id": "SD", "is_main": true, "broadcast_area": ["c/US"], "languages": ["eng"] } ]
    })
}

fn source_doc(follow: &str, origin_extra: Value, ua: Option<&str>) -> Value {
    let mut origin = json!({ "provider": "iptv-org", "channel": "A.us", "feed": "SD",
        "stream_url": "https://a/1.m3u8", "follow": follow });
    for (k, v) in origin_extra.as_object().cloned().unwrap_or_default() {
        origin[k] = v;
    }
    let mut body =
        json!({ "id": "linked", "kind": "hls", "url": "https://a/1.m3u8", "origin": origin });
    if let Some(ua) = ua {
        body["user_agent"] = json!(ua);
    }
    json!({ "name": "linked", "body": body })
}

fn stored_source(path: &std::path::Path) -> Value {
    let desired = load_desired(path)
        .unwrap()
        .expect("desired config persisted");
    let all = serde_json::to_value(&desired.config.sources).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "linked")
        .cloned()
        .expect("linked source persisted")
}

#[tokio::test]
async fn follow_auto_survives_restart_and_header_edits_stay_media() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _rx) = command_bus(64);
    let cat = Catalogue::new(
        dir.path().join("cat"),
        &multiview_config::IptvOrgConfig::default(),
        Arc::new(NoNet),
    );
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.clone(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )))
    .with_catalogue(cat, probe::ProbeService::new(Arc::new(Never), "h".into()));
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let prospective: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, prospective, layouts)
        .await
        .unwrap();
    let router = multiview_control::router(state);

    let imp = |b: Value| post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &b);
    assert_eq!(
        send(&router, imp(bundle(None))).await.status(),
        StatusCode::OK
    );

    // Create the linked source with follow = auto: durable in the boot file at once.
    let r = send(
        &router,
        post_json(
            "/api/v1/sources/linked",
            ADMIN_TOKEN,
            &source_doc("auto", json!({}), None),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(stored_source(&path)["origin"]["follow"], "auto");

    // Upstream now needs a User-Agent: auto-follow applies it through the durable path.
    assert_eq!(
        send(&router, imp(bundle(Some("UA/2")))).await.status(),
        StatusCode::OK
    );
    let after = stored_source(&path);
    assert_eq!(
        after["user_agent"], "UA/2",
        "auto-follow committed: {after}"
    );
    assert_eq!(
        after["origin"]["follow"], "auto",
        "follow survives the apply"
    );

    // "Restart": a fresh desired-config read (what boot loads) still carries both.
    let reloaded = stored_source(&path);
    assert_eq!(reloaded["origin"]["follow"], "auto");
    assert_eq!(reloaded["user_agent"], "UA/2");

    // A user_agent-only PUT is a media change: it must not take the guide-only path.
    let cur = send(&router, get("/api/v1/sources/linked", ADMIN_TOKEN)).await;
    let tag = etag(&cur).unwrap();
    let ua_only = source_doc("auto", json!({}), Some("UA/3"));
    let r = send(
        &router,
        put_json("/api/v1/sources/linked", ADMIN_TOKEN, Some(&tag), &ua_only),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let ua_apply = r
        .headers()
        .get("x-multiview-apply")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_ne!(
        ua_apply.as_deref(),
        Some("live"),
        "UA-only edit is media: {ua_apply:?}"
    );

    // A referrer-only PUT likewise.
    let tag = etag(&send(&router, get("/api/v1/sources/linked", ADMIN_TOKEN)).await).unwrap();
    let mut rf = source_doc("auto", json!({}), Some("UA/3"));
    rf["body"]["referrer"] = json!("https://ref.example/");
    let r = send(
        &router,
        put_json("/api/v1/sources/linked", ADMIN_TOKEN, Some(&tag), &rf),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let rf_apply = r
        .headers()
        .get("x-multiview-apply")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_ne!(
        rf_apply.as_deref(),
        Some("live"),
        "referrer-only edit is media: {rf_apply:?}"
    );

    // An origin-only PUT (follow mode) may take the metadata-only path.
    let tag = etag(&send(&router, get("/api/v1/sources/linked", ADMIN_TOKEN)).await).unwrap();
    let mut origin_only = rf.clone();
    origin_only["body"]["origin"]["follow"] = json!("notify");
    let r = send(
        &router,
        put_json(
            "/api/v1/sources/linked",
            ADMIN_TOKEN,
            Some(&tag),
            &origin_only,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(stored_source(&path)["origin"]["follow"], "notify");
    let _ = body_json(r).await;
}
