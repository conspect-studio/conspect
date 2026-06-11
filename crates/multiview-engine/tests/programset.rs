//! MP-1 acceptance: the `ProgramSet` supervisor + N independent clocks
//! (ADR-0030 §2). These are the work-schedule's required MP-1 tests.
//!
//! The bar (work-schedule MP-1):
//!
//! * **(a) Concurrent independent cadences.** Two `Multiview` programs at 25 and
//!   60 fps run **concurrently**, each on its **own** [`OutputClock`] reading the
//!   **same** shared `Arc<dyn TimeSource>`. Over the same wall interval the 60 fps
//!   program's `ticks_emitted` advances ~2.4× the 25 fps program's — proving the
//!   clocks are independent (one program's cadence never gates another's).
//! * **(b) Stop one, the other keeps ticking.** Stopping ONE program leaves the
//!   OTHER advancing on its cadence (independent supervised tasks + stop signals).
//! * **(chaos / inv #10 + #1 per program) Wedge A's egress, B keeps ticking.** A
//!   stuck/slow per-program egress consumer on program A must NOT stall program A's
//!   own clock (drop-oldest, inv #1) and must NOT stall program B's clock
//!   (cross-program isolation, inv #10): B's `ticks_emitted` keeps advancing on
//!   cadence while A's egress is wedged.
//!
//! Pacing uses the shared [`ManualTimeSource`]: the test advances ONE shared
//! monotonic reference and each program's independent clock samples it at its own
//! cadence — exactly the "shared time *source*, independent output *clocks*"
//! design (ADR-0030 §2.1). No real sleeps on the deterministic-cadence assertions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::ProgramSpec;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas as CoreCanvas, Layout as CoreLayout};
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::{
    CompositorDrive, OutputClock, Program, ProgramSet, RealtimePacer, TimeSource,
};

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap()
}

/// A bare, cell-free 16x16 drive at `cadence`.
fn empty_drive(cadence: Rational) -> CompositorDrive<Nv12Image> {
    let layout = CoreLayout {
        name: "mp1".to_owned(),
        canvas: CoreCanvas {
            width: 16,
            height: 16,
            fps_num: cadence.num,
            fps_den: cadence.den,
        },
        cells: Vec::new(),
    };
    CompositorDrive::new(
        Arc::new(layout),
        HashMap::new(),
        nosignal_card(16, 16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

/// A `"main"`-style multiview spec at `num/den` fps with a given id.
fn spec_with_id(id: &str, num: i64, den: i64) -> ProgramSpec {
    let json = format!(
        r##"{{
            "id": "{id}",
            "kind": "multiview",
            "canvas": {{
                "width": 16,
                "height": 16,
                "fps": "{num}/{den}",
                "pixel_format": "nv12",
                "background": "#000000",
                "color": {{ "profile": "sdr-bt709-limited" }}
            }},
            "layout": {{ "kind": "preset", "preset": "1x1" }}
        }}"##
    );
    serde_json::from_str(&json).expect("multiview spec deserializes")
}

/// Build a multiview [`Program`] from a spec at the spec's cadence over a shared
/// time source, using the **real-time** pacer (production pacing).
///
/// MP-1's "two concurrent independent cadences" is, by its nature, a test of two
/// real clocks racing in wall-clock time — each program paces to its own deadline
/// off the shared [`MonotonicTimeSource`]. (The cooperative test pacer is a
/// lock-step tool driven tick-by-tick from the test thread, not a free-running
/// drainer for a spawned task, so it is the wrong tool for the cadence-ratio bar;
/// real-time pacing is the honest one and matches the work-schedule acceptance.)
fn program_at(
    spec: &ProgramSpec,
    cadence: Rational,
    time: Arc<dyn TimeSource>,
) -> Program<RealtimePacer> {
    let clock = OutputClock::new(cadence).unwrap();
    let drive = empty_drive(cadence);
    Program::multiview(spec, clock, drive, time, RealtimePacer).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_programs_run_concurrently_each_on_its_own_cadence() {
    // (a) Two Multiview programs at 25 and 60 fps run CONCURRENTLY, each on its OWN
    // independent clock, both reading the ONE shared monotonic time source. Over
    // the SAME real wall interval the 60 fps program's ticks_emitted advances ~2.4x
    // (= 60/25) the 25 fps program's — proving the clocks are independent (one
    // program's cadence never gates another's).
    let time: Arc<dyn TimeSource> = Arc::new(multiview_engine::MonotonicTimeSource::new());

    let spec_slow = spec_with_id("slow", 25, 1);
    let spec_fast = spec_with_id("fast", 60, 1);
    let slow = program_at(&spec_slow, Rational::FPS_25, time.clone());
    let fast = program_at(&spec_fast, Rational::FPS_60, time.clone());

    let mut set = ProgramSet::new(time.clone());
    set.start(slow).unwrap();
    set.start(fast).unwrap();
    assert!(set.is_running("slow"));
    assert!(set.is_running("fast"));

    // Sample each program's ticks over the SAME real wall interval. Take the
    // measurement window between two readings so seed/startup skew cancels out.
    // ~1 s gives ~25 vs ~60 ticks — comfortably above the noise floor.
    let slow0 = set.ticks_emitted("slow").unwrap();
    let fast0 = set.ticks_emitted("fast").unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let slow_d = set.ticks_emitted("slow").unwrap() - slow0;
    let fast_d = set.ticks_emitted("fast").unwrap() - fast0;

    // Both clocks advanced (neither stalled), and each paced to ITS cadence over
    // the interval (allowing generous slack for debug-build + shared-CI jitter).
    assert!(
        slow_d >= 15,
        "25fps program must keep ticking on its own clock (advanced {slow_d} in ~1s)"
    );
    assert!(
        fast_d >= 40,
        "60fps program must keep ticking on its own clock (advanced {fast_d} in ~1s)"
    );

    // The 60 fps program emitted ~2.4x the 25 fps program's ticks over the SAME
    // interval — the cadences are independent. Compared via exact integer cross-
    // multiplication (no float cast): ratio = fast_d / slow_d must lie in
    // [1.8, 3.0], i.e. 18*slow_d <= 10*fast_d AND 10*fast_d <= 30*slow_d, around the
    // exact 60/25 = 2.4. Generous bounds absorb scheduler jitter on a debug/CI build.
    assert!(
        10 * fast_d >= 18 * slow_d && 10 * fast_d <= 30 * slow_d,
        "60fps program must emit ~2.4x the 25fps program's ticks over the same wall interval (fast_d={fast_d}, slow_d={slow_d})"
    );

    set.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_one_program_leaves_the_other_ticking() {
    // (b) Stop ONE program; the OTHER keeps advancing on its cadence.
    let time: Arc<dyn TimeSource> = Arc::new(multiview_engine::MonotonicTimeSource::new());

    let spec_a = spec_with_id("a", 30, 1);
    let spec_b = spec_with_id("b", 30, 1);
    let a = program_at(&spec_a, Rational::FPS_30, time.clone());
    let b = program_at(&spec_b, Rational::FPS_30, time.clone());

    // Retain A's wait-free ticks counter BEFORE moving A into the set, so we can
    // prove A's clock is FROZEN after it is stopped (the supervisor removes a
    // stopped program from its map, but the counter Arc outlives it).
    let a_ticks = a.ticks_counter();
    let a_count = || a_ticks.load(Ordering::Acquire);

    let mut set = ProgramSet::new(time.clone());
    set.start(a).unwrap();
    set.start(b).unwrap();

    // Let both run for a moment so both are demonstrably ticking.
    let started = Instant::now();
    loop {
        if a_count() >= 5 && set.ticks_emitted("b").unwrap() >= 5 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "programs stalled"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Stop program A and join it (its loop returns after its current tick). B is
    // untouched and stays in the running set.
    set.stop("a").await;
    assert!(!set.is_running("a"));
    assert!(set.is_running("b"));
    // A's final tick count, sampled AFTER the stop+join completed — A's clock can
    // never tick past this.
    let a_final = a_count();

    // Run another interval; B must keep advancing, A must be frozen (stopped+joined
    // → its clock never ticks again).
    let b_before = set.ticks_emitted("b").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    loop {
        if set.ticks_emitted("b").unwrap() >= b_before + 10 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "program B stalled after A was stopped — A must not gate B"
        );
        tokio::task::yield_now().await;
    }

    // A's tick count never moved after the stop+join (its clock is frozen).
    assert_eq!(
        a_count(),
        a_final,
        "a stopped program must not emit further ticks"
    );
    assert!(
        set.ticks_emitted("b").unwrap() >= b_before + 10,
        "B kept ticking on its cadence while A was stopped"
    );

    set.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wedging_one_programs_egress_does_not_stall_another() {
    // CHAOS GATE (inv #10 + #1 per program): wedge program A's egress (a stuck
    // sink that never drains), and assert (1) A's OWN clock keeps ticking
    // (drop-oldest, inv #1 — the loop never blocks on its egress) and (2) program
    // B's clock keeps advancing on cadence (cross-program isolation, inv #10).
    //
    // Uses the REALTIME pacer + real time (not the manual source): the wedge is a
    // genuine blocked OS-thread-style consumer, and we measure that B advances in
    // real wall-clock time while A's egress is stuck.
    let time: Arc<dyn TimeSource> = Arc::new(multiview_engine::MonotonicTimeSource::new());

    let spec_a = spec_with_id("wedged", 60, 1);
    let spec_b = spec_with_id("healthy", 60, 1);

    // Program A's egress is WEDGED: the per-tick egress consumer blocks forever on
    // this gate. Program A's drop-oldest queue fills; its CLOCK must keep ticking.
    let wedge = Arc::new(AtomicBool::new(true));
    let wedge_for_a = Arc::clone(&wedge);
    let a_egress_calls = Arc::new(AtomicU64::new(0));
    let a_calls = Arc::clone(&a_egress_calls);

    let clock_a = OutputClock::new(Rational::FPS_60).unwrap();
    let a = Program::multiview_with_egress(
        &spec_a,
        clock_a,
        empty_drive(Rational::FPS_60),
        time.clone(),
        RealtimePacer,
        move |_tick: u64| {
            a_calls.fetch_add(1, Ordering::Release);
            // Wedge: spin-block as long as the gate is held (a stuck/slow sink).
            while wedge_for_a.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        },
    )
    .unwrap();

    let clock_b = OutputClock::new(Rational::FPS_60).unwrap();
    let b = Program::multiview(
        &spec_b,
        clock_b,
        empty_drive(Rational::FPS_60),
        time.clone(),
        RealtimePacer,
    )
    .unwrap();

    let mut set = ProgramSet::new(time.clone());
    set.start(a).unwrap();
    set.start(b).unwrap();

    // Let real time pass with A's egress WEDGED. B must keep ticking on cadence.
    let a_before = set.ticks_emitted("wedged").unwrap();
    let b_before = set.ticks_emitted("healthy").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let a_after = set.ticks_emitted("wedged").unwrap();
    let b_after = set.ticks_emitted("healthy").unwrap();

    // (1) A's OWN clock kept ticking despite the wedged egress (its loop never
    // blocked on its egress — drop-oldest, inv #1). At 60 fps, ~500 ms ≈ 30 ticks;
    // require a healthy fraction to be robust to scheduler jitter.
    assert!(
        a_after - a_before >= 10,
        "wedged program A's OWN clock must keep ticking (drop-oldest egress, inv #1): {a_before} -> {a_after}"
    );
    // (2) B's clock kept advancing on cadence while A's egress was wedged
    // (cross-program isolation, inv #10).
    assert!(
        b_after - b_before >= 10,
        "healthy program B must keep ticking while A's egress is wedged (inv #10): {b_before} -> {b_after}"
    );
    // A's egress was actually entered (it really is wedged, not skipped).
    assert!(
        a_egress_calls.load(Ordering::Acquire) >= 1,
        "A's egress consumer must have been reached (the wedge is real)"
    );

    // Release the wedge so teardown can join cleanly, then shut the set down.
    wedge.store(false, Ordering::Release);
    set.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_a_duplicate_program_id_is_rejected() {
    let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
    let spec = spec_with_id("dup", 25, 1);
    let p1 = program_at(&spec, Rational::FPS_25, time.clone());
    let p2 = program_at(&spec, Rational::FPS_25, time.clone());

    let mut set = ProgramSet::new(time.clone());
    set.start(p1).unwrap();
    // A second program with the same id is rejected (unique ids per set).
    assert!(set.start(p2).is_err());
    assert_eq!(set.running_ids().len(), 1);

    set.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_is_bounded_even_when_a_program_cannot_observe_stop() {
    // REGRESSION (PR #95 hang): teardown must never block forever on a program
    // whose run loop cannot reach the top-of-loop stop check.
    //
    // We construct exactly the wedge that hung the duplicate-id test on a loaded
    // runner: a `RealtimePacer` over a `ManualTimeSource` that is NEVER advanced.
    // Once the program's clock has paced tick 0 (deadline 0, due immediately) it
    // parks inside the pacer's real `tokio::time::sleep` waiting for tick 1's
    // deadline — a wall-clock instant the frozen manual source never reaches — so
    // the loop never returns to its top-of-loop `stop` check. `stop()`/`shutdown()`
    // raise the signal, but the wedged loop can never see it.
    //
    // The engine's teardown posture (`SINK_WEDGE_GRACE` / ENG-1) is "bounded join,
    // then detach a wedged peer — never a join that cannot return". This asserts
    // `ProgramSet::shutdown` honours that: it must complete in a bounded time, not
    // hang. A bounded `tokio::time::timeout` around `shutdown` is the loud-failure
    // tripwire (the bug manifested as a >35-minute hang to the CI job timeout).
    //
    // To make the wedge deterministic (not a start-vs-stop race) we first let the
    // program demonstrably pace at least one tick, so it is parked in the pacer's
    // real sleep BEFORE we tear down.
    let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
    let spec = spec_with_id("wedged-clock", 25, 1);
    let p = program_at(&spec, Rational::FPS_25, time.clone());
    let ticks = p.ticks_counter();

    let mut set = ProgramSet::new(time.clone());
    set.start(p).unwrap();

    // Wait until the program has emitted tick 0 (deadline 0) and is therefore
    // parked in the pacer waiting for tick 1's deadline — i.e. it is wedged and
    // can no longer observe a stop. Bounded so the test itself never hangs.
    let started = Instant::now();
    while ticks.load(Ordering::Acquire) < 1 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "program never paced its first tick"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The fix: shutdown must be bounded. Give it a generous multiple of the
    // teardown grace; if it has not returned by then it is the regression.
    tokio::time::timeout(Duration::from_secs(10), set.shutdown())
        .await
        .expect("ProgramSet::shutdown must be bounded, not block forever on a program that cannot observe stop");

    assert!(set.is_empty(), "shutdown must drain the set");
}
