//! R3 chaos test for issue #395 (ADR-0109 §9): a hung / slow GPU readback
//! never stalls Program.
//!
//! A real output-clock thread (`clock_thread::spawn` + `RealtimePacer` +
//! `MonotonicTimeSource`) runs 30 fps over the GPU-free simulated GPU backend
//! (`RunBackend::simulated_gpu`), which routes every frame through the SAME
//! readback deadline/stall state machine as the wgpu compositor:
//!
//! healthy (1 ms readback) → hung for 40 ticks (≈1.3 s, beyond `STALL_HOLD`)
//! → slow (60 ms > the 10 ms deadline) for 10 ticks → healthy again.
//!
//! Asserted: every tick is emitted with contiguous indices and strictly
//! increasing PTS; misses are counted and repeated as last-good; the long hang
//! hands frames to the CPU reference; and the GPU path recovers by itself.
//! Only counts and design bounds with generous slack (loaded macOS runner,
//! #321/#284) — no percentiles. As in `gpu_failure_chaos` (#447), every
//! assertion is exact per-frame provenance (each emitted frame samples the
//! backend's deadline-miss and CPU-fallback counters, so a fresh frame is
//! exactly a GPU or a CPU composite and each miss is attributed to its tick)
//! or liveness measured in ticks: the run continues past `TICKS` until the
//! recovered GPU path has delivered its fresh frames, failing only if it has
//! not within `MAX_TICKS`. How many ticks a starved host misses after
//! recovery is scheduling, not behaviour (#475).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock_thread;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, RunStop, StopSignal, Tick, TimeSource,
};

const TICKS: u64 = 150;
const HANG_FROM: u64 = 30;
const SLOW_FROM: u64 = 70;
const HEALTHY_FROM: u64 = 80;
/// The liveness cap: a run whose recovered GPU path has not met its goal
/// after this many ticks (120 s at 30 fps) fails. A healthy host stops at
/// `TICKS`.
const MAX_TICKS: u64 = 3600;
/// Fresh GPU frames the recovered path must deliver from `HEALTHY_FROM` on.
const FRESH_GOAL: usize = 20;
/// Of them, fresh GPU frames that must ALSO arrive after the minimum `TICKS`
/// window: a path that recovers and then stops delivering never meets this.
const CONTINUING_GOAL: usize = 10;

#[derive(Debug, Clone, Copy)]
struct Seen {
    index: u64,
    pts_nanos: i64,
    repeat: bool,
    /// The backend's CPU-fallback count right after this frame was composed.
    cpu_fallbacks: u64,
    /// The backend's readback deadline-miss count right after this frame.
    gate_misses: u64,
}

impl Seen {
    /// Whether this fresh frame came from the GPU worker (no CPU fallback was
    /// counted for it), given the frame before it.
    fn is_gpu(&self, prev: &Self) -> bool {
        !self.repeat && self.cpu_fallbacks == prev.cpu_fallbacks
    }
}

/// Fresh GPU frames emitted at tick `from` or later.
fn fresh_gpu(seen: &[Seen], from: u64) -> usize {
    seen.windows(2)
        .filter(|p| p[1].index >= from && p[1].is_gpu(&p[0]))
        .count()
}

/// The recovery liveness goal.
fn recovered(seen: &[Seen]) -> bool {
    fresh_gpu(seen, HEALTHY_FROM) >= FRESH_GOAL && fresh_gpu(seen, TICKS) >= CONTINUING_GOAL
}

#[test]
fn hung_and_slow_gpu_readback_never_stalls_program_and_recovers() {
    run_chaos(Duration::ZERO);
}

/// Review INV-1: the CPU fallback after the stall hold is itself slower than a
/// tick (50 ms per frame against a 33 ms period). The clock still emits every
/// tick (ADR-T018 repeats-to-cadence absorbs the slow compose, as for any slow
/// CPU-backend Program), stamps stay contiguous, and the GPU path recovers.
#[test]
fn slower_than_a_tick_cpu_fallback_still_keeps_every_tick() {
    run_chaos(Duration::from_millis(50));
}

fn run_chaos(fallback_cost: Duration) {
    let cadence = Rational::new(30, 1);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.block_on(async {
        clock_thread::spawn("gpu-chaos", move || {
            let (backend, gpu) = RunBackend::simulated_gpu(Duration::from_millis(1)).unwrap();
            gpu.set_cpu_fallback_cost(fallback_cost);
            let drive = CompositorDrive::new(
                Arc::new(Layout {
                    name: "gpu-chaos".to_owned(),
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
            .with_backend(backend);
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
                        cpu_fallbacks: observe.cpu_fallback_frames(),
                        gate_misses: observe.deadline_misses(),
                    });
                    if u64::try_from(seen.len()).unwrap() >= TICKS && recovered(&seen) {
                        stop.stop();
                    }
                },
                |_f: &CompositedFrame| None::<()>,
                move |_d: &mut CompositorDrive<Nv12Image>, tick: Tick| {
                    // Idempotent by phase, so a catch-up repeat burst (which
                    // runs this hook less often) cannot skip a transition.
                    match tick.index {
                        i if i < HANG_FROM => {
                            steer.release();
                            steer.set_latency(Duration::from_millis(1));
                        }
                        i if i < SLOW_FROM => steer.hang(),
                        i if i < HEALTHY_FROM => {
                            steer.release();
                            steer.set_latency(Duration::from_millis(60));
                        }
                        _ => {
                            steer.release();
                            steer.set_latency(Duration::from_millis(1));
                        }
                    }
                },
            ))
            .unwrap();
            (
                outcome,
                seen,
                runtime.compose_deadline_misses(),
                runtime.frames_repeated(),
                gpu.cpu_fallback_frames(),
                gpu.deadline_misses(),
                gpu.gpu_frames(),
                gpu.is_stalled(),
            )
        })
        .unwrap()
    });
    let (outcome, seen, misses, repeats, cpu_fallbacks, gate_misses, gpu_frames, stalled) =
        rt.block_on(handle.join()).unwrap();

    // Every tick emitted; the run ended because the recovered GPU path met its
    // goal (`Stopped`), not at the `MAX_TICKS` cap, and never on a miss.
    assert_eq!(
        outcome.stop,
        RunStop::Stopped,
        "the recovered GPU path did not deliver {FRESH_GOAL} fresh frames \
         ({CONTINUING_GOAL} after tick {TICKS}) within {MAX_TICKS} ticks"
    );
    assert!(outcome.ticks >= TICKS);
    assert_eq!(u64::try_from(seen.len()).unwrap(), outcome.ticks);
    // Contiguous indices, strictly increasing PTS.
    for (i, s) in seen.iter().enumerate() {
        assert_eq!(
            s.index,
            u64::try_from(i).unwrap(),
            "tick indices must be contiguous"
        );
    }
    for pair in seen.windows(2) {
        assert!(
            pair[1].pts_nanos > pair[0].pts_nanos,
            "PTS must strictly increase"
        );
    }

    // Exact provenance per frame: a repeat never counts a CPU fallback, a fresh
    // frame counts at most one, and a readback miss is always a repeat.
    for pair in seen.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let fallback = cur.cpu_fallbacks - prev.cpu_fallbacks;
        let tick_misses = cur.gate_misses - prev.gate_misses;
        if cur.repeat {
            assert_eq!(
                fallback, 0,
                "tick {}: a repeat counted a CPU fallback",
                cur.index
            );
        } else {
            assert!(
                fallback <= 1,
                "tick {}: {fallback} CPU fallbacks",
                cur.index
            );
            assert_eq!(
                tick_misses, 0,
                "tick {}: a missed readback was emitted fresh",
                cur.index
            );
        }
        assert!(
            tick_misses <= 1,
            "tick {}: {tick_misses} misses on one tick",
            cur.index
        );
    }
    // Misses attributed to the faulted window (hung + slow readbacks).
    let window_misses = seen
        .iter()
        .find(|s| s.index + 1 == HEALTHY_FROM)
        .unwrap()
        .gate_misses
        - seen
            .iter()
            .find(|s| s.index + 1 == HANG_FROM)
            .unwrap()
            .gate_misses;
    let fresh_after = fresh_gpu(&seen, HEALTHY_FROM);
    let continuing = fresh_gpu(&seen, TICKS);
    println!(
        "gpu_readback_chaos: ticks={} compose_deadline_misses={misses} frames_repeated={repeats} \
         cpu_fallback_frames={cpu_fallbacks} gate_misses={gate_misses} gpu_frames={gpu_frames} \
         window_misses={window_misses} fresh_gpu_after_recovery={fresh_after} \
         fresh_gpu_after_tick_{TICKS}={continuing}",
        outcome.ticks
    );

    // Misses are counted and every one was emitted as a last-good repeat.
    assert!(misses >= 1, "the hang must produce deadline misses");
    assert!(repeats >= misses, "each miss is a counted repeat");
    assert_eq!(
        misses, gate_misses,
        "every compositor GpuDeadline became exactly one runtime miss"
    );
    // The long hang (≈1.3 s > STALL_HOLD 500 ms) hands frames to the CPU.
    assert!(
        cpu_fallbacks > 0,
        "a GPU hung past the stall hold composites on the CPU"
    );
    // Misses are bounded: the hang fails fast until the hold, then the CPU
    // takes over, so far fewer misses than hung + slow ticks.
    assert!(
        window_misses < HEALTHY_FROM - HANG_FROM,
        "misses {window_misses} in the faulted window must be bounded by the hold, \
         not the hang length"
    );
    // Automatic recovery: the stall cleared and the GPU path delivers fresh
    // frames again, continuing past the minimum window (liveness in ticks).
    assert!(
        !stalled,
        "the stall must clear after the device answers again"
    );
    assert!(
        recovered(&seen),
        "recovered GPU path: {fresh_after} fresh from tick {HEALTHY_FROM} (need {FRESH_GOAL}), \
         {continuing} after tick {TICKS} (need {CONTINUING_GOAL})"
    );
}
