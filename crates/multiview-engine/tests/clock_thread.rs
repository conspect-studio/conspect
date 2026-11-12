//! The dedicated output-clock thread (#311, ADR-0109): deterministic contracts.
//!
//! * exact tick count and PTS over a long cooperative run driven on a real
//!   clock thread (`clock_thread::spawn` + `block_on`);
//! * no drift at 29.97 over a simulated day (every PTS and deadline is the exact
//!   rational of its tick index; the last deadline is a day of media time);
//! * the migration thread handoff: OLD and NEW together emit every tick index
//!   exactly once, deterministic (manual clock, with and without catch-up
//!   repeats around the cut) and on real clock threads (`ProgramSet`);
//! * shutdown while the clock thread sleeps is prompt and freezes the clock;
//! * a panicking egress consumer never stops the clock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::ProgramSpec;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::clock_thread;
use multiview_engine::{
    CompositedFrame, CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime,
    MonotonicTimeSource, OutputClock, Pacer, Program, ProgramSet, RealtimePacer, RunStop,
    StopSignal, Tick, TimeSource,
};

fn drive(cadence: Rational) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(Layout {
            name: "clock-thread".to_owned(),
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
}

fn spec(id: &str, cadence: Rational) -> ProgramSpec {
    let json = format!(
        r##"{{
            "id": "{id}",
            "kind": "multiview",
            "canvas": {{
                "width": 16,
                "height": 16,
                "fps": "{}/{}",
                "pixel_format": "nv12",
                "background": "#000000",
                "color": {{ "profile": "sdr-bt709-limited" }}
            }},
            "layout": {{ "kind": "preset", "preset": "1x1" }}
        }}"##,
        cadence.num, cadence.den
    );
    serde_json::from_str(&json).unwrap()
}

/// Run `ticks` ticks of a cooperative runtime ON A CLOCK THREAD, advancing the
/// manual clock by exactly one tick period per fresh compose (so every deadline
/// is reached when the pacer asks), and check every emitted tick inline.
/// Returns (ticks emitted, PTS mismatches, last index, last deadline - seed).
fn cooperative_run_on_clock_thread(cadence: Rational, ticks: u64) -> (u64, u64, u64, i64) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.block_on(async {
        clock_thread::spawn("clock-coop", move || {
            assert!(clock_thread::on_clock_thread());
            let time = Arc::new(ManualTimeSource::new());
            let ts: Arc<dyn TimeSource> = time.clone();
            let clock = OutputClock::new(cadence).unwrap();
            let mut runtime = EngineRuntime::new(clock, drive(cadence), ts, CooperativePacer);
            let seed = runtime.seed_nanos();
            let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
            let mismatches = AtomicU64::new(0);
            let last = AtomicU64::new(0);
            let reference = OutputClock::new(cadence).unwrap();
            let advance = Arc::clone(&time);
            let outcome = clock_thread::block_on(runtime.run_for_with_tick_control(
                &publisher,
                &StopSignal::new(),
                ticks,
                |f: &CompositedFrame| {
                    let i = f.tick.index;
                    let exact = MediaTime::from_tick(i as i64, cadence);
                    if f.pts() != exact || f.tick.pts != reference.pts_at(i) || f.repeat {
                        mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                    last.store(i, Ordering::Relaxed);
                    i
                },
                |_f: &CompositedFrame| None::<()>,
                move |_d: &mut CompositorDrive<Nv12Image>, tick: Tick| {
                    // Reach the NEXT tick's deadline exactly (exact rational).
                    advance.set(
                        seed + MediaTime::from_tick(tick.index as i64 + 1, cadence).as_nanos(),
                    );
                },
            ))
            .unwrap();
            assert_eq!(outcome.stop, RunStop::Completed);
            let last_index = last.load(Ordering::Relaxed);
            (
                outcome.ticks,
                mismatches.load(Ordering::Relaxed),
                last_index,
                reference.deadline_nanos(last_index, seed) - seed,
            )
        })
        .unwrap()
    });
    rt.block_on(handle.join()).unwrap()
}

#[test]
fn a_long_cooperative_run_on_a_clock_thread_emits_exactly_one_correct_frame_per_tick() {
    let ticks = 200_000;
    let (emitted, mismatches, last, _) = cooperative_run_on_clock_thread(Rational::FPS_25, ticks);
    assert_eq!(emitted, ticks, "exactly one frame per tick");
    assert_eq!(mismatches, 0, "every PTS is f(tick), no repeats");
    assert_eq!(last, ticks - 1, "indices are contiguous from 0");
}

#[test]
fn no_drift_at_29_97_over_a_simulated_day() {
    // One day of 30000/1001 fps: 86 400 s * 30000 / 1001 = 2 589 410.6 ticks.
    let ticks = 2_589_410_u64;
    let (emitted, mismatches, last, last_deadline) =
        cooperative_run_on_clock_thread(Rational::FPS_29_97, ticks);
    assert_eq!(emitted, ticks);
    assert_eq!(mismatches, 0, "every PTS is the exact rational of its tick");
    // The last tick's deadline is exactly last * 1001 / 30000 s (rounded to the
    // nearest ns once, never accumulated). A float-accumulated 29.97 would be
    // off by ~86 s here.
    let exact_ns = (i128::from(last) * 1001 * 1_000_000_000 + 15_000) / 30_000;
    assert!(
        (i128::from(last_deadline) - exact_ns).abs() <= 1,
        "deadline of tick {last} is {last_deadline} ns, exact {exact_ns} ns"
    );
    assert!(last_deadline > 86_399_000_000_000 && last_deadline < 86_400_000_000_000);
}

/// A pacer that wakes exactly at the deadline by moving the manual clock there.
struct JumpPacer(Arc<ManualTimeSource>);

impl Pacer for JumpPacer {
    fn wait_until(
        &self,
        deadline_nanos: i64,
        source: &dyn TimeSource,
        _stop: &StopSignal,
    ) -> impl std::future::Future<Output = ()> + Send {
        if source.now_nanos() < deadline_nanos {
            self.0.set(deadline_nanos);
        }
        std::future::ready(())
    }
}

/// OLD runs until its own hook requests a handoff at `request_at`; NEW (same
/// seed) then runs. `slow_from` makes every fresh compose from that tick on
/// cost `cost` (so the cadence-hold path emits repeats around the cut).
fn deterministic_handoff(
    request_at: u64,
    slow_from: u64,
    cost: Duration,
) -> (Vec<u64>, Vec<u64>, u64, u64) {
    let cadence = Rational::FPS_25;
    let time = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time.clone();
    let mut old = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        Arc::clone(&ts),
        JumpPacer(Arc::clone(&time)),
    );
    let seed = old.seed_nanos();
    let handoff = old.handoff();
    let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
    let old_seen = Mutex::new(Vec::new());
    let request = Arc::clone(&handoff);
    let slow = Arc::clone(&time);
    let outcome = clock_thread::block_on(old.run_for_with_tick_control(
        &publisher,
        &StopSignal::new(),
        10_000,
        |f: &CompositedFrame| {
            old_seen.lock().unwrap().push(f.tick.index);
            f.tick.index
        },
        |_f: &CompositedFrame| None::<()>,
        move |_d: &mut CompositorDrive<Nv12Image>, tick: Tick| {
            if tick.index >= slow_from {
                slow.advance(cost);
            }
            if tick.index == request_at {
                request.request();
            }
        },
    ))
    .unwrap();
    assert_eq!(outcome.stop, RunStop::HandedOff);
    let cut = handoff.cut().expect("OLD chose a cut");
    let old_repeats = old.frames_repeated();

    let mut new = EngineRuntime::with_seed(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        ts,
        JumpPacer(Arc::clone(&time)),
        seed,
    );
    new.succeed(Arc::clone(&handoff));
    let new_seen = Mutex::new(Vec::new());
    let outcome = clock_thread::block_on(new.run_for(
        &publisher,
        &StopSignal::new(),
        50,
        |f: &CompositedFrame| {
            // Same timeline: PTS is f(tick) on the shared seed.
            assert_eq!(f.pts(), MediaTime::from_tick(f.tick.index as i64, cadence));
            new_seen.lock().unwrap().push(f.tick.index);
            f.tick.index
        },
        |_f: &CompositedFrame| None::<()>,
    ))
    .unwrap();
    assert_eq!(outcome.ticks, 50);
    (
        old_seen.into_inner().unwrap(),
        new_seen.into_inner().unwrap(),
        cut,
        old_repeats,
    )
}

fn assert_contiguous_once(old: &[u64], new: &[u64], cut: u64) {
    assert!(
        old.iter().all(|i| *i < cut),
        "OLD never emits the cut or later"
    );
    assert!(
        new.iter().all(|i| *i >= cut),
        "NEW never emits before the cut"
    );
    let mut all: Vec<u64> = old.iter().chain(new).copied().collect();
    let n = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), n, "no tick emitted twice");
    let expected: Vec<u64> = (0..n as u64).collect();
    assert_eq!(all, expected, "no tick missed: 0..{n} contiguous");
    assert_eq!(old.last().copied(), Some(cut - 1));
    assert_eq!(new.first().copied(), Some(cut));
}

#[test]
fn migration_handoff_emits_every_tick_exactly_once() {
    let (old, new, cut, repeats) = deterministic_handoff(100, u64::MAX, Duration::ZERO);
    assert_eq!(repeats, 0);
    assert_eq!(cut, 101 + multiview_engine::HANDOFF_LEAD_TICKS);
    assert_contiguous_once(&old, &new, cut);
}

#[test]
fn migration_handoff_is_exact_while_old_is_catching_up_with_repeats() {
    // From tick 90 every fresh compose costs 2.5 periods: OLD emits last-good
    // repeats to hold cadence right across the request and the cut.
    let (old, new, cut, repeats) = deterministic_handoff(100, 90, Duration::from_millis(100));
    assert!(
        repeats > 0,
        "OLD was catching up with repeats around the cut"
    );
    assert_contiguous_once(&old, &new, cut);
}

#[test]
fn migration_handoff_is_exact_when_old_resyncs_past_the_cut() {
    // From tick 95 one compose costs 10 s (250 periods > MAX_REPEATS_PER_TICK):
    // OLD emits the cap of repeats, then would `skip_to` far past the cut. The
    // resync is clamped to the cut, so nothing in [cut, floor) is lost.
    let cadence = Rational::FPS_25;
    let time = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time.clone();
    let mut old = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        Arc::clone(&ts),
        JumpPacer(Arc::clone(&time)),
    );
    let seed = old.seed_nanos();
    let handoff = old.handoff();
    let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
    let old_seen = Mutex::new(Vec::new());
    let request = Arc::clone(&handoff);
    let slow = Arc::clone(&time);
    let outcome = clock_thread::block_on(old.run_for_with_tick_control(
        &publisher,
        &StopSignal::new(),
        10_000,
        |f: &CompositedFrame| {
            old_seen.lock().unwrap().push(f.tick.index);
            f.tick.index
        },
        |_f: &CompositedFrame| None::<()>,
        move |_d: &mut CompositorDrive<Nv12Image>, tick: Tick| {
            if tick.index == 95 {
                request.request();
                slow.advance(Duration::from_secs(10));
            }
        },
    ))
    .unwrap();
    assert_eq!(outcome.stop, RunStop::HandedOff);
    let cut = handoff.cut().unwrap();
    assert_eq!(cut, 96 + multiview_engine::HANDOFF_LEAD_TICKS);
    // Re-running OLD after the handoff emits nothing (its clock never rewinds).
    let rerun = clock_thread::block_on(old.run_for(
        &publisher,
        &StopSignal::new(),
        10,
        |f: &CompositedFrame| f.tick.index,
        |_f: &CompositedFrame| None::<()>,
    ))
    .unwrap();
    assert_eq!((rerun.ticks, rerun.stop), (0, RunStop::HandedOff));
    let mut new = EngineRuntime::with_seed(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        ts,
        JumpPacer(Arc::clone(&time)),
        seed,
    );
    new.succeed(Arc::clone(&handoff));
    let new_seen = Mutex::new(Vec::new());
    clock_thread::block_on(new.run_for(
        &publisher,
        &StopSignal::new(),
        20,
        |f: &CompositedFrame| {
            new_seen.lock().unwrap().push(f.tick.index);
            f.tick.index
        },
        |_f: &CompositedFrame| None::<()>,
    ))
    .unwrap();
    assert_contiguous_once(
        &old_seen.into_inner().unwrap(),
        &new_seen.into_inner().unwrap(),
        cut,
    );
}

#[test]
fn a_successor_starts_when_its_predecessor_stops_before_choosing_a_cut() {
    // OLD's handoff is requested but OLD stops (or fails) before it reaches a
    // frame boundary: the cut is published at exit, so NEW never waits forever.
    let cadence = Rational::FPS_25;
    let time = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time.clone();
    let mut old = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        Arc::clone(&ts),
        JumpPacer(Arc::clone(&time)),
    );
    let seed = old.seed_nanos();
    let handoff = old.handoff();
    let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
    let stop = StopSignal::new();
    let (raise, request) = (stop.clone(), Arc::clone(&handoff));
    let outcome = clock_thread::block_on(old.run_for_with_tick_control(
        &publisher,
        &stop,
        10_000,
        |f: &CompositedFrame| f.tick.index,
        |_f: &CompositedFrame| None::<()>,
        move |_d: &mut CompositorDrive<Nv12Image>, tick: Tick| {
            if tick.index == 9 {
                request.request();
                raise.stop();
            }
        },
    ))
    .unwrap();
    assert_eq!((outcome.ticks, outcome.stop), (10, RunStop::Stopped));
    assert_eq!(
        handoff.cut(),
        Some(10),
        "the exit publishes the first unemitted tick"
    );
    let mut new = EngineRuntime::with_seed(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        ts,
        JumpPacer(Arc::clone(&time)),
        seed,
    );
    new.succeed(handoff);
    let first = Mutex::new(None);
    clock_thread::block_on(new.run_for(
        &publisher,
        &StopSignal::new(),
        1,
        |f: &CompositedFrame| {
            first.lock().unwrap().get_or_insert(f.tick.index);
            f.tick.index
        },
        |_f: &CompositedFrame| None::<()>,
    ))
    .unwrap();
    assert_eq!(*first.lock().unwrap(), Some(10));
}

#[test]
fn a_retracted_request_leaves_the_predecessor_running() {
    // Request then retract before OLD reaches a boundary: OLD keeps emitting
    // and never hands off; a retract after the cut reports the cut.
    let handoff = multiview_engine::Handoff::default();
    handoff.request();
    assert_eq!(handoff.retract(), Ok(()));
    assert_eq!(handoff.cut(), None);
    let cadence = Rational::FPS_25;
    let time = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time.clone();
    let mut old = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        ts,
        JumpPacer(Arc::clone(&time)),
    );
    let cell = old.handoff();
    cell.request();
    assert_eq!(cell.retract(), Ok(()));
    let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
    let outcome = clock_thread::block_on(old.run_for(
        &publisher,
        &StopSignal::new(),
        50,
        |f: &CompositedFrame| f.tick.index,
        |_f: &CompositedFrame| None::<()>,
    ))
    .unwrap();
    assert_eq!((outcome.ticks, outcome.stop), (50, RunStop::Completed));
    assert_eq!(cell.cut(), None, "no cut after a retract, not even at exit");
    cell.request();
    clock_thread::block_on(old.run_for(
        &publisher,
        &StopSignal::new(),
        5,
        |f: &CompositedFrame| f.tick.index,
        |_f: &CompositedFrame| None::<()>,
    ))
    .unwrap();
    let cut = cell.cut().unwrap();
    assert_eq!(cell.retract(), Err(cut), "a chosen cut cannot be withdrawn");
}

#[test]
fn a_rolled_back_successor_leaves_a_slow_predecessor_alive_and_ticking() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // OLD at 1/4 fps: after tick 0 it sleeps 4 s, longer than the 2 s
        // handoff grace, so start_successor times out and rolls back. OLD
        // must then keep running and emit tick 1, never hand off.
        let cadence = Rational::new(1, 4);
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut set: ProgramSet<RealtimePacer> = ProgramSet::new(Arc::clone(&time));
        let old = Program::multiview(
            &spec("main", cadence),
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            Arc::clone(&time),
            RealtimePacer,
        )
        .unwrap();
        let old_ticks = old.ticks_counter();
        set.start(old).unwrap();
        let started = Instant::now();
        while old_ticks.load(Ordering::Acquire) < 1 {
            assert!(started.elapsed() < Duration::from_secs(30));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let new = Program::multiview_succeeding(
            &spec("main.migrating", cadence),
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            Arc::clone(&time),
            RealtimePacer,
            set.seed_nanos("main").unwrap(),
            |_| {},
        )
        .unwrap();
        let rolled_back = set.start_successor("main", new).await;
        assert!(
            rolled_back.is_err(),
            "OLD was asleep past the grace: rollback"
        );
        assert!(!set.is_running("main.migrating"));
        while old_ticks.load(Ordering::Acquire) < 2 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "OLD kept ticking after the rollback"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(set.is_running("main"));
        set.shutdown().await;
    });
}

/// An actor that fails `fail_first` times, then completes; counts its runs.
struct Flaky {
    runs: Arc<AtomicU64>,
    fail_first: u64,
}

impl multiview_engine::Actor for Flaky {
    fn name(&self) -> &'static str {
        "flaky"
    }
    async fn run(&mut self) -> multiview_engine::ActorExit {
        let n = self.runs.fetch_add(1, Ordering::AcqRel);
        // A real actor awaits its work; yield once so this one does too.
        tokio::task::yield_now().await;
        if n < self.fail_first {
            multiview_engine::ActorExit::Failed
        } else {
            multiview_engine::ActorExit::Completed
        }
    }
}

#[test]
fn a_failing_program_restarts_after_its_backoff_on_a_clock_thread_and_stop_cuts_a_backoff_short() {
    use multiview_engine::{RestartPolicy, StopReason, Supervisor};
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    // (a) Two failures, 20 ms base backoff: the runtime timer wakes the clock
    //     thread, so it restarts and completes.
    let runs = Arc::new(AtomicU64::new(0));
    let actor = Flaky {
        runs: Arc::clone(&runs),
        fail_first: 2,
    };
    let policy = RestartPolicy::new(Duration::from_millis(20), Duration::from_millis(40), 10);
    let started = Instant::now();
    let handle = rt.block_on(async move {
        clock_thread::spawn("clock-flaky", move || {
            clock_thread::block_on(Supervisor::new(policy).supervise_until(actor, None))
        })
        .unwrap()
    });
    let outcome = rt.block_on(handle.join()).unwrap();
    assert_eq!(
        (outcome.restarts, outcome.reason),
        (2, StopReason::Completed)
    );
    assert_eq!(runs.load(Ordering::Acquire), 3);
    assert!(started.elapsed() < Duration::from_secs(10));
    // (b) Always failing with a 30 s backoff: a raised stop ends supervision
    //     within a few poll steps, not after the backoff.
    let actor = Flaky {
        runs: Arc::new(AtomicU64::new(0)),
        fail_first: u64::MAX,
    };
    let policy = RestartPolicy::new(Duration::from_secs(30), Duration::from_secs(30), 10);
    let stop = StopSignal::new();
    let inner = stop.clone();
    let handle = rt.block_on(async move {
        clock_thread::spawn("clock-failing", move || {
            clock_thread::block_on(Supervisor::new(policy).supervise_until(actor, Some(&inner)))
        })
        .unwrap()
    });
    std::thread::sleep(Duration::from_millis(100));
    let stopping = Instant::now();
    stop.stop();
    let outcome = rt.block_on(handle.join()).unwrap();
    assert!(
        stopping.elapsed() < Duration::from_secs(2),
        "{:?}",
        stopping.elapsed()
    );
    assert_eq!(outcome.restarts, 1);
}

#[test]
fn migration_handoff_on_real_clock_threads_misses_and_doubles_nothing() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let cadence = Rational::FPS_60;
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut set: ProgramSet<RealtimePacer> = ProgramSet::new(Arc::clone(&time));
        let old_seen = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&old_seen);
        let old = Program::multiview_with_egress(
            &spec("main", cadence),
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            Arc::clone(&time),
            RealtimePacer,
            move |i| rec.lock().unwrap().push(i),
        )
        .unwrap();
        let old_ticks = old.ticks_counter();
        set.start(old).unwrap();
        let started = Instant::now();
        while old_ticks.load(Ordering::Acquire) < 10 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "OLD never ticked"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let seed = set.seed_nanos("main").unwrap();
        let new_seen = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&new_seen);
        let new = Program::multiview_succeeding(
            &spec("main.migrating", cadence),
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            Arc::clone(&time),
            RealtimePacer,
            seed,
            move |i| rec.lock().unwrap().push(i),
        )
        .unwrap();
        let new_ticks = new.ticks_counter();
        let cut = set.start_successor("main", new).await.unwrap();
        while new_ticks.load(Ordering::Acquire) < 20 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "NEW never ticked"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // OLD stopped by itself exactly below the cut: its count is the cut.
        assert!(set.stop("main").await);
        assert_eq!(old_ticks.load(Ordering::Acquire), cut, "OLD emitted 0..cut");
        let new_drops = set.egress_dropped("main.migrating");
        set.shutdown().await;
        // Egress is drop-on-full (cap 4); compare indices when nothing was shed.
        let old_seen = old_seen.lock().unwrap().clone();
        let new_seen = new_seen.lock().unwrap().clone();
        assert!(
            new_seen.iter().all(|i| *i >= cut),
            "NEW never emitted before the cut"
        );
        assert!(
            old_seen.iter().all(|i| *i < cut),
            "OLD never emitted the cut"
        );
        // NEW's first emitted index is the cut (unless its egress shed it).
        if new_drops == Some(0) {
            assert_eq!(
                new_seen.first().copied(),
                Some(cut),
                "NEW starts at the cut"
            );
        }
    });
}

#[test]
fn stop_while_the_clock_thread_sleeps_is_prompt_and_freezes_the_clock() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // 1 fps: after tick 0 the clock thread sleeps ~1 s for tick 1.
        let cadence = Rational::new(1, 1);
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut set: ProgramSet<RealtimePacer> = ProgramSet::new(Arc::clone(&time));
        let p = Program::multiview(
            &spec("slow", cadence),
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            Arc::clone(&time),
            RealtimePacer,
        )
        .unwrap();
        let ticks = p.ticks_counter();
        set.start(p).unwrap();
        let started = Instant::now();
        while ticks.load(Ordering::Acquire) < 1 {
            assert!(started.elapsed() < Duration::from_secs(30));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stopping = Instant::now();
        assert!(set.stop("slow").await);
        let took = stopping.elapsed();
        assert!(
            took < Duration::from_millis(900),
            "stop is observed mid-sleep (STOP_POLL), not at the 1 s deadline: {took:?}"
        );
        let frozen = ticks.load(Ordering::Acquire);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            ticks.load(Ordering::Acquire),
            frozen,
            "a stopped clock stays stopped"
        );
    });
}

#[test]
fn a_panicking_egress_consumer_never_stops_the_clock() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let cadence = Rational::FPS_60;
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut set: ProgramSet<RealtimePacer> = ProgramSet::new(Arc::clone(&time));
        let p = Program::multiview_with_egress(
            &spec("main", cadence),
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            Arc::clone(&time),
            RealtimePacer,
            |tick| {
                assert!(tick < 3, "consumer bug at tick {tick}");
            },
        )
        .unwrap();
        let ticks = p.ticks_counter();
        set.start(p).unwrap();
        let started = Instant::now();
        while ticks.load(Ordering::Acquire) < 60 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the clock kept ticking after its consumer panicked"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(set.is_running("main"));
        assert!(
            set.egress_dropped("main").unwrap() > 0,
            "the dead consumer's ticks are shed"
        );
        set.shutdown().await;
    });
}

#[test]
fn run_on_clock_thread_drives_a_borrowing_loop_on_a_named_thread() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let cadence = Rational::FPS_60;
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut runtime = EngineRuntime::new(
            OutputClock::new(cadence).unwrap(),
            drive(cadence),
            time,
            RealtimePacer,
        );
        let publisher: EnginePublisher<u64, ()> = EnginePublisher::new(8);
        let names = Mutex::new(Vec::new());
        let outcome = clock_thread::run_on_clock_thread(
            "clock-borrow",
            runtime.run_for(
                &publisher,
                &StopSignal::new(),
                30,
                |f: &CompositedFrame| {
                    names
                        .lock()
                        .unwrap()
                        .push(std::thread::current().name().map(str::to_owned));
                    f.tick.index
                },
                |_f: &CompositedFrame| None::<()>,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outcome.ticks, 30);
        let names = names.into_inner().unwrap();
        assert!(names.iter().all(|n| n.as_deref() == Some("clock-borrow")));
        assert_eq!(runtime.lateness().snapshot().count, 30);
    });
}
