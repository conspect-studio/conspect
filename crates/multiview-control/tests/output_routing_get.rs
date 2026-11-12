//! ADR-0108 slice 1 — `GET /api/v1/routing`: per-output feed readback,
//! filtered per object, with redaction of feeds the caller may not read.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::BootModel, command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{
    body_json, get, send, OPERATOR_TOKEN, OUTPUT_SCOPED_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

const DOC: &str = r##"
schema_version = 1
[canvas]
width = 640
height = 360
fps = "30000/1001"
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
id = "a"
kind = "test"
[[sources]]
id = "bars"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "/tmp/output-routing-get/a/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
kind = "hls"
path = "/tmp/output-routing-get/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn router_for(path: &Path, extra: &str) -> axum::Router {
    let config = MultiviewConfig::load_from_toml(&format!("{DOC}{extra}")).unwrap();
    config.validate().unwrap();
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
        path.to_path_buf(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )));
    multiview_control::router(state)
}

#[tokio::test]
async fn default_config_reads_back_program_follow_for_every_output() {
    let dir = tempfile::tempdir().unwrap();
    let router = router_for(&dir.path().join("boot.toml"), "");
    let resp = send(&router, get("/api/v1/routing", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let outputs = body["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 2);
    for (row, id) in outputs.iter().zip(["output-0", "output-1"]) {
        assert_eq!(row["output"], id);
        assert_eq!(row["video"], serde_json::json!({"bus": "program"}));
        assert_eq!(row["audio"], "follow");
        assert_eq!(row["effective_audio"], "program");
        assert_eq!(row["tally"], "program");
    }
}

#[tokio::test]
async fn a_source_feed_is_direct_and_carries_its_own_audio() {
    let dir = tempfile::tempdir().unwrap();
    let extra = "\n[[routing.output]]\noutput = \"hls /tmp/output-routing-get/b/index.m3u8\"\nvideo = { source = \"bars\" }\n";
    let router = router_for(&dir.path().join("boot.toml"), extra);
    let body = body_json(send(&router, get("/api/v1/routing", VIEWER_TOKEN)).await).await;
    let rows = body["outputs"].as_array().unwrap();
    assert_eq!(rows[0]["tally"], "program");
    assert_eq!(rows[1]["video"], serde_json::json!({"source": "bars"}));
    assert_eq!(rows[1]["tally"], "direct");
    assert_eq!(
        rows[1]["effective_audio"],
        serde_json::json!({"source": "bars"})
    );
}

#[tokio::test]
async fn rows_and_feeds_are_filtered_per_object() {
    let dir = tempfile::tempdir().unwrap();
    let router = router_for(&dir.path().join("boot.toml"), "");
    // Object-scoped to an unrelated id: no output rows at all (never a leak of ids).
    let body = body_json(send(&router, get("/api/v1/routing", SCOPED_TOKEN)).await).await;
    assert_eq!(body["outputs"], serde_json::json!([]));
    // Output-scoped without the Program scope: rows visible, the bus feed redacted.
    let body = body_json(send(&router, get("/api/v1/routing", OUTPUT_SCOPED_TOKEN)).await).await;
    let rows = body["outputs"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["video"], serde_json::json!({"redacted": true}));
    assert_eq!(rows[0]["tally"], "redacted");
}

#[tokio::test]
async fn unauthenticated_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let router = router_for(&dir.path().join("boot.toml"), "");
    let resp = send(&router, get("/api/v1/routing", "nope.nope")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
