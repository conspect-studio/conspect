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
[[media_players]]
id = "vt-2"
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
        (
            "vt-2".to_owned(),
            Arc::new(TileStore::<Nv12Image>::with_defaults("vt-2".to_owned())),
        ),
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

fn alias(id: &str, player: &str) -> Box<multiview_config::Source> {
    Box::new(
        serde_json::from_value(
            serde_json::json!({"id": id, "kind": "media_player", "player": player}),
        )
        .unwrap(),
    )
}

/// Review A1/C2/C1: live add, repoint between two audio-bearing players, reuse,
/// and a kind change move the alias's audio with its video; the kind change
/// gets an independent store and the player's store is untouched.
#[tokio::test]
async fn live_alias_moves_audio_with_video_and_a_kind_change_never_shares_the_player_store() {
    let config = config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, rx) = command_bus(16);
    let hub = LiveSourceHub::start(stop_registry(), shared_stores(HashMap::new()));
    let (mut drive, player) = drive(&config);
    let player2 = drive.store("vt-2").cloned().unwrap();
    let audio = Arc::new(multiview_cli::audio::AudioControlHandle::new());
    let audio_slot = Arc::new(multiview_framestore::SwapCell::new(Some(Arc::clone(
        &audio,
    ))));
    let layout = Arc::new(multiview_framestore::SwapCell::from_pointee(
        config.solve_layout().unwrap(),
    ));
    let mut drain = multiview_cli::control::command_drain_with_audio_and_players(
        rx,
        config.clone(),
        publisher,
        audio_slot,
        hub.handle(),
        HashMap::new(),
        layout,
    );
    let submit = |command: Command| sender.try_submit(command).unwrap();

    // Live add: video aliases vt-1's store AND audio resolves to vt-1.
    submit(Command::UpsertSource {
        op: OperationId::new(),
        source: alias("vt-a", "vt-1"),
    });
    drain(&mut drive);
    assert!(Arc::ptr_eq(drive.store("vt-a").unwrap(), &player));
    assert_eq!(audio.channel_id("vt-a"), "vt-1");

    // Repoint A -> B: both move in the same frame boundary.
    submit(Command::UpsertSource {
        op: OperationId::new(),
        source: alias("vt-a", "vt-2"),
    });
    drain(&mut drive);
    assert!(Arc::ptr_eq(drive.store("vt-a").unwrap(), &player2));
    assert_eq!(audio.channel_id("vt-a"), "vt-2", "A's audio is not kept");
    assert!(
        Arc::ptr_eq(drive.store("vt-1").unwrap(), &player),
        "vt-1 untouched"
    );

    // Kind change to a producer: independent store, alias audio withdrawn.
    submit(Command::UpsertSource {
        op: OperationId::new(),
        source: Box::new(
            serde_json::from_value(serde_json::json!({"id": "vt-a", "kind": "bars"})).unwrap(),
        ),
    });
    drain(&mut drive);
    let own = drive.store("vt-a").cloned().expect("producer store");
    assert!(!Arc::ptr_eq(&own, &player) && !Arc::ptr_eq(&own, &player2));
    assert!(
        Arc::ptr_eq(drive.store("vt-2").unwrap(), &player2),
        "player store kept"
    );
    assert_eq!(audio.channel_id("vt-a"), "vt-a", "alias withdrawn");
    // The bars producer must never publish into a player's store.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !player2.is_primed() && !player.is_primed(),
        "no interleaved frames"
    );

    // Reuse: back to an alias of vt-1.
    submit(Command::UpsertSource {
        op: OperationId::new(),
        source: alias("vt-a", "vt-1"),
    });
    drain(&mut drive);
    assert!(Arc::ptr_eq(drive.store("vt-a").unwrap(), &player));
    assert_eq!(audio.channel_id("vt-a"), "vt-1");

    // Remove: the alias leaves video and audio; the player channel stays.
    submit(Command::RemoveSource {
        op: OperationId::new(),
        id: "vt-a".to_owned(),
    });
    drain(&mut drive);
    assert!(drive.store("vt-a").is_none());
    assert_eq!(audio.channel_id("vt-a"), "vt-a");
    assert!(Arc::ptr_eq(drive.store("vt-1").unwrap(), &player));

    // A producer can never take a player's id (its store is the channel).
    submit(Command::UpsertSource {
        op: OperationId::new(),
        source: Box::new(
            serde_json::from_value(serde_json::json!({"id": "vt-2", "kind": "bars"})).unwrap(),
        ),
    });
    drain(&mut drive);
    assert!(Arc::ptr_eq(drive.store("vt-2").unwrap(), &player2));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !player2.is_primed(),
        "no producer writes into the player channel"
    );
    hub.shutdown();
}

/// Merge review (#474 x #477): a producer -> `media_player` alias transition
/// while the hub queue is FULL keeps the replaced producer's teardown pending
/// and retries it until the producer stops (never left to run teardown), and
/// new aliases are admitted only under the live source cap, counting an id
/// that is both an alias and a pending teardown once.
#[test]
fn a_shed_producer_to_alias_teardown_is_retried_and_aliases_respect_the_cap() {
    let config = config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, rx) = command_bus(16);
    let registry = stop_registry();
    let hub = LiveSourceHub::start(Arc::clone(&registry), shared_stores(HashMap::new()));
    let fault = multiview_cli::live_sources::SubmitFault::default();
    let (mut drive, player) = drive(&config);
    let boot = multiview_cli::control::CommandDrain::new(rx, config.clone(), publisher)
        .with_live_sources(hub.handle().with_fault(fault.clone()));
    let cap = boot.applied_sources() + 2;
    let mut drain = boot.with_source_cap(cap);
    let submit = |drain: &mut multiview_cli::control::CommandDrain,
                  drive: &mut CompositorDrive<Nv12Image>,
                  source: Box<multiview_config::Source>| {
        sender
            .try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source,
            })
            .unwrap();
        drain.apply(drive);
    };
    let running = |id: &str| registry.lock().unwrap().get(id).cloned();
    let wait = |mut f: Box<dyn FnMut() -> bool>| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        f()
    };

    // A hub-known bars producer under `p`.
    submit(
        &mut drain,
        &mut drive,
        Box::new(serde_json::from_value(serde_json::json!({"id": "p", "kind": "bars"})).unwrap()),
    );
    assert!(
        wait(Box::new(|| running("p").is_some())),
        "producer running"
    );
    let producer = running("p").unwrap();

    // The queue fills; `p` becomes an alias of vt-1. Its teardown is shed.
    fault.full();
    submit(&mut drain, &mut drive, alias("p", "vt-1"));
    assert!(
        Arc::ptr_eq(drive.store("p").unwrap(), &player),
        "alias bound"
    );
    assert_eq!(drain.pending_teardowns(), 1, "the shed teardown is kept");
    assert!(!producer.stop_requested(), "still pending while full");

    // Cap: `p` (alias + pending teardown) counts once, so one more alias fits
    // and the next is held.
    submit(&mut drain, &mut drive, alias("q", "vt-1"));
    assert!(drive.store("q").is_some(), "q admitted under the cap");
    submit(&mut drain, &mut drive, alias("r", "vt-1"));
    assert!(drive.store("r").is_none(), "r held at the cap");
    assert_eq!(drain.applied_sources(), cap);

    // The queue recovers: the retried teardown stops the old producer.
    fault.clear();
    drain.apply(&mut drive);
    assert_eq!(drain.pending_teardowns(), 0, "teardown reached the hub");
    assert!(
        wait(Box::new(
            || producer.stop_requested() && running("p").is_none()
        )),
        "the replaced producer stopped"
    );
    assert!(
        Arc::ptr_eq(drive.store("p").unwrap(), &player),
        "alias kept"
    );
    hub.shutdown();
}
