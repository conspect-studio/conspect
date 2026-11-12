//! R3 chaos (invariant #1, issue #311, ADR-0109 §6): the REAL engine tick loop
//! (`EngineRuntime` + `RealtimePacer` + `MonotonicTimeSource`) on its dedicated
//! output-clock thread, while
//!
//! * the **control runtime** (the multi-thread runtime that spawned the clock,
//!   standing in for `#[tokio::main]`) is saturated: one CPU-bound busy task per
//!   worker that never yields for 5 ms at a time, plus `spawn_blocking` calls
//!   that sleep, plus a timer storm; and
//! * the **front door** is flooded (the same multi-source flood as
//!   `chaos_flood.rs`).
//!
//! Each run is measured twice, same host, same load: the new topology (clock
//! thread) and the old one (the loop as a `tokio::spawn`ed task on the saturated
//! runtime, `RealtimePacer`'s async sleep). The old one is reported, not
//! asserted (it is what #311 fixes); the new one is asserted:
//!
//! * one frame per tick: emitted within 2 of `elapsed / period`;
//! * no tick behind: 0 repeats on Linux; at most 1% on macOS (a shared macOS
//!   runner deschedules whole processes, #321);
//! * p99 lateness <= half a period (16.7 ms at 30 fps) — the ADR-0109 §6
//!   margin: a tick is "behind" only past one whole period;
//! * Linux: the clock thread's mean lateness is below the old task's under the
//!   same load (the comparison #311 asks for; reported only on macOS, whose
//!   shared runners deschedule whole processes, #321).
//!
//! The tight target (p99 < 2 ms) is a test-server measurement after deploy, not
//! a CI assertion: on a shared, oversubscribed runner the residual lateness is
//! the OS run queue, which no pacing mechanism controls.
//!
//! `CLOCK_CHAOS_SECS` (default 5) sets each run's length; `CLOCK_CHAOS_PPS`
//! (default 20 000) the flood rate. The PR evidence uses 30 s at 60 000 pps.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::print_stderr,
    clippy::doc_markdown
)]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::{
    clock_thread, CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime,
    MonotonicTimeSource, OutputClock, RealtimePacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;
use multiview_frontdoor::{FrontDoor, FrontDoorConfig};

const FPS: i64 = 30;
const CONTROL_WORKERS: usize = 2;

fn secs() -> u64 {
    std::env::var("CLOCK_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

fn pps() -> u64 {
    std::env::var("CLOCK_CHAOS_PPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

fn build_drive() -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "clock-chaos".to_owned(),
            canvas: Canvas {
                width: 64,
                height: 36,
                fps_num: FPS,
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
        Nv12Image::solid(
            64,
            36,
            16,
            128,
            128,
            ColorInfo::default().resolve_defaults(1920, 1080),
        )
        .unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

#[derive(Debug, Clone, Copy)]
struct ClockRun {
    ticks: u64,
    expected: u64,
    repeats: u64,
    mean_ns: i64,
    p99_ns: i64,
    max_ns: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Topology {
    /// The loop on a dedicated clock thread (#311).
    Thread,
    /// The loop as a `tokio::spawn`ed task on the control runtime (before #311).
    Task,
}

/// Saturate the control runtime for as long as `stop` is clear: per worker one
/// busy task that hogs its worker 5 ms at a time, a `spawn_blocking` storm, and
/// a timer storm. Returns the join handles.
fn saturate(
    rt: &tokio::runtime::Runtime,
    stop: &Arc<AtomicBool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    for _ in 0..CONTROL_WORKERS {
        let stop = Arc::clone(stop);
        tasks.push(rt.spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                // A CPU-bound handler that does not yield (the worst case for
                // a co-scheduled task: it owns the worker until it returns).
                let until = Instant::now() + Duration::from_millis(5);
                let mut x = 0u64;
                while Instant::now() < until {
                    x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                }
                std::hint::black_box(x);
                tokio::task::yield_now().await;
            }
        }));
    }
    for _ in 0..4 {
        let stop = Arc::clone(stop);
        tasks.push(rt.spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ =
                    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(3)))
                        .await;
            }
        }));
    }
    for _ in 0..64 {
        let stop = Arc::clone(stop);
        tasks.push(rt.spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_micros(300)).await;
            }
        }));
    }
    tasks
}

/// One measured run of the real tick loop in `topology` on a saturated
/// control runtime.
fn run_clock(topology: Topology, duration: Duration) -> ClockRun {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(CONTROL_WORKERS)
        .thread_name("control")
        .enable_all()
        .build()
        .unwrap();
    let load_stop = Arc::new(AtomicBool::new(false));
    let load = saturate(&rt, &load_stop);
    // Let the load reach steady state before the clock seeds.
    std::thread::sleep(Duration::from_millis(300));
    let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
    let lateness = Arc::new(Mutex::new(Vec::<i64>::with_capacity(4096)));
    let stop = StopSignal::new();
    let body = {
        let time = Arc::clone(&time);
        let lateness = Arc::clone(&lateness);
        let stop = stop.clone();
        move || {
            let clock = OutputClock::new(Rational::new(FPS, 1)).unwrap();
            let mut runtime =
                EngineRuntime::new(clock, build_drive(), Arc::clone(&time), RealtimePacer);
            let seed = runtime.seed_nanos();
            let publisher: EnginePublisher<u64, u64> = EnginePublisher::new(8);
            let started = Instant::now();
            let ts = Arc::clone(&time);
            let lat = Arc::clone(&lateness);
            let fut = async move {
                runtime
                    .run(
                        &publisher,
                        &stop,
                        move |f: &CompositedFrame| {
                            let index = f.tick.index as i64;
                            let deadline = seed + index * 1_000_000_000 / FPS;
                            lat.lock().unwrap().push(ts.now_nanos() - deadline);
                            f.tick.index
                        },
                        |_| None,
                    )
                    .await
                    .unwrap();
                (runtime.frames_repeated(), started.elapsed())
            };
            fut
        }
    };
    let stopper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            stop.stop();
        })
    };
    let (repeats, elapsed) = match topology {
        Topology::Thread => {
            let handle = rt.block_on(async {
                clock_thread::spawn("clock-chaos", move || clock_thread::block_on(body())).unwrap()
            });
            rt.block_on(handle.join()).unwrap()
        }
        Topology::Task => rt.block_on(async { tokio::spawn(body()).await.unwrap() }),
    };
    stopper.join().unwrap();
    load_stop.store(true, Ordering::Relaxed);
    for task in load {
        task.abort();
    }
    rt.shutdown_timeout(Duration::from_secs(2));
    let period_ns = 1_000_000_000 / FPS;
    let mut l = lateness.lock().unwrap().clone();
    let mean_ns = l.iter().sum::<i64>() / (l.len().max(1) as i64);
    l.sort_unstable();
    let pick = |q: f64| l[((l.len() as f64 - 1.0) * q) as usize];
    ClockRun {
        ticks: l.len() as u64,
        expected: (elapsed.as_nanos() as i64 / period_ns) as u64,
        repeats,
        mean_ns,
        p99_ns: pick(0.99),
        max_ns: *l.last().unwrap(),
    }
}

struct Flood {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Flood {
    fn start(target_port: u16, pps: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..4u8)
            .map(|t| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    // Linux routes all of 127/8 to lo; macOS only 127.0.0.1.
                    let mut sockets: Vec<UdpSocket> = (0..50u8)
                        .filter_map(|i| {
                            UdpSocket::bind((format!("127.0.0.{}", 2 + t * 50 + i), 0)).ok()
                        })
                        .collect();
                    if sockets.is_empty() {
                        sockets.push(UdpSocket::bind("127.0.0.1:0").unwrap());
                    }
                    let target: SocketAddr = format!("127.0.0.1:{target_port}").parse().unwrap();
                    let mut garbage = vec![0x5au8; 300];
                    garbage[0] = 0x0a;
                    let mut initial = vec![0xa5u8; 1200];
                    initial[0] = 0xc3;
                    initial[1..5].copy_from_slice(&1u32.to_be_bytes());
                    let mut rtp = vec![0x33u8; 1100];
                    rtp[0] = 0x80;
                    rtp[1] = 96;
                    let shapes = [garbage, initial, rtp];
                    let batch = 64u64;
                    let per_thread = (pps / 4).max(1);
                    let interval = Duration::from_nanos(1_000_000_000 * batch / per_thread);
                    let mut next = Instant::now();
                    let mut n = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..batch {
                            let s = &sockets[n % sockets.len()];
                            let _ = s.send_to(&shapes[n % shapes.len()], target);
                            n = n.wrapping_add(1);
                        }
                        next += interval;
                        let now = Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        } else if now - next > Duration::from_millis(100) {
                            next = now;
                        }
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn report(name: &str, r: &ClockRun) {
    eprintln!(
        "clock chaos {name}: ticks {} (expected {}) repeats {} lateness mean {:.3} ms p99 {:.3} ms max {:.3} ms",
        r.ticks,
        r.expected,
        r.repeats,
        r.mean_ns as f64 / 1e6,
        r.p99_ns as f64 / 1e6,
        r.max_ns as f64 / 1e6,
    );
}

fn assert_thread_bounds(name: &str, r: &ClockRun) {
    let macos = cfg!(target_os = "macos");
    assert!(
        r.ticks + 2 >= r.expected && r.ticks <= r.expected + 2,
        "{name}: one frame per tick: {r:?}"
    );
    if macos {
        assert!(
            r.repeats * 100 <= r.ticks.max(1),
            "{name}: at most 1% behind on macOS: {r:?}"
        );
    } else {
        assert_eq!(r.repeats, 0, "{name}: no tick fell behind: {r:?}");
    }
    let half_period = 1_000_000_000 / FPS / 2;
    assert!(
        r.p99_ns <= half_period,
        "{name}: p99 lateness above the ADR-0109 bound: {r:?}"
    );
}

#[test]
fn the_clock_thread_holds_cadence_on_a_saturated_control_runtime_under_a_front_door_flood() {
    let duration = Duration::from_secs(secs());

    // 1. Control runtime saturated, no flood.
    let task_busy = run_clock(Topology::Task, duration);
    report("control saturated, old task", &task_busy);
    let thread_busy = run_clock(Topology::Thread, duration);
    report("control saturated, clock thread", &thread_busy);

    // 2. Same, plus a front-door flood (door up, its own runtime, as in prod).
    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let port = door.local_addr().port();
    {
        let mut lane = door.take_rtc_lane().unwrap();
        door.handle()
            .spawn(async move { while lane.recv().await.is_some() {} });
    }
    let flood = Flood::start(port, pps());
    std::thread::sleep(Duration::from_secs(1));
    let task_flood = run_clock(Topology::Task, duration);
    report("control saturated + flood, old task", &task_flood);
    let thread_flood = run_clock(Topology::Thread, duration);
    report("control saturated + flood, clock thread", &thread_flood);
    flood.stop();
    let received = door.stats().snapshot().received;
    door.shutdown_blocking();
    assert!(received > 0, "the flood reached the front door");

    assert_thread_bounds("saturated", &thread_busy);
    assert_thread_bounds("saturated + flood", &thread_flood);
    if cfg!(target_os = "macos") {
        return;
    }
    for (name, thread, task) in [
        ("saturated", &thread_busy, &task_busy),
        ("saturated + flood", &thread_flood, &task_flood),
    ] {
        assert!(
            thread.mean_ns < task.mean_ns,
            "{name}: the clock thread's mean lateness ({:.3} ms) is below the old task's ({:.3} ms)",
            thread.mean_ns as f64 / 1e6,
            task.mean_ns as f64 / 1e6
        );
    }
}
