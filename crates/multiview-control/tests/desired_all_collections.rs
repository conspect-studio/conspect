//! Review H1 (PR #190): once `desired.json` exists it outranks `active.toml` on
//! restart, so EVERY runtime-mutable config collection must commit through it.
//! Each family is mutated over HTTP, then an unrelated source commit lands, then
//! the process is "restarted" (a fresh state rebuilt from the accepted envelope
//! on disk): the edit must survive both.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use support::{body_json, delete_if_match, etag, get, post_json, put_json, send, ADMIN_TOKEN};

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
path = "/tmp/desired-all-collections.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// Build the control state for a run whose accepted intent is `config`.
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
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )))
}

/// A run that has already written its first desired envelope (the condition
/// under which the defect lost edits).
async fn running_with_desired() -> (tempfile::TempDir, std::path::PathBuf, Router) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let state = state_for(&path, &config);
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let initial: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, initial, layouts)
        .await
        .unwrap();
    assert!(load_desired(&path).unwrap().is_some());
    let router = multiview_control::router(state);
    (dir, path, router)
}

/// An unrelated later desired commit (a source edit) must not revert anything.
async fn unrelated_source_commit(router: &Router) {
    let current = send(router, get("/api/v1/sources/in_a", ADMIN_TOKEN)).await;
    assert_eq!(current.status(), StatusCode::OK);
    let tag = etag(&current).unwrap();
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

/// "Restart": rebuild the whole control state from the accepted envelope.
fn restart(path: &Path) -> Router {
    let desired = load_desired(path).unwrap().expect("accepted envelope");
    let state = multiview_control::boot_model::restore_accepted_stores(
        state_for(path, &desired.config),
        &desired,
    )
    .unwrap();
    multiview_control::router(state)
}

async fn upsert(router: &Router, uri: &str, put_only: bool, body: &Value) {
    let existing = send(router, get(uri, ADMIN_TOKEN)).await;
    let response = if existing.status() == StatusCode::OK {
        let tag = etag(&existing);
        send(router, put_json(uri, ADMIN_TOKEN, tag.as_deref(), body)).await
    } else if put_only {
        send(router, put_json(uri, ADMIN_TOKEN, None, body)).await
    } else {
        send(router, post_json(uri, ADMIN_TOKEN, body)).await
    };
    assert!(
        response.status().is_success(),
        "{uri}: {}",
        response.status()
    );
}

/// Read a field of a resource body; salvos/tally return the bare document.
async fn read(router: &Router, uri: &str) -> Option<Value> {
    let response = send(router, get(uri, ADMIN_TOKEN)).await;
    if response.status() == StatusCode::NOT_FOUND {
        return None;
    }
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let value = body_json(response).await;
    Some(value.get("body").cloned().unwrap_or(value))
}

async fn delete(router: &Router, uri: &str) {
    let existing = send(router, get(uri, ADMIN_TOKEN)).await;
    let tag = etag(&existing);
    let response = send(router, delete_if_match(uri, ADMIN_TOKEN, tag.as_deref())).await;
    assert!(
        response.status().is_success(),
        "{uri}: {}",
        response.status()
    );
}

#[tokio::test]
async fn overlay_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = running_with_desired().await;
    let uri = "/api/v1/overlays/ov_clock";
    upsert(
        &router,
        uri,
        false,
        &json!({"name": "c", "body": {"kind": "clock", "target": "canvas"}}),
    )
    .await;
    upsert(
        &router,
        uri,
        false,
        &json!({"name": "c", "body": {"kind": "clock", "target": "canvas", "format": "%H:%M"}}),
    )
    .await;
    unrelated_source_commit(&router).await;
    let restarted = restart(&path);
    let body = read(&restarted, uri)
        .await
        .expect("overlay survives restart");
    assert_eq!(body["format"], "%H:%M");
    delete(&restarted, uri).await;
    unrelated_source_commit(&restarted).await;
    assert!(
        read(&restart(&path), uri).await.is_none(),
        "delete survives restart"
    );
}

#[tokio::test]
async fn probe_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = running_with_desired().await;
    let uri = "/api/v1/probes/probe-1";
    let probe = |threshold: u32| {
        json!({"name": "p", "body": {
            "id": "probe-1", "cell": "cell_a", "kind": "black", "luma_threshold": threshold,
            "dwell": {"up_ms": 2000, "down_ms": 1000}, "severity": "Major", "latched": false
        }})
    };
    upsert(&router, uri, false, &probe(16)).await;
    upsert(&router, uri, false, &probe(20)).await;
    unrelated_source_commit(&router).await;
    let restarted = restart(&path);
    assert_eq!(
        read(&restarted, uri).await.expect("probe")["luma_threshold"],
        20
    );
    delete(&restarted, uri).await;
    assert!(read(&restart(&path), uri).await.is_none());
}

#[tokio::test]
async fn device_and_sync_group_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = running_with_desired().await;
    for (id, octet) in [("dev-left", "41"), ("dev-right", "42")] {
        upsert(
            &router,
            &format!("/api/v1/devices/{id}"),
            false,
            &json!({"name": id, "body": {"id": id, "driver": "zowietek",
                "address": format!("http://[fd00:db8::{octet}]"), "desired_mode": "decoder"}}),
        )
        .await;
    }
    upsert(
        &router,
        "/api/v1/devices/dev-left",
        false,
        &json!({"name": "dev-left", "body": {"id": "dev-left", "driver": "zowietek",
            "address": "http://[fd00:db8::99]", "desired_mode": "decoder"}}),
    )
    .await;
    let group = |skew: u32| {
        json!({"name": "g", "body": {"id": "wall", "mode": "auto", "target_skew_ms": skew,
            "members": [{"device": "dev-left", "offset_ms": 0}, {"device": "dev-right", "offset_ms": 0}]}})
    };
    upsert(&router, "/api/v1/sync-groups/wall", false, &group(50)).await;
    upsert(&router, "/api/v1/sync-groups/wall", false, &group(70)).await;
    unrelated_source_commit(&router).await;
    let restarted = restart(&path);
    assert_eq!(
        read(&restarted, "/api/v1/devices/dev-left")
            .await
            .expect("device")["address"],
        "http://[fd00:db8::99]"
    );
    assert_eq!(
        read(&restarted, "/api/v1/sync-groups/wall")
            .await
            .expect("group")["target_skew_ms"],
        70
    );
    delete(&restarted, "/api/v1/sync-groups/wall").await;
    unrelated_source_commit(&restarted).await;
    assert!(read(&restart(&path), "/api/v1/sync-groups/wall")
        .await
        .is_none());
}

#[tokio::test]
async fn salvo_and_tally_profile_edits_survive_restart_and_unrelated_commits() {
    let (_dir, path, router) = running_with_desired().await;
    let salvo = |name: &str| json!({"id": "wide", "display_name": name, "tally": [{"cell": "cell_a", "color": "Red"}]});
    upsert(&router, "/api/v1/salvos/wide", true, &salvo("Wide")).await;
    upsert(&router, "/api/v1/salvos/wide", true, &salvo("Wide v2")).await;
    let profile = |color: &str| json!({"id": "main", "bit_colors": [{"bit": 0, "color": color}], "index_cells": [{"index": 0, "cell": "cell_a"}]});
    upsert(
        &router,
        "/api/v1/tally/profiles/main",
        true,
        &profile("Red"),
    )
    .await;
    upsert(
        &router,
        "/api/v1/tally/profiles/main",
        true,
        &profile("Green"),
    )
    .await;
    unrelated_source_commit(&router).await;
    let restarted = restart(&path);
    assert_eq!(
        read(&restarted, "/api/v1/salvos/wide")
            .await
            .expect("salvo")["display_name"],
        "Wide v2"
    );
    assert_eq!(
        read(&restarted, "/api/v1/tally/profiles/main")
            .await
            .expect("profile")["bit_colors"][0]["color"],
        "Green"
    );
    delete(&restarted, "/api/v1/salvos/wide").await;
    delete(&restarted, "/api/v1/tally/profiles/main").await;
    unrelated_source_commit(&restarted).await;
    let again = restart(&path);
    assert!(read(&again, "/api/v1/salvos/wide").await.is_none());
    assert!(read(&again, "/api/v1/tally/profiles/main").await.is_none());
}

fn etag_version(tag: &str) -> u64 {
    tag.trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .expect("numeric weak etag")
}

/// Edit `uri` twice, restart, then require the version to hold and the
/// pre-restart stale tag to be refused.
async fn etag_survives_restart(
    router: &Router,
    path: &Path,
    uri: &str,
    put_only: bool,
    first: &Value,
    second: &Value,
) {
    upsert(router, uri, put_only, first).await;
    let stale = etag(&send(router, get(uri, ADMIN_TOKEN)).await).unwrap();
    upsert(router, uri, put_only, second).await;
    let current = etag(&send(router, get(uri, ADMIN_TOKEN)).await).unwrap();
    assert!(etag_version(&current) > etag_version(&stale), "{uri}");
    let restarted = restart(path);
    let after = etag(&send(&restarted, get(uri, ADMIN_TOKEN)).await).unwrap();
    assert_eq!(after, current, "{uri}: restart moved the ETag");
    let refused = send(&restarted, put_json(uri, ADMIN_TOKEN, Some(&stale), second)).await;
    assert_eq!(
        refused.status(),
        StatusCode::PRECONDITION_FAILED,
        "{uri}: a pre-restart stale If-Match must be refused"
    );
}

#[tokio::test]
async fn resource_names_survive_restart() {
    let (_dir, path, router) = running_with_desired().await;
    let uri = "/api/v1/overlays/ov_clock";
    let body = json!({"kind": "clock", "target": "canvas"});
    upsert(&router, uri, false, &json!({"name": "first", "body": body})).await;
    upsert(
        &router,
        uri,
        false,
        &json!({"name": "renamed", "body": body}),
    )
    .await;
    let restarted = restart(&path);
    let value = body_json(send(&restarted, get(uri, ADMIN_TOKEN)).await).await;
    assert_eq!(value["name"], "renamed", "an acknowledged rename was lost");
}

#[tokio::test]
async fn resource_etags_survive_restart_for_every_family() {
    let (_dir, path, router) = running_with_desired().await;
    etag_survives_restart(
        &router,
        &path,
        "/api/v1/overlays/ov_clock",
        false,
        &json!({"name": "c", "body": {"kind": "clock", "target": "canvas"}}),
        &json!({"name": "c", "body": {"kind": "clock", "target": "canvas", "format": "%H:%M"}}),
    )
    .await;
    let salvo = |name: &str| json!({"id": "wide", "display_name": name, "tally": [{"cell": "cell_a", "color": "Red"}]});
    etag_survives_restart(
        &router,
        &path,
        "/api/v1/salvos/wide",
        true,
        &salvo("a"),
        &salvo("b"),
    )
    .await;
    let profile = |color: &str| json!({"id": "main", "bit_colors": [{"bit": 0, "color": color}], "index_cells": [{"index": 0, "cell": "cell_a"}]});
    etag_survives_restart(
        &router,
        &path,
        "/api/v1/tally/profiles/main",
        true,
        &profile("Red"),
        &profile("Green"),
    )
    .await;
}

/// With an accepted model present, whole-document validation rejects a dangling
/// reference before anything is stored, rather than poisoning the envelope.
#[tokio::test]
async fn a_dangling_reference_is_refused_before_any_durable_write() {
    let (_dir, path, router) = running_with_desired().await;
    let before = std::fs::read(path.parent().unwrap().join(".multiview/desired.json")).unwrap();
    let response = send(
        &router,
        post_json(
            "/api/v1/probes/orphan",
            ADMIN_TOKEN,
            &json!({"name": "p", "body": {
                "id": "orphan", "cell": "no_such_cell", "kind": "black", "luma_threshold": 16,
                "dwell": {"up_ms": 2000, "down_ms": 1000}, "severity": "Major", "latched": false
            }}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(read(&router, "/api/v1/probes/orphan").await.is_none());
    let after = std::fs::read(path.parent().unwrap().join(".multiview/desired.json")).unwrap();
    assert_eq!(before, after, "a refused edit must not touch the envelope");
}
