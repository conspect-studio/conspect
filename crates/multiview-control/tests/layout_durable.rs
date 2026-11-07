//! Durable layout mutations do not depend on the derived repository projection.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, ControlError, ControlResult, EngineStateSnapshot, InMemoryRepository,
    LayoutInput, Repository, VersionedLayout,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use std::sync::Arc;
use support::{get, put_json, send, OPERATOR_TOKEN};

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
path = "/tmp/layout-durable.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

struct FailedProjection;
impl Repository for FailedProjection {
    fn list_layouts(&self) -> ControlResult<Vec<VersionedLayout>> {
        Err(fault())
    }
    fn get_layout(&self, _: &str) -> ControlResult<VersionedLayout> {
        Err(fault())
    }
    fn create_layout(&self, _: &str, _: LayoutInput) -> ControlResult<VersionedLayout> {
        Err(fault())
    }
    fn update_layout(&self, _: &str, _: LayoutInput) -> ControlResult<VersionedLayout> {
        Err(fault())
    }
    fn delete_layout(&self, _: &str) -> ControlResult<()> {
        Err(fault())
    }
}
fn fault() -> ControlError {
    ControlError::Repository("projection unavailable".into())
}

#[tokio::test]
async fn committed_layout_survives_projection_failure() {
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
    let id = state.working_layout_id.clone().unwrap();
    let resource = format!("/api/v1/layouts/{id}");
    // Publish a first envelope while the initial repository is available.
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let desired_config: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, desired_config, layouts)
        .await
        .unwrap();
    let mut failed = state.clone();
    failed.repository = Arc::new(FailedProjection);
    let router = multiview_control::router(failed);
    let before = send(&router, get(&resource, OPERATOR_TOKEN)).await;
    assert_eq!(before.status(), StatusCode::OK);
    let etag = support::etag(&before).unwrap();
    let desired = load_desired(&path).unwrap().unwrap();
    let body = desired
        .layouts
        .iter()
        .find(|layout| layout.id == id)
        .unwrap()
        .body
        .clone();
    let replacement = serde_json::json!({"name":"Committed", "body": body});
    let response = send(
        &router,
        put_json(&resource, OPERATOR_TOKEN, Some(&etag), &replacement),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let next_etag = support::etag(&response).unwrap();
    assert_ne!(next_etag, etag);
    assert_eq!(
        load_desired(&path)
            .unwrap()
            .unwrap()
            .layouts
            .iter()
            .find(|layout| layout.id == id)
            .unwrap()
            .name,
        "Committed"
    );
    let observed = send(&router, get(&resource, OPERATOR_TOKEN)).await;
    assert_eq!(observed.status(), StatusCode::OK);
    assert_eq!(support::etag(&observed), Some(next_etag));
}

/// A stored layout authored with only canvas geometry (the shape the web UI
/// saves) applies onto the running canvas: omitted canvas fields keep their
/// running values instead of failing the apply with a 422.
#[tokio::test]
async fn applying_a_partial_canvas_layout_keeps_running_canvas_fields() {
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
    let router = multiview_control::router(state);
    let layout = serde_json::json!({
        "name": "Two up",
        "body": {
            "canvas": {"width": 64, "height": 64, "fps": "25/1"},
            "layout": {"kind": "grid", "columns": ["1fr", "1fr"], "rows": ["1fr"], "areas": ["a b"]},
            "cells": [
                {"id": "left", "area": "a", "source": {"input_id": "in_a"}},
                {"id": "right", "area": "b", "source": {"input_id": "in_a"}}
            ]
        }
    });
    let created = send(
        &router,
        support::post_json("/api/v1/layouts/two-up", OPERATOR_TOKEN, &layout),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let applied = send(
        &router,
        support::post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &serde_json::json!({"layout": "two-up"}),
        ),
    )
    .await;
    assert_eq!(applied.status(), StatusCode::ACCEPTED);
    let desired = load_desired(&path).unwrap().unwrap();
    let config = desired.config;
    assert_eq!(config.cells.len(), 2);
    assert_eq!(config.canvas.pixel_format, "nv12");
    assert_eq!(config.canvas.background, "#101014");
}
