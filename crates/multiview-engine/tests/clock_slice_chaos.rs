//! #311 reopen, ADR-0109 §8: the clock thread's short EEVDF request size
//! against CPU-bound neighbours on the same core (R3 chaos test).
//!
//! On the test server the clock thread woke on time when idle but queued
//! behind busy threads under load (decoders, the control plane at 400 req/s).
//! This pins a real clock thread (`clock_thread::spawn`, `RealtimePacer`,
//! monotonic time, 2 ms of work per tick in the control hook, like a compose)
//! and six busy-spinning threads to ONE core and measures
//! the clock's wake lateness from the engine's own `wake` histogram, twice:
//! with the kernel's default request size and with [`CLOCK_SLICE`].
//!
//! Asserted on every Linux host: every tick is emitted (exact count). Where
//! the kernel applies the slice (EEVDF custom slices, 6.12+), with the slice:
//! no tick is repeated, no wake is later than [`ONE_CORE_WAKE_CEILING_MS`] (the
//! oversubscribed bound ADR-0109 §8 gives), and — when the default run was
//! contended (mean wake above 0.5 ms) — the mean wake is under half the
//! default run's and fewer wakes are later than 1 ms (counts, not a single
//! worst sample: two runs' maxima can tie on a CI runner). Measured on the build host (6.12): default mean 2.1–2.3 ms,
//! max 11.9 ms; slice mean 0.2–0.5 ms, max 4.0–6.7 ms. Older kernels and hosts
//! that forbid pinning only report (#321: no tight percentiles).
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::as_conversions
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use multiview_clocksys::Slice;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock_thread::{self, CLOCK_SLICE};
use multiview_engine::lateness::FINE_LATENESS_EDGES_NS;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, StopSignal, TimeSource,
};

const CADENCE: Rational = Rational::new(30, 1);
const TICKS: u64 = 90;
const NEIGHBOURS: usize = 6;
/// The wake ceiling with the slice on one core shared with [`NEIGHBOURS`]
/// CPU-bound threads (ADR-0109 §8). The default request size exceeds it.
const ONE_CORE_WAKE_CEILING_MS: f64 = 10.0;

fn drive() -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(Layout {
            name: "slice-chaos".to_owned(),
            canvas: Canvas {
                width: 16,
                height: 16,
                fps_num: CADENCE.num,
                fps_den: CADENCE.den,
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
}

/// The last CPU this process may run on: the shared core of one run.
fn shared_cpu() -> Option<usize> {
    let set = rustix::thread::sched_getaffinity(None).ok()?;
    (0..rustix::thread::CpuSet::MAX_CPU)
        .rev()
        .find(|cpu| set.is_set(*cpu))
}

fn pin(cpu: usize) -> bool {
    let mut set = rustix::thread::CpuSet::new();
    set.set(cpu);
    rustix::thread::sched_setaffinity(None, &set).is_ok()
}

/// Per-tick work on the clock thread (a compose), spun on the monotonic clock.
const WORK: std::time::Duration = std::time::Duration::from_millis(2);

#[derive(Debug)]
struct Run {
    slice: Slice,
    ticks: u64,
    repeats: u64,
    wake_mean_ms: f64,
    wake_p99_ms: f64,
    wake_max_ms: f64,
    /// Wakes later than 1 ms (fine buckets above the 1 ms edge).
    late_wakes: u64,
}

/// One run: `NEIGHBOURS` spinners and a clock thread on `cpu`; the clock
/// thread keeps [`CLOCK_SLICE`] (`custom`) or resets to the kernel default.
fn run(cpu: usize, custom: bool) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..NEIGHBOURS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                assert!(pin(cpu), "a spinner could not pin to cpu {cpu}");
                let mut x = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_add(1));
                }
            })
        })
        .collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let handle = clock_thread::spawn("clock-slice", move || {
        assert!(pin(cpu), "the clock thread could not pin to cpu {cpu}");
        // `spawn` already asked for CLOCK_SLICE; the default run resets it.
        let slice = multiview_clocksys::set_current_thread_slice(if custom {
            u64::try_from(CLOCK_SLICE.as_nanos()).unwrap()
        } else {
            0
        });
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut runtime = EngineRuntime::new(
            OutputClock::new(CADENCE).unwrap(),
            drive(),
            time,
            RealtimePacer,
        );
        let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
        let outcome = clock_thread::block_on(runtime.run_for_with_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |f: &CompositedFrame| f.tick.index,
            |_f: &CompositedFrame| None::<()>,
            |_d: &mut CompositorDrive<Nv12Image>| {
                let until = std::time::Instant::now() + WORK;
                while std::time::Instant::now() < until {
                    std::hint::spin_loop();
                }
            },
        ))
        .unwrap();
        let wake = runtime.phases().snapshot().wake;
        Run {
            slice,
            ticks: outcome.ticks,
            repeats: runtime.frames_repeated(),
            wake_mean_ms: wake.mean_ns().unwrap_or(0) as f64 / 1e6,
            wake_p99_ms: wake.fine_quantile_upper_ns(990).unwrap_or(u64::MAX) as f64 / 1e6,
            wake_max_ms: wake.max_ns as f64 / 1e6,
            late_wakes: wake
                .fine_buckets
                .iter()
                .zip(std::iter::once(&0).chain(FINE_LATENESS_EDGES_NS.iter()))
                .filter(|(_, lower)| **lower >= 1_000_000)
                .map(|(count, _)| *count)
                .sum(),
        }
    })
    .unwrap();
    let result = rt.block_on(handle.join()).unwrap();
    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        spinner.join().unwrap();
    }
    result
}

#[test]
fn a_short_slice_wakes_the_clock_through_cpu_bound_neighbours() {
    let Some(cpu) = shared_cpu().filter(|cpu| pin_probe(*cpu)) else {
        println!("cannot pin threads to one cpu here: skipped");
        return;
    };
    let default = run(cpu, false);
    let short = run(cpu, true);
    for (name, r) in [("default", &default), ("short", &short)] {
        println!(
            "slice chaos cpu {cpu} {name}: {:?} ticks {} repeats {} wake mean {:.3} ms p99 <= {:.3} ms max {:.3} ms, {} wakes > 1 ms",
            r.slice, r.ticks, r.repeats, r.wake_mean_ms, r.wake_p99_ms, r.wake_max_ms, r.late_wakes
        );
    }
    assert_eq!(default.ticks, TICKS, "{default:?}");
    assert_eq!(short.ticks, TICKS, "{short:?}");
    let Slice::Applied(applied) = short.slice else {
        println!(
            "kernel has no custom EEVDF slice ({:?}): bounds not asserted",
            short.slice
        );
        return;
    };
    assert_eq!(applied, u64::try_from(CLOCK_SLICE.as_nanos()).unwrap());
    assert_eq!(
        short.repeats, 0,
        "no tick fell behind with the slice: {short:?}"
    );
    assert!(
        short.wake_max_ms < ONE_CORE_WAKE_CEILING_MS,
        "every wake within the one-core ceiling with the slice: {short:?}"
    );
    if default.wake_mean_ms > 0.5 {
        assert!(
            short.wake_mean_ms < default.wake_mean_ms / 2.0,
            "the slice at least halves the mean wake: {short:?} vs {default:?}"
        );
        assert!(
            short.late_wakes < default.late_wakes,
            "the slice leaves fewer wakes later than 1 ms: {short:?} vs {default:?}"
        );
    }
}

/// Whether this host lets a thread pin itself (containers may forbid it).
fn pin_probe(cpu: usize) -> bool {
    std::thread::spawn(move || pin(cpu)).join().unwrap_or(false)
}
