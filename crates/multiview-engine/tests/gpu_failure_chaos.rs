//! R3 chaos tests for issue #415 (ADR-0109 §9, invariant #1): a GPU that is
//! lost, blocks in submit, or hangs at teardown never stops, stalls or panics
//! the output clock.
//!
//! A real output-clock thread (`clock_thread::spawn` + `RealtimePacer` +
//! `MonotonicTimeSource`) runs 30 fps over the GPU-free simulated GPU backend
//! (`RunBackend::simulated_gpu`), whose fake device can:
//!
//! * lose the device: its readback poll then PANICS, exactly like wgpu 30's
//!   `Device::poll` on a non-timeout device error (`handle_error_fatal`);
//! * block in "submit" (a driver-side `queue.submit` that does not return);
//! * block its drop while submit is blocked (wgpu's `Queue` drop waits for the
//!   device to idle).
//!
//! Only counts and design bounds with generous slack (loaded macOS runner,
//! #321/#284) — no percentiles.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_compositor::readback::SimulatedGpuControl;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock_thread;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, RunStop, StopSignal, Tick, TimeSource,
};

const TICKS: u64 = 150;
const FAULT_AT: u64 = 30;
const TAIL: usize = 30;
/// The longest gap between two emitted frames the design allows, with slack
/// for a loaded runner: the clock waits at most `period / 2` for the GPU job,
/// so a stuck GPU can never hold a frame back for anything near this.
const MAX_EMIT_GAP: Duration = Duration::from_millis(600);
/// How long the simulated submit stays blocked (well past `MAX_EMIT_GAP`, so a
/// clock that waits on submit is caught).
const SUBMIT_BLOCK: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy)]
struct Seen {
    index: u64,
    pts_nanos: i64,
    repeat: bool,
    gpu_frames: u64,
    at: Instant,
}

struct Report {
    stop: RunStop,
    ticks: u64,
    seen: Vec<Seen>,
    cpu_fallbacks: u64,
    gpu_frames: u64,
    lost: bool,
    teardown: Duration,
    clock_thread: String,
}

fn drive(backend: RunBackend, cadence: Rational) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(Layout {
            name: "gpu-fail".to_owned(),
            canvas: Canvas {
                width: 16,
                height: 16,
                fps_num: cadence.num,
                fps_den: cadence.den,
            },
            cells: Vec::new(),
        }),
        HashMap::new(),
        Nv12Image::solid(
            16,
            16,
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
    .with_backend(backend)
}

/// Run `TICKS` at 30 fps; `fault(control, tick)` runs before each compose.
/// After the run, `before_teardown` runs, then the runtime (and with it the
/// backend) is dropped ON THE CLOCK THREAD and the drop is timed.
fn run(
    fault: impl FnMut(&SimulatedGpuControl, Tick) + Send + 'static,
    before_teardown: impl FnOnce(&SimulatedGpuControl) + Send + 'static,
) -> (Report, SimulatedGpuControl) {
    let cadence = Rational::new(30, 1);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (backend, gpu) = RunBackend::simulated_gpu(Duration::from_millis(1)).unwrap();
    let control = gpu.clone();
    let handle = rt.block_on(async {
        clock_thread::spawn("gpu-fail", move || {
            let mut fault = fault;
            let drive = drive(backend, cadence);
            let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
            let clock = OutputClock::new(cadence).unwrap();
            let mut runtime = EngineRuntime::new(clock, drive, time, RealtimePacer);
            let publisher: EnginePublisher<(), ()> = EnginePublisher::new(8);
            let mut seen: Vec<Seen> = Vec::new();
            let observe = gpu.clone();
            let steer = gpu.clone();
            let outcome = clock_thread::block_on(runtime.run_for_with_tick_control(
                &publisher,
                &StopSignal::new(),
                TICKS,
                |f: &CompositedFrame| {
                    seen.push(Seen {
                        index: f.tick.index,
                        pts_nanos: f.tick.pts.as_nanos(),
                        repeat: f.repeat,
                        gpu_frames: observe.gpu_frames(),
                        at: Instant::now(),
                    });
                },
                |_f: &CompositedFrame| None::<()>,
                move |_d: &mut CompositorDrive<Nv12Image>, tick: Tick| fault(&steer, tick),
            ))
            .unwrap();
            let cpu_fallbacks = gpu.cpu_fallback_frames();
            let lost = gpu.is_lost();
            before_teardown(&gpu);
            let t = Instant::now();
            drop(runtime);
            let teardown = t.elapsed();
            Report {
                stop: outcome.stop,
                ticks: outcome.ticks,
                seen,
                cpu_fallbacks,
                gpu_frames: gpu.gpu_frames(),
                lost,
                teardown,
                clock_thread: std::thread::current().name().unwrap_or("").to_owned(),
            }
        })
        .unwrap()
    });
    let report = rt.block_on(handle.join()).unwrap();
    (report, control)
}

/// Every tick emitted, contiguous indices, strictly increasing PTS, and no
/// gap between two emitted frames beyond the design bound.
fn assert_cadence(r: &Report) {
    assert_eq!(r.stop, RunStop::Completed);
    assert_eq!(r.ticks, TICKS);
    assert_eq!(u64::try_from(r.seen.len()).unwrap(), TICKS);
    for (i, s) in r.seen.iter().enumerate() {
        assert_eq!(s.index, u64::try_from(i).unwrap(), "contiguous ticks");
    }
    let mut max_gap = Duration::ZERO;
    for pair in r.seen.windows(2) {
        assert!(pair[1].pts_nanos > pair[0].pts_nanos, "PTS must increase");
        max_gap = max_gap.max(pair[1].at.duration_since(pair[0].at));
    }
    println!("max emit gap {max_gap:?}");
    assert!(
        max_gap < MAX_EMIT_GAP,
        "the clock was held back {max_gap:?} by the GPU"
    );
}

fn tail_fresh(r: &Report) -> usize {
    r.seen[r.seen.len() - TAIL..]
        .iter()
        .filter(|s| !s.repeat)
        .count()
}

/// The device is lost mid-run (its poll panics like wgpu's). The clock never
/// panics or ends; every later frame is composed on the CPU for good.
#[test]
fn device_loss_never_panics_the_clock_and_falls_back_to_cpu_for_good() {
    let (r, _gpu) = run(
        |gpu, tick| {
            if tick.index >= FAULT_AT {
                gpu.lose_device();
            }
        },
        |_| {},
    );
    assert_cadence(&r);
    let gpu_at_loss = r.seen[usize::try_from(FAULT_AT).unwrap()].gpu_frames;
    println!(
        "device loss: cpu_fallbacks={} gpu_frames={} (at loss {gpu_at_loss}) lost={} tail_fresh={}",
        r.cpu_fallbacks,
        r.gpu_frames,
        r.lost,
        tail_fresh(&r)
    );
    assert!(r.lost, "the backend must record the device loss");
    assert_eq!(
        r.gpu_frames, gpu_at_loss,
        "no GPU frame is produced after the device is lost"
    );
    assert!(
        r.cpu_fallbacks >= TICKS - FAULT_AT - u64::try_from(TAIL).unwrap(),
        "frames after the loss are composed on the CPU"
    );
    assert!(
        tail_fresh(&r) >= TAIL * 2 / 3,
        "fresh CPU frames after loss"
    );
}

/// "Submit" blocks for 1.5 s (a driver-side `queue.submit` that does not
/// return). The clock keeps its cadence: it never waits on the submit, and
/// frames move on the CPU after the stall hold.
#[test]
fn blocked_submit_never_holds_the_clock() {
    let started = Arc::new(AtomicBool::new(false));
    let (r, _gpu) = run(
        move |gpu, tick| {
            if tick.index >= FAULT_AT && !started.swap(true, Ordering::AcqRel) {
                gpu.block_submit();
                let gpu = gpu.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(SUBMIT_BLOCK);
                    gpu.unblock_submit();
                });
            }
        },
        |_| {},
    );
    assert_cadence(&r);
    println!(
        "blocked submit: cpu_fallbacks={} gpu_frames={} tail_fresh={}",
        r.cpu_fallbacks,
        r.gpu_frames,
        tail_fresh(&r)
    );
    assert!(
        r.cpu_fallbacks > 0,
        "a GPU stuck past the stall hold composites on the CPU"
    );
    assert!(!r.lost, "a slow submit is not a device loss");
    let tail = &r.seen[r.seen.len() - TAIL..];
    let tail_gpu = tail.last().unwrap().gpu_frames - tail.first().unwrap().gpu_frames;
    assert!(
        usize::try_from(tail_gpu).unwrap() >= TAIL * 2 / 3,
        "the GPU path recovers by itself ({tail_gpu} GPU frames in the tail)"
    );
}

/// Teardown with the GPU stuck (submit blocked, and the device's drop waits
/// for it like wgpu's `Queue` drop): dropping the runtime on the clock thread
/// returns at once, and the GPU is torn down off the clock thread.
#[test]
fn teardown_with_a_hung_gpu_is_bounded_and_off_the_clock_thread() {
    let release = Arc::new(AtomicBool::new(false));
    let release_late = Arc::clone(&release);
    let (r, gpu) = run(
        |_, _| {},
        move |gpu| {
            gpu.block_submit();
            // Watchdog so a regression ends instead of hanging the suite.
            let gpu = gpu.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(5));
                release_late.store(true, Ordering::Release);
                gpu.unblock_submit();
            });
        },
    );
    assert_cadence(&r);
    println!("teardown took {:?}", r.teardown);
    assert!(
        r.teardown < Duration::from_millis(500),
        "teardown waited {:?} on a hung GPU",
        r.teardown
    );
    gpu.unblock_submit();
    let deadline = Instant::now() + Duration::from_secs(10);
    let dropped_on = loop {
        if let Some(name) = gpu.dropped_on() {
            break name;
        }
        assert!(Instant::now() < deadline, "the GPU was never torn down");
        std::thread::sleep(Duration::from_millis(5));
    };
    println!("GPU dropped on thread {dropped_on:?}");
    assert!(
        !release.load(Ordering::Acquire),
        "teardown needed the watchdog"
    );
    assert_ne!(
        dropped_on, r.clock_thread,
        "the GPU must never be torn down on the clock thread"
    );
}

/// Review INV-2: the GPU is stuck from the very first tick (submit blocked)
/// and the CPU fallback is slower than a tick. There is no last-good yet, but
/// the first tick is still emitted on time (a black stand-in, never an inline
/// composite on the clock), every tick follows, and fresh frames arrive.
#[test]
fn a_gpu_stuck_from_tick_zero_with_a_slow_cpu_still_emits_every_tick() {
    let started = Arc::new(AtomicBool::new(false));
    let (r, _gpu) = run(
        move |gpu, _tick| {
            if !started.swap(true, Ordering::AcqRel) {
                gpu.set_cpu_fallback_cost(Duration::from_millis(50));
                gpu.block_submit();
                let gpu = gpu.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(SUBMIT_BLOCK);
                    gpu.unblock_submit();
                });
            }
        },
        |_| {},
    );
    assert_cadence(&r);
    println!(
        "tick-zero stall: first repeat={} cpu_fallbacks={} gpu_frames={} tail_fresh={}",
        r.seen[0].repeat,
        r.cpu_fallbacks,
        r.gpu_frames,
        tail_fresh(&r)
    );
    assert!(r.seen[0].repeat, "tick 0 is the stand-in, not a composite");
    assert!(r.cpu_fallbacks > 0, "the slow CPU fallback still delivers");
    assert!(
        tail_fresh(&r) >= TAIL * 2 / 3,
        "fresh frames after recovery"
    );
}
