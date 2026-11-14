//! The PR #190 durable-state contract, applied to the families rc3 added:
//! graphics data sources, items and channels, weather, IPTV, source event
//! automation rules, and the audio-routing output propagation.
//!
//! Each family is mutated over HTTP, the state is rebuilt from disk exactly as
//! the binary does at boot, and the edit must still be present, must survive an
//! unrelated later commit, and must keep an `ETag` that never repeats.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus,
    config_watch::{spawn as spawn_watch, WatchOptions},
    routes::graphics::effective_graphics,
    AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_graphics::GraphicsHub;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::{
    body_bytes, body_json, delete_if_match, etag, get, post_json, put_json, send, ADMIN_TOKEN,
};

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

const SOURCE: &str = "/api/v1/graphics/data-sources/sydney";
const ITEM: &str = "/api/v1/graphics/items/syd7";
const CHANNEL: &str = "/api/v1/graphics/channels/gfx-channel-1";
const RUNDOWN: &str = "/api/v1/graphics/channels/gfx-channel-1/rundown";

/// Build the control state the way the binary does for accepted `config`.
fn state_for(path: &Path, config: &MultiviewConfig) -> AppState {
    let seeded = multiview_control::seed_resources(config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, receiver) = command_bus(64);
    // A running engine holds the receiver for the life of the run; a closed bus
    // means shutdown, and the config-file watcher commits nothing then (#367).
    // Leaked on purpose (a test process ends anyway).
    Box::leak(Box::new(receiver));
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

async fn read(router: &Router, uri: &str) -> (StatusCode, Option<String>, Value) {
    let response = send(router, get(uri, ADMIN_TOKEN)).await;
    let status = response.status();
    let tag = etag(&response);
    (status, tag, body_json(response).await)
}

fn version(tag: &str) -> u64 {
    tag.trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .expect("numeric weak ETag")
}

fn data_source(name: &str) -> Value {
    json!({"name": name, "kind": "bom_weather", "forecast_product": "IDN11060", "aac": "NSW_PT131"})
}

fn item(name: &str) -> Value {
    json!({"name": name, "template": "weather.today", "data_source": "sydney"})
}

/// Create the data source and item both graphics tests build on.
async fn seed_graphics(router: &Router) {
    put_ok(router, SOURCE, None, &data_source("Sydney")).await;
    put_ok(router, ITEM, None, &item("Syd")).await;
}

#[tokio::test]
async fn graphics_data_source_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = first_run();
    let first = put_ok(&router, SOURCE, None, &data_source("Sydney")).await;
    let second = put_ok(&router, SOURCE, Some(&first), &data_source("Sydney CBD")).await;
    assert!(version(&second) > version(&first));
    let stale = send(
        &router,
        put_json(SOURCE, ADMIN_TOKEN, Some(&first), &data_source("Lost")),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

    unrelated_commit(&router).await;
    let restarted = restart(&path);
    let (status, tag, body) = read(&restarted, SOURCE).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"]["name"], "Sydney CBD");
    assert_eq!(
        tag.as_deref(),
        Some(second.as_str()),
        "restart moved the ETag"
    );
    let stale = send(
        &restarted,
        put_json(SOURCE, ADMIN_TOKEN, Some(&first), &data_source("Lost")),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

    let deleted = send(
        &restarted,
        delete_if_match(SOURCE, ADMIN_TOKEN, Some(&second)),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    unrelated_commit(&restarted).await;
    let again = restart(&path);
    assert_eq!(read(&again, SOURCE).await.0, StatusCode::NOT_FOUND);
    // A recreated identity continues its counter: the old tag can never match.
    let recreated = put_ok(&again, SOURCE, None, &data_source("Back")).await;
    assert!(version(&recreated) > version(&second));
}

#[tokio::test]
async fn graphics_item_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = first_run();
    seed_graphics(&router).await;
    let first = read(&router, ITEM).await.1.unwrap();
    let second = put_ok(&router, ITEM, Some(&first), &item("Syd edited")).await;
    assert!(version(&second) > version(&first));
    unrelated_commit(&router).await;
    let restarted = restart(&path);
    let (status, tag, body) = read(&restarted, ITEM).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Syd edited");
    assert_eq!(tag.as_deref(), Some(second.as_str()));

    let deleted = send(&restarted, delete_if_match(ITEM, ADMIN_TOKEN, Some(&first))).await;
    assert_eq!(deleted.status(), StatusCode::PRECONDITION_FAILED);
    let deleted = send(
        &restarted,
        delete_if_match(ITEM, ADMIN_TOKEN, Some(&second)),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    unrelated_commit(&restarted).await;
    assert_eq!(read(&restart(&path), ITEM).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graphics_channel_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = first_run();
    seed_graphics(&router).await;
    // The hub's default channel exists before any envelope records it, so its
    // pre-envelope tag must be honoured and then advanced.
    let initial = read(&router, CHANNEL).await.1.unwrap();
    let first = put_ok(
        &router,
        RUNDOWN,
        Some(&initial),
        &json!({"rundown": ["syd7"], "name": "Main"}),
    )
    .await;
    assert!(version(&first) > version(&initial));
    let second = put_ok(
        &router,
        RUNDOWN,
        Some(&first),
        &json!({"rundown": [], "name": "Main edited"}),
    )
    .await;
    assert!(version(&second) > version(&first));
    let third = put_ok(
        &router,
        RUNDOWN,
        Some(&second),
        &json!({"rundown": ["syd7"], "name": "Main edited"}),
    )
    .await;
    unrelated_commit(&router).await;
    let restarted = restart(&path);
    let (status, tag, body) = read(&restarted, CHANNEL).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Main edited");
    assert_eq!(body["rundown"], json!(["syd7"]));
    assert_eq!(tag.as_deref(), Some(third.as_str()));
    let stale = send(
        &restarted,
        put_json(RUNDOWN, ADMIN_TOKEN, Some(&second), &json!({"rundown": []})),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn weather_location_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = first_run();
    let uri = "/api/v1/weather/locations/legacy";
    let body =
        |name: &str| json!({"name": name, "forecast_product": "IDN11060", "aac": "NSW_PT131"});
    // A boot-file location is an existing resource at the pre-envelope tag.
    let initial = read(&router, uri).await.1.unwrap();
    let first = put_ok(&router, uri, Some(&initial), &body("Edited")).await;
    assert!(version(&first) > version(&initial));
    unrelated_commit(&router).await;

    let desired = load_desired(&path).unwrap().unwrap();
    assert_eq!(desired.config.weather.locations[0].name, "Edited");
    let restarted = restart(&path);
    let (status, tag, view) = read(&restarted, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["location"]["name"], "Edited");
    assert_eq!(tag.as_deref(), Some(first.as_str()));

    let deleted = send(&restarted, delete_if_match(uri, ADMIN_TOKEN, Some(&first))).await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    unrelated_commit(&restarted).await;
    assert_eq!(read(&restart(&path), uri).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn iptv_edits_survive_restart_and_etags_never_repeat() {
    let (_dir, path, router) = first_run();
    let uri = "/api/v1/iptv";
    let doc = |name: &str| json!({"enabled": true, "entries": [{"kind": "output", "ref": "output-0", "name": name}]});
    let initial = read(&router, uri).await.1.unwrap();
    let a = put_ok(&router, uri, Some(&initial), &doc("A")).await;
    let b = put_ok(&router, uri, Some(&a), &doc("B")).await;
    // A -> B -> A: identical content, yet the first A tag must stay stale.
    let a_again = put_ok(&router, uri, Some(&b), &doc("A")).await;
    assert_ne!(a_again, a, "an ETag repeated across an A-B-A edit");
    let stale = send(&router, put_json(uri, ADMIN_TOKEN, Some(&a), &doc("Lost"))).await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

    unrelated_commit(&router).await;
    let restarted = restart(&path);
    let (status, tag, body) = read(&restarted, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["config"]["entries"][0]["name"], "A");
    assert_eq!(tag.as_deref(), Some(a_again.as_str()));
    let stale = send(
        &restarted,
        put_json(uri, ADMIN_TOKEN, Some(&a), &doc("Lost")),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn event_automation_rules_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = first_run();
    let uri = "/api/v1/inputs/in_b/events/automation";
    let source = "/api/v1/sources/in_b";
    let rule = |seconds: u32| {
        json!({"rule": {"on_break_start": {"action": "swap_cell", "target": "bars"},
            "max_duration_s": seconds}})
    };
    let before = read(&router, source).await.1.unwrap();
    let response = send(&router, put_json(uri, ADMIN_TOKEN, None, &rule(120))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let edited = read(&router, source).await.1.unwrap();
    assert!(version(&edited) > version(&before));
    let response = send(&router, put_json(uri, ADMIN_TOKEN, None, &rule(240))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = send(
        &router,
        post_json(&format!("{uri}/arm"), ADMIN_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let armed = read(&router, source).await.1.unwrap();
    assert!(version(&armed) > version(&edited));

    unrelated_commit(&router).await;
    let restarted = restart(&path);
    let (status, _, body) = read(&restarted, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rule"]["max_duration_s"], 240);
    assert_eq!(body["rule"]["armed"], true);
    assert_eq!(read(&restarted, source).await.1, Some(armed));

    let response = send(
        &restarted,
        put_json(uri, ADMIN_TOKEN, None, &json!({"rule": null})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    unrelated_commit(&restarted).await;
    let (_, _, body) = read(&restart(&path), uri).await;
    assert!(body["rule"].is_null(), "a removed rule came back");
}

fn routing(track: &str) -> Value {
    json!({"sample_rate_hz": 48_000, "routes": [{
        "input_id": "in_a", "channels": {"kind": "stereo"}, "target_track": track,
        "include_in_program_bus": true, "gain_db": 0.0, "mute": false
    }]})
}

/// The second rename is the regression: once an envelope exists, re-deriving
/// identities refused the rewritten id-less output as ambiguous.
#[tokio::test]
async fn audio_output_propagation_survives_restart_with_id_less_outputs() {
    let (_dir, path, router) = first_run();
    let audio = "/api/v1/audio-routing";
    let output = "/api/v1/outputs/output-0";
    let output_before = read(&router, output).await.1.unwrap();
    let tag = read(&router, audio).await.1.unwrap();
    let response = send(
        &router,
        put_json(audio, ADMIN_TOKEN, Some(&tag), &routing("t-two")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let tag = etag(&response).unwrap();
    assert_eq!(
        body_json(response).await["outputs_updated"][0]["output"],
        "output-0"
    );

    let response = send(
        &router,
        put_json(audio, ADMIN_TOKEN, Some(&tag), &routing("t-three")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "second rename refused");
    let tag = etag(&response).unwrap();
    let updated = body_json(response).await;
    assert_eq!(updated["outputs_updated"][0]["output"], "output-0");
    assert_eq!(updated["outputs_updated"][0]["renamed"][0]["to"], "t-three");
    let (_, output_tag, _) = read(&router, output).await;
    let output_tag = output_tag.unwrap();
    assert!(version(&output_tag) > version(&output_before));

    unrelated_commit(&router).await;
    let restarted = restart(&path);
    let (_, after, body) = read(&restarted, output).await;
    assert_eq!(body["body"]["audio"]["tracks"], json!(["prog", "t-three"]));
    assert_eq!(after, Some(output_tag), "restart moved the output ETag");
    assert_eq!(
        read(&restarted, "/api/v1/outputs/output-1").await.0,
        StatusCode::OK
    );
    let (_, audio_tag, body) = read(&restarted, audio).await;
    assert_eq!(body["routing"]["routes"][0]["target_track"], "t-three");
    assert_eq!(audio_tag, Some(tag));
}

/// The section families have no store, so export and `active.toml` must read
/// them from the envelope rather than the startup document.
#[tokio::test]
async fn export_carries_accepted_graphics_and_iptv() {
    let (_dir, _path, router) = first_run();
    seed_graphics(&router).await;
    let tag = read(&router, "/api/v1/iptv").await.1.unwrap();
    put_ok(
        &router,
        "/api/v1/iptv",
        Some(&tag),
        &json!({"enabled": true, "entries": [{"kind": "output", "ref": "output-0", "name": "Exported"}]}),
    )
    .await;
    let response = send(&router, get("/api/v1/config/export", ADMIN_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(text.contains("syd7"), "{text}");
    assert!(text.contains("Exported"), "{text}");
}

/// Revert must reach the hub: a stale hub would resurrect a reverted item on
/// the next graphics commit.
#[tokio::test]
async fn revert_to_start_removes_graphics_edits_for_good() {
    let (_dir, path, router) = first_run();
    seed_graphics(&router).await;
    let response = send(
        &router,
        post_json("/api/v1/config/revert-to-start", ADMIN_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(read(&router, ITEM).await.0, StatusCode::NOT_FOUND);
    put_ok(
        &router,
        "/api/v1/graphics/data-sources/other",
        None,
        &data_source("Other"),
    )
    .await;
    let restarted = restart(&path);
    assert_eq!(read(&restarted, ITEM).await.0, StatusCode::NOT_FOUND);
    assert_eq!(read(&restarted, SOURCE).await.0, StatusCode::NOT_FOUND);
}

/// A file edit's graphics change must reach the hub: the hub is the base of
/// the next graphics mutation, so a stale hub would undo the file edit on the
/// next API commit. Content the file edit did not touch (the item the API
/// created) stays (#367: a file edit applies only what the FILE changed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_edit_cannot_be_undone_by_the_next_graphics_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let state = state_for(&path, &config);
    let router = multiview_control::router(state.clone());
    let (options, mut poll) = WatchOptions::default().with_manual_poll();
    let watch = spawn_watch(path.clone(), config, state.clone(), options);
    for _ in 0..2 {
        assert!(poll.poll_once().await);
    }
    seed_graphics(&router).await;

    let file_source = r#"
[[graphics.data_sources]]
id = "from-file"
name = "From the file"
kind = "bom_weather"
forecast_product = "IDV10753"
aac = "VIC_PT042"
"#;
    let edited = dir.path().join("edited.toml");
    std::fs::write(&edited, format!("{BOOT}{file_source}")).unwrap();
    std::fs::rename(&edited, &path).unwrap();
    for _ in 0..2 {
        assert!(poll.poll_once().await);
    }
    assert_eq!(state.config_watch.snapshot().applied_count, 1);

    put_ok(
        &router,
        "/api/v1/graphics/data-sources/other",
        None,
        &data_source("Other"),
    )
    .await;
    let desired = load_desired(&path).unwrap().unwrap();
    let ids: Vec<&str> = desired
        .config
        .graphics
        .data_sources
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    assert!(
        ids.contains(&"from-file"),
        "a graphics commit undid the file edit (stale hub): {ids:?}"
    );
    assert!(ids.contains(&"other"), "{ids:?}");
    watch.stop();
}
