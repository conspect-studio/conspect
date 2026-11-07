//! Review H2 (PR #190): the FIRST desired-envelope commit must continue the
//! legacy repository versions, never restart them. Otherwise the first changed
//! PUT against the initial `W/"1"` returns `W/"1"` again and a concurrent stale
//! PUT carrying that same tag passes the precondition.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository, LayoutInput,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::json;
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
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/desired-first-commit.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// A run with NO desired envelope yet: the legacy stores are authoritative.
fn fresh() -> (tempfile::TempDir, std::path::PathBuf, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
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
    )));
    assert!(load_desired(&path).unwrap().is_none());
    (dir, path, state)
}

fn version(tag: &str) -> u64 {
    tag.trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .expect("numeric weak etag")
}

/// First changed PUT strictly advances the tag; the stale tag then gets 412.
async fn first_put_advances(router: &Router, uri: &str, replacement: &serde_json::Value) {
    let before = send(router, get(uri, ADMIN_TOKEN)).await;
    assert_eq!(before.status(), StatusCode::OK, "{uri}");
    let initial = etag(&before).unwrap();
    let first = send(
        router,
        put_json(uri, ADMIN_TOKEN, Some(&initial), replacement),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK, "{uri}");
    let fresh = etag(&first).unwrap();
    assert!(
        version(&fresh) > version(&initial),
        "{uri}: first commit must advance {initial} (got {fresh})"
    );
    let stale = send(
        router,
        put_json(uri, ADMIN_TOKEN, Some(&initial), replacement),
    )
    .await;
    assert_eq!(
        stale.status(),
        StatusCode::PRECONDITION_FAILED,
        "{uri}: a stale If-Match must be refused"
    );
}

#[tokio::test]
async fn first_source_commit_advances_the_etag_and_refuses_the_stale_tag() {
    let (_dir, _path, state) = fresh();
    let router = multiview_control::router(state);
    first_put_advances(
        &router,
        "/api/v1/sources/in_a",
        &json!({"name": "a", "body": {"id": "in_a", "kind": "solid", "color": "#abcdef"}}),
    )
    .await;
}

#[tokio::test]
async fn first_output_commit_advances_the_etag_and_refuses_the_stale_tag() {
    let (_dir, _path, state) = fresh();
    let router = multiview_control::router(state);
    let body = body_json(send(&router, get("/api/v1/outputs/output-0", ADMIN_TOKEN)).await).await
        ["body"]
        .clone();
    let mut changed = body.clone();
    changed["segment_ms"] = json!(2000);
    first_put_advances(
        &router,
        "/api/v1/outputs/output-0",
        &json!({"name": "renamed", "body": changed}),
    )
    .await;
}

#[tokio::test]
async fn first_commit_preserves_an_advanced_source_counter() {
    // A legacy store counter already past 1 must not be reset by an
    // unrelated first envelope commit (an output edit).
    let (_dir, _path, state) = fresh();
    state
        .sources
        .update(
            "in_a",
            multiview_control::ResourceInput {
                name: "in_a".into(),
                body: json!({"id": "in_a", "kind": "solid", "color": "#010203"}),
            },
        )
        .unwrap();
    state
        .sources
        .update(
            "in_a",
            multiview_control::ResourceInput {
                name: "in_a".into(),
                body: json!({"id": "in_a", "kind": "solid", "color": "#040506"}),
            },
        )
        .unwrap();
    let router = multiview_control::router(state);
    let before = etag(&send(&router, get("/api/v1/sources/in_a", ADMIN_TOKEN)).await).unwrap();
    assert_eq!(version(&before), 3);
    let body = body_json(send(&router, get("/api/v1/outputs/output-0", ADMIN_TOKEN)).await).await
        ["body"]
        .clone();
    let out_tag = etag(&send(&router, get("/api/v1/outputs/output-0", ADMIN_TOKEN)).await).unwrap();
    let mut changed = body;
    changed["segment_ms"] = json!(2000);
    let response = send(
        &router,
        put_json(
            "/api/v1/outputs/output-0",
            ADMIN_TOKEN,
            Some(&out_tag),
            &json!({"name": "hls", "body": changed}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let after = etag(&send(&router, get("/api/v1/sources/in_a", ADMIN_TOKEN)).await).unwrap();
    assert!(
        version(&after) >= version(&before),
        "unaffected counter regressed from {before} to {after}"
    );
}

#[tokio::test]
async fn first_layout_commit_continues_an_existing_repository_version() {
    let (_dir, _path, state) = fresh();
    let id = state.working_layout_id.clone().unwrap();
    // A pre-existing repository layout already at version 3.
    let current = state.repository.get_layout(&id).unwrap();
    for name in ["v2", "v3"] {
        state
            .repository
            .update_layout(
                &id,
                LayoutInput {
                    name: name.into(),
                    body: current.layout.body.clone(),
                },
            )
            .unwrap();
    }
    assert_eq!(state.repository.get_layout(&id).unwrap().version.get(), 3);
    let router = multiview_control::router(state);
    let uri = format!("/api/v1/layouts/{id}");
    let before = send(&router, get(&uri, ADMIN_TOKEN)).await;
    let initial = etag(&before).unwrap();
    assert_eq!(version(&initial), 3);
    let replacement = json!({"name": "renamed", "body": current.layout.body});
    let first = send(
        &router,
        put_json(&uri, ADMIN_TOKEN, Some(&initial), &replacement),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let advanced = etag(&first).unwrap();
    assert!(
        version(&advanced) > 3,
        "first layout commit reset the repository counter: {advanced}"
    );
    let refused = send(
        &router,
        put_json(&uri, ADMIN_TOKEN, Some(&initial), &replacement),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::PRECONDITION_FAILED);
}
