//! The real engine tick loop on a separate program runtime, measured per tick
//! (a copy of `chaos_flood.rs`'s harness for the TCP chaos test).
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::print_stderr,
    clippy::missing_panics_doc
)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;

/// 30 fps: an exact 33.333 ms period expressed as a rational.
pub const FPS: (i64, i64) = (30, 1);

pub fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

fn build_drive() -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "chaos".to_owned(),
            canvas: Canvas {
                width: 64,
                height: 36,
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
pub struct ClockRun {
    pub ticks: u64,
    pub expected: u64,
    pub repeats: u64,
    pub p50_ns: i64,
    pub p99_ns: i64,
    pub max_ns: i64,
    pub late_over_2ms: usize,
}

/// Run the real engine tick loop for `duration` on a 2-worker program runtime
/// and measure each tick's lateness against its wall-clock deadline.
pub fn run_clock(duration: Duration) -> ClockRun {
    let program_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("program")
        .enable_all()
        .build()
        .unwrap();
    program_rt.block_on(async move {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let clock = OutputClock::new(Rational::new(FPS.0, FPS.1)).unwrap();
        let mut runtime =
            EngineRuntime::new(clock, build_drive(), Arc::clone(&time), RealtimePacer);
        let seed = runtime.seed_nanos();
        let period_ns = 1_000_000_000i64 * FPS.1 / FPS.0;
        let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
        let lateness = Arc::new(Mutex::new(Vec::<i64>::with_capacity(4096)));
        let lat = Arc::clone(&lateness);
        let ts = Arc::clone(&time);
        let stop = StopSignal::new();
        let stop2 = stop.clone();
        let repeats = runtime.frames_repeated_counter();
        let started = Instant::now();
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            stop2.stop();
        });
        runtime
            .run(
                &publisher,
                &stop,
                move |f: &CompositedFrame| {
                    // Deadline of this tick on the time source the pacer uses.
                    let index = i64::try_from(f.tick.index).unwrap_or(i64::MAX);
                    let deadline = seed + (index * 1_000_000_000 * FPS.1) / FPS.0;
                    let late = ts.now_nanos() - deadline;
                    if let Ok(mut l) = lat.lock() {
                        l.push(late);
                    }
                    f.tick.index
                },
                |_| None,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        stopper.await.unwrap();
        let raw = lateness.lock().unwrap().clone();
        let mut worst: Vec<(usize, i64)> = raw.iter().copied().enumerate().collect();
        worst.sort_by_key(|&(_, v)| std::cmp::Reverse(v));
        eprintln!(
            "chaos worst ticks (index, ms): {:?}",
            worst
                .iter()
                .take(5)
                .map(|&(i, v)| (i, v / 1_000_000))
                .collect::<Vec<_>>()
        );
        let mut l = raw;
        l.sort_unstable();
        let pick = |q: f64| l[((l.len() as f64 - 1.0) * q) as usize];
        ClockRun {
            ticks: l.len() as u64,
            expected: (elapsed.as_nanos() as i64 / period_ns) as u64,
            repeats: repeats.load(Ordering::Relaxed),
            p50_ns: pick(0.50),
            p99_ns: pick(0.99),
            max_ns: *l.last().unwrap(),
            late_over_2ms: l.iter().filter(|&&x| x > 2_000_000).count(),
        }
    })
}
