//! Integrated engine-runtime soak/chaos test — invariants #1 and #10 together.
//!
//! This is THE test that proves "one valid frame per tick, on schedule, forever,
//! independent of inputs and clients." It drives the real [`EngineRuntime`] tick
//! loop for a large number of ticks while:
//!
//! * one input store is permanently empty (a stalled/absent producer), and
//! * a contending consumer thread CONCURRENTLY hammers the isolation channels
//!   (subscribes, receives, reads the latest-state slot) — including one
//!   subscriber that never drains, forcing drop-oldest lag.
//!
//! and asserts, for every one of the ticks:
//!
//! * exactly one valid composited frame is produced (the runtime's tick count
//!   advances by exactly one and a fresh snapshot lands in the latest-state slot),
//! * the published `pts` equals an INDEPENDENT i128 oracle (`round(tick * 1e9 *
//!   den / num)`, half away from zero — not computed via the code under test),
//! * the schedule/cadence is kept (the per-tick compose+publish wall-clock
//!   latency stays well below the tick budget), and
//! * the run never stalls (it completes all ticks).
//!
//! Pacing uses the [`ManualTimeSource`] + [`CooperativePacer`] so the loop is
//! fully deterministic with zero real sleeps; the test advances the source to
//! each tick's absolute deadline and confirms the runtime emits exactly that one
//! tick before advancing — which also verifies the runtime never runs ahead of
//! its deadline (it paces to the clock, never free-runs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: the soak test is one cohesive scenario (build drive -> spawn
    // adversarial consumers -> pace+verify every tick) that reads more clearly as
    // a single narrative than carved into helpers; it legitimately exceeds 100
    // lines and splitting it would obscure the invariant being proven.
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;

/// Independent i128 oracle for `out_pts = f(tick)`, computed WITHOUT calling the
/// clock's `rescale`/`from_tick`. Half-away-from-zero rounding made explicit.
fn oracle_pts_ns(tick: i64, cadence: Rational) -> i64 {
    let numerator: i128 = i128::from(tick) * 1_000_000_000_i128 * i128::from(cadence.den);
    let denominator: i128 = i128::from(cadence.num);
    let q = numerator / denominator;
    let r = numerator % denominator;
    let rounded = if r * 2 >= denominator { q + 1 } else { q };
    i64::try_from(rounded).expect("oracle pts fits in i64")
}

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap()
}

/// A two-cell layout, both cells bound to sources whose stores will be empty.
fn layout(w: u32, h: u32) -> Layout {
    Layout {
        name: "soak".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![
            Cell {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            },
            Cell {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-b".to_owned()),
                ..Cell::default()
            },
        ],
    }
}

/// The per-tick state snapshot the engine publishes (kept tiny on purpose).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TickSnapshot {
    index: u64,
    pts_ns: i64,
    width: u32,
    height: u32,
}

#[test]
fn soak_one_valid_frame_per_tick_on_schedule_while_inputs_and_clients_misbehave() {
    // Big enough to be a genuine soak; tiny canvas keeps the CPU reference fast.
    const TICKS: u64 = 100_000;
    let (w, h) = (32, 24);
    let cadence = Rational::FPS_60;

    // Build the drive over two stores that are NEVER fed (stalled inputs) ->
    // every tile is NoSignal, yet a valid frame must still be produced per tick.
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    stores.insert(
        "cam-b".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b")),
    );
    let drive = CompositorDrive::new(
        Arc::new(layout(w, h)),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();

    let clock = OutputClock::new(cadence).unwrap();
    let time_source = Arc::new(ManualTimeSource::new());
    let ts_for_runtime: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<TickSnapshot, u64>> = Arc::new(EnginePublisher::new(64));

    let mut runtime = EngineRuntime::new(clock, drive, ts_for_runtime, CooperativePacer);
    let seed = runtime.seed_nanos();

    // The per-tick deadline for verifying the pacing seam.
    let pts_at = |i: u64| -> i64 {
        // seed + oracle(i) is the absolute deadline; the runtime must not emit
        // tick i before the source reaches it.
        seed + oracle_pts_ns(i64::try_from(i).unwrap(), cadence)
    };

    // ---- Adversarial contending consumers (real OS threads -> true contention).
    let stop_consumers = Arc::new(AtomicBool::new(false));

    // Consumer 1: hammers subscribe + drains events as fast as it can.
    let c1_pub = Arc::clone(&publisher);
    let c1_stop = Arc::clone(&stop_consumers);
    let c1 = std::thread::spawn(move || {
        let mut sub = c1_pub.subscribe();
        let mut events_seen: u64 = 0;
        while !c1_stop.load(Ordering::Acquire) {
            match sub.try_recv() {
                Ok(_) => events_seen = events_seen.saturating_add(1),
                Err(multiview_engine::TryRecvError::Lagged(_)) => {
                    // fell behind -> resync, exactly as a real client would.
                    sub = sub.resubscribe();
                }
                Err(_) => std::thread::yield_now(),
            }
            // Also pound the latest-state slot (the wait-free path).
            let _ = c1_pub.state.latest();
        }
        events_seen
    });

    // Consumer 2: subscribes and then NEVER reads (forces drop-oldest lag), while
    // re-reading the latest-state slot in a tight loop.
    let c2_pub = Arc::clone(&publisher);
    let c2_stop = Arc::clone(&stop_consumers);
    let c2 = std::thread::spawn(move || {
        let _never_drained = c2_pub.subscribe();
        while !c2_stop.load(Ordering::Acquire) {
            let _ = c2_pub.state.latest();
            std::thread::yield_now();
        }
    });

    // ---- Drive the runtime in the background; pace it tick-by-tick from here.
    let run_pub = Arc::clone(&publisher);
    let stop = StopSignal::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();

    let engine_join = rt.spawn(async move {
        runtime
            .run_for(
                run_pub.as_ref(),
                &stop,
                TICKS,
                |f| TickSnapshot {
                    index: f.tick.index,
                    pts_ns: f.pts().as_nanos(),
                    width: f.canvas.width(),
                    height: f.canvas.height(),
                },
                |f| Some(f.tick.index),
            )
            .await
    });

    // Pace + verify: for each tick i, advance the manual clock to the tick's
    // deadline, then wait for the runtime to emit exactly that tick, measuring
    // the per-tick wall-clock latency.
    // Per-tick compose+publish headroom. On a dedicated machine the engine
    // reacts well within one tick period — that tight bound is the real
    // (release) perf claim and stays asserted in release builds. `cargo test`
    // runs an unoptimized debug build, and CI runners are shared/CPU-starved:
    // the OS can deschedule the worker thread for tens of ms mid-tick, so a
    // single tick's *wall-clock* latency can momentarily exceed one tick period
    // without the engine faltering. Debug therefore uses a generous ceiling
    // that still catches a gross perf regression (seconds per tick) but does
    // not false-fail on scheduler jitter. The inv-#1 guarantee itself — exactly
    // one valid, correctly-timestamped frame per tick, in order, never stalling
    // — is enforced by the pacing/index/dims/PTS-oracle asserts and the 30s
    // stall guard below, which are tight and always-on regardless of build.
    let tick_budget = if cfg!(debug_assertions) {
        Duration::from_secs(2)
    } else {
        Duration::from_nanos(
            u64::try_from(oracle_pts_ns(1, cadence)).unwrap(), // one tick period (16.67ms @60)
        )
    };
    let mut worst_latency = Duration::ZERO;

    for i in 0..TICKS {
        // Before we advance the source, tick i's deadline (pts_at(i)) is still in
        // the future for i >= 1 (the source sits at pts_at(i-1)), so the runtime
        // must be parked: it has emitted exactly ticks 0..=i-1 and not run ahead.
        // (Tick 0's deadline equals the seed, which the source already meets, so
        // it may legitimately be emitted before the loop starts — skip i == 0.)
        if i >= 1 {
            let before = publisher.state.sequence();
            assert_eq!(
                before, i,
                "runtime ran ahead of the schedule at tick {i} (must pace to the clock)"
            );
        }

        // Advance the manual time source to tick i's absolute deadline.
        time_source.set(pts_at(i));

        // Wait for the runtime to compose+publish exactly this one tick, then
        // snapshot the wait-free slot. We poll the SNAPSHOT (not the slot's
        // sequence counter) because the latest-state slot stamps its sequence
        // before storing the value, so `sequence()` can momentarily run one ahead
        // of `latest()`; reading the snapshot's own `index` is the consistent,
        // race-free measurement of "this tick's frame has landed".
        let started = Instant::now();
        let snap = loop {
            if let Some(snap) = publisher.state.latest() {
                if snap.index >= i {
                    break snap;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "runtime STALLED at tick {i} (clients/inputs must never stall it)"
            );
            std::thread::yield_now();
        };
        let latency = started.elapsed();
        if latency > worst_latency {
            worst_latency = latency;
        }

        // Exactly one valid frame for this tick landed in the wait-free slot,
        // with the correct dimensions and the PTS the independent oracle says.
        assert_eq!(snap.index, i, "exactly one frame per tick, in order");
        assert_eq!(snap.width, w);
        assert_eq!(snap.height, h);
        assert_eq!(
            snap.pts_ns,
            oracle_pts_ns(i64::try_from(i).unwrap(), cadence),
            "published pts must equal the independent oracle at tick {i}"
        );

        // Schedule kept: the compose+publish latency for this tick is well under
        // one tick period (the engine has the whole budget; on CI this is µs).
        assert!(
            latency < tick_budget,
            "tick {i} compose+publish latency {latency:?} exceeded the tick budget {tick_budget:?}"
        );
    }

    // The engine ran every tick and returned cleanly (Completed, never stalled).
    let outcome = rt.block_on(engine_join).unwrap().unwrap();
    assert_eq!(outcome.ticks, TICKS, "the runtime produced every tick");
    assert_eq!(outcome.stop, multiview_engine::RunStop::Completed);
    assert_eq!(publisher.state.sequence(), TICKS);

    // Tear down the contending consumers.
    stop_consumers.store(true, Ordering::Release);
    let _ = c1.join();
    let _ = c2.join();

    // Sanity: the worst per-tick latency stayed under budget across the whole run
    // (proves the schedule was kept the entire time, not just on average).
    assert!(
        worst_latency < tick_budget,
        "worst per-tick latency {worst_latency:?} must stay under the tick budget {tick_budget:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_keeps_schedule_while_a_consumer_is_stalled() {
    // Smaller, async-native variant that verifies SCHEDULE/CADENCE (not merely
    // PTS monotonicity): a stalled async subscriber never drains, while the
    // runtime is paced tick-by-tick via the ManualTimeSource. We assert the
    // runtime never emits a tick before its deadline (cadence), produces exactly
    // one frame per tick with the oracle PTS, and that the stalled consumer made
    // no progress (it never gated the engine).
    const TICKS: u64 = 5_000;
    let (w, h) = (16, 16);
    let cadence = Rational::FPS_30; // 30/1

    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    let drive = CompositorDrive::new(
        Arc::new(Layout {
            name: "one".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 30,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();

    let clock = OutputClock::new(cadence).unwrap();
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<i64, u64>> = Arc::new(EnginePublisher::new(8));

    let mut runtime = EngineRuntime::new(clock, drive, ts, CooperativePacer);
    let seed = runtime.seed_nanos();

    // A stalled async consumer that subscribes and then sleeps forever.
    let stalled_pub = Arc::clone(&publisher);
    let progressed = Arc::new(AtomicBool::new(false));
    let p2 = Arc::clone(&progressed);
    let consumer = tokio::spawn(async move {
        let _sub = stalled_pub.subscribe();
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            p2.store(true, Ordering::Release);
        }
    });

    let run_pub = Arc::clone(&publisher);
    let stop = StopSignal::new();
    let engine = tokio::spawn(async move {
        runtime
            .run_for(
                run_pub.as_ref(),
                &stop,
                TICKS,
                |f| f.pts().as_nanos(),
                |f| Some(f.tick.index),
            )
            .await
    });

    for i in 0..TICKS {
        let deadline = seed + oracle_pts_ns(i64::try_from(i).unwrap(), cadence);
        // Cadence check: for i >= 1 the deadline is still in the future, so the
        // runtime must be parked at exactly tick i (it never free-runs ahead).
        // Tick 0's deadline equals the seed (already met) -> skip i == 0.
        if i >= 1 {
            assert_eq!(
                publisher.state.sequence(),
                i,
                "runtime ran ahead of the schedule at tick {i}"
            );
        }
        time_source.set(deadline);

        // Cooperatively wait for exactly this tick (no real sleeps). We poll the
        // published SNAPSHOT value, not the slot's sequence counter: the slot
        // stamps its sequence before storing the value, so `sequence()` can
        // momentarily run one ahead of `latest()`. Since the published pts is
        // strictly increasing, waiting for the snapshot to reach this tick's
        // oracle pts is the consistent, race-free measurement.
        let expected = oracle_pts_ns(i64::try_from(i).unwrap(), cadence);
        let started = Instant::now();
        let snap = loop {
            if let Some(snap) = publisher.state.latest() {
                if *snap >= expected {
                    break snap;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "runtime stalled at tick {i}"
            );
            tokio::task::yield_now().await;
        };
        assert_eq!(*snap, expected, "pts must equal the oracle at tick {i}");
    }

    let outcome = engine.await.unwrap().unwrap();
    assert_eq!(outcome.ticks, TICKS);
    assert_eq!(outcome.stop, multiview_engine::RunStop::Completed);
    // The stalled consumer never made progress -> it never gated the engine.
    assert!(!progressed.load(Ordering::Acquire));
    consumer.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_stops_promptly_on_stop_signal() {
    // The loop is cancellable: raising the StopSignal makes it return after the
    // current tick rather than running to the (here, unbounded) tick budget.
    let (w, h) = (16, 16);
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    let drive = CompositorDrive::new(
        Arc::new(Layout {
            name: "one".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 60,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();

    // The pacer gates tick `index` on `seed + pts_at(index)`, where `seed` is the
    // source instant captured at construction. To make the pacer never gate for
    // the frames we observe, advance the source one full second PAST the seed
    // (~60 ticks at 60fps, far more than the 10 we wait for) AFTER constructing
    // the runtime, so every tick's deadline is already in the past.
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));

    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::FPS_60).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    // One second of headroom past the seed: covers ~60 ticks' deadlines so the
    // pacer releases every tick the test waits for without ever gating.
    time_source.set(runtime.seed_nanos() + 1_000_000_000);
    let stop = StopSignal::new();
    let stop2 = stop.clone();
    let run_pub = Arc::clone(&publisher);

    let engine = tokio::spawn(async move {
        // `run` (not `run_for`) -> only the stop signal ends it.
        runtime
            .run(
                run_pub.as_ref(),
                &stop2,
                |f| f.tick.index,
                |f| Some(f.tick.index),
            )
            .await
    });

    // Let it produce some frames, then ask it to stop.
    let started = Instant::now();
    while publisher.state.sequence() < 10 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "runtime stalled"
        );
        tokio::task::yield_now().await;
    }
    stop.stop();

    let outcome = engine.await.unwrap().unwrap();
    assert_eq!(outcome.stop, multiview_engine::RunStop::Stopped);
    assert!(outcome.ticks >= 10, "produced frames before stopping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_hook_runs_every_tick_and_its_layout_swap_drives_the_composed_frame() {
    // The control seam (inv #11): a per-tick hook applies hot reconfiguration at
    // the frame boundary. Prove BOTH that it runs once per tick AND that a
    // `set_layout` it performs is visible to the very next compose — i.e. the
    // rebind actually drives program output, without faltering (inv #1).
    const TICKS: u64 = 20;
    let (w, h) = (16, 16);
    // Two stores: "cam-a" stays empty (NoSignal); "cam-b" is primed LIVE.
    let mut stores = HashMap::new();
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    // Prime cam-b with a solid frame so a cell bound to it reads LIVE.
    store_b.publish_arc(
        Arc::new(Nv12Image::solid(w, h, 120, 128, 128, resolved_color()).unwrap()),
        multiview_core::time::MediaTime::from_nanos(0),
    );
    stores.insert("cam-a".to_owned(), Arc::clone(&store_a));
    stores.insert("cam-b".to_owned(), Arc::clone(&store_b));

    let cell_bound_to = |source: &str| Cell {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
        z: 0,
        fit: FitMode::Contain,
        source: Some(source.to_owned()),
        ..Cell::default()
    };
    let layout_with = |name: &str, source: &str| {
        Arc::new(Layout {
            name: name.to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 60,
                fps_den: 1,
            },
            cells: vec![cell_bound_to(source)],
        })
    };

    // Start bound to the EMPTY source (cam-a) → the tile is NoSignal.
    let drive = CompositorDrive::new(
        layout_with("initial", "cam-a"),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    // State projection records, per tick, whether the composed frame's bound
    // source is the LIVE cam-b (proving the swap drove the picture).
    let publisher: Arc<EnginePublisher<bool, u64>> = Arc::new(EnginePublisher::new(8));

    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::FPS_60).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    time_source.set(runtime.seed_nanos() + 1_000_000_000);

    // Swap to cam-b at tick index 5 (a frame boundary). Count hook invocations.
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let calls_in_hook = Arc::clone(&calls);
    let swap_to_b = layout_with("reconfigured", "cam-b");
    let stop = StopSignal::new();

    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            TICKS,
            // state_of: did THIS frame draw the LIVE cam-b source?
            |f: &multiview_engine::CompositedFrame| f.source_states.contains_key("cam-b"),
            |_f| None::<u64>,
            // control: on the 6th invocation, hot-swap the layout to cam-b.
            move |drive: &mut CompositorDrive<Nv12Image>| {
                let n = calls_in_hook.fetch_add(1, Ordering::AcqRel);
                if n == 5 {
                    drive
                        .set_layout(Arc::clone(&swap_to_b))
                        .expect("swap layout is valid");
                }
            },
        )
        .await
        .expect("control run completes");

    // The hook ran exactly once per tick.
    assert_eq!(
        calls.load(Ordering::Acquire),
        TICKS,
        "the control hook runs once per tick at the frame boundary"
    );
    // The output never faltered: exactly TICKS frames, clean completion.
    assert_eq!(outcome.ticks, TICKS);
    assert_eq!(outcome.stop, multiview_engine::RunStop::Completed);
    // After the swap the composed frame draws cam-b (the rebind reached the
    // picture). The latest published state must be `true`.
    let drew_b = publisher.state.latest().expect("a state was published");
    assert!(
        *drew_b,
        "after the control hook's set_layout, compose must draw the new source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_of_none_publishes_no_events_while_state_advances_every_tick() {
    // The sparse-event contract: an `event_of` that returns `None` publishes
    // ZERO events, yet the wait-free state slot still refreshes every tick. (The
    // positive case — events flow when `event_of` is `Some` — is exercised by the
    // soak's adversarial consumer above.) This is what lets the control plane
    // carry state-change events, not a per-tick flood, without any change to the
    // output clock's one-frame-per-tick guarantee.
    let (w, h) = (16, 16);
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    let drive = CompositorDrive::new(
        Arc::new(Layout {
            name: "one".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 60,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    // Subscribe BEFORE the run, so any event the engine published would be seen.
    let events = publisher.events.subscribe();

    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::FPS_60).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    time_source.set(runtime.seed_nanos() + 1_000_000_000);
    let stop = StopSignal::new();
    let stop2 = stop.clone();
    let run_pub = Arc::clone(&publisher);

    let engine = tokio::spawn(async move {
        // State every tick; events NEVER (`event_of` is always `None`).
        runtime
            .run(run_pub.as_ref(), &stop2, |f| f.tick.index, |_f| None::<u64>)
            .await
    });

    let started = Instant::now();
    while publisher.state.sequence() < 10 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "runtime stalled"
        );
        tokio::task::yield_now().await;
    }
    stop.stop();
    let outcome = engine.await.unwrap().unwrap();

    assert!(outcome.ticks >= 10, "state must advance for >= 10 ticks");
    // Despite >= 10 ticks of fresh state, the event broadcast received nothing.
    assert_eq!(
        events.len(),
        0,
        "event_of returning None must publish zero events"
    );
}
