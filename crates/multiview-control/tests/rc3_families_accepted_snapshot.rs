//! The PR #190 round 2/3 rules (R2-01, R3-01, R3-03), applied to the families
//! rc3 owns: graphics data sources/items/channels and the IPTV publication.
//!
//! * R3-03: graphics and IPTV reads, and their `ETag`s, come from the
//!   in-memory accepted snapshot, never a `desired.json` read per request.
//!   Proven two ways: the reads keep answering the committed body and tag
//!   after `desired.json` is made unreadable, and the per-read cost is
//!   measured against the old `load_desired` path (as in
//!   `accepted_snapshot_latency.rs`).
//! * R3-01: export and promote carry the committed graphics and IPTV
//!   sections from the accepted envelope.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel, GRAPHICS_ITEMS_KEY, IPTV_KEY},
    command_bus,
    routes::graphics::effective_graphics,
    AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_graphics::GraphicsHub;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use support::{body_bytes, body_json, delete_if_match, etag, get, put_json, send, ADMIN_TOKEN};

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
path = "/tmp/rc3-families-accepted-snapshot.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

const SOURCE: &str = "/api/v1/graphics/data-sources/sydney";
const ITEM: &str = "/api/v1/graphics/items/syd7";
const IPTV: &str = "/api/v1/iptv";

fn state_for(path: &Path, config: &MultiviewConfig) -> AppState {
    let seeded = multiview_control::seed_resources(config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
    AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(config).unwrap())
    .with_graphics(GraphicsHub::new(effective_graphics(config)))
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )))
}

fn first_run() -> (tempfile::TempDir, PathBuf, AppState, Router) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let state = state_for(&path, &config);
    let router = multiview_control::router(state.clone());
    (dir, path, state, router)
}

async fn put_ok(router: &Router, uri: &str, tag: Option<&str>, body: &Value) -> String {
    let response = send(router, put_json(uri, ADMIN_TOKEN, tag, body)).await;
    assert!(
        response.status().is_success(),
        "{uri}: {}",
        response.status()
    );
    etag(&response).expect("a committed write returns its ETag")
}

async fn read(router: &Router, uri: &str) -> (StatusCode, Option<String>, Value) {
    let response = send(router, get(uri, ADMIN_TOKEN)).await;
    let status = response.status();
    let tag = etag(&response);
    (status, tag, body_json(response).await)
}

fn data_source() -> Value {
    json!({"name": "Sydney", "kind": "bom_weather", "forecast_product": "IDN11060", "aac": "NSW_PT131"})
}

fn item(name: &str) -> Value {
    json!({"name": name, "template": "weather.today", "data_source": "sydney"})
}

fn iptv(name: &str) -> Value {
    json!({"enabled": true, "entries": [{"kind": "output", "ref": "output-0", "name": name}]})
}

/// Commit a graphics item and an IPTV publication; returns their tags.
async fn commit_families(router: &Router) -> (String, String) {
    put_ok(router, SOURCE, None, &data_source()).await;
    let first = put_ok(router, ITEM, None, &item("Syd")).await;
    let item_tag = put_ok(router, ITEM, Some(&first), &item("Syd committed")).await;
    let initial = read(router, IPTV).await.1.unwrap();
    let iptv_tag = put_ok(router, IPTV, Some(&initial), &iptv("Committed")).await;
    (item_tag, iptv_tag)
}

fn desired_file(path: &Path) -> PathBuf {
    multiview_control::boot_model::state_dir_for(path).join("desired.json")
}

/// R3-03: once committed, graphics and IPTV GETs (body, `ETag` and the
/// `If-Match` check) never touch `desired.json`. The file is replaced with
/// garbage that `load_desired` refuses; every read still answers the
/// committed truth, and a stale tag is still refused.
#[tokio::test]
async fn graphics_and_iptv_reads_are_served_from_memory_with_no_disk_read() {
    let (_dir, path, _state, router) = first_run();
    let (item_tag, iptv_tag) = commit_families(&router).await;
    std::fs::write(desired_file(&path), "not json").unwrap();
    assert!(
        load_desired(&path).is_err(),
        "the disk record is unreadable"
    );

    let (status, tag, body) = read(&router, ITEM).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag.as_deref(), Some(item_tag.as_str()));
    assert_eq!(body["name"], "Syd committed");
    let (status, _, list) = read(&router, "/api/v1/graphics/items").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["name"], "Syd committed");
    let (status, tag, body) = read(&router, IPTV).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag.as_deref(), Some(iptv_tag.as_str()));
    assert_eq!(body["config"]["entries"][0]["name"], "Committed");
    // The public playlist is composed from the same snapshot.
    let playlist = send(
        &router,
        axum::http::Request::builder()
            .uri("/iptv/playlist.m3u8")
            .header(axum::http::header::HOST, "mv.example.test:8080")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(playlist.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(playlist).await).unwrap();
    assert!(text.contains("Committed"), "{text}");
    // A stale tag is refused from memory (the precondition reads no file).
    let stale = send(&router, delete_if_match(ITEM, ADMIN_TOKEN, Some("W/\"1\""))).await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
}

/// Median of `n` timed calls, after one warm-up call.
fn median_ns(n: usize, mut f: impl FnMut()) -> u128 {
    f();
    let mut samples: Vec<u128> = (0..n)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_nanos()
        })
        .collect();
    samples.sort_unstable();
    samples.get(samples.len() / 2).copied().expect("samples")
}

/// R3-03 cost, measured the way `accepted_snapshot_latency.rs` does: the
/// version lookup behind every graphics/IPTV `ETag` and `If-Match` against
/// the old per-request `load_desired` read+parse+validate. A regression to a
/// disk read per request fails here.
#[tokio::test]
async fn graphics_and_iptv_version_lookups_beat_a_fresh_disk_read() {
    const SAMPLES: usize = 200;
    const MIN_SPEEDUP: u128 = 3;
    let (_dir, path, state, router) = first_run();
    commit_families(&router).await;
    let old_ns = median_ns(SAMPLES, || {
        let desired = load_desired(&path).unwrap().expect("envelope");
        assert!(desired.resource_versions[GRAPHICS_ITEMS_KEY]["syd7"] > 1);
    });
    let new_ns = median_ns(SAMPLES, || {
        let version =
            multiview_control::boot_model::accepted_version(&state, GRAPHICS_ITEMS_KEY, "syd7")
                .unwrap()
                .expect("modeled");
        assert!(version.get() > 1);
        let version =
            multiview_control::boot_model::accepted_version(&state, IPTV_KEY, "singleton")
                .unwrap()
                .expect("modeled");
        assert!(version.get() > 1);
    });
    assert!(
        new_ns.saturating_mul(MIN_SPEEDUP) <= old_ns.max(new_ns),
        "two snapshot version lookups must be at least {MIN_SPEEDUP}x faster than one \
         disk read+parse+validate: old={old_ns}ns new={new_ns}ns"
    );
}

/// R3-01: export and promote compose graphics and IPTV from the accepted
/// envelope — the committed item body and publication, nothing older.
#[tokio::test]
async fn export_and_promote_carry_committed_graphics_and_iptv() {
    let (_dir, path, _state, router) = first_run();
    commit_families(&router).await;

    let response = send(&router, get("/api/v1/config/export", ADMIN_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let exported: Value =
        toml::from_str(&String::from_utf8(body_bytes(response).await).unwrap()).unwrap();
    assert_eq!(exported["graphics"]["items"][0]["id"], "syd7");
    assert_eq!(exported["graphics"]["items"][0]["name"], "Syd committed");
    assert_eq!(exported["iptv"]["entries"][0]["name"], "Committed");

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/config/promote")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {ADMIN_TOKEN}"),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let promoted = send(&router, request).await;
    assert_eq!(promoted.status(), StatusCode::OK);
    let boot: Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(boot["graphics"]["items"][0]["name"], "Syd committed");
    assert_eq!(boot["graphics"]["data_sources"][0]["id"], "sydney");
    assert_eq!(boot["iptv"]["entries"][0]["name"], "Committed");
}
