//! `sources[].activation` is durable: set over the source API (which commits
//! through `commit_resource` under `lock_config_mutation`), it survives a
//! restart and an unrelated later commit, and the restored document still
//! reports it in `/sources/{id}/status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, SourceActivation, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
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
use support::{body_json, etag, get, put_json, send, ADMIN_TOKEN};

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
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/desired-rc3-a.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.audio]
mode = "tracks"
tracks = ["prog", "t-one"]
[[outputs]]
kind = "hls"
path = "/tmp/desired-rc3-b.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "in_a"
channels = { kind = "stereo" }
target_track = "t-one"
include_in_program_bus = true
gain_db = 0.0
mute = false
[weather]
[[weather.locations]]
id = "legacy"
name = "Legacy"
forecast_product = "IDN11060"
aac = "NSW_PT131"
"##;

/// Build the control state the way the binary does for accepted `config`.
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

/// A first run with no envelope yet.
fn first_run() -> (tempfile::TempDir, PathBuf, Router) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let router = multiview_control::router(state_for(&path, &config));
    (dir, path, router)
}

/// "Restart": rebuild from the accepted envelope on disk, as `control.rs` does.
fn restart(path: &Path) -> Router {
    let desired = load_desired(path).unwrap().expect("accepted envelope");
    let state = multiview_control::boot_model::restore_accepted_stores(
        state_for(path, &desired.config),
        &desired,
    )
    .unwrap();
    multiview_control::router(state)
}

/// An unrelated later commit on a source no test family depends on.
async fn unrelated_commit(router: &Router) {
    let tag = etag(&send(router, get("/api/v1/sources/in_a", ADMIN_TOKEN)).await).unwrap();
    let response = send(
        router,
        put_json(
            "/api/v1/sources/in_a",
            ADMIN_TOKEN,
            Some(&tag),
            &json!({"name": "unrelated", "body": {"id": "in_a", "kind": "solid", "color": "#123456"}}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
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

#[tokio::test]
async fn activation_survives_restart_and_an_unrelated_commit() {
    let (_dir, path, router) = first_run();
    let tag = etag(&send(&router, get("/api/v1/sources/in_b", ADMIN_TOKEN)).await).unwrap();
    put_ok(
        &router,
        "/api/v1/sources/in_b",
        Some(&tag),
        &json!({"name": "in_b", "body": {"id": "in_b", "kind": "bars", "activation": "on_demand"}}),
    )
    .await;
    let status =
        body_json(send(&router, get("/api/v1/sources/in_b/status", ADMIN_TOKEN)).await).await;
    assert_eq!(status["activation"], "on_demand");

    let router = restart(&path);
    unrelated_commit(&router).await;
    let router = restart(&path);
    let response = send(&router, get("/api/v1/sources/in_b", ADMIN_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let source: Value = body_json(response).await;
    assert_eq!(source["body"]["activation"], "on_demand", "{source}");
    let status =
        body_json(send(&router, get("/api/v1/sources/in_b/status", ADMIN_TOKEN)).await).await;
    assert_eq!(status["activation"], "on_demand");
    // The untouched source keeps the default.
    let status =
        body_json(send(&router, get("/api/v1/sources/in_a/status", ADMIN_TOKEN)).await).await;
    assert_eq!(status["activation"], "always_on");

    // The accepted document the binary boots from carries it too.
    let desired = load_desired(&path).unwrap().expect("accepted envelope");
    let in_b = desired
        .config
        .sources
        .iter()
        .find(|s| s.id == "in_b")
        .unwrap();
    assert_eq!(in_b.effective_activation(), SourceActivation::OnDemand);
}
