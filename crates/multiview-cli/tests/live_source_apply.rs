//! Live source add / edit / remove on the running engine (ADR-W018).
//!
//! The command drain applies `UpsertSource`/`RemoveSource` at the frame
//! boundary: cheap registration (store + route key + config mirror) on the
//! output-clock loop, with every heavy step (producer spawn/teardown, preview
//! registry) handed to the off-thread [`LiveSourceHub`] over a bounded channel.
//! These tests prove the vertical slice end-to-end on the software engine:
//!
//! * an upserted synthetic source becomes rebindable + produces real frames;
//! * an edit under the same id reuses the SAME `TileStore` (the tile holds
//!   last-good through the producer swap — never a slate flash);
//! * a remove slates bound cells at the next boundary and tears the producer
//!   down (bounded, off the hot path);
//! * a realtime run sees the added tile reach LIVE and the removed tile return
//!   to `NO_SIGNAL`, with the output **never faltering** (invariant #1);
//! * a continuous upsert/route/remove churn flood cannot stall the clock or
//!   skip a frame (invariants #1 + #10) — the soak gate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::control::command_drain_with_live_sources;
use multiview_cli::live_sources::{shared_stores, stop_registry, LiveSourceHub};
use multiview_cli::run::SoftwareEngine;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_core::time::MediaTime;
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;
use multiview_framestore::TileStore;

/// A 2-cell config with two declared bars sources bound to the cells.
fn two_cell_config() -> MultiviewConfig {
    let doc = r##"schema_version = 1
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "bars"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[outputs]]
kind = "hls"
path = "/tmp/live-source-apply.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse two-cell config")
}

/// A real `CompositorDrive` over the config's solved layout with one empty
/// registered store per declared source (the drain's frame-boundary target).
fn test_drive(config: &MultiviewConfig) -> CompositorDrive<Nv12Image> {
    let layout = config.solve_layout().expect("solve layout");
    let canvas_color = CanvasColor::default();
    let nosignal = Nv12Image::solid(
        config.canvas.width,
        config.canvas.height,
        16,
        128,
        128,
        canvas_color.output_tag(),
    )
    .expect("nosignal card");
    let mut stores = HashMap::new();
    for source in &config.sources {
        stores.insert(
            source.id.clone(),
            Arc::new(TileStore::<Nv12Image>::with_defaults(source.id.clone())),
        );
    }
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal,
        canvas_color,
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .expect("build drive")
}

/// Parse a validated `multiview_config::Source` from JSON (the same shape the
/// typed sources route stores after ADR-W015 validation).
fn source_doc(json: serde_json::Value) -> multiview_config::Source {
    serde_json::from_value(json).expect("valid source document")
}

/// Poll `predicate` every 10 ms until it holds or `deadline` elapses.
fn wait_for(deadline: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

#[tokio::test]
async fn upsert_source_registers_routes_and_produces_frames() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let registry = stop_registry();
    let preview = shared_stores(HashMap::new());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
    let mut drain = command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );
    let mut drive = test_drive(&config);

    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(source_doc(
                serde_json::json!({ "id": "live1", "kind": "bars" }),
            )),
        })
        .expect("submit upsert");
    sender
        .try_submit(Command::SwapSource {
            op: OperationId::new(),
            tile: "cell_a".to_owned(),
            source: "live1".to_owned(),
        })
        .expect("submit swap");
    drain(&mut drive);

    // Frame boundary: the store is registered and the cell re-pointed in the
    // SAME pass (FIFO — upsert before route).
    let store = drive
        .store("live1")
        .cloned()
        .expect("live1 store registered");
    assert_eq!(
        drive.effective_cell_source("cell_a").as_deref(),
        Some("live1"),
        "cell_a re-points to the live-added source at the frame boundary"
    );

    // The hub spawns the SAME generator_loop the startup path runs: the store
    // primes with a real bars frame shortly after (off the drain thread).
    assert!(
        wait_for(Duration::from_secs(5), || store.is_primed()),
        "the hub-spawned generator must publish into the live store"
    );
    // ... and the preview registry gains the input (the provider's id set is
    // dynamic now).
    assert!(
        wait_for(Duration::from_secs(5), || preview
            .load()
            .contains_key("live1")),
        "the live-added source must appear in the shared preview store map"
    );
    assert!(
        wait_for(Duration::from_secs(5), || {
            registry.lock().is_ok_and(|map| map.contains_key("live1"))
        }),
        "the live producer registers its per-source stop flag"
    );
    hub.shutdown();
}

#[tokio::test]
async fn remove_source_slates_bound_cells_and_tears_down_the_producer() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let registry = stop_registry();
    let preview = shared_stores(HashMap::new());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
    let mut drain = command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );
    let mut drive = test_drive(&config);

    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(source_doc(
                serde_json::json!({ "id": "live1", "kind": "bars" }),
            )),
        })
        .expect("submit upsert");
    sender
        .try_submit(Command::SwapSource {
            op: OperationId::new(),
            tile: "cell_a".to_owned(),
            source: "live1".to_owned(),
        })
        .expect("submit swap");
    drain(&mut drive);
    let store = drive
        .store("live1")
        .cloned()
        .expect("live1 store registered");
    assert!(wait_for(Duration::from_secs(5), || store.is_primed()));

    sender
        .try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: "live1".to_owned(),
        })
        .expect("submit remove");
    drain(&mut drive);

    // Composition plane: unregistered at the frame boundary; the bound cell
    // rides its on_loss slate with the honest NoSignal state.
    assert!(drive.store("live1").is_none(), "the store unregisters");
    let frame = drive
        .compose(multiview_engine::clock::Tick {
            index: 1,
            pts: MediaTime::from_nanos(40_000_000),
        })
        .expect("compose after remove");
    assert_eq!(
        frame.source_states.get("live1"),
        Some(&multiview_core::traits::SourceState::NoSignal),
        "a removed source's bound cell is honestly NO_SIGNAL"
    );

    // Producer plane (async, bounded, off the hot path): the stop flag is
    // raised + deregistered and the preview entry goes away; the generator
    // observes the flag and stops publishing (its sequence goes quiet).
    assert!(
        wait_for(Duration::from_secs(5), || {
            registry.lock().is_ok_and(|map| !map.contains_key("live1"))
        }),
        "the per-source stop flag deregisters on teardown"
    );
    assert!(
        wait_for(Duration::from_secs(5), || !preview
            .load()
            .contains_key("live1")),
        "the preview registry drops the removed source"
    );
    assert!(
        wait_for(Duration::from_secs(5), || {
            let seq = store.sequence();
            std::thread::sleep(Duration::from_millis(150));
            store.sequence() == seq
        }),
        "the torn-down generator must stop publishing"
    );
    hub.shutdown();
}

#[tokio::test]
async fn edit_reuses_the_store_and_swaps_the_producer() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let registry = stop_registry();
    let preview = shared_stores(HashMap::new());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
    let mut drain = command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );
    let mut drive = test_drive(&config);

    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(source_doc(
                serde_json::json!({ "id": "live1", "kind": "bars" }),
            )),
        })
        .expect("submit upsert");
    drain(&mut drive);
    let first = drive.store("live1").cloned().expect("store registered");
    assert!(wait_for(Duration::from_secs(5), || first.is_primed()));

    // EDIT: upsert the SAME id with a different synthetic kind. The store must
    // be reused (the tile holds last-good through the producer swap) and the
    // new producer's uniform solid frame must replace the bars.
    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(source_doc(serde_json::json!({
                "id": "live1", "kind": "solid", "color": "#22aa44"
            }))),
        })
        .expect("submit edit");
    drain(&mut drive);
    let second = drive
        .store("live1")
        .cloned()
        .expect("store still registered");
    assert!(
        Arc::ptr_eq(&first, &second),
        "an edit must reuse the SAME TileStore (the tile never flashes the slate)"
    );

    // The replacement producer publishes the solid colour into the same store.
    let now = || MediaTime::from_nanos(i64::MAX / 2);
    assert!(
        wait_for(Duration::from_secs(5), || {
            let read = second.read_at(now());
            let Some(frame) = read.frame() else {
                return false;
            };
            // A solid frame is uniform; the bars frame is not.
            let a = frame.sample(2, 2);
            a.is_some() && a == frame.sample(60, 60)
        }),
        "the edited producer must publish the solid frame into the reused store"
    );
    hub.shutdown();
}

/// SOAK (invariants #1 + #10): a continuous flood of live-source churn —
/// upserts, re-points to the churned source, removes — must never stall the
/// output clock or skip a frame: exactly N frames for N ticks, never faltered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_source_churn_flood_never_falters_the_output_clock() {
    use multiview_engine::{CooperativePacer, ManualTimeSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    const TICKS: u64 = 240;

    let cfg = two_cell_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");
    let (tx, rx) = command_bus(64);

    let registry = stop_registry();
    let preview = shared_stores(engine.preview_stores());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));

    // Background flooder: live-source churn at full speed for the whole run. A
    // full bus just sheds the submit (inv #10).
    let stop_flood = Arc::new(AtomicBool::new(false));
    let flooder = {
        let stop_flood = Arc::clone(&stop_flood);
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while !stop_flood.load(Ordering::Relaxed) {
                let id = format!("churn{}", n % 4);
                let _ = tx.try_submit(Command::UpsertSource {
                    op: OperationId::new(),
                    source: Box::new(
                        serde_json::from_value(serde_json::json!({ "id": id, "kind": "bars" }))
                            .expect("valid churn source"),
                    ),
                });
                let _ = tx.try_submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: "cell_b".to_owned(),
                    source: id.clone(),
                });
                let _ = tx.try_submit(Command::RemoveSource {
                    op: OperationId::new(),
                    id,
                });
                n = n.wrapping_add(1);
            }
        })
    };

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let drain =
        command_drain_with_live_sources(rx, cfg.clone(), Arc::clone(&publisher), hub.handle());

    let time = Arc::new(ManualTimeSource::new());
    // Bounded so a drive-loop regression that parks/spins the output clock under
    // this jumped-clock bounded run (the ADR-T018 cadence-hold `skip_to` vs
    // `max_ticks` desync that hung CI ~37 min) fails FAST here instead of hanging.
    // The honest sleep-free run finishes in well under a second.
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.run_for_with_control(Arc::clone(&time), CooperativePacer, TICKS, drain),
    )
    .await
    .expect("output clock stalled under live-source churn (bounded run never completed)")
    .expect("software run under live-source churn");

    stop_flood.store(true, Ordering::Relaxed);
    let _ = flooder.join();
    hub.shutdown();

    assert_eq!(
        report.frames, TICKS,
        "live-source churn must still produce exactly N frames for N ticks"
    );
    assert!(
        !report.faltered,
        "live-source churn must never falter the output clock (invariants #1 + #10)"
    );
}

/// REALTIME PROOF: on a wall-clock run, a live-added source's tile reaches
/// LIVE (observed via the engine's own `tile.state` events) and a live remove
/// returns it to `NO_SIGNAL` — while the output never falters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn realtime_live_add_goes_live_and_remove_slates() {
    let cfg = two_cell_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");
    let (tx, rx) = command_bus(16);

    let registry = stop_registry();
    let preview = shared_stores(engine.preview_stores());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let drain =
        command_drain_with_live_sources(rx, cfg.clone(), Arc::clone(&publisher), hub.handle());

    let stop = StopSignal::new();
    let mut sub = publisher.subscribe();

    // Driver: add + bind the live source, await its LIVE transition, remove it,
    // await NO_SIGNAL, then stop the run. Timeouts fail the test rather than
    // hanging it.
    let driver_stop = stop.clone();
    let driver = tokio::spawn(async move {
        tx.try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(
                serde_json::from_value(serde_json::json!({ "id": "live1", "kind": "bars" }))
                    .expect("valid source"),
            ),
        })
        .expect("submit upsert");
        tx.try_submit(Command::SwapSource {
            op: OperationId::new(),
            tile: "cell_a".to_owned(),
            source: "live1".to_owned(),
        })
        .expect("submit swap");

        let wait_state = |sub: &mut multiview_engine::EventSubscription<Event>,
                          want: multiview_events::LifecycleState| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if Instant::now() > deadline {
                    return false;
                }
                match sub.try_recv() {
                    Ok(envelope) => {
                        if let Event::TileState(ts) = envelope.event.as_ref() {
                            if ts.input.as_deref() == Some("live1") && ts.to == want {
                                return true;
                            }
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        };

        let went_live = tokio::task::block_in_place(|| {
            wait_state(&mut sub, multiview_events::LifecycleState::Live)
        });

        tx.try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: "live1".to_owned(),
        })
        .expect("submit remove");
        let went_dark = tokio::task::block_in_place(|| {
            wait_state(&mut sub, multiview_events::LifecycleState::NoSignal)
        });

        driver_stop.stop();
        (went_live, went_dark)
    });

    let report = engine
        .run_until_stopped_with_control(&stop, publisher.as_ref(), drain)
        .await
        .expect("realtime run with live add/remove");
    let (went_live, went_dark) = driver.await.expect("driver task");
    hub.shutdown();

    assert!(
        went_live,
        "the live-added source's tile must reach LIVE on a realtime run"
    );
    assert!(
        went_dark,
        "the removed source's tile must return to NO_SIGNAL"
    );
    assert!(
        !report.faltered,
        "live add/remove must never falter the output clock (invariant #1)"
    );
}

/// The startup generator path registers per-source stop flags in the shared
/// registry (the uniform teardown seam a live remove uses). Needs `overlay`
/// (the only startup-generated kind is the animated clock, which renders via
/// the overlay rasterizer).
#[cfg(feature = "overlay")]
#[test]
fn startup_generators_register_per_source_stop_flags() {
    let doc = r##"schema_version = 1
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
id = "clk"
kind = "clock"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "clk"
[[outputs]]
kind = "hls"
path = "/tmp/live-source-clk.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    let cfg = MultiviewConfig::load_from_toml(doc).expect("parse clock config");
    let engine = SoftwareEngine::build(&cfg).expect("build");
    let registry = engine.stop_registry();
    let generators = engine.spawn_generators();
    assert!(
        registry.lock().is_ok_and(|map| map.contains_key("clk")),
        "a startup generator must register its per-source stop flag"
    );
    generators.shutdown();
}

/// One raw HTTP/1.0 exchange against the bound control server.
async fn http(addr: std::net::SocketAddr, request: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

/// The registered producer for `id` (its stop/exit registration).
fn producer(
    registry: &multiview_cli::live_sources::StopRegistry,
    id: &str,
) -> Option<multiview_cli::live_sources::ProducerStop> {
    registry.lock().ok()?.get(id).cloned()
}

/// Review R3-03: on the software / no-demand run path, an automation-only
/// (and an automation-plus-metadata) edit through the GENERIC source PUT goes
/// through the REAL command drain and leaves the running synthetic producer
/// alone; a real media edit still swaps it.
#[allow(clippy::too_many_lines)] // one REST walk through the policy edits
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_policy_only_generic_put_never_restarts_a_synthetic_producer() {
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let registry = stop_registry();
    let preview = shared_stores(HashMap::new());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
    let mut drain = command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );
    let mut drive = test_drive(&config);
    // The running producer for `in_a` (the same hub spawn a live add uses).
    sender
        .try_submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(config.sources[0].clone()),
        })
        .unwrap();
    drain(&mut drive);
    assert!(wait_for(Duration::from_secs(5), || producer(
        &registry, "in_a"
    )
    .is_some()));
    let running = producer(&registry, "in_a").unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr, server, _state) = multiview_cli::control::bind_and_serve(
        "[::1]:0",
        &config,
        Arc::clone(&publisher),
        sender,
        multiview_control::no_preview(),
        None, // switcher absent (legacy path)
        None, // switcher lease owner absent
        None,
        None,
        None,
        None,
        None,
        multiview_control::LiveApplyCaps::default(),
        multiview_control::LiveSourceCapability::synthetic_only(),
        None,
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");
    let put = |version: u32, body: serde_json::Value| {
        let body = serde_json::json!({ "name": "in_a", "body": body }).to_string();
        format!(
            "PUT /api/v1/sources/in_a HTTP/1.0\r\nHost: {addr}\r\n\
             Authorization: Bearer admin.test-secret\r\nIf-Match: W/\"{version}\"\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let armed = serde_json::json!({ "automation": { "armed": true,
        "on_break_start": { "action": "swap_cell", "target": "in_b" } } });
    for (version, body) in [
        (
            1,
            serde_json::json!({ "id": "in_a", "kind": "bars", "events": armed }),
        ),
        (
            2,
            serde_json::json!({ "id": "in_a", "kind": "bars", "events": armed,
                "playlist_metadata": { "name": "Bars" }, "activation": "on_demand" }),
        ),
        (3, serde_json::json!({ "id": "in_a", "kind": "bars" })),
    ] {
        let response = http(addr, put(version, body.clone())).await;
        assert!(
            response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"),
            "{response}"
        );
        drain(&mut drive);
        std::thread::sleep(Duration::from_millis(200));
        let now = producer(&registry, "in_a").expect("producer still registered");
        assert!(
            now.is_same_producer(&running) && !running.stop_requested(),
            "a policy-only edit restarted the producer: {body}"
        );
    }
    // A real media edit still swaps the producer.
    let response = http(
        addr,
        put(
            4,
            serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44" }),
        ),
    )
    .await;
    assert!(response.contains(" 200 "), "{response}");
    drain(&mut drive);
    assert!(
        wait_for(Duration::from_secs(5), || producer(&registry, "in_a")
            .is_some_and(|now| !now.is_same_producer(&running))),
        "a media edit must replace the producer"
    );
    assert!(running.stop_requested());
    let _ = shutdown_tx.send(());
    let _ = server.await;
    hub.shutdown();
}

/// Whether `store` now carries the uniform solid frame (media B) rather than
/// the bars (media A).
fn shows_solid(store: &TileStore<Nv12Image>) -> bool {
    // The latest published frame, independent of the store's staleness
    // thresholds (the startup store uses the default ones).
    let Some(frame) = store.slot().load() else {
        return false;
    };
    let a = frame.sample(2, 2);
    a.is_some() && a == frame.sample(60, 60)
}

/// A boxed real command drain.
type Drain = Box<dyn FnMut(&mut CompositorDrive<Nv12Image>)>;

/// One running bars producer for `in_a` behind a real drain whose hub handle
/// can be forced to shed submissions.
struct ShedRig {
    sender: multiview_control::CommandSender,
    drain: Drain,
    drive: CompositorDrive<Nv12Image>,
    registry: multiview_cli::live_sources::StopRegistry,
    fault: multiview_cli::live_sources::SubmitFault,
    hub: LiveSourceHub,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    config: MultiviewConfig,
}

impl ShedRig {
    fn start() -> Self {
        let config = two_cell_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(16);
        let registry = stop_registry();
        let preview = shared_stores(HashMap::new());
        let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
        let fault = multiview_cli::live_sources::SubmitFault::default();
        let drain = command_drain_with_live_sources(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            hub.handle().with_fault(fault.clone()),
        );
        let mut rig = Self {
            sender,
            drain: Box::new(drain),
            drive: test_drive(&config),
            registry,
            fault,
            hub,
            publisher,
            config,
        };
        rig.upsert(serde_json::json!({ "id": "in_a", "kind": "bars" }));
        assert!(wait_for(Duration::from_secs(5), || producer(
            &rig.registry,
            "in_a"
        )
        .is_some()));
        rig
    }

    fn upsert(&mut self, body: serde_json::Value) {
        self.sender
            .try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source: Box::new(source_doc(body)),
            })
            .expect("submit upsert");
        (self.drain)(&mut self.drive);
    }

    fn running(&self) -> multiview_cli::live_sources::ProducerStop {
        producer(&self.registry, "in_a").expect("producer registered")
    }
}

/// Media B plus an armed automation rule (a policy-only change of B).
fn solid_with_policy() -> serde_json::Value {
    serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44",
        "events": { "automation": { "armed": true,
            "on_break_start": { "action": "swap_cell", "target": "in_b" } } } })
}

/// Shed A→B (via `shed`), recover, send B plus a policy edit: the producer
/// must become B (review R4-01).
fn shed_media_change_then_policy_edit_reaches_the_producer(
    shed: impl Fn(&multiview_cli::live_sources::SubmitFault),
) {
    let mut rig = ShedRig::start();
    let a = rig.running();
    shed(&rig.fault);
    rig.upsert(serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44" }));
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        rig.running().is_same_producer(&a) && !a.stop_requested(),
        "a shed submission must leave producer A running"
    );
    rig.fault.clear();
    rig.upsert(solid_with_policy());
    assert!(
        wait_for(Duration::from_secs(5), || producer(&rig.registry, "in_a")
            .is_some_and(|now| !now.is_same_producer(&a))),
        "the recovery upsert of B was swallowed: producer A kept running"
    );
    let store = rig.drive.store("in_a").cloned().expect("store");
    assert!(
        wait_for(Duration::from_secs(5), || shows_solid(&store)),
        "the producer must now run media B"
    );
    // Once B is applied, a further policy-only edit leaves it alone.
    let b = rig.running();
    rig.upsert(serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44" }));
    std::thread::sleep(Duration::from_millis(200));
    assert!(rig.running().is_same_producer(&b) && !b.stop_requested());
    rig.hub.shutdown();
}

/// Review R4-01 (1): a media change shed on a FULL hub queue, then B plus a
/// policy edit, ends with the producer running B.
#[test]
fn a_policy_edit_after_a_full_hub_shed_still_replaces_the_producer() {
    shed_media_change_then_policy_edit_reaches_the_producer(
        multiview_cli::live_sources::SubmitFault::full,
    );
}

/// Review R4-01 (2): the same with the hub GONE, then recovered.
#[test]
fn a_policy_edit_after_a_gone_hub_recovers_still_replaces_the_producer() {
    shed_media_change_then_policy_edit_reaches_the_producer(
        multiview_cli::live_sources::SubmitFault::gone,
    );
}

/// Review R4-01 (3): a guide-only REST edit sends no engine command, so the
/// drain's mirror goes stale. An identical re-apply of the latest REST body
/// (the documented retry) must reach the producer when the applied media
/// differs, and stays a no-op when the applied media already matches.
#[allow(clippy::too_many_lines)] // one REST walk through shed, guide edit, retry
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_reapply_after_a_guide_only_edit_retries_the_producer() {
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let rig = ShedRig::start();
    let ShedRig {
        sender,
        mut drain,
        mut drive,
        registry,
        fault,
        hub,
        publisher,
        config,
    } = rig;
    let a = producer(&registry, "in_a").unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr, server, _state) = multiview_cli::control::bind_and_serve(
        "[::1]:0",
        &config,
        Arc::clone(&publisher),
        sender,
        multiview_control::no_preview(),
        None, // switcher absent (legacy path)
        None, // switcher lease owner absent
        None,
        None,
        None,
        None,
        None,
        multiview_control::LiveApplyCaps::default(),
        multiview_control::LiveSourceCapability::synthetic_only(),
        None,
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");
    let put = |version: u32, body: &serde_json::Value| {
        let body = serde_json::json!({ "name": "in_a", "body": body }).to_string();
        format!(
            "PUT /api/v1/sources/in_a HTTP/1.0\r\nHost: {addr}\r\n\
             Authorization: Bearer admin.test-secret\r\nIf-Match: W/\"{version}\"\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let solid = serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44" });
    let guided = serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44",
        "playlist_metadata": { "name": "Green" } });
    let renamed = serde_json::json!({ "id": "in_a", "kind": "solid", "color": "#22aa44",
        "playlist_metadata": { "name": "Greener" } });

    // A→B is shed by a full hub.
    fault.full();
    let response = http(addr, put(1, &solid)).await;
    assert!(response.contains(" 200 "), "{response}");
    drain(&mut drive);
    fault.clear();
    std::thread::sleep(Duration::from_millis(100));
    assert!(producer(&registry, "in_a").unwrap().is_same_producer(&a));

    // A guide-only edit: committed, but no engine command.
    let response = http(addr, put(2, &guided)).await;
    assert!(response.contains(" 200 "), "{response}");
    drain(&mut drive);
    std::thread::sleep(Duration::from_millis(100));
    assert!(producer(&registry, "in_a").unwrap().is_same_producer(&a));

    // The documented retry: re-apply the latest body unchanged.
    let response = http(addr, put(3, &guided)).await;
    assert!(response.contains(" 200 "), "{response}");
    drain(&mut drive);
    assert!(
        wait_for(Duration::from_secs(5), || producer(&registry, "in_a")
            .is_some_and(|now| !now.is_same_producer(&a))),
        "the identical re-apply was swallowed: producer A kept running"
    );
    let store = drive.store("in_a").cloned().expect("store");
    assert!(
        wait_for(Duration::from_secs(5), || shows_solid(&store)),
        "the retried producer must run media B"
    );

    // B is applied: a guide-only edit then an identical re-apply is a no-op.
    let b = producer(&registry, "in_a").unwrap();
    let response = http(addr, put(4, &renamed)).await;
    assert!(response.contains(" 200 "), "{response}");
    drain(&mut drive);
    let response = http(addr, put(5, &renamed)).await;
    assert!(response.contains(" 200 "), "{response}");
    drain(&mut drive);
    std::thread::sleep(Duration::from_millis(200));
    let now = producer(&registry, "in_a").unwrap();
    assert!(
        now.is_same_producer(&b) && !b.stop_requested(),
        "a re-apply whose media is already applied must not restart the producer"
    );
    let _ = shutdown_tx.send(());
    let _ = server.await;
    hub.shutdown();
}
