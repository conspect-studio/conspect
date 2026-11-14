//! Chaos for issue #464 (invariants #1 and #10): 20 live source adds on a
//! SWITCHER run, under a command flood and an M/E command flood.
//!
//! The real `CommandDrain` (with the run's demand seam and the real live-source
//! hub + ingest spawner of a built switcher `Pipeline`) runs inside the real
//! `SwitcherFrameHook`, over a deterministic manual clock. Assertions are on
//! counts and bounds only (never wall-clock percentiles), so they hold on a
//! loaded CI runner: exactly one frame per tick, contiguous, tick-derived PTS,
//! zero repeated frames, and every added source catalogued, visible to the
//! M/E and `ready` without a restart.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]
// reason: a chaos test asserts many counts over one run.

use multiview_cli::live_sources::{shared_stores, LiveSourceHub};
use multiview_cli::pipeline::Pipeline;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_core::color::ColorInfo;
use multiview_core::switcher::SourceRef;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, DemandPhase, EnginePublisher, EngineRuntime, OutputClock,
    StopSignal, TimeSource,
};
use multiview_events::Event;
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TICKS: u64 = 750;
const TICK_NS: u64 = 40_000_000;
const ADDS: usize = 20;
const BUS: usize = 64;

fn config(out: &std::path::Path) -> MultiviewConfig {
    let text = format!(
        r##"
schema_version = 1
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
id = "a"
kind = "solid"
color = "#c0c0c0"
[[sources]]
id = "b"
kind = "solid"
color = "#303030"
[[cells]]
id = "pgm"
area = "a"
fit = "fill"
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "{}"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
"##,
        out.display()
    );
    let cfg = MultiviewConfig::load_from_toml(&text).unwrap();
    cfg.validate().unwrap();
    cfg
}

fn added(n: usize) -> String {
    format!("live{n:02}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_live_adds_under_load_on_a_switcher_run_never_move_the_clock() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("out/index.m3u8"));
    let mut pipeline = Pipeline::build(&cfg).expect("pipeline builds");
    let demand = pipeline.demand();
    demand.start();
    let live_stores = pipeline.live_bus_stores();
    let hub = LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        shared_stores(pipeline.preview_stores()),
        Some(pipeline.live_ingest_spawner()),
    );

    // The clock's own a/b stores, published per tick by the test (the
    // deterministic sources on the buses).
    let color = ColorInfo::default().resolve_defaults(64, 64);
    let stores: HashMap<String, Arc<TileStore<Nv12Image>>> = ["a", "b"]
        .into_iter()
        .map(|id| (id.to_owned(), Arc::new(TileStore::with_defaults(id))))
        .collect();
    let publish = |stores: &HashMap<String, Arc<TileStore<Nv12Image>>>, tick: u64| {
        let at = MediaTime::from_nanos(i64::try_from(tick * TICK_NS).unwrap());
        stores["a"].publish(Nv12Image::solid(64, 64, 200, 128, 128, color).unwrap(), at);
        stores["b"].publish(Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(), at);
    };
    publish(&stores, 0);
    let layout = pipeline.switcher_program_layout();
    // The config's one Program cell (the M/E bus is the document's own
    // single full-canvas cell, not a synthesized one).
    let cell_id = "pgm".to_owned();
    let drive = CompositorDrive::new(
        Arc::clone(&layout),
        stores.clone(),
        Nv12Image::solid(64, 64, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some(cell_id.clone())]);

    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores.clone());
    driver.set_live_stores(Arc::clone(&live_stores));
    let mut preview_scene = (*layout).clone();
    preview_scene.cells[0].source = Some("b".into());
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: Arc::clone(&layout),
            preview: Arc::new(preview_scene),
        })
        .unwrap();

    // The real drain with the run's demand seam; its event publisher has a
    // subscriber that never reads (a stalled client).
    let control_publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let _stalled = control_publisher.subscribe();
    let (sender, commands) = command_bus(BUS);
    let drain = multiview_cli::control::command_drain_with_live_sources_and_demand(
        commands,
        cfg.clone(),
        Arc::clone(&control_publisher),
        hub.handle(),
        pipeline.drain_demand(),
    );
    let mut hook = SwitcherFrameHook::new(driver, cell_id.clone(), drain);
    hook.live_stores = Some(Arc::clone(&live_stores));

    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let _stalled_frames = publisher.subscribe();

    // Load: a producer floods the command bus with noise and interleaves the
    // 20 adds (each retried until the bounded bus takes it); another floods
    // the M/E mailbox with cuts and PVW selects.
    let flooding = Arc::new(AtomicBool::new(true));
    let bus_full = Arc::new(AtomicU64::new(0));
    let adds_sent = Arc::new(AtomicU64::new(0));
    let producer = {
        let flooding = Arc::clone(&flooding);
        let bus_full = Arc::clone(&bus_full);
        let adds_sent = Arc::clone(&adds_sent);
        std::thread::spawn(move || {
            let mut next_add = 0_usize;
            let mut n: u64 = 0;
            while flooding.load(Ordering::Acquire) {
                let command = if next_add < ADDS && n % 5 == 0 {
                    Command::UpsertSource {
                        op: OperationId::new(),
                        source: Box::new(
                            serde_json::from_value(serde_json::json!({
                                "id": added(next_add), "kind": "solid", "color": "#808080"
                            }))
                            .unwrap(),
                        ),
                    }
                } else {
                    Command::SwapSource {
                        op: OperationId::new(),
                        tile: "pgm".into(),
                        source: "b".into(),
                    }
                };
                let is_add = matches!(command, Command::UpsertSource { .. });
                if sender.try_submit(command).is_ok() {
                    if is_add {
                        next_add += 1;
                        adds_sent.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    bus_full.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_micros(200));
                }
                n += 1;
            }
        })
    };
    let me_full = Arc::new(AtomicU64::new(0));
    let me_flood = {
        let flooding = Arc::clone(&flooding);
        let me_full = Arc::clone(&me_full);
        let handle = handle.clone();
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while flooding.load(Ordering::Acquire) {
                let verb = if n % 2 == 0 {
                    SwitcherVerb::Cut
                } else {
                    SwitcherVerb::SelectPreview(SourceRef::new(if n % 4 == 1 { "a" } else { "b" }))
                };
                if handle
                    .try_send(SwitcherCommand {
                        operation_id: format!("me-{n}"),
                        scene_version: 1,
                        verb,
                    })
                    .is_err()
                {
                    me_full.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_micros(200));
                }
                n += 1;
            }
        })
    };

    let mut frames: Vec<(u64, i64)> = Vec::new();
    let pace = Arc::clone(&clock);
    let stop_flood = Arc::clone(&flooding);
    let mut producers = Some((producer, me_flood));
    let flood_until = TICKS - 50;
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                frames.push((frame.tick.index, frame.tick.pts.as_nanos()));
                if frame.tick.index + 1 == flood_until {
                    stop_flood.store(false, Ordering::Release);
                    let (a, b) = producers.take().unwrap();
                    a.join().unwrap();
                    b.join().unwrap();
                }
                // Give the off-clock hub and demand workers real time between
                // manual ticks; the clock itself never waits on them.
                std::thread::sleep(Duration::from_millis(4));
                pace.advance(Duration::from_nanos(TICK_NS));
                publish(&stores, frame.tick.index + 1);
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();

    // Invariant #1: exactly one frame per tick, contiguous, tick-derived PTS,
    // no repeat, through the adds and both floods.
    assert_eq!(u64::try_from(frames.len()).unwrap(), TICKS);
    for (i, (index, pts)) in frames.iter().enumerate() {
        assert_eq!(*index, u64::try_from(i).unwrap(), "contiguous ticks");
        assert_eq!(
            *pts,
            i64::try_from(*index * TICK_NS).unwrap(),
            "PTS is the tick counter times the exact cadence"
        );
    }
    assert_eq!(repeated.load(Ordering::Acquire), 0, "0 missed ticks");

    // Invariant #10: the floods met full bounded queues and never blocked.
    assert!(
        bus_full.load(Ordering::Relaxed) > 0,
        "the command bus filled"
    );
    assert!(
        me_full.load(Ordering::Relaxed) > 0,
        "the M/E mailbox filled"
    );
    assert_eq!(
        adds_sent.load(Ordering::Relaxed),
        u64::try_from(ADDS).unwrap(),
        "all 20 adds reached the bus"
    );

    // Every add was catalogued live (never refused) and visible to the M/E,
    // and starts without a restart (always_on: its standing pin starts it).
    let deadline = Instant::now() + Duration::from_secs(30);
    let pending = || -> Vec<String> {
        (0..ADDS)
            .map(added)
            .filter(|id| {
                !live_stores.load().contains_key(id)
                    || demand.status(id).phase != DemandPhase::Ready
            })
            .collect()
    };
    let mut left = pending();
    while !left.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        left = pending();
    }
    assert!(
        left.is_empty(),
        "added sources not catalogued+ready: {left:?}"
    );
    hub.shutdown();
    demand.shutdown();
}
