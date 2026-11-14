//! Chaos for #307 (review I2; invariants #1 and #10): a multi-cell document
//! under a switcher. Program is the M/E's synthesized one-cell canvas, driven
//! by the REAL `CommandDrain` (the production `command_drain` closure) inside
//! the REAL `SwitcherFrameHook`, over a deterministic manual clock.
//!
//! Assertions are on counts and bounds only, never on wall-clock percentiles,
//! so they hold on a loaded CI runner.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]
// reason: a chaos test asserts many counts over one run; splitting it hides
// the single-run relationship between the flood and the clock.
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId, ResolvedLayout};
use multiview_core::color::ColorInfo;
use multiview_core::switcher::SourceRef;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb, MAX_COMMANDS_PER_TICK,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_events::Event;
use multiview_framestore::TileStore;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const LUMA_A: u8 = 200;
const LUMA_B: u8 = 60;
const TICKS: u64 = 600;
const TICK_NS: u64 = 40_000_000;
/// Source `b` publishes no frame in `[LOSS_FROM, LOSS_TO)`.
const LOSS_FROM: u64 = 250;
const LOSS_TO: u64 = 350;
/// The floods stop here; the final Cut proves Program still rebinds.
const FLOOD_UNTIL: u64 = 480;
const FINAL_CUT_AT: u64 = 500;
/// After the flood: Preview is set to the source NOT on Program here, so the
/// final Cut must visibly change Program.
const FINAL_PVW_AT: u64 = 490;
/// The command bus capacity (the binary's default order of magnitude).
const BUS: usize = 64;

/// A two-cell wall (`a | b`) with one M/E (Program `a`, Preview `b`) and a
/// salvo recalling a cell source: `Pipeline::build` gives it the synthesized
/// one-cell Program canvas.
const DOC: &str = r##"schema_version = 1
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
id = "a"
kind = "solid"
color = "#c0c0c0"
[[sources]]
id = "b"
kind = "solid"
color = "#303030"
[[cells]]
id = "wall_a"
area = "a"
[cells.source]
input_id = "a"
[[cells]]
id = "wall_b"
area = "b"
[cells.source]
input_id = "b"
[[salvos]]
id = "swap"
[[salvos.sources]]
cell = "wall_a"
input_id = "b"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
"##;

fn stored_wall() -> ResolvedLayout {
    let body = serde_json::json!({
        "canvas": { "width": 64, "height": 64, "fps": "25/1" },
        "layout": { "kind": "absolute" },
        "cells": [
            { "id": "l", "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 1.0 }, "source": { "input_id": "b" } },
            { "id": "r", "rect": { "x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0 }, "source": { "input_id": "a" } }
        ]
    });
    let document = multiview_config::LayoutDocument::from_body(&body).unwrap();
    let solved = document.solve_named("wall-x").unwrap();
    ResolvedLayout::new(solved, document)
}

fn near(y: u8, want: u8) -> bool {
    y.abs_diff(want) < 8
}

/// Flood the command bus with every layout / repoint / salvo command and the
/// M/E with selects and cuts while a stalled event consumer never reads and
/// source `b` goes away for 100 ticks. The real drain + switcher hook emit
/// exactly one full-canvas, single-source Program frame per tick with
/// tick-derived timestamps, never adopt the wall, keep both queues bounded,
/// and still take a Cut after the flood.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layout_and_switcher_flood_on_the_synthesized_program_canvas_never_moves_the_clock() {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    assert_eq!(config.solve_layout().unwrap().cells.len(), 2);
    let program = config
        .program_canvas(config.solve_layout().unwrap())
        .unwrap();
    assert!(program.synthesized, "the wall is not the M/E bus");
    let cell_id = program.cell_id.clone().unwrap();
    let program_layout = Arc::new(program.layout);
    assert_eq!(program_layout.cells.len(), 1);

    let color = ColorInfo::default().resolve_defaults(64, 64);
    let stores: HashMap<String, Arc<TileStore<Nv12Image>>> = ["a", "b"]
        .into_iter()
        .map(|id| (id.to_owned(), Arc::new(TileStore::with_defaults(id))))
        .collect();
    let publish = |stores: &HashMap<String, Arc<TileStore<Nv12Image>>>, tick: u64| {
        let at = MediaTime::from_nanos(i64::try_from(tick * TICK_NS).unwrap());
        stores["a"].publish(
            Nv12Image::solid(64, 64, LUMA_A, 128, 128, color).unwrap(),
            at,
        );
        if !(LOSS_FROM..LOSS_TO).contains(&tick) {
            stores["b"].publish(
                Nv12Image::solid(64, 64, LUMA_B, 128, 128, color).unwrap(),
                at,
            );
        }
    };
    publish(&stores, 0);

    let drive = CompositorDrive::new(
        Arc::clone(&program_layout),
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
    let mut preview_scene = (*program_layout).clone();
    preview_scene.cells[0].source = Some("b".into());
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: Arc::clone(&program_layout),
            preview: Arc::new(preview_scene),
        })
        .unwrap();

    // The real drain, exactly as the binary builds it. Its event publisher has
    // a subscriber that never reads (a stalled client): drop-oldest, never a
    // block.
    let control_publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let _stalled = control_publisher.subscribe();
    let (sender, commands) = command_bus(BUS);
    let drain = multiview_cli::control::command_drain(
        commands,
        config.clone(),
        Arc::clone(&control_publisher),
    );
    let hook = SwitcherFrameHook::new(driver, cell_id.clone(), drain);

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
    let mut outcomes = handle.outcomes();

    let flooding = Arc::new(AtomicBool::new(true));
    let bus_full = Arc::new(AtomicU64::new(0));
    let bus_sent = Arc::new(AtomicU64::new(0));
    let me_full = Arc::new(AtomicU64::new(0));
    let me_accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let producer = {
        let flooding = Arc::clone(&flooding);
        let bus_full = Arc::clone(&bus_full);
        let bus_sent = Arc::clone(&bus_sent);
        let me_full = Arc::clone(&me_full);
        let me_accepted = Arc::clone(&me_accepted);
        let handle = handle.clone();
        let wall = stored_wall();
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while flooding.load(Ordering::Acquire) {
                let command = match n % 7 {
                    0 => Command::ApplyLayout {
                        op: OperationId::new(),
                        layout: "wall-x".into(),
                        document: Some(Box::new(wall.clone())),
                    },
                    1 => Command::ApplyLayout {
                        op: OperationId::new(),
                        layout: "schema_v1".into(),
                        document: None,
                    },
                    2 => Command::SwapSource {
                        op: OperationId::new(),
                        tile: "wall_a".into(),
                        source: "b".into(),
                    },
                    3 => Command::RouteVideo {
                        op: OperationId::new(),
                        cell: cell_id_for_route(n),
                        source: multiview_config::routing::StreamRef::best(
                            "b".to_owned(),
                            multiview_core::stream::StreamKind::Video,
                        ),
                    },
                    4 => Command::ArmSalvo {
                        op: OperationId::new(),
                        salvo: "swap".into(),
                        head: None,
                    },
                    5 => Command::TakeSalvo {
                        op: OperationId::new(),
                        salvo: Some("swap".into()),
                        head: None,
                    },
                    _ => Command::TakeSalvo {
                        op: OperationId::new(),
                        salvo: None,
                        head: None,
                    },
                };
                if sender.try_submit(command).is_ok() {
                    bus_sent.fetch_add(1, Ordering::Relaxed);
                } else {
                    bus_full.fetch_add(1, Ordering::Relaxed);
                }
                let op = format!("me-{n}");
                let verb = if n % 2 == 0 {
                    SwitcherVerb::Cut
                } else {
                    SwitcherVerb::SelectPreview(SourceRef::new(if n % 4 == 1 { "a" } else { "b" }))
                };
                let me = SwitcherCommand {
                    operation_id: op.clone(),
                    scene_version: 1,
                    verb,
                };
                if handle.try_send(me).is_ok() {
                    me_accepted.lock().unwrap().push(op);
                } else {
                    me_full.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                n += 1;
            }
        })
    };

    let mut frames: Vec<(u64, i64, u8, u8, usize)> = Vec::new();
    let mut seen = HashSet::new();
    let mut per_tick: HashMap<u64, usize> = HashMap::new();
    let mut final_cut: Option<(bool, Option<String>)> = None;
    let mut final_pvw: Option<(bool, Option<String>)> = None;
    let mut cut_from: Option<String> = None;
    let mut sent_at_tick: Vec<u64> = Vec::new();
    let mut lagged = 0_u64;
    let pace = Arc::clone(&clock);
    let cut_handle = handle.clone();
    let bus_sent_cb = Arc::clone(&bus_sent);
    let stop_flood = Arc::clone(&flooding);
    let mut producer = Some(producer);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                let index = frame.tick.index;
                frames.push((
                    index,
                    frame.tick.pts.as_nanos(),
                    frame.canvas.sample(8, 8).unwrap().0,
                    frame.canvas.sample(56, 56).unwrap().0,
                    frame.source_states.len(),
                ));
                loop {
                    match outcomes.try_recv() {
                        Ok(item) => {
                            let event = &item.event;
                            assert!(
                                seen.insert(event.operation_id.clone()),
                                "one outcome per op"
                            );
                            *per_tick.entry(event.tick).or_default() += 1;
                            if event.operation_id == "final-cut" {
                                final_cut = Some((event.completed, event.error.clone()));
                            }
                            if event.operation_id == "final-pvw" {
                                final_pvw = Some((event.completed, event.error.clone()));
                            }
                        }
                        Err(multiview_engine::isolation::TryRecvError::Lagged(n)) => lagged += n,
                        Err(_) => break,
                    }
                }
                if index + 1 == FLOOD_UNTIL {
                    stop_flood.store(false, Ordering::Release);
                    producer.take().unwrap().join().unwrap();
                }
                sent_at_tick.push(bus_sent_cb.load(Ordering::Relaxed));
                if index + 1 == FINAL_PVW_AT {
                    let on_air = cut_handle.snapshot().unwrap().program.as_str().to_owned();
                    let other = if on_air == "a" { "b" } else { "a" };
                    cut_from = Some(on_air);
                    cut_handle
                        .try_send(SwitcherCommand {
                            operation_id: "final-pvw".into(),
                            scene_version: 1,
                            verb: SwitcherVerb::SelectPreview(SourceRef::new(other)),
                        })
                        .unwrap();
                }
                if index + 1 == FINAL_CUT_AT {
                    cut_handle
                        .try_send(SwitcherCommand {
                            operation_id: "final-cut".into(),
                            scene_version: 1,
                            verb: SwitcherVerb::Cut,
                        })
                        .unwrap();
                }
                pace.advance(std::time::Duration::from_nanos(TICK_NS));
                publish(&stores, index + 1);
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();

    // Invariant #1: exactly one frame per tick, contiguous, tick-derived PTS,
    // no repeat, through the floods and the source loss.
    assert_eq!(u64::try_from(frames.len()).unwrap(), TICKS);
    for (i, (index, pts, ..)) in frames.iter().enumerate() {
        assert_eq!(*index, u64::try_from(i).unwrap(), "contiguous ticks");
        assert_eq!(
            *pts,
            i64::try_from(*index * TICK_NS).unwrap(),
            "PTS is the tick counter times the exact cadence"
        );
    }
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");

    // Program never adopted the wall: one bound source, full canvas, and that
    // source is a or b whenever both are fresh.
    for (index, _, tl, br, bound) in &frames {
        assert!(
            *bound <= 1,
            "tick {index}: Program binds one source, not the wall"
        );
        let healthy = *index < LOSS_FROM || *index >= LOSS_TO + 2;
        if healthy {
            assert!(
                near(*tl, LUMA_A) || near(*tl, LUMA_B),
                "tick {index}: luma {tl}"
            );
            assert_eq!(
                tl, br,
                "tick {index}: full-canvas Program, never the split wall"
            );
        }
    }

    // Invariant #10: both producers met a full bounded queue and never blocked;
    // the command bus really carried the flood to the drain.
    assert!(
        bus_full.load(Ordering::Relaxed) > 0,
        "the command bus filled"
    );
    assert!(
        bus_sent.load(Ordering::Relaxed) >= TICKS,
        "the drain consumed the flood: {}",
        bus_sent.load(Ordering::Relaxed)
    );
    assert!(
        me_full.load(Ordering::Relaxed) > 0,
        "the M/E mailbox filled"
    );
    assert_eq!(lagged, 0, "a per-tick reader never lags the outcome ring");
    assert!(
        per_tick.values().all(|&n| n <= MAX_COMMANDS_PER_TICK + 1),
        "bounded M/E work per tick: {:?}",
        per_tick.values().max()
    );
    let accepted = me_accepted.lock().unwrap().len();
    assert!(
        seen.len() >= accepted / 2,
        "the M/E flood was processed: {} of {accepted}",
        seen.len()
    );

    // The drain's per-tick command intake is bounded (invariant #1): between
    // two frames a producer can submit at most the free bus slots plus what one
    // bounded `try_drain` took, so never more than twice the capacity, even
    // though the producer refills the bus while the drain runs.
    let max_intake = sent_at_tick.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        max_intake <= 2 * u64::try_from(BUS).unwrap(),
        "per-tick command intake {max_intake} exceeds 2 x {BUS}"
    );

    // Program still rebinds after the floods: with Preview forced to the
    // source not on air, the final Cut completes and Program shows it.
    let from = cut_from.expect("Program source before the final Cut");
    let (completed, error) = final_pvw.expect("the final select has an outcome");
    assert!(completed, "the final select completed: {error:?}");
    let (completed, error) = final_cut.expect("the final Cut has an outcome");
    assert!(completed, "the final Cut completed: {error:?}");
    let (before, after) = if from == "a" {
        (LUMA_A, LUMA_B)
    } else {
        (LUMA_B, LUMA_A)
    };
    let at = usize::try_from(FINAL_CUT_AT).unwrap();
    assert!(
        near(frames[at - 1].2, before),
        "Program shows {from} before the Cut"
    );
    let snapshot = handle.snapshot().unwrap();
    assert_ne!(snapshot.program.as_str(), from, "the Cut moved Program");
    assert!(
        near(frames.last().unwrap().2, after),
        "Program shows the cut source"
    );
}

/// Alternate the route target between a wall cell and the M/E's own Program
/// cell id: neither may move Program.
fn cell_id_for_route(n: u64) -> String {
    if n % 14 == 3 {
        "me1.program".into()
    } else {
        "wall_b".into()
    }
}
