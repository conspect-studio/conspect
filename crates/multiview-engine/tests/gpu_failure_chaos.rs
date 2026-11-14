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
//! #321/#284) — no percentiles. Every assertion is either exact per-frame
//! provenance (each emitted fresh frame is accounted to the GPU or the CPU
//! fallback by the backend's own counters, sampled at emission) or liveness
//! measured in ticks: a run continues past `TICKS` until the required fresh
//! frames have arrived, failing only if they do not within `MAX_TICKS`. How
//! many ticks a starved worker misses is host scheduling, not behaviour
//! (issue #447).
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
/// The liveness cap: a run that has not met its fresh-frame goal after this
/// many ticks (120 s at 30 fps) fails. A healthy host stops at `TICKS` + a
/// few; a single core starved by 3 busy loops at nice 15 needed up to 1206.
const MAX_TICKS: u64 = 3600;
/// Fresh frames a recovered (or CPU-fallback) path must deliver.
const FRESH_GOAL: usize = 20;
/// Fresh frames of the same provenance that must ALSO arrive after the
/// minimum `TICKS` window (review R1): a path that recovers and then stops
/// delivering for good never meets this and fails at `MAX_TICKS`.
const CONTINUING_GOAL: usize = 10;

/// The liveness goal: `FRESH_GOAL` fresh `which` frames from `from` on, and
/// `CONTINUING_GOAL` of them after the minimum `TICKS` window.
fn goal_met(seen: &[Seen], from: u64, which: fn(&Seen, &Seen) -> bool) -> bool {
    count_fresh(seen, from, which) >= FRESH_GOAL
        && count_fresh(seen, TICKS, which) >= CONTINUING_GOAL
}

/// `goal_met`, asserted with the counts in the message.
fn assert_goal(seen: &[Seen], from: u64, which: fn(&Seen, &Seen) -> bool, what: &str) {
    let total = count_fresh(seen, from, which);
    let continuing = count_fresh(seen, TICKS, which);
    assert!(
        total >= FRESH_GOAL && continuing >= CONTINUING_GOAL,
        "{what}: {total} fresh from tick {from} (need {FRESH_GOAL}), \
         {continuing} after tick {TICKS} (need {CONTINUING_GOAL})"
    );
}
const FAULT_AT: u64 = 30;
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
    /// The backend's CPU-fallback count right after this frame was composed:
    /// a fresh frame that raised it by one is a CPU composite; a fresh frame
    /// that left it unchanged came from the GPU worker.
    cpu_fallbacks: u64,
    /// Whether the backend had recorded the device as lost at this frame.
    lost: bool,
    at: Instant,
}

impl Seen {
    /// Whether this fresh frame was composited by the CPU fallback, given the
    /// frame before it.
    fn is_cpu(&self, prev: &Self) -> bool {
        !self.repeat && self.cpu_fallbacks == prev.cpu_fallbacks + 1
    }

    /// Whether this fresh frame came from the GPU worker.
    fn is_gpu(&self, prev: &Self) -> bool {
        !self.repeat && self.cpu_fallbacks == prev.cpu_fallbacks
    }
}

/// Every frame's provenance is exact: a repeat never counts a CPU fallback,
/// and a fresh frame counts at most one (no fallback is ever counted for a
/// frame that is not emitted, and none is emitted uncounted).
fn assert_provenance(seen: &[Seen]) {
    for (i, pair) in seen.windows(2).enumerate() {
        let (prev, cur) = (&pair[0], &pair[1]);
        let delta = cur.cpu_fallbacks - prev.cpu_fallbacks;
        if cur.repeat {
            assert_eq!(delta, 0, "tick {}: a repeat counted a CPU fallback", i + 1);
        } else {
            assert!(
                delta <= 1,
                "tick {}: one fresh frame counted {delta} CPU fallbacks",
                i + 1
            );
        }
    }
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

/// Run at 30 fps for at least `TICKS` and until `done(seen)` holds (at most
/// `MAX_TICKS`); `fault(control, tick)` runs before each compose. After the
/// run, `before_teardown` runs, then the runtime (and with it the backend)
/// is dropped ON THE CLOCK THREAD and the drop is timed.
fn run(
    fault: impl FnMut(&SimulatedGpuControl, Tick) + Send + 'static,
    done: impl Fn(&[Seen]) -> bool + Send + 'static,
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
            let stop = StopSignal::new();
            let outcome = clock_thread::block_on(runtime.run_for_with_tick_control(
                &publisher,
                &stop,
                MAX_TICKS,
                |f: &CompositedFrame| {
                    seen.push(Seen {
                        index: f.tick.index,
                        pts_nanos: f.tick.pts.as_nanos(),
                        repeat: f.repeat,
                        gpu_frames: observe.gpu_frames(),
                        cpu_fallbacks: observe.cpu_fallback_frames(),
                        lost: observe.is_lost(),
                        at: Instant::now(),
                    });
                    if u64::try_from(seen.len()).unwrap() >= TICKS && done(&seen) {
                        stop.stop();
                    }
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
/// gap between two emitted frames beyond the design bound. The run ended
/// because its goal was met (`Stopped`), not at the `MAX_TICKS` cap.
fn assert_cadence(r: &Report) {
    assert_eq!(
        r.stop,
        RunStop::Stopped,
        "the fresh-frame goal was not met within {MAX_TICKS} ticks"
    );
    assert!(r.ticks >= TICKS);
    assert_eq!(u64::try_from(r.seen.len()).unwrap(), r.ticks);
    for (i, s) in r.seen.iter().enumerate() {
        assert_eq!(s.index, u64::try_from(i).unwrap(), "contiguous ticks");
    }
    let mut max_gap = Duration::ZERO;
    for pair in r.seen.windows(2) {
        assert!(pair[1].pts_nanos > pair[0].pts_nanos, "PTS must increase");
        max_gap = max_gap.max(pair[1].at.duration_since(pair[0].at));
    }
    println!("ticks {} max emit gap {max_gap:?}", r.ticks);
    assert!(
        max_gap < MAX_EMIT_GAP,
        "the clock was held back {max_gap:?} by the GPU"
    );
}

/// Fresh frames from index `from` on whose provenance is `which`.
fn count_fresh(seen: &[Seen], from: u64, which: fn(&Seen, &Seen) -> bool) -> usize {
    seen.windows(2)
        .filter(|p| p[1].index >= from && which(&p[1], &p[0]))
        .count()
}

/// Release a blocked submit once it has been blocked for `SUBMIT_BLOCK` AND
/// the CPU fallback has delivered `CPU_BEFORE_RELEASE` frames, so the CPU
/// phase is observed however slowly the host schedules the workers (a
/// watchdog releases it after 60 s; the run then fails its assertions).
fn release_after_cpu_phase(gpu: &SimulatedGpuControl) {
    let gpu = gpu.clone();
    std::thread::spawn(move || {
        let blocked = Instant::now();
        while blocked.elapsed() < SUBMIT_BLOCK
            || (gpu.cpu_fallback_frames() < CPU_BEFORE_RELEASE
                && blocked.elapsed() < Duration::from_secs(60))
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        gpu.unblock_submit();
    });
}

/// CPU-fallback frames a blocked-submit scenario must see before the submit
/// returns.
const CPU_BEFORE_RELEASE: u64 = 3;

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
        |seen| goal_met(seen, FAULT_AT, Seen::is_cpu),
        |_| {},
    );
    assert_cadence(&r);
    assert_provenance(&r.seen);
    let gpu_at_loss = r.seen[usize::try_from(FAULT_AT).unwrap()].gpu_frames;
    let cpu_after = count_fresh(&r.seen, FAULT_AT, Seen::is_cpu);
    let gpu_after = count_fresh(&r.seen, FAULT_AT, Seen::is_gpu);
    println!(
        "device loss: cpu_fallbacks={} gpu_frames={} (at loss {gpu_at_loss}) lost={} \
         cpu_after={cpu_after} gpu_after={gpu_after}",
        r.cpu_fallbacks, r.gpu_frames, r.lost
    );
    assert!(r.lost, "the backend must record the device loss");
    // No GPU frame reaches the output from the loss on: every fresh frame is
    // a CPU composite, and everything else is a repeat of last-good.
    assert_eq!(
        gpu_after, 0,
        "a GPU frame was emitted after the device loss"
    );
    // Once the backend has recorded the loss it is permanent, and the GPU is
    // never used again. The count is sampled at the first frame that saw the
    // loss recorded, not at the fault tick: a GPU job still in flight from an
    // earlier tick can finish its readback before the fault and count it
    // after (its result is discarded, `gpu_after` above). GPU jobs run one at
    // a time on the one GPU worker, so none can finish after the job that
    // recorded the loss.
    let first_lost = r.seen.iter().position(|s| s.lost).unwrap();
    assert!(
        r.seen[first_lost..].iter().all(|s| s.lost),
        "loss is permanent"
    );
    assert_eq!(
        r.gpu_frames, r.seen[first_lost].gpu_frames,
        "no GPU frame is produced after the device loss is recorded"
    );
    assert!(
        r.gpu_frames <= gpu_at_loss + 1,
        "at most the one in-flight job"
    );
    assert_goal(
        &r.seen,
        FAULT_AT,
        Seen::is_cpu,
        "fresh CPU frames after the loss",
    );
}

/// "Submit" blocks for at least 1.5 s (a driver-side `queue.submit` that
/// does not return). The clock keeps its cadence: it never waits on the
/// submit, frames move on the CPU after the stall hold, and the GPU path
/// recovers by itself once the submit returns.
#[test]
fn blocked_submit_never_holds_the_clock() {
    let started = Arc::new(AtomicBool::new(false));
    let (r, _gpu) = run(
        move |gpu, tick| {
            if tick.index >= FAULT_AT && !started.swap(true, Ordering::AcqRel) {
                gpu.block_submit();
                release_after_cpu_phase(gpu);
            }
        },
        |seen| goal_met(seen, FAULT_AT, Seen::is_gpu),
        |_| {},
    );
    assert_cadence(&r);
    assert_provenance(&r.seen);
    let cpu_after = count_fresh(&r.seen, FAULT_AT, Seen::is_cpu);
    let gpu_after = count_fresh(&r.seen, FAULT_AT, Seen::is_gpu);
    println!(
        "blocked submit: cpu_fallbacks={} gpu_frames={} cpu_after={cpu_after} gpu_after={gpu_after}",
        r.cpu_fallbacks, r.gpu_frames
    );
    assert!(
        cpu_after >= usize::try_from(CPU_BEFORE_RELEASE).unwrap(),
        "a GPU stuck past the stall hold composites on the CPU"
    );
    assert!(!r.lost, "a slow submit is not a device loss");
    assert_goal(
        &r.seen,
        FAULT_AT,
        Seen::is_gpu,
        "the GPU path recovers by itself",
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
        |_| true,
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
/// composite on the clock), every tick follows, the slow CPU fallback
/// delivers, and fresh GPU frames arrive once the submit returns.
#[test]
fn a_gpu_stuck_from_tick_zero_with_a_slow_cpu_still_emits_every_tick() {
    let started = Arc::new(AtomicBool::new(false));
    let (r, _gpu) = run(
        move |gpu, _tick| {
            if !started.swap(true, Ordering::AcqRel) {
                gpu.set_cpu_fallback_cost(Duration::from_millis(50));
                gpu.block_submit();
                release_after_cpu_phase(gpu);
            }
        },
        |seen| goal_met(seen, 0, Seen::is_gpu),
        |_| {},
    );
    assert_cadence(&r);
    assert_provenance(&r.seen);
    let cpu = count_fresh(&r.seen, 0, Seen::is_cpu);
    let gpu = count_fresh(&r.seen, 0, Seen::is_gpu);
    println!(
        "tick-zero stall: first repeat={} cpu_fallbacks={} gpu_frames={} cpu={cpu} gpu={gpu}",
        r.seen[0].repeat, r.cpu_fallbacks, r.gpu_frames
    );
    assert!(r.seen[0].repeat, "tick 0 is the stand-in, not a composite");
    assert_eq!(r.seen[0].cpu_fallbacks, 0, "tick 0 is never composited");
    assert!(
        cpu >= usize::try_from(CPU_BEFORE_RELEASE).unwrap(),
        "the slow CPU fallback still delivers"
    );
    assert_goal(&r.seen, 0, Seen::is_gpu, "fresh GPU frames after recovery");
}
