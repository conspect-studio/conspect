//! Clock harness for the UDP chaos tests (`chaos_flood.rs`, `stall_witness.rs`,
//! loaded with `#[path = "common/witness.rs"] mod common;`):
//! the REAL engine tick loop (`EngineRuntime` + `RealtimePacer` +
//! `MonotonicTimeSource`) on its own "program" runtime, a per-tick record, and
//! the **stall witness** (issue #321).
//!
//! Why the witness: the engine absorbs a stall it cannot prevent (the whole
//! process descheduled by the host) by emitting last-good repeats under fresh
//! ticks (ADR-T018); that is the design working. Asserting `repeats == 0`
//! asserted that the CI host never stalls the process for more than a period,
//! which no code under test can promise (macOS run 43192931229: one ~104 ms
//! process stall, three repeats, door off). The witness is a plain std thread
//! that sleeps 1 ms in a loop on the same time base as the tick deadlines and
//! records every sleep longer than [`MIN_GAP_NS`]: while it was not running, the
//! process was not running. Each gap is stamped *inside* the sleep (the clock
//! read just before `sleep` and just after it returns), so it holds only time
//! the witness was blocked; a gap counts from [`MIN_GAP_NS`] (5 ms, five
//! sleeps: a real deschedule, not timer jitter).
//!
//! What needs explaining is a **repeat**: the engine's "tick behind"
//! (ADR-T018, `frames_repeated`, the metric issue #289 asserts). A fresh tick
//! that runs a little past a period carries its own tick and PTS (asserted
//! exactly for every tick) and is not behind; the slice-1 suite never asserted
//! on it either. A repeat is **explained** only when its
//! lateness minus the witness-observed stall time inside its `[deadline,
//! emit]` window is less than one period: without the time the process was
//! seen not running, the tick would not have been a period late. The excuse is
//! proportional, never a whole-period pass for any overlap, so a slip the code
//! under test caused (the program runtime blocked or starved while the process
//! ran) stays unexplained even when a stall sits next to it (review PR 333),
//! while a host that deschedules the witness and the clock thread at slightly
//! different moments (macOS CI run 43324610127: gaps of 43.6/76.5 ms against
//! repeats 50/103 ms late) is not blamed on the code. The controls in
//! `stall_witness.rs` prove both.
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
    clippy::doc_markdown
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;

/// 30 fps: an exact rational cadence.
pub const FPS: (i64, i64) = (30, 1);
/// The tick period in whole nanoseconds (for gap thresholds only; expected
/// PTS values come from `MediaTime::from_tick`).
pub const PERIOD_NS: i64 = 1_000_000_000 * FPS.1 / FPS.0;

pub fn cadence() -> Rational {
    Rational::new(FPS.0, FPS.1)
}

pub fn build_drive() -> CompositorDrive<Nv12Image> {
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

/// One emitted tick.
#[derive(Debug, Clone, Copy)]
pub struct TickRecord {
    pub index: u64,
    pub pts: MediaTime,
    /// Its deadline on the time source.
    pub deadline: i64,
    /// When `state_of` saw it, on the same time source.
    pub emit: i64,
    pub repeat: bool,
}

impl TickRecord {
    pub fn lateness(&self) -> i64 {
        self.emit - self.deadline
    }
}

#[derive(Debug, Clone)]
pub struct ClockRun {
    pub ticks: Vec<TickRecord>,
    pub expected: u64,
    pub repeats: u64,
    /// Witness gaps `[start, end]` (time-source nanoseconds).
    pub gaps: Vec<(i64, i64)>,
}

/// Whether the witness can attribute a stall on this platform. Linux: yes
/// (build host and ubuntu CI record 0 gaps in an idle 3 s run, and the
/// controls in `stall_witness.rs` show it never excuses a starved clock).
/// macOS: no. On macOS CI (run 43331219140) the witness recorded 255-302 gaps
/// of at least 5 ms in 3 s with no stall injected, and a 136 ms gap while
/// another thread merely spun (headless processes there get coalesced
/// timers), so it can neither prove nor excuse anything; there the suite
/// asserts what the design guarantees on any host (one frame per tick,
/// consecutive indices, exact PTS) and reports repeats without judging them.
/// The production target (mda001) is Linux.
pub const WITNESS_RELIABLE: bool = cfg!(target_os = "linux");

/// The shortest witness sleep that counts as a gap: five times the 1 ms sleep.
pub const MIN_GAP_NS: i64 = 5_000_000;

/// The witness thread: sleeps 1 ms in a loop and records every sleep that
/// took at least [`MIN_GAP_NS`], stamped inside the sleep.
struct Witness {
    stop: Arc<AtomicBool>,
    gaps: Arc<Mutex<Vec<(i64, i64)>>>,
    thread: std::thread::JoinHandle<()>,
}

impl Witness {
    fn start(time: Arc<dyn TimeSource>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let gaps = Arc::new(Mutex::new(Vec::new()));
        let (s, g) = (Arc::clone(&stop), Arc::clone(&gaps));
        let thread = std::thread::Builder::new()
            .name("stall-witness".to_owned())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    let before = time.now_nanos();
                    std::thread::sleep(Duration::from_millis(1));
                    let after = time.now_nanos();
                    if after - before >= MIN_GAP_NS {
                        g.lock().unwrap().push((before, after));
                    }
                }
            })
            .unwrap();
        Self { stop, gaps, thread }
    }

    fn stop(self) -> Vec<(i64, i64)> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
        let g = self.gaps.lock().unwrap().clone();
        g
    }
}

/// Run the real engine tick loop for `duration` on a 2-worker program runtime.
/// `on_tick(index)` runs inside the tick's `state_of` (on the program runtime):
/// the negative control blocks there.
pub fn run_clock(duration: Duration, on_tick: impl FnMut(u64) + Send + 'static) -> ClockRun {
    let program_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("program")
        .enable_all()
        .build()
        .unwrap();
    let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
    let witness = Witness::start(Arc::clone(&time));
    let mut on_tick = on_tick;
    let (records, expected, repeats) = program_rt.block_on(async move {
        let clock = OutputClock::new(cadence()).unwrap();
        let mut runtime =
            EngineRuntime::new(clock, build_drive(), Arc::clone(&time), RealtimePacer);
        let seed = runtime.seed_nanos();
        let deadline_clock = OutputClock::new(cadence()).unwrap();
        let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
        let records = Arc::new(Mutex::new(Vec::<TickRecord>::with_capacity(4096)));
        let rec = Arc::clone(&records);
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
                    let emit = ts.now_nanos();
                    let record = TickRecord {
                        index: f.tick.index,
                        pts: f.tick.pts,
                        deadline: deadline_clock.deadline_nanos(f.tick.index, seed),
                        emit,
                        repeat: f.repeat,
                    };
                    if let Ok(mut r) = rec.lock() {
                        r.push(record);
                    }
                    on_tick(f.tick.index);
                    f.tick.index
                },
                |_| None,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        stopper.await.unwrap();
        let r = records.lock().unwrap().clone();
        (
            r,
            (elapsed.as_nanos() as i64 / PERIOD_NS) as u64,
            repeats.load(Ordering::Relaxed),
        )
    });
    let gaps = witness.stop();
    ClockRun {
        ticks: records,
        expected,
        repeats,
        gaps,
    }
}

/// The classification of every tick that needed help.
#[derive(Debug, Clone, Default)]
pub struct Classification {
    /// Repeats (the engine's ticks behind).
    pub needing_explanation: usize,
    /// Of those, explained by the witness (see [`classify`]).
    pub explained: usize,
    /// The rest: slips the code under test is responsible for.
    pub unexplained: Vec<(u64, i64, bool)>,
}

pub fn classify(run: &ClockRun) -> Classification {
    let mut c = Classification::default();
    for t in &run.ticks {
        if !t.repeat {
            continue;
        }
        c.needing_explanation += 1;
        if t.lateness() - stalled_within(&run.gaps, t.deadline, t.emit) < PERIOD_NS {
            c.explained += 1;
        } else {
            c.unexplained
                .push((t.index, t.lateness() / 1_000_000, t.repeat));
        }
    }
    c
}

/// Witness-observed stall time inside `[from, to]`: the length of the union of
/// the gaps' intersections with it (gaps never overlap: one thread records
/// them in order).
pub fn stalled_within(gaps: &[(i64, i64)], from: i64, to: i64) -> i64 {
    gaps.iter()
        .map(|&(start, end)| (end.min(to) - start.max(from)).max(0))
        .sum()
}

pub fn report(name: &str, r: &ClockRun) {
    let mut late: Vec<i64> = r.ticks.iter().map(TickRecord::lateness).collect();
    late.sort_unstable();
    let max = late.last().copied().unwrap_or(0);
    let c = classify(r);
    let longest_gap = r.gaps.iter().map(|(s, e)| e - s).max().unwrap_or(0);
    eprintln!(
        "chaos {name}: ticks {} (expected {}) repeats {} late>1 period {} max lateness {:.3} ms; \
         witness gaps {} (longest {:.1} ms); needing explanation {}, explained {}, unexplained {:?}",
        r.ticks.len(),
        r.expected,
        r.repeats,
        r.ticks.iter().filter(|t| t.lateness() > PERIOD_NS).count(),
        max as f64 / 1e6,
        r.gaps.len(),
        longest_gap as f64 / 1e6,
        c.needing_explanation,
        c.explained,
        c.unexplained
    );
}

/// The chaos suite's per-arm assertion (issues #289, #321), also used by the
/// `stall_witness.rs` controls: [`assert_shape`] on every platform, and where
/// [`WITNESS_RELIABLE`] (Linux) zero repeats not explained by a whole-process
/// stall. Absolute, never relative to another arm.
pub fn assert_cadence(name: &str, r: &ClockRun) {
    assert_shape(name, r);
    let c = classify(r);
    if !WITNESS_RELIABLE {
        eprintln!(
            "{name}: {} repeats not judged (the stall witness is unreliable on this platform; \
             see WITNESS_RELIABLE)",
            c.needing_explanation
        );
        return;
    }
    assert!(
        c.unexplained.is_empty(),
        "{name}: {} repeats/slips not explained by a whole-process stall: {:?} (gaps {:?})",
        c.unexplained.len(),
        c.unexplained,
        r.gaps
    );
}

/// What the design guarantees on any host, whatever the load: one frame per
/// tick, consecutive indices, every PTS exactly `from_tick(index)`.
pub fn assert_shape(name: &str, r: &ClockRun) {
    let ticks = r.ticks.len() as u64;
    assert!(
        ticks + 2 >= r.expected && ticks <= r.expected + 2,
        "{name}: one frame per tick: {ticks} vs expected {}",
        r.expected
    );
    for pair in r.ticks.windows(2) {
        assert_eq!(
            pair[1].index,
            pair[0].index + 1,
            "{name}: consecutive tick indices"
        );
    }
    for t in &r.ticks {
        let expected = MediaTime::from_tick(i64::try_from(t.index).unwrap(), cadence());
        assert_eq!(
            t.pts, expected,
            "{name}: PTS of tick {} from the tick",
            t.index
        );
    }
}

/// The assertion #321 retired: "no tick ever repeated". It attributes host
/// stalls (which the clock absorbs by design) to the code under test.
pub fn old_assert_cadence(name: &str, r: &ClockRun) {
    let ticks = r.ticks.len() as u64;
    assert!(
        ticks + 2 >= r.expected && ticks <= r.expected + 2,
        "{name}: one frame per tick"
    );
    assert_eq!(r.repeats, 0, "{name}: no tick fell behind compose");
}
