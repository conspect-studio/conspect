//! Review R2-01/R2-02 (PR #190) for the rc3 families: a store write that
//! fails AFTER the desired envelope commits must never let a stale `ETag`
//! through, for graphics items and the IPTV publication.
//!
//! Graphics and IPTV have no resource store of their own; the store write a
//! commit of theirs performs after the envelope lands is the `SQLite` layout
//! projection. The fault is real (`SQLite` triggers abort every write to
//! `layouts`, as a full disk or locked database would). Under it, every
//! graphics/IPTV write must still report its committed tag, GET must serve the
//! committed body and tag, the pre-commit tag must be refused, a delete then
//! recreate must never reuse a tag, and a restart must keep the tag.
#![cfg(feature = "sqlite")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::layout_projection::LayoutProjection;
use multiview_control::routes::graphics::effective_graphics;
use multiview_control::sqlite::SqliteRepository;
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository, LayoutInput, Repository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_graphics::GraphicsHub;
use serde_json::{json, Value};
use support::{body_json, delete_if_match, etag, get, put_json, send, ADMIN_TOKEN};

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
path = "/tmp/rc3-families-projection-fault.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

const SOURCE: &str = "/api/v1/graphics/data-sources/sydney";
const ITEM: &str = "/api/v1/graphics/items/syd7";
const IPTV: &str = "/api/v1/iptv";

const FAULT: [&str; 3] = [
    "CREATE TRIGGER fault_insert BEFORE INSERT ON layouts \
     BEGIN SELECT RAISE(ABORT, 'injected projection fault'); END;",
    "CREATE TRIGGER fault_update BEFORE UPDATE ON layouts \
     BEGIN SELECT RAISE(ABORT, 'injected projection fault'); END;",
    "CREATE TRIGGER fault_delete BEFORE DELETE ON layouts \
     BEGIN SELECT RAISE(ABORT, 'injected projection fault'); END;",
];

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

struct Rig {
    _dir: tempfile::TempDir,
    path: PathBuf,
    db: sqlx::SqlitePool,
    router: Router,
}

/// A modeled run whose layout projection is a real `layouts.db`.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiview.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let mut state = state_for(&path, &config);
    let working = state.working_layout_id.clone().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("layouts.db").display()
    );
    let sqlite = SqliteRepository::connect(&url).await.unwrap();
    let seed = state.repository.get_layout(&working).unwrap();
    sqlite
        .create_layout(
            &working,
            LayoutInput {
                name: seed.layout.name,
                body: seed.layout.body,
            },
        )
        .unwrap();
    state.repository = Arc::new(sqlite);
    // A long backoff: nothing repairs the projection during the test.
    state.layout_projection = Arc::new(LayoutProjection::with_retry_policy(
        2,
        Duration::from_secs(30),
    ));
    let db = sqlx::SqlitePool::connect(&url).await.unwrap();
    Rig {
        _dir: dir,
        path,
        db,
        router: multiview_control::router(state),
    }
}

/// Make every `layouts` write abort, then diverge the accepted library from
/// the database so every later commit's projection really writes, and fails.
async fn inject_fault(rig: &Rig) {
    for statement in FAULT {
        sqlx::query(statement).execute(&rig.db).await.unwrap();
    }
    let created = send(
        &rig.router,
        support::post_json(
            "/api/v1/layouts/spare",
            ADMIN_TOKEN,
            &json!({"name": "Spare", "body": {"canvas": {}, "cells": []}}),
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
}

/// The projection really failed: degraded, and the row never landed.
async fn assert_projection_failed(rig: &Rig) {
    let end = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status =
            body_json(send(&rig.router, get("/api/v1/config/boot-model", ADMIN_TOKEN)).await).await
                ["layout_projection"]
                .clone();
        if status["degraded"] == true {
            break;
        }
        assert!(
            std::time::Instant::now() < end,
            "projection never failed: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM layouts WHERE id = 'spare'")
        .fetch_all(&rig.db)
        .await
        .unwrap();
    assert!(rows.is_empty(), "the injected fault did not hold");
}

fn restart(path: &Path) -> Router {
    let desired = load_desired(path).unwrap().expect("accepted envelope");
    let state = multiview_control::boot_model::restore_accepted_stores(
        state_for(path, &desired.config),
        &desired,
    )
    .unwrap();
    multiview_control::router(state)
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

fn item(name: &str) -> Value {
    json!({"name": name, "template": "weather.today", "data_source": "sydney"})
}

fn iptv(name: &str) -> Value {
    json!({"enabled": true, "entries": [{"kind": "output", "ref": "output-0", "name": name}]})
}

/// Graphics item: edit, delete and recreate while every projection write
/// fails after its commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graphics_item_store_write_failure_after_commit_never_authorizes_a_stale_tag() {
    let rig = rig().await;
    put_ok(
        &rig.router,
        SOURCE,
        None,
        &json!({"name": "Sydney", "kind": "bom_weather", "forecast_product": "IDN11060", "aac": "NSW_PT131"}),
    )
    .await;
    let old = put_ok(&rig.router, ITEM, None, &item("Syd")).await;
    inject_fault(&rig).await;

    let fresh = put_ok(&rig.router, ITEM, Some(&old), &item("Syd committed")).await;
    assert_ne!(fresh, old);
    assert_projection_failed(&rig).await;
    let (status, tag, body) = read(&rig.router, ITEM).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag.as_deref(), Some(fresh.as_str()), "GET tag");
    assert_eq!(body["name"], "Syd committed", "GET body");
    let stale = send(
        &rig.router,
        put_json(ITEM, ADMIN_TOKEN, Some(&old), &item("lost update")),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

    // Delete then recreate under the fault: the tag never repeats.
    let deleted = send(
        &rig.router,
        delete_if_match(ITEM, ADMIN_TOKEN, Some(&fresh)),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(read(&rig.router, ITEM).await.0, StatusCode::NOT_FOUND);
    let recreated = put_ok(&rig.router, ITEM, None, &item("Syd again")).await;
    assert_ne!(recreated, fresh, "a recreate reused the old tag");
    assert_ne!(recreated, old, "a recreate reused the old tag");
    for tag in [&old, &fresh] {
        let stale = send(
            &rig.router,
            put_json(ITEM, ADMIN_TOKEN, Some(tag), &item("lost update")),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    }

    // A restart keeps the tag and still refuses every earlier incarnation's.
    let restarted = restart(&rig.path);
    let (status, tag, body) = read(&restarted, ITEM).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        tag.as_deref(),
        Some(recreated.as_str()),
        "restart moved the tag"
    );
    assert_eq!(body["name"], "Syd again");
    for tag in [&old, &fresh] {
        let stale = send(
            &restarted,
            put_json(ITEM, ADMIN_TOKEN, Some(tag), &item("lost update")),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    }
}

/// IPTV: a publication commit whose projection write fails still reports the
/// committed (durable-version) tag; the pre-commit tag is refused, before and
/// after a restart, and the restart keeps the tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iptv_store_write_failure_after_commit_never_authorizes_a_stale_tag() {
    let rig = rig().await;
    let initial = read(&rig.router, IPTV).await.1.unwrap();
    let old = put_ok(&rig.router, IPTV, Some(&initial), &iptv("A")).await;
    inject_fault(&rig).await;

    let fresh = put_ok(&rig.router, IPTV, Some(&old), &iptv("B")).await;
    assert_ne!(fresh, old);
    assert_projection_failed(&rig).await;
    let (status, tag, body) = read(&rig.router, IPTV).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag.as_deref(), Some(fresh.as_str()));
    assert_eq!(body["config"]["entries"][0]["name"], "B");
    for tag in [&initial, &old] {
        let stale = send(
            &rig.router,
            put_json(IPTV, ADMIN_TOKEN, Some(tag), &iptv("lost")),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    }
    // Back to identical content: the durable version still moves the tag.
    let again = put_ok(&rig.router, IPTV, Some(&fresh), &iptv("A")).await;
    assert_ne!(again, old, "an A-B-A edit repeated a tag under the fault");

    let restarted = restart(&rig.path);
    let (status, tag, body) = read(&restarted, IPTV).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        tag.as_deref(),
        Some(again.as_str()),
        "restart moved the tag"
    );
    assert_eq!(body["config"]["entries"][0]["name"], "A");
    let stale = send(
        &restarted,
        put_json(IPTV, ADMIN_TOKEN, Some(&old), &iptv("lost")),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
}
