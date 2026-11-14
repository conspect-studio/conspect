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
const TAIL: usize = 30;

#[derive(Debug, Clone, Copy)]
struct Seen {
    index: u64,
    pts_nanos: i64,
    repeat: bool,
    gpu_frames: u64,
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
            let (backend, gpu) = RunBackend::simulated_gpu(Duration::from_millis(1));
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
                    });
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

    // Every tick emitted: the run completed its budget, never ended on a miss.
    assert_eq!(outcome.stop, RunStop::Completed);
    assert_eq!(outcome.ticks, TICKS);
    assert_eq!(u64::try_from(seen.len()).unwrap(), TICKS);
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

    let tail = &seen[seen.len() - TAIL..];
    let tail_fresh = tail.iter().filter(|s| !s.repeat).count();
    let tail_gpu = tail.last().unwrap().gpu_frames - tail.first().unwrap().gpu_frames;
    println!(
        "gpu_readback_chaos: ticks={} compose_deadline_misses={misses} frames_repeated={repeats} \
         cpu_fallback_frames={cpu_fallbacks} gate_misses={gate_misses} gpu_frames={gpu_frames} \
         recovered_fresh_tail={tail_fresh}/{TAIL} tail_gpu_frames={tail_gpu}",
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
        misses < HEALTHY_FROM - HANG_FROM,
        "misses {misses} must be bounded by the hold, not the hang length"
    );
    // Automatic recovery: the tail is fresh GPU frames (slack for a loaded
    // runner's catch-up repeats).
    assert!(
        !stalled,
        "the stall must clear after the device answers again"
    );
    assert!(
        tail_fresh >= TAIL * 2 / 3,
        "only {tail_fresh}/{TAIL} fresh ticks after recovery"
    );
    assert!(
        usize::try_from(tail_gpu).unwrap() >= TAIL * 2 / 3,
        "only {tail_gpu} GPU frames in the last {TAIL} ticks"
    );
}
