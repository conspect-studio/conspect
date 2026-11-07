//! A `SQLite` layout-projection fault after the desired envelope commits
//! (release gate, `docs/architecture/live-durable-desired-state.md`).
//!
//! The fault is real: `SQLite` triggers abort every write to `layouts`, so the
//! repository fails exactly as a full disk or a locked database would. The API
//! must keep answering committed truth with no restart, report the fault on
//! `GET /api/v1/config/boot-model`, and repair `layouts.db` both by its bounded
//! retry and, once that budget is spent, at the next commit.
#![cfg(feature = "sqlite")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::layout_projection::LayoutProjection;
use multiview_control::sqlite::SqliteRepository;
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository, LayoutInput, Repository,
};
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
path = "/tmp/layout-projection-fault.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

struct Rig {
    _dir: tempfile::TempDir,
    boot_path: std::path::PathBuf,
    db: sqlx::SqlitePool,
    router: Router,
    working: String,
}

/// A modeled run whose layout repository is a real `layouts.db`.
async fn rig(max_attempts: u32, base_delay: Duration) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
    let mut state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )));
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
    state.layout_projection = Arc::new(LayoutProjection::with_retry_policy(
        max_attempts,
        base_delay,
    ));
    let db = sqlx::SqlitePool::connect(&url).await.unwrap();
    let router = multiview_control::router(state.clone());
    Rig {
        _dir: dir,
        boot_path,
        db,
        router,
        working,
    }
}

const FAULT: [&str; 3] = [
    "CREATE TRIGGER fault_insert BEFORE INSERT ON layouts \
     BEGIN SELECT RAISE(ABORT, 'injected projection fault'); END;",
    "CREATE TRIGGER fault_update BEFORE UPDATE ON layouts \
     BEGIN SELECT RAISE(ABORT, 'injected projection fault'); END;",
    "CREATE TRIGGER fault_delete BEFORE DELETE ON layouts \
     BEGIN SELECT RAISE(ABORT, 'injected projection fault'); END;",
];
const CLEAR: [&str; 3] = [
    "DROP TRIGGER fault_insert",
    "DROP TRIGGER fault_update",
    "DROP TRIGGER fault_delete",
];

/// Make every write to `layouts` abort inside `SQLite`.
async fn inject_fault(db: &sqlx::SqlitePool) {
    for statement in FAULT {
        sqlx::query(statement).execute(db).await.unwrap();
    }
}

async fn clear_fault(db: &sqlx::SqlitePool) {
    for statement in CLEAR {
        sqlx::query(statement).execute(db).await.unwrap();
    }
}

/// `(id, name)` rows actually stored in `layouts.db`, id-sorted.
async fn rows(db: &sqlx::SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT id, name FROM layouts ORDER BY id")
        .fetch_all(db)
        .await
        .unwrap()
}

/// `(id, name)` of the accepted library, id-sorted.
fn accepted(rig: &Rig) -> Vec<(String, String)> {
    let mut library: Vec<_> = load_desired(&rig.boot_path)
        .unwrap()
        .unwrap()
        .layouts
        .into_iter()
        .map(|layout| (layout.id, layout.name))
        .collect();
    library.sort();
    library
}

async fn projection(router: &Router) -> Value {
    let response = send(router, get("/api/v1/config/boot-model", ADMIN_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await["layout_projection"].clone()
}

/// Poll the reported projection status until `done` holds (30 s cap).
async fn wait_for(router: &Router, done: impl Fn(&Value) -> bool) -> Value {
    let end = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = projection(router).await;
        if done(&status) {
            return status;
        }
        assert!(
            std::time::Instant::now() < end,
            "projection status never settled: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn rename(router: &Router, uri: &str, name: &str) -> String {
    let current = send(router, get(uri, ADMIN_TOKEN)).await;
    assert_eq!(current.status(), StatusCode::OK, "{uri}");
    let tag = etag(&current).unwrap();
    let mut body = body_json(current).await;
    body["name"] = json!(name);
    let response = send(router, put_json(uri, ADMIN_TOKEN, Some(&tag), &body)).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    etag(&response).unwrap()
}

/// The committed library and `ETag`s are what every read returns.
async fn assert_reads_committed(rig: &Rig, uri: &str, name: &str, tag: &str) {
    let one = send(&rig.router, get(uri, ADMIN_TOKEN)).await;
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(etag(&one).as_deref(), Some(tag), "{uri}: stale ETag");
    assert_eq!(body_json(one).await["name"], name, "{uri}: stale name");
    let listed = body_json(send(&rig.router, get("/api/v1/layouts", ADMIN_TOKEN)).await).await;
    let mut ids: Vec<(String, String)> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|layout| {
            (
                layout["id"].as_str().unwrap().to_owned(),
                layout["name"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    ids.sort();
    assert_eq!(ids, accepted(rig), "list must serve the accepted library");
    // The running document composes from accepted intent, not the projection.
    let export = send(&rig.router, get("/api/v1/config/export", ADMIN_TOKEN)).await;
    assert_eq!(export.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_projection_serves_committed_truth_and_repairs_itself() {
    // A budget far longer than the test, so only the retry can repair this.
    let rig = rig(12, Duration::from_millis(100)).await;
    let working = format!("/api/v1/layouts/{}", rig.working);
    // A first commit while healthy writes the envelope and projects it.
    let pre_fault = rename(&rig.router, &working, "healthy").await;
    let status = wait_for(&rig.router, |s| s["retrying"] == false).await;
    assert_eq!(status["degraded"], false, "{status}");
    assert_eq!(rows(&rig.db).await, accepted(&rig));

    inject_fault(&rig.db).await;
    // Edit, create and delete while every projection write aborts.
    let edited = rename(&rig.router, &working, "edited under fault").await;
    let created = send(
        &rig.router,
        post_json(
            "/api/v1/layouts/spare",
            ADMIN_TOKEN,
            &json!({"name": "Spare", "body": {"canvas": {}, "cells": []}}),
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let doomed = send(
        &rig.router,
        post_json(
            "/api/v1/layouts/doomed",
            ADMIN_TOKEN,
            &json!({"name": "Doomed", "body": {"canvas": {}, "cells": []}}),
        ),
    )
    .await;
    let doomed_tag = etag(&doomed).unwrap();
    let delete = Request::builder()
        .method("DELETE")
        .uri("/api/v1/layouts/doomed")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, &doomed_tag)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&rig.router, delete).await.status(),
        StatusCode::NO_CONTENT
    );

    // The fault is visible, and the API still serves what was committed.
    let status = wait_for(&rig.router, |s| s["degraded"] == true).await;
    // The database really did refuse: it still holds the pre-fault library.
    assert_eq!(
        rows(&rig.db).await,
        [(rig.working.clone(), "healthy".into())]
    );
    assert!(
        status["last_error"]
            .as_str()
            .unwrap()
            .contains("injected projection fault"),
        "{status}"
    );
    assert_reads_committed(&rig, &working, "edited under fault", &edited).await;
    assert_eq!(
        send(&rig.router, get("/api/v1/layouts/doomed", ADMIN_TOKEN))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // The pre-fault tag still matches the projection's row, so only the
    // committed version can refuse it.
    let stale = send(
        &rig.router,
        put_json(
            &working,
            ADMIN_TOKEN,
            Some(&pre_fault),
            &json!({"name": "lost update", "body": {}}),
        ),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

    // Bounded retry repairs the projection once the fault clears: no restart,
    // no further request.
    clear_fault(&rig.db).await;
    let status = wait_for(&rig.router, |s| {
        s["degraded"] == false && s["retrying"] == false
    })
    .await;
    assert!(status["repaired_at_ms"].is_i64(), "{status}");
    assert_eq!(rows(&rig.db).await, accepted(&rig));
    assert_reads_committed(&rig, &working, "edited under fault", &edited).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spent_retry_budget_is_repaired_by_the_next_commit() {
    let rig = rig(4, Duration::from_millis(20)).await;
    let working = format!("/api/v1/layouts/{}", rig.working);
    rename(&rig.router, &working, "healthy").await;
    wait_for(&rig.router, |s| s["retrying"] == false).await;
    inject_fault(&rig.db).await;
    let edited = rename(&rig.router, &working, "edited under fault").await;
    // Four attempts at 20/40/80 ms: the budget is spent and the task stops.
    let status = wait_for(&rig.router, |s| {
        s["degraded"] == true && s["retrying"] == false
    })
    .await;
    assert_eq!(status["degraded"], true, "{status}");
    assert_eq!(status["consecutive_failures"], 4, "{status}");
    clear_fault(&rig.db).await;
    // Nothing is retrying, so the projection stays stale (and says so) while
    // reads still serve the committed library.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(projection(&rig.router).await["degraded"], true);
    assert_eq!(
        rows(&rig.db).await,
        [(rig.working.clone(), "healthy".into())]
    );
    assert_reads_committed(&rig, &working, "edited under fault", &edited).await;
    // The next commit of any family is a repair point.
    let source = send(&rig.router, get("/api/v1/sources/in_a", ADMIN_TOKEN)).await;
    let tag = etag(&source).unwrap();
    let response = send(
        &rig.router,
        put_json(
            "/api/v1/sources/in_a",
            ADMIN_TOKEN,
            Some(&tag),
            &json!({"name": "in_a", "body": {"id": "in_a", "kind": "solid", "color": "#203040"}}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let status = wait_for(&rig.router, |s| {
        s["degraded"] == false && s["retrying"] == false
    })
    .await;
    assert_eq!(status["consecutive_failures"], 0, "{status}");
    assert_eq!(rows(&rig.db).await, accepted(&rig));
}
