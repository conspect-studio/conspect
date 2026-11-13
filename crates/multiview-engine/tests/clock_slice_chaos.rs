//! #311 reopen, ADR-0109 §8: the clock thread's short EEVDF request size
//! against CPU-bound neighbours on the same core (R3 chaos test).
//!
//! On the test server the clock thread woke on time when idle but queued
//! behind busy threads under load (decoders, the control plane at 400 req/s).
//! This pins a real clock thread (`clock_thread::spawn`, `RealtimePacer`,
//! monotonic time) and three busy-spinning threads to ONE core and measures
//! the clock's wake lateness from the engine's own `wake` histogram, twice:
//! with the kernel's default request size and with [`CLOCK_SLICE`].
//!
//! Asserted on every Linux host: every tick is emitted (exact count). Where
//! the kernel applies the slice (EEVDF, 6.12+): no tick is repeated and no
//! wake is later than half a tick period, with the slice — the bound ADR-0109
//! §8 gives a clock thread whose neighbours are CPU-bound. The default-slice
//! run is reported side by side, not asserted (its tail depends on the host;
//! #321). Older kernels only report.
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
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, StopSignal, TimeSource,
};

const CADENCE: Rational = Rational::new(30, 1);
const TICKS: u64 = 90;
const NEIGHBOURS: usize = 6;

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
fn shared_cpu() -> usize {
    let set = rustix::thread::sched_getaffinity(None).unwrap();
    (0..rustix::thread::CpuSet::MAX_CPU)
        .rev()
        .find(|cpu| set.is_set(*cpu))
        .unwrap()
}

fn pin(cpu: usize) {
    let mut set = rustix::thread::CpuSet::new();
    set.set(cpu);
    rustix::thread::sched_setaffinity(None, &set).unwrap();
}

#[derive(Debug)]
struct Run {
    slice: Slice,
    ticks: u64,
    repeats: u64,
    wake_mean_ms: f64,
    wake_p99_ms: f64,
    wake_max_ms: f64,
}

/// One run: `NEIGHBOURS` spinners and a clock thread on `cpu`; the clock
/// thread keeps [`CLOCK_SLICE`] (`custom`) or resets to the kernel default.
fn run(cpu: usize, custom: bool) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..NEIGHBOURS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                pin(cpu);
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
        pin(cpu);
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
        let outcome = clock_thread::block_on(runtime.run_for(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |f: &CompositedFrame| f.tick.index,
            |_f: &CompositedFrame| None::<()>,
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
    let cpu = shared_cpu();
    let default = run(cpu, false);
    let short = run(cpu, true);
    for (name, r) in [("default", &default), ("short", &short)] {
        println!(
            "slice chaos cpu {cpu} {name}: {:?} ticks {} repeats {} wake mean {:.3} ms p99 <= {:.3} ms max {:.3} ms",
            r.slice, r.ticks, r.repeats, r.wake_mean_ms, r.wake_p99_ms, r.wake_max_ms
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
    let half_period_ms = 1_000.0 / 2.0 / (CADENCE.num as f64 / CADENCE.den as f64);
    assert!(
        short.wake_max_ms < half_period_ms,
        "every wake within half a period with the slice: {short:?}"
    );
}
