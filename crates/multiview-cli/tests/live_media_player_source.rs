//! Issue #474 / ADR-M012: a `media_player` source added, re-pointed and
//! removed on the running engine. The drain binds the source id to the player
//! channel's store at the frame boundary — no producer is spawned (the player
//! boot owns its ingest), nothing is "unsupported".
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use multiview_cli::control::command_drain_with_live_sources;
use multiview_cli::live_sources::{shared_stores, stop_registry, LiveSourceHub};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::Event;
use multiview_framestore::TileStore;

fn config() -> MultiviewConfig {
    let doc = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "cam"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "cam"
[[media_players]]
id = "vt-1"
[[outputs]]
kind = "hls"
path = "/tmp/live-media-player-source.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse")
}

/// A drive with the declared source's store and the player channel's store,
/// as the pipeline registers them.
fn drive(config: &MultiviewConfig) -> (CompositorDrive<Nv12Image>, Arc<TileStore<Nv12Image>>) {
    let layout = config.solve_layout().expect("layout");
    let canvas = CanvasColor::default();
    let nosignal = Nv12Image::solid(64, 64, 16, 128, 128, canvas.output_tag()).unwrap();
    let player = Arc::new(TileStore::<Nv12Image>::with_defaults("vt-1".to_owned()));
    let stores = HashMap::from([
        (
            "cam".to_owned(),
            Arc::new(TileStore::<Nv12Image>::with_defaults("cam".to_owned())),
        ),
        ("vt-1".to_owned(), Arc::clone(&player)),
    ]);
    let drive = CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal,
        canvas,
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .unwrap();
    (drive, player)
}

#[tokio::test]
async fn live_media_player_source_aliases_the_player_store_and_spawns_nothing() {
    let config = config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, rx) = command_bus(16);
    let registry = stop_registry();
    let hub = LiveSourceHub::start(Arc::clone(&registry), shared_stores(HashMap::new()));
    let mut drain = command_drain_with_live_sources(rx, config.clone(), publisher, hub.handle());
    let (mut drive, player) = drive(&config);

    let source: multiview_config::Source = serde_json::from_value(
        serde_json::json!({"id": "vt-a", "kind": "media_player", "player": "vt-1"}),
    )
    .unwrap();
    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(source),
        })
        .unwrap();
    sender
        .try_submit(Command::SwapSource {
            op: OperationId::new(),
            tile: "cell_a".to_owned(),
            source: "vt-a".to_owned(),
        })
        .unwrap();
    drain(&mut drive);

    let alias = drive.store("vt-a").cloned().expect("alias registered");
    assert!(
        Arc::ptr_eq(&alias, &player),
        "the alias IS the player's store"
    );
    assert_eq!(
        drive.effective_cell_source("cell_a").as_deref(),
        Some("vt-a"),
        "a cell re-points onto the media_player source"
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        registry.lock().is_ok_and(|map| !map.contains_key("vt-a")),
        "no producer is spawned for an alias"
    );

    sender
        .try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: "vt-a".to_owned(),
        })
        .unwrap();
    drain(&mut drive);
    assert!(drive.store("vt-a").is_none(), "the alias is unregistered");
    assert!(
        drive.store("vt-1").is_some_and(|s| Arc::ptr_eq(s, &player)),
        "removing the alias never removes the player channel"
    );
    hub.shutdown();
}

#[tokio::test]
async fn a_live_alias_to_a_player_without_a_channel_reads_no_signal() {
    let config = config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, rx) = command_bus(16);
    let hub = LiveSourceHub::start(stop_registry(), shared_stores(HashMap::new()));
    let mut drain = command_drain_with_live_sources(rx, config.clone(), publisher, hub.handle());
    let (mut drive, _) = drive(&config);
    drive.remove_store("vt-1");
    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(
                serde_json::from_value(
                    serde_json::json!({"id": "vt-a", "kind": "media_player", "player": "vt-1"}),
                )
                .unwrap(),
            ),
        })
        .unwrap();
    drain(&mut drive);
    let alias = drive.store("vt-a").expect("still registered");
    assert!(!alias.is_primed(), "never fed: the tile reads no-signal");
    hub.shutdown();
}
