//! Review of #316 (delta): `GET /api/v1/routing` row ids come from each output
//! resource with its own body, never a positional `output-{n}` (BOLA).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, AppState, EngineStateSnapshot, InMemoryRepository};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_json, get, send, OPERATOR_TOKEN};

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

/// Review #316 (delta) finding: with no accepted envelope, the old handler
/// synthesized `output-{n}` and zipped by position. Here the store's output
/// resources carry ids that are neither `output-{n}` nor in config order; each
/// row must keep its own resource's id with that resource's own feed.
#[tokio::test]
async fn store_only_rows_use_each_resource_id_with_its_own_body() {
    use multiview_control::{InMemoryOutputStore, ResourceInput, ResourceRepository};
    let extra = "\n[[routing.output]]\noutput = \"hls /tmp/output-routing-get/b/index.m3u8\"\nvideo = { source = \"bars\" }\n";
    let config = MultiviewConfig::load_from_toml(&format!("{DOC}{extra}")).unwrap();
    let mut seeded = multiview_control::seed_resources(&config).unwrap();
    // Re-key the outputs: the SECOND config output (the routed one) sorts first.
    let store = InMemoryOutputStore::new();
    let bodies: Vec<_> = config
        .outputs
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    store
        .create(
            "zz-plain",
            ResourceInput {
                name: "hls".into(),
                body: bodies[0].clone(),
            },
        )
        .unwrap();
    store
        .create(
            "aa-routed",
            ResourceInput {
                name: "hls".into(),
                body: bodies[1].clone(),
            },
        )
        .unwrap();
    seeded.outputs = Arc::new(store);
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap());
    let router = multiview_control::router(state);
    let body = body_json(send(&router, get("/api/v1/routing", OPERATOR_TOKEN)).await).await;
    let rows = body["outputs"].as_array().unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["output"] == id).cloned();
    assert_eq!(
        by_id("aa-routed").unwrap()["video"],
        serde_json::json!({"source": "bars"}),
        "{body}"
    );
    assert_eq!(by_id("zz-plain").unwrap()["tally"], "program", "{body}");
    assert!(
        by_id("output-0").is_none() && by_id("output-1").is_none(),
        "{body}"
    );
}
