//! End-to-end software smoke of invariant #1 via the `run --software` path.
//!
//! This wires the engine the CLI builds — output clock + CPU reference
//! compositor + per-source framestores fed by built-in test-pattern sources —
//! and drives it deterministically (manual time source + cooperative pacer, no
//! real sleeps). It asserts the load-bearing property of the whole product:
//! **exactly N frames for N ticks, at the configured cadence, output never
//! faltered** — independent of input health.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use multiview_cli::run::SoftwareEngine;
use multiview_config::MultiviewConfig;
use multiview_engine::{CooperativePacer, ManualTimeSource};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join(name)
}

fn load(name: &str) -> MultiviewConfig {
    let text = std::fs::read_to_string(example(name)).expect("read example");
    let cfg = MultiviewConfig::load_from_toml(&text).expect("parse example");
    cfg.validate().expect("example validates");
    cfg
}

/// A small (320x240) 2x2 grid of built-in test sources, exercising the full
/// config parse + grid-solve path. The CPU reference compositor is O(pixels x
/// tiles) per frame, so a small canvas keeps the deterministic frame-count and
/// PTS tests fast in a debug build while proving the exact same invariant the
/// 1080p example would (frames == ticks, PTS = f(tick)).
fn small_config() -> MultiviewConfig {
    let toml = r##"
schema_version = 1

[canvas]
width = 320
height = 240
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
kind = "test"

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
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "in_c"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "in_d"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;
    let cfg = MultiviewConfig::load_from_toml(toml).expect("parse small config");
    cfg.validate().expect("small config validates");
    cfg
}

/// The control plane serves the API **while** the engine is running, sharing the
/// engine's outbound publisher — and the engine's output never falters under a
/// concurrent client (invariants #1 + #10). Also asserts the compact engine
/// state snapshot reaches the shared latest-state slot (the dashboard bridge).
#[test]
fn swap_source_command_drain_rebinds_the_tile_on_the_compositor() {
    // End-to-end A3b: a SwapSource command, drained by the control hook, rebinds
    // a tile on the LIVE CompositorDrive (re-solve + hot set_layout). Exercise the
    // drain against a real drive built from the same config the engine uses.
    use multiview_cli::control::command_drain;
    use multiview_compositor::blend::LinearRgba;
    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
    use multiview_core::color::ColorInfo;
    use multiview_engine::{CompositorDrive, EnginePublisher};
    use multiview_events::Event;
    use multiview_framestore::TileStore;
    use std::collections::HashMap;

    // A drop-oldest outcome publisher; the swap path emits no event, so it is only
    // here to satisfy the drain signature (its events are ignored in this test).
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));

    let cfg = small_config();
    let (w, h) = (cfg.canvas.width, cfg.canvas.height);
    let solved = Arc::new(cfg.solve_layout().expect("solve layout"));

    // One store per declared source (empty is fine — we assert the binding, not
    // pixels), built from the SAME source ids the config declares.
    let mut stores = HashMap::new();
    for source in &cfg.sources {
        stores.insert(
            source.id.clone(),
            Arc::new(TileStore::<Nv12Image>::with_defaults(source.id.as_str())),
        );
    }
    let color = ColorInfo::default().resolve_defaults(w, h);
    let nosignal = Nv12Image::solid(w, h, 16, 128, 128, color).expect("nosignal");
    let mut drive = CompositorDrive::new(
        solved,
        stores,
        nosignal,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .expect("build drive");

    let count_bound_to = |drive: &CompositorDrive<Nv12Image>, src: &str| {
        drive
            .layout()
            .cells
            .iter()
            .filter(|c| c.source.as_deref() == Some(src))
            .count()
    };

    // Baseline: cell_a → in_a, cell_d → in_d (one cell each).
    assert_eq!(count_bound_to(&drive, "in_a"), 1);
    assert_eq!(count_bound_to(&drive, "in_d"), 1);

    // Submit a SwapSource (cell_a → in_d) and drain it onto the live drive.
    let (tx, rx) = command_bus(8);
    tx.try_submit(Command::SwapSource {
        op: OperationId::new(),
        tile: "cell_a".to_owned(),
        source: "in_d".to_owned(),
    })
    .expect("submit swap");
    let mut drain = command_drain(rx, cfg.clone(), Arc::clone(&publisher));
    drain(&mut drive);

    // The hot set_layout took effect: cell_a now also binds in_d, and in_a is no
    // longer bound to any cell.
    assert_eq!(
        count_bound_to(&drive, "in_d"),
        2,
        "after the swap, cell_a must also bind in_d"
    );
    assert_eq!(
        count_bound_to(&drive, "in_a"),
        0,
        "after the swap, no cell binds in_a"
    );

    // An unknown tile id is a no-op (drained, ignored, layout unchanged).
    let (tx2, rx2) = command_bus(8);
    tx2.try_submit(Command::SwapSource {
        op: OperationId::new(),
        tile: "no_such_cell".to_owned(),
        source: "in_b".to_owned(),
    })
    .expect("submit swap");
    let mut drain2 = command_drain(rx2, cfg, Arc::clone(&publisher));
    drain2(&mut drive);
    assert_eq!(
        count_bound_to(&drive, "in_d"),
        2,
        "an unknown tile id must not change any binding"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn software_run_serves_the_control_api_while_running() {
    use multiview_cli::control;
    use multiview_control::{command_bus, EngineStateSnapshot};
    use multiview_engine::{EnginePublisher, StopSignal};
    use multiview_events::Event;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let cfg = small_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");

    // The engine's outbound publisher, shared (read-only) with the control plane.
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _command_rx) = command_bus(8);
    let stop = StopSignal::new();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // IPv6-first: the serve path must bind the IPv6 loopback `[::1]`.
    let (addr, server) = control::bind_and_serve(
        "[::1]:0",
        &cfg,
        Arc::clone(&publisher),
        commands,
        multiview_control::no_preview(),
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");

    // Client: GET the unauthenticated OpenAPI doc while the engine runs, let a
    // few frames produce, then raise the engine's stop signal.
    let stop_for_client = stop.clone();
    let client = tokio::spawn(async move {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /api/v1/openapi.json HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        stop_for_client.stop();
        String::from_utf8_lossy(&buf).into_owned()
    });

    // The engine emits program output AND the control plane serves concurrently;
    // this returns once the client raises stop.
    let report = engine
        .run_until_stopped(&stop, publisher.as_ref())
        .await
        .expect("software serving run");
    assert!(
        !report.faltered,
        "the output clock must not falter while the API is served"
    );
    assert!(
        report.frames >= 1,
        "the engine produced frames while serving"
    );

    let body = client.await.unwrap();
    let status = body.lines().next().unwrap_or_default();
    assert_eq!(
        status.split_whitespace().nth(1),
        Some("200"),
        "openapi status line: {status:?}"
    );
    assert!(body.contains("openapi"), "served an OpenAPI document");

    // The shared publisher carries the engine's compact state snapshot — the
    // bridge the dashboard reads from the wait-free latest-state slot.
    let snap = publisher
        .state
        .latest()
        .expect("the engine published a state snapshot");
    assert_eq!(
        snap.as_ref()["canvas"]["width"].as_u64(),
        Some(320),
        "the snapshot carries the canvas geometry"
    );

    let _ = shutdown_tx.send(());
    let _ = server.await;
}

#[tokio::test]
async fn software_run_emits_exactly_n_frames_for_n_ticks() {
    const TICKS: u64 = 90;

    let cfg = small_config();
    let cadence = cfg.canvas.fps.rational();

    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");

    // Deterministic clock: the engine jumps the manual time source past the last
    // tick's deadline so the pacer never gates — no wall-clock sleeps.
    let time = Arc::new(ManualTimeSource::new());
    let pacer = CooperativePacer;

    let report = engine
        .run_for(Arc::clone(&time), pacer, TICKS)
        .await
        .expect("software run succeeds");

    assert_eq!(
        report.frames, TICKS,
        "N ticks must produce exactly N frames"
    );
    assert_eq!(report.ticks, TICKS);
    assert!(!report.faltered, "output must never falter (invariant #1)");
    assert_eq!(
        report.cadence, cadence,
        "the reported cadence must be the canvas cadence (exact rational, never float)"
    );
    // Every produced frame carries the right canvas geometry.
    assert_eq!(report.canvas_width, cfg.canvas.width);
    assert_eq!(report.canvas_height, cfg.canvas.height);
}

#[tokio::test]
async fn software_frame_pts_advances_by_exactly_one_tick_period() {
    let cfg = small_config();
    let cadence = cfg.canvas.fps.rational();
    let mut engine = SoftwareEngine::build(&cfg).expect("build");

    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for(Arc::clone(&time), CooperativePacer, 30)
        .await
        .expect("run");

    // PTS of tick i must equal MediaTime::from_tick(i, cadence) — pure f(tick).
    let first = multiview_core::time::MediaTime::from_tick(0, cadence);
    let last = multiview_core::time::MediaTime::from_tick(29, cadence);
    assert_eq!(report.first_pts, Some(first));
    assert_eq!(report.last_pts, Some(last));
    // Strictly monotone increasing pts (positive cadence) — never stalls.
    assert!(report.last_pts.unwrap().as_nanos() > report.first_pts.unwrap().as_nanos());
}

#[tokio::test]
async fn software_run_survives_a_config_with_no_live_inputs() {
    // The test sources normally publish frames; here we additionally prove the
    // loop produces frames even without any published frame, because a starved
    // tile yields the NoSignal slate rather than stalling.
    let cfg = small_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build");
    // Do NOT pump the test sources: leave every store empty.
    engine.set_publish_test_frames(false);

    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for(Arc::clone(&time), CooperativePacer, 10)
        .await
        .expect("run with no inputs still succeeds");
    assert_eq!(report.frames, 10, "output is independent of input health");
    assert!(!report.faltered);
}

#[tokio::test]
async fn software_run_builds_and_drives_the_1080p_example() {
    // Prove the shipped 1920x1080 example builds the engine and composites
    // frames end-to-end. Kept to two ticks because the CPU reference compositor
    // is O(pixels x tiles) and a full 1080p canvas is expensive in debug.
    let cfg = load("2x2.toml");
    let mut engine = SoftwareEngine::build(&cfg).expect("build 1080p engine");
    assert_eq!(
        engine.source_count(),
        4,
        "the 2x2 example wires four sources"
    );

    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for(Arc::clone(&time), CooperativePacer, 2)
        .await
        .expect("1080p software run");
    assert_eq!(report.frames, 2);
    assert_eq!(report.canvas_width, 1920);
    assert_eq!(report.canvas_height, 1080);
    assert!(!report.faltered);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realtime_software_run_paces_to_wall_clock() {
    // With the production realtime pacer over the real monotonic clock, a small
    // bounded run produces exactly N frames and consumes roughly N tick-periods
    // of real wall-clock — proving the loop paces (against true elapsed time)
    // rather than free-running. A small canvas keeps composite cost negligible
    // so the (~167 ms) pacing dominates.
    const TICKS: u64 = 5;

    let cfg = small_config();
    let cadence = cfg.canvas.fps.rational();
    let mut engine = SoftwareEngine::build(&cfg).expect("build");

    let start = std::time::Instant::now();
    let report = engine
        .run_for_realtime(TICKS)
        .await
        .expect("realtime software run");
    let elapsed = start.elapsed();

    assert_eq!(
        report.frames, TICKS,
        "N ticks must produce exactly N frames"
    );
    assert!(!report.faltered, "output must never falter (invariant #1)");

    // One tick period at 30000/1001 ≈ 33.37 ms; 5 ticks ≈ 167 ms. Allow a wide
    // lower band (half) — we only need to prove it paced, not spun.
    let period_ns = multiview_core::time::MediaTime::from_tick(1, cadence).as_nanos();
    let expected =
        Duration::from_nanos(u64::try_from(period_ns).unwrap()) * u32::try_from(TICKS).unwrap();
    assert!(
        elapsed >= expected / 2,
        "realtime pacing should consume roughly N tick-periods, got {elapsed:?} (expected ~{expected:?})"
    );
}

#[tokio::test]
async fn control_command_flood_never_falters_the_output_clock() {
    // CTL-1 invariant guard (#1 output-clock + #10 isolation): the command drain
    // runs on the output-clock loop, so a CONTINUOUSLY-FLOODED command bus must
    // never stall the clock or skip a frame — exactly N frames for N ticks, never
    // faltered, no matter how many control commands are pending each tick. This is
    // the engine-level re-assertion the swap/unit tests cannot make (they call the
    // drain closure directly rather than through a real bounded run).
    use multiview_cli::control::command_drain;
    use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
    use multiview_engine::EnginePublisher;
    use multiview_events::Event;
    use std::sync::atomic::{AtomicBool, Ordering};

    const TICKS: u64 = 120;

    let cfg = small_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");

    // A small bus so it is trivially saturated; pre-fill it to capacity so the
    // very first per-tick drain already faces a full queue.
    let (tx, rx) = command_bus(64);
    while tx
        .try_submit(Command::Start {
            op: OperationId::new(),
        })
        .is_ok()
    {}

    // Background flooder: keep the bus saturated with a mix of EVERY command class
    // for the whole run. A full bus just drops the submit (Err) — the sender can
    // only ever pressure, never coordinate with, the clock (invariant #10).
    let stop_flood = Arc::new(AtomicBool::new(false));
    let flooder = {
        let stop_flood = Arc::clone(&stop_flood);
        std::thread::spawn(move || {
            while !stop_flood.load(Ordering::Relaxed) {
                let _ = tx.try_submit(Command::Start {
                    op: OperationId::new(),
                });
                let _ = tx.try_submit(Command::Stop {
                    op: OperationId::new(),
                });
                let _ = tx.try_submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: "cell_a".to_owned(),
                    source: "in_b".to_owned(),
                });
                let _ = tx.try_submit(Command::ApplyLayout {
                    op: OperationId::new(),
                    layout: "schema_v1".to_owned(),
                    document: None,
                });
            }
        })
    };

    // The drop-oldest outcome publisher the drain emits to (capacity 8, so the
    // flood of OutputStatus/Salvo echoes also stresses the drop-oldest path); its
    // events are not inspected here — the property under test is the clock cadence.
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let drain = command_drain(rx, cfg.clone(), Arc::clone(&publisher));

    // Deterministic clock (jumped past the final deadline — no real sleeps).
    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for_with_control(Arc::clone(&time), CooperativePacer, TICKS, drain)
        .await
        .expect("software run with a flooded control bus succeeds");

    stop_flood.store(true, Ordering::Relaxed);
    let _ = flooder.join();

    assert_eq!(
        report.frames, TICKS,
        "a flooded command bus must still produce exactly N frames for N ticks"
    );
    assert_eq!(report.ticks, TICKS);
    assert!(
        !report.faltered,
        "a flooded command bus must never falter the output clock (invariants #1 + #10)"
    );
}

#[tokio::test]
async fn stored_layout_apply_storm_swaps_live_and_never_falters() {
    // ADR-W019 + invariant #1/#10 soak: a STORM of stored-layout applies (each
    // carrying a route-solved document) is drained at frame boundaries through a
    // real bounded run — the stored layout actually becomes the active layout
    // (geometry + bindings swap live), and the output clock still emits exactly
    // N frames for N ticks, never faltered.
    use multiview_cli::control::CommandDrain;
    use multiview_compositor::pipeline::Nv12Image;
    use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
    use multiview_engine::{CompositorDrive, EnginePublisher};
    use multiview_events::Event;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    const TICKS: u64 = 120;

    let cfg = small_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");

    // The stored document, solved exactly as the apply-layout route solves it:
    // ONE full-canvas cell bound to in_b on the running canvas (320x240@30000/1001).
    let stored = || {
        let body = serde_json::json!({
            "canvas": { "width": 320, "height": 240, "fps": "30000/1001" },
            "layout": { "kind": "absolute" },
            "cells": [{
                "id": "stored_cell",
                "rect": { "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0 },
                "z": 0,
                "source": { "input_id": "in_b" }
            }]
        });
        let document =
            multiview_config::LayoutDocument::from_body(&body).expect("stored body parses");
        let solved = document.solve_named("wall-x").expect("stored body solves");
        multiview_control::ResolvedLayout::new(solved, document)
    };

    let (tx, rx) = command_bus(64);
    // Background flooder: keep the bus saturated with stored-layout applies for
    // the whole run (each drained at a frame boundary; a full bus just sheds).
    let stop_flood = Arc::new(AtomicBool::new(false));
    let flooder = {
        let stop_flood = Arc::clone(&stop_flood);
        std::thread::spawn(move || {
            while !stop_flood.load(Ordering::Relaxed) {
                let _ = tx.try_submit(Command::ApplyLayout {
                    op: OperationId::new(),
                    layout: "wall-x".to_owned(),
                    document: Some(Box::new(stored())),
                });
            }
        })
    };

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let mut drain = CommandDrain::new(rx, cfg.clone(), Arc::clone(&publisher));
    // Record the ACTIVE layout each tick so the swap is provable post-run.
    let active_layout = Arc::new(Mutex::new(String::new()));
    let hook = {
        let active_layout = Arc::clone(&active_layout);
        move |drive: &mut CompositorDrive<Nv12Image>| {
            let _ = drain.apply(drive);
            if let Ok(mut name) = active_layout.lock() {
                drive.layout().name.clone_into(&mut name);
            }
        }
    };

    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for_with_control(Arc::clone(&time), CooperativePacer, TICKS, hook)
        .await
        .expect("software run under a stored-layout apply storm succeeds");

    stop_flood.store(true, Ordering::Relaxed);
    let _ = flooder.join();

    assert_eq!(
        report.frames, TICKS,
        "a stored-layout apply storm must still produce exactly N frames for N ticks"
    );
    assert!(
        !report.faltered,
        "a stored-layout apply storm must never falter the output clock (inv #1 + #10)"
    );
    assert_eq!(
        active_layout.lock().expect("name").as_str(),
        "wall-x",
        "the stored layout must actually be the ACTIVE layout (the apply is real)"
    );
}

/// A 1x1 grid whose only source is an analog **clock** — the animated synthetic
/// kind that must be driven by a generator thread (one bake/sec), not primed as
/// a single static placeholder. The canvas runs at 25 fps so a real second is a
/// whole number of ticks.
fn clock_config() -> MultiviewConfig {
    let toml = r##"
schema_version = 1

[canvas]
width = 240
height = 240
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
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;
    let cfg = MultiviewConfig::load_from_toml(toml).expect("parse clock config");
    cfg.validate().expect("clock config validates");
    cfg
}

/// GPU-2 — under `feature = "overlay"`, a `clock` source is driven by a live
/// generator thread, so its `TileStore` content **changes across a real second
/// boundary** (one bake/sec) instead of holding a single static placeholder.
///
/// Content-aware (the same shape as `synth::analog_clock_renders_and_animates`):
/// build the engine, spawn the generators, read the clock store's y-plane, sleep
/// ~1.1s of REAL wall time (the generator is wall-clock based), read again, and
/// assert the y-plane differs — the second hand moved. The generator publishes
/// into the lock-free store the engine only samples (inv #10); the read never
/// blocks.
#[cfg(feature = "overlay")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_source_generator_animates_the_tile_store() {
    use multiview_core::time::MediaTime;

    let cfg = clock_config();
    let engine = SoftwareEngine::build(&cfg).expect("build clock engine");

    let stores = engine.preview_stores();
    let clock = stores.get("clk").expect("clock store wired").clone();

    // Read latched far in the future so HoldForever always hands back the latest
    // published frame (we assert content change, not the freshness ladder).
    let far = MediaTime::from_nanos(60 * 1_000_000_000);
    let plane_now = |s: &std::sync::Arc<
        multiview_framestore::TileStore<multiview_compositor::pipeline::Nv12Image>,
    >|
     -> Option<Vec<u8>> { s.read(far).frame().map(|f| f.y_plane().to_vec()) };

    let generators = engine.spawn_generators();

    // Wait until the generator has published its first frame (bounded), so the
    // baseline is a real clock bake rather than an empty store.
    let mut first: Option<Vec<u8>> = None;
    for _ in 0..40 {
        if let Some(p) = plane_now(&clock) {
            first = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first = first.expect("the clock generator published a first frame");

    // Cross a real second boundary: the displayed second changes, so the clock
    // re-bakes and the store's content differs.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let second = plane_now(&clock).expect("the clock store still holds a frame");

    generators.shutdown();

    assert_ne!(
        first, second,
        "an animated clock source must change its tile-store content across a second boundary"
    );
}

/// GPU-2 inv #1 re-assertion: with a clock generator running (overlay on), a
/// bounded realtime run still emits exactly N frames for N ticks and never
/// falters. The generator only ever writes the lock-free store the engine
/// samples, so it can neither pace nor stall the output clock (inv #1 + #10).
#[cfg(feature = "overlay")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_run_emits_n_frames_for_n_ticks_without_faltering() {
    const TICKS: u64 = 10;

    let cfg = clock_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build clock engine");

    let report = engine
        .run_for_realtime(TICKS)
        .await
        .expect("realtime clock run succeeds");

    assert_eq!(
        report.frames, TICKS,
        "a clock generator must not change the one-frame-per-tick contract"
    );
    assert_eq!(report.ticks, TICKS);
    assert!(
        !report.faltered,
        "the output clock must not falter with a live clock generator (inv #1)"
    );
}

/// GPU-2 honesty: in a build **without** the `overlay` feature the clock
/// generator cannot render (it returns `OverlayRequired`), so the run must still
/// produce a valid slate/card frame per tick — exactly N frames for N ticks,
/// never faltered (inv #1/#2). This proves the non-overlay fallback path.
#[cfg(not(feature = "overlay"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_run_without_overlay_still_produces_a_frame_per_tick() {
    const TICKS: u64 = 8;

    let cfg = clock_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build clock engine");

    let report = engine
        .run_for_realtime(TICKS)
        .await
        .expect("non-overlay clock run succeeds");

    assert_eq!(
        report.frames, TICKS,
        "output is independent of clock render"
    );
    assert_eq!(report.ticks, TICKS);
    assert!(
        !report.faltered,
        "a non-overlay clock build must still emit a valid frame per tick (inv #1)"
    );
}
