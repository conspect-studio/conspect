//! Source/output HTTP reads, versions and preconditions use the accepted envelope,
//! even when their read/write projections fail after a confirmed commit.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, ControlError, ControlResult, EngineStateSnapshot, InMemoryRepository,
    ResourceInput, ResourceRepository, VersionedResource,
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
path = "/tmp/source-output-durable.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

struct FailedProjection;
fn fault() -> ControlError {
    ControlError::Repository("projection unavailable".into())
}
impl ResourceRepository for FailedProjection {
    fn kind(&self) -> &'static str {
        "projection"
    }
    fn list(&self) -> ControlResult<Vec<VersionedResource>> {
        Err(fault())
    }
    fn get(&self, _: &str) -> ControlResult<VersionedResource> {
        Err(fault())
    }
    fn create(&self, _: &str, _: ResourceInput) -> ControlResult<VersionedResource> {
        Err(fault())
    }
    fn update(&self, _: &str, _: ResourceInput) -> ControlResult<VersionedResource> {
        Err(fault())
    }
    fn delete(&self, _: &str) -> ControlResult<()> {
        Err(fault())
    }
}

#[tokio::test]
async fn accepted_sources_and_outputs_survive_failed_projection_with_stable_etags() {
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
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let prospective: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, prospective, layouts)
        .await
        .unwrap();
    let mut failed = state.clone();
    failed.sources = Arc::new(FailedProjection);
    failed.outputs = Arc::new(FailedProjection);
    let router = multiview_control::router(failed);

    let src_path = "/api/v1/sources/in_a";
    let before = send(&router, get(src_path, ADMIN_TOKEN)).await;
    assert_eq!(before.status(), StatusCode::OK);
    let old = etag(&before).unwrap();
    let replacement =
        json!({"name":"Accepted source", "body":{"id":"in_a", "kind":"solid", "color":"#abcdef"}});
    let updated = send(
        &router,
        put_json(src_path, ADMIN_TOKEN, Some(&old), &replacement),
    )
    .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let fresh = etag(&updated).unwrap();
    assert_ne!(fresh, old);
    assert_eq!(body_json(updated).await["name"], "Accepted source");
    let observed = send(&router, get(src_path, ADMIN_TOKEN)).await;
    assert_eq!(etag(&observed), Some(fresh.clone()));
    let stale = send(
        &router,
        put_json(src_path, ADMIN_TOKEN, Some(&old), &replacement),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    let listed = body_json(send(&router, get("/api/v1/sources", ADMIN_TOKEN)).await).await;
    assert_eq!(listed[0]["name"], "Accepted source");
    assert_eq!(
        load_desired(&path).unwrap().unwrap().config.sources[0].id,
        "in_a"
    );

    let output_path = "/api/v1/outputs/output-0";
    let before = send(&router, get(output_path, ADMIN_TOKEN)).await;
    assert_eq!(before.status(), StatusCode::OK);
    let old = etag(&before).unwrap();
    let body = body_json(before).await["body"].clone();
    let replacement = json!({"name":"Accepted output", "body": body});
    let updated = send(
        &router,
        put_json(output_path, ADMIN_TOKEN, Some(&old), &replacement),
    )
    .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let fresh = etag(&updated).unwrap();
    assert_ne!(fresh, old);
    assert_eq!(body_json(updated).await["name"], "Accepted output");
    let observed = send(&router, get(output_path, ADMIN_TOKEN)).await;
    assert_eq!(etag(&observed), Some(fresh));
    let stale = send(
        &router,
        put_json(output_path, ADMIN_TOKEN, Some(&old), &replacement),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    let listed = body_json(send(&router, get("/api/v1/outputs", ADMIN_TOKEN)).await).await;
    assert_eq!(listed[0]["name"], "Accepted output");
}
