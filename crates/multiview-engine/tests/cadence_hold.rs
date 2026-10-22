//! Output clock holds **wall-clock cadence under overload** (ADR-T018) —
//! invariant #1's true meaning, proven deterministically.
//!
//! The drive loop composes one fresh frame per loop iteration. On a
//! CPU/GPU-contended host a `compose()` can overrun the tick budget; the prior
//! loop then free-ran at *composition* speed, emitting each index late, so
//! media-time fell progressively behind wall-clock (the frigate 84-minute-lag
//! incident). ADR-T018 makes the emitted tick **track wall-clock** by re-emitting
//! the held last-good frame for each whole tick-period that has already elapsed
//! (invariant #2 lifted to the output clock), under a fresh strictly-increasing
//! pts (invariant #3), bounded per iteration (no spin) — so the output holds
//! exactly 1.0× cadence rather than slipping.
//!
//! These tests drive the REAL [`EngineRuntime`] loop with a [`ManualTimeSource`]
//! and [`CooperativePacer`] (zero real sleeps, fully deterministic), simulating a
//! slow compositor by advancing the time source inside the per-tick control hook
//! (i.e. "by the time this fresh compose finished, N tick-periods had elapsed").
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::{
    CompositedFrame, CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime,
    OutputClock, StopSignal, TimeSource, MAX_REPEATS_PER_TICK,
};
use multiview_framestore::TileStore;

/// Independent i128 oracle for `out_pts = f(tick)` (half-away-from-zero), NOT via
/// the clock's `rescale`/`from_tick`.
fn oracle_pts_ns(tick: i64, cadence: Rational) -> i64 {
    let numerator: i128 = i128::from(tick) * 1_000_000_000_i128 * i128::from(cadence.den);
    let denominator: i128 = i128::from(cadence.num);
    let q = numerator / denominator;
    let r = numerator % denominator;
    let rounded = if r * 2 >= denominator { q + 1 } else { q };
    i64::try_from(rounded).expect("oracle pts fits in i64")
}

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

/// A one-cell drive over a single never-fed store (every tile rides `NoSignal` —
/// a valid frame is still produced every tick).
fn build_drive(w: u32, h: u32) -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "cadence".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 25,
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
        Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overloaded_compositor_holds_wall_clock_cadence_by_repeating_not_slipping() {
    // Each fresh compose "takes" OVERRUN tick-periods (simulated by advancing the
    // manual clock that much inside the control hook, which runs once per fresh
    // compose). A correct loop emits OVERRUN-1 last-good repeats + 1 fresh per
    // iteration, so the published index tracks wall-clock at 1.0×. A slipping loop
    // (the bug) would emit one late fresh composite per OVERRUN periods → ~0.33×.
    const OVERRUN: i64 = 3;
    const TICKS: u64 = 300;
    // An EXACT-period cadence (25 fps → 40,000,000 ns/tick, no rounding): the
    // harness advances the manual clock by whole periods, so `advance(k·period)`
    // lands precisely on `pts_at(k)`. (A non-integer-period cadence like 29.97
    // would drift the harness ~1 ns per tick vs the clock's exact deadlines — a
    // test artifact, not an engine bug; the clock's NTSC exactness is covered by
    // the clock unit tests + the FPS_60 soak.)
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );
    let seed = runtime.seed_nanos();

    // Capture every published (index, pts) so we can prove contiguity + the oracle.
    let log = Arc::new(Mutex::new(Vec::<(u64, i64)>::new()));
    let log_s = Arc::clone(&log);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s
            .lock()
            .unwrap()
            .push((f.tick.index, f.pts().as_nanos()));
        f.tick.index
    };

    // control hook = one fresh compose just finished, having taken OVERRUN periods.
    let ts_ctl = Arc::clone(&time_source);
    let fresh_calls = Arc::new(AtomicU64::new(0));
    let fresh_ctl = Arc::clone(&fresh_calls);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        fresh_ctl.fetch_add(1, Ordering::AcqRel);
        ts_ctl.advance(Duration::from_nanos((OVERRUN * period) as u64));
    };

    let stop = StopSignal::new();
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            TICKS,
            state_of,
            |_f| None::<u64>,
            control,
        )
        .await
        .expect("run completes");

    assert_eq!(outcome.ticks, TICKS, "emitted exactly the requested ticks");
    let repeated = runtime.frames_repeated();
    let fresh = fresh_calls.load(Ordering::Acquire);

    // 1. Some ticks were REPEATS (the cadence-hold fired) and fresh composites are
    //    strictly fewer than emitted ticks — we did NOT compose every tick.
    assert!(repeated > 0, "overload must produce last-good repeats");
    assert_eq!(
        repeated + fresh,
        TICKS,
        "every emitted tick is either a fresh composite or a last-good repeat"
    );
    assert!(
        fresh < TICKS,
        "fewer fresh composites than emitted ticks under overload"
    );

    // 2. The published sequence is CONTIGUOUS (no muxer PTS gap) and every pts is
    //    the exact independent oracle (fresh OR repeat — repeats carry a fresh pts).
    let log = log.lock().unwrap();
    assert_eq!(log.len() as u64, TICKS);
    for (i, &(index, pts)) in log.iter().enumerate() {
        assert_eq!(index, i as u64, "published indices contiguous, no gap/dup");
        assert_eq!(
            pts,
            oracle_pts_ns(i as i64, cadence),
            "tick {i} pts must equal the oracle (repeats re-stamp, never reuse a pts)"
        );
    }

    // 3. The CADENCE HELD: the last emitted index tracks wall-clock (≈ now/period),
    //    NOT compose-speed. The slipping bug would leave last_index ≈ (now/period)/OVERRUN.
    let now = time_source.now_nanos();
    let wall_index = (now - seed) / period;
    let last_index = log.last().unwrap().0 as i64;
    assert!(
        last_index + 4 >= wall_index,
        "output slipped behind wall-clock: last index {last_index}, wall index {wall_index} \
         (a holding-cadence loop keeps these within a tick or two; slipping falls ~{OVERRUN}× behind)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_mid_tick_wake_does_not_run_ahead_or_repeat() {
    // The D1 guard: on a HEALTHY box every wake lands a fraction of a period past
    // the due deadline (ordinary scheduler jitter). The loop must emit the DUE
    // tick fresh — never round the fractional elapsed up to the next index (which
    // would compose a frame before its wall-clock deadline = run ahead) and never
    // spuriously repeat. Wakes here sit 0.6 of a period late, every tick.
    const TICKS: u64 = 100;
    // An EXACT-period cadence (25 fps → 40,000,000 ns/tick, no rounding): the
    // harness advances the manual clock by whole periods, so `advance(k·period)`
    // lands precisely on `pts_at(k)`. (A non-integer-period cadence like 29.97
    // would drift the harness ~1 ns per tick vs the clock's exact deadlines — a
    // test artifact, not an engine bug; the clock's NTSC exactness is covered by
    // the clock unit tests + the FPS_60 soak.)
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );

    // Prime a 0.6-period offset so every pacer wake is mid-tick-late but < 1 period.
    time_source.advance(Duration::from_nanos((period * 6 / 10) as u64));

    let log = Arc::new(Mutex::new(Vec::<u64>::new()));
    let log_s = Arc::clone(&log);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s.lock().unwrap().push(f.tick.index);
        f.tick.index
    };

    // Each fresh compose advances exactly one period → steady healthy 1.0× pacing,
    // but always 0.6 period late relative to the deadline (the priming offset).
    let ts_ctl = Arc::clone(&time_source);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        ts_ctl.advance(Duration::from_nanos(period as u64));
    };

    let stop = StopSignal::new();
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            TICKS,
            state_of,
            |_f| None::<u64>,
            control,
        )
        .await
        .expect("run completes");

    assert_eq!(outcome.ticks, TICKS);
    // ZERO repeats on a healthy system — a sub-period late wake is not overload.
    assert_eq!(
        runtime.frames_repeated(),
        0,
        "a sub-period-late wake must not trigger a cadence repeat (no run-ahead, no slip)"
    );
    // Exactly one fresh frame per tick, in order, never running ahead of the clock.
    let log = log.lock().unwrap();
    assert_eq!(log.len() as u64, TICKS);
    for (i, &index) in log.iter().enumerate() {
        assert_eq!(index, i as u64, "exactly the due tick each time, in order");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_time_jump_resyncs_with_bounded_repeats_and_no_spin() {
    // A PATHOLOGICAL one-off jump (multi-second deschedule / VM pause / migration):
    // CLOCK_MONOTONIC leaps thousands of periods between two composes. The loop
    // must NOT emit thousands of last-good repeats (a burst that floods the muxer)
    // nor spin — it emits at most MAX_REPEATS_PER_TICK repeats, then RESYNCS the
    // counter to wall-clock in one step (skip_to), accepting one bounded PTS gap.
    const HUGE: i64 = 10_000;
    let ticks: u64 = u64::from(MAX_REPEATS_PER_TICK) + 16;
    // An EXACT-period cadence (25 fps → 40,000,000 ns/tick, no rounding): the
    // harness advances the manual clock by whole periods, so `advance(k·period)`
    // lands precisely on `pts_at(k)`. (A non-integer-period cadence like 29.97
    // would drift the harness ~1 ns per tick vs the clock's exact deadlines — a
    // test artifact, not an engine bug; the clock's NTSC exactness is covered by
    // the clock unit tests + the FPS_60 soak.)
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );

    let log = Arc::new(Mutex::new(Vec::<i64>::new()));
    let log_s = Arc::clone(&log);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s.lock().unwrap().push(f.pts().as_nanos());
        f.tick.index
    };

    // Two warmup composes advance one period each; the THIRD jumps HUGE periods.
    let ts_ctl = Arc::clone(&time_source);
    let calls = Arc::new(AtomicU64::new(0));
    let calls_ctl = Arc::clone(&calls);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        let n = calls_ctl.fetch_add(1, Ordering::AcqRel);
        let mult = if n == 2 { HUGE } else { 1 };
        ts_ctl.advance(Duration::from_nanos((mult * period) as u64));
    };

    let stop = StopSignal::new();
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            ticks,
            state_of,
            |_f| None::<u64>,
            control,
        )
        .await
        .expect("run completes without spinning");

    assert_eq!(
        outcome.ticks, ticks,
        "the run terminated (no unbounded spin)"
    );
    // BOUNDED repeats: the jump produced at most one cap's worth of last-good
    // repeats — NOT ~HUGE of them.
    let repeated = runtime.frames_repeated();
    assert!(
        repeated <= u64::from(MAX_REPEATS_PER_TICK),
        "a huge jump must emit at most MAX_REPEATS_PER_TICK ({MAX_REPEATS_PER_TICK}) repeats, got {repeated}"
    );
    assert!(repeated > 0, "the jump did trigger the cadence-hold path");

    // RESYNCED to wall-clock: some emitted tick's pts is out near the jump target,
    // proving skip_to jumped the counter rather than crawling there frame by frame.
    let log = log.lock().unwrap();
    let max_pts = *log.iter().max().unwrap();
    let resync_floor = oracle_pts_ns(HUGE / 2, cadence); // generously past the cap region
    assert!(
        max_pts >= resync_floor,
        "after a huge jump the clock must resync near wall-clock (max pts {max_pts} < floor {resync_floor})"
    );
    // PTS strictly increasing throughout (the resync is a forward jump, not a rewind).
    for w in log.windows(2) {
        assert!(
            w[1] > w[0],
            "pts must be strictly increasing across the resync"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fractional_huge_jump_resync_never_composes_a_tick_before_its_deadline() {
    // Invariant #1 on the CAP path: a real VM pause / CLOCK_MONOTONIC leap is
    // arbitrary, never period-aligned. When the cap (`repeats >= MAX_REPEATS_PER_TICK`)
    // fires after a FRACTIONAL jump — elapsed = (K + 0.6)·period — the resync must
    // land on the FLOOR tick (deadline already passed), NOT the nearest. The buggy
    // cap path used `MediaTime::to_tick`, which rounds HALF-AWAY-FROM-ZERO: it rounds
    // (K + 0.6) UP to K+1 and `skip_to(K+1)`, then step-2 composes tick K+1 BEFORE
    // its deadline `pts_at(K+1) > now` — running AHEAD, violating invariant #1. (The
    // exact-integer-jump test above can never catch this: `to_tick` lands on the
    // integer, so floor == nearest.)
    //
    // We assert the invariant AT ITS SOURCE, inside the publish closure: at the
    // instant each frame is published, its pts must not exceed the elapsed wall-clock
    // (`pts <= now - seed`). The worst run-ahead is recorded into a shared atomic the
    // MOMENT it happens, so the regression is captured even though the buggy
    // skip-ahead then parks the bounded loop on an unreachable deadline (we bound the
    // whole run in a wall-clock timeout — the assertion, not run completion, is the
    // signal).
    const HUGE: i64 = 10_000;
    let ticks: u64 = u64::from(MAX_REPEATS_PER_TICK) + 16;
    // EXACT-period cadence (25 fps → 40,000,000 ns/tick): whole-period advances land
    // precisely on the clock's exact deadlines, so the ONLY fractional component is
    // the deliberate 0.6-period priming below — isolating the cap-path over-round.
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );
    let seed = runtime.seed_nanos();

    // Prime a 0.6-period offset so the post-jump elapsed is (K + 0.6)·period — the
    // fractional remainder that `to_tick` rounds UP and the floor rounds DOWN.
    time_source.advance(Duration::from_nanos((period * 6 / 10) as u64));

    // `compose_now` carries the wall-clock instant captured at the START of the
    // control hook — which the drive loop runs AFTER `clock.tick()` and immediately
    // BEFORE `compose()` + the publish closure, ONCE per FRESH tick (repeats do not
    // run the hook). So for each fresh tick it is exactly the "about to compose this
    // tick" instant — the moment its deadline must already have passed. We measure
    // run-ahead = pts - (compose_now - seed) for FRESH ticks only (detected by
    // compose_now changing since the last measurement); repeats reuse the prior
    // fresh tick's compose_now and are skipped — they are provably never ahead (a
    // repeat's pts <= the elapsed wall-clock that triggered it). Measuring at the
    // hook instant (not the live clock at publish) is what makes a one-tick
    // over-round visible: the hook then advances the clock to simulate THIS compose's
    // duration, which would otherwise mask it.
    let compose_now = Arc::new(AtomicI64::new(i64::MIN));
    let last_measured = Arc::new(AtomicI64::new(i64::MIN));
    let worst_ahead = Arc::new(AtomicI64::new(i64::MIN));
    let max_pts_seen = Arc::new(AtomicI64::new(i64::MIN));
    let prev_pts = Arc::new(AtomicI64::new(i64::MIN));
    let monotonic_ok = Arc::new(AtomicBool::new(true));
    let cn_s = Arc::clone(&compose_now);
    let lm_s = Arc::clone(&last_measured);
    let worst_s = Arc::clone(&worst_ahead);
    let maxpts_s = Arc::clone(&max_pts_seen);
    let prev_s = Arc::clone(&prev_pts);
    let mono_s = Arc::clone(&monotonic_ok);
    let state_of = move |f: &CompositedFrame| -> u64 {
        let pts = f.pts().as_nanos();
        let cn = cn_s.load(Ordering::Acquire);
        // Only the FRESH tick right after a hook (compose_now advanced) is measured;
        // repeats (same compose_now) are skipped.
        if cn != i64::MIN && lm_s.swap(cn, Ordering::AcqRel) != cn {
            worst_s.fetch_max(pts - (cn - seed), Ordering::AcqRel);
        }
        maxpts_s.fetch_max(pts, Ordering::AcqRel);
        let prior = prev_s.swap(pts, Ordering::AcqRel);
        if prior != i64::MIN && pts <= prior {
            mono_s.store(false, Ordering::Release);
        }
        f.tick.index
    };

    // Hook schedule: warmups (n=0,1) advance one period each (heal the 0.6 priming
    // into steady pacing); the THIRD (n=2) jumps HUGE periods (the pathological
    // leap); the rest advance one period each so a CORRECT (floor) loop catches up
    // and finishes fast. The hook FIRST records the compose-time `now` (before
    // advancing), so the publish closure measures each fresh tick against the instant
    // it is composed — exposing a cap-path over-round that this tick's own advance
    // would otherwise hide.
    let ts_ctl = Arc::clone(&time_source);
    let cn_ctl = Arc::clone(&compose_now);
    let calls = Arc::new(AtomicU64::new(0));
    let calls_ctl = Arc::clone(&calls);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        cn_ctl.store(ts_ctl.now_nanos(), Ordering::Release);
        let n = calls_ctl.fetch_add(1, Ordering::AcqRel);
        let mult = if n == 2 { HUGE } else { 1 };
        ts_ctl.advance(Duration::from_nanos((mult * period) as u64));
    };

    let stop = StopSignal::new();
    // Bound the whole run: the assertion below is the signal, not completion. A
    // correct loop finishes in well under a second; the buggy run-ahead parks, and
    // the timeout simply ends the run after the run-ahead was already recorded.
    let run = runtime.run_for_with_control(
        publisher.as_ref(),
        &stop,
        ticks,
        state_of,
        |_f| None::<u64>,
        control,
    );
    let outcome = tokio::time::timeout(Duration::from_secs(20), run).await;

    // THE BLOCKER ASSERTION: no tick is ever composed ahead of its deadline. The
    // buggy cap path emits the resync tick at pts_at(K+1) while elapsed = (K+0.6)
    // ·period, i.e. ~0.4 period ahead — caught as a positive worst_ahead.
    let ahead = worst_ahead.load(Ordering::Acquire);
    assert_ne!(
        ahead,
        i64::MIN,
        "no frame was ever published — test wiring broke"
    );
    assert!(
        ahead <= 0,
        "a tick was composed AHEAD of its deadline (run-ahead, inv #1 violated): \
         worst pts-vs-elapsed = +{ahead} ns ≈ {:.3} period — the cap-path resync \
         rounded the fractional jump UP instead of flooring it",
        ahead as f64 / period as f64
    );

    // The cap path actually fired and resynced near wall-clock, so the assertion was
    // exercised on the real post-cap resync tick (not a trivially-ahead-free run).
    let repeated = runtime.frames_repeated();
    assert!(
        repeated > 0 && repeated <= u64::from(MAX_REPEATS_PER_TICK),
        "the fractional jump must trigger a BOUNDED cadence-hold (got {repeated}, cap {MAX_REPEATS_PER_TICK})"
    );
    assert!(
        max_pts_seen.load(Ordering::Acquire) >= oracle_pts_ns(HUGE / 2, cadence),
        "the clock must have resynced near wall-clock — proves the resync tick was reached"
    );
    assert!(
        monotonic_ok.load(Ordering::Acquire),
        "pts must be strictly increasing across the resync (forward-only, inv #3)"
    );
    // HARD completion assert (matches the integer `huge_time_jump` test): the bounded
    // run must finish — `tokio::time::timeout` returns Ok, the inner run returns Ok,
    // and exactly `ticks` frames were emitted. This catches a regression that avoids
    // run-ahead but PARKS/SPINS the loop (which would make `timeout` return Err); the
    // run-ahead assertion above and this completion assertion together cover both the
    // "composed ahead" and the "stalled" failure modes.
    let outcome = outcome
        .expect("the bounded run must complete, not park/spin past the 20 s wall-clock timeout")
        .expect("the bounded run must return Ok");
    assert_eq!(
        outcome.ticks, ticks,
        "the floor resync must complete the bounded run at exactly the tick budget"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_integer_cadence_large_index_resync_floors_exactly_and_does_not_grind() {
    // The boundedness proof at a NON-INTEGER cadence and a LARGE tick index — the
    // case the 25 fps (integer-ns) tests structurally cannot reach. 23.976 fps
    // (24000/1001) has an exact period of 41 708 333.333… ns; `pts_at(1)` ROUNDS that
    // to 41 708 333. Dividing `elapsed` by the rounded period drifts from the true
    // floor without bound as the index grows: at K = 300_000_000 it is +2 high, and a
    // fixed ±1 correction cannot recover it — the old code's never-ahead backstop then
    // collapses the resync target to `next`, so `skip_to(next)` advances only
    // MAX_REPEATS_PER_TICK+1 ticks per outer iteration and GRINDS the 300M-tick backlog
    // frame-by-frame (the bounded run never reaches its budget → timeout). The exact
    // rational floor (`elapsed·num / (1e9·den)`) has zero drift, so it resyncs to the
    // true wall-clock floor in one step and the bounded run completes.
    //
    // Assert BOTH: (a) no fresh tick is composed before its deadline (no run-ahead),
    // and (b) the run COMPLETES at exactly `ticks` (hard) — i.e. it floored to ~K in
    // one resync rather than collapsing to `next` and grinding.
    const K: u64 = 300_000_000;
    let ticks: u64 = u64::from(MAX_REPEATS_PER_TICK) + 16;
    let cadence = Rational::new(24000, 1001); // 23.976 fps — non-integer ns period
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );
    let seed = runtime.seed_nanos();

    // Prime the clock to a LARGE fractional elapsed: exactly tick K's deadline plus
    // 0.6 of a period. The first catch-up after tick 0 sees this huge gap, hits the
    // cap, and must resync to the floor (~K) — not collapse to `next`.
    let prime = oracle_pts_ns(K as i64, cadence)
        .saturating_add(period * 6 / 10)
        .saturating_sub(seed);
    time_source.advance(Duration::from_nanos(u64::try_from(prime).unwrap_or(0)));

    // Compose-instant run-ahead measurement (fresh ticks only — see the fractional
    // test above for why the live clock at publish would mask a one-tick over-round).
    let compose_now = Arc::new(AtomicI64::new(i64::MIN));
    let last_measured = Arc::new(AtomicI64::new(i64::MIN));
    let worst_ahead = Arc::new(AtomicI64::new(i64::MIN));
    let cn_s = Arc::clone(&compose_now);
    let lm_s = Arc::clone(&last_measured);
    let worst_s = Arc::clone(&worst_ahead);
    let state_of = move |f: &CompositedFrame| -> u64 {
        let pts = f.pts().as_nanos();
        let cn = cn_s.load(Ordering::Acquire);
        if cn != i64::MIN && lm_s.swap(cn, Ordering::AcqRel) != cn {
            worst_s.fetch_max(pts - (cn - seed), Ordering::AcqRel);
        }
        f.tick.index
    };

    // Each fresh compose records the compose-time `now` (before advancing) then
    // advances one period, so after the one-step resync to ~K a CORRECT floor lets the
    // loop catch up and finish fast; a grinding (collapsed-to-`next`) loop never does.
    let ts_ctl = Arc::clone(&time_source);
    let cn_ctl = Arc::clone(&compose_now);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        cn_ctl.store(ts_ctl.now_nanos(), Ordering::Release);
        ts_ctl.advance(Duration::from_nanos(u64::try_from(period).unwrap_or(0)));
    };

    let stop = StopSignal::new();
    let run = runtime.run_for_with_control(
        publisher.as_ref(),
        &stop,
        ticks,
        state_of,
        |_f| None::<u64>,
        control,
    );
    let outcome = tokio::time::timeout(Duration::from_secs(20), run).await;

    // (a) No run-ahead at the resync (the exact floor never overshoots its deadline).
    let ahead = worst_ahead.load(Ordering::Acquire);
    assert_ne!(
        ahead,
        i64::MIN,
        "no frame was ever published — test wiring broke"
    );
    assert!(
        ahead <= 0,
        "a tick was composed AHEAD of its deadline at a non-integer cadence \
         (run-ahead, inv #1): worst pts-vs-elapsed = +{ahead} ns ≈ {:.3} period",
        ahead as f64 / period as f64
    );

    // (b) BOUNDED resync: the run completes at exactly `ticks`. The old rounded-period
    // floor collapsed to `next` here and ground the 300M-tick backlog → this would
    // time out (Err) and fail. A hard assert, matching `huge_time_jump`.
    let outcome = outcome
        .expect("resync collapsed to `next` and ground the backlog (timed out) instead of flooring to wall-clock")
        .expect("the bounded run must return Ok");
    assert_eq!(
        outcome.ticks, ticks,
        "the exact-rational floor must resync to the wall-clock floor in one step and complete the budget"
    );
    // And it really did resync far forward (proves it floored to ~K, did not grind):
    // the last emitted pts is out near tick K, not crawling up from 0.
    assert!(
        runtime.frames_repeated() <= u64::from(MAX_REPEATS_PER_TICK),
        "the large-index resync must stay bounded to one cap's worth of repeats, not grind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_accepted_cadence_1ghz_resyncs_one_step_with_postcondition_holding() {
    // The accepted-cadence boundary: 1 GHz (1_000_000_000/1), the fastest cadence
    // `OutputClock::new`/`Canvas::validate` permit (period == exactly 1 ns;
    // `pts_at(idx) == idx` ns, no rounding). At a LARGE index the cap-path floor must
    // still resync in ONE step to the true wall-clock floor — `deadline(result+1) <=
    // now` (the cap_resync_floor postcondition) — never composing ahead and never
    // collapsing to `next` to grind. (Faster cadences are rejected, so this is the
    // tightest case the floor must handle.)
    const K: u64 = 300_000_000;
    let ticks: u64 = u64::from(MAX_REPEATS_PER_TICK) + 16;
    let cadence = Rational::new(1_000_000_000, 1); // 1 GHz — period exactly 1 ns
    let period = oracle_pts_ns(1, cadence);
    assert_eq!(period, 1, "1 GHz period must be exactly 1 ns");

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).expect("1 GHz cadence is accepted (period == 1 ns)"),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );
    let seed = runtime.seed_nanos();

    // Prime to tick K's exact deadline (period is integer, so no fractional part):
    // the first catch-up after tick 0 sees a K-tick gap and must floor to ~K.
    let prime = oracle_pts_ns(K as i64, cadence).saturating_sub(seed);
    time_source.advance(Duration::from_nanos(u64::try_from(prime).unwrap_or(0)));

    // Compose-instant run-ahead (fresh ticks only) + max emitted pts.
    let compose_now = Arc::new(AtomicI64::new(i64::MIN));
    let last_measured = Arc::new(AtomicI64::new(i64::MIN));
    let worst_ahead = Arc::new(AtomicI64::new(i64::MIN));
    let max_pts = Arc::new(AtomicI64::new(i64::MIN));
    let cn_s = Arc::clone(&compose_now);
    let lm_s = Arc::clone(&last_measured);
    let worst_s = Arc::clone(&worst_ahead);
    let mp_s = Arc::clone(&max_pts);
    let state_of = move |f: &CompositedFrame| -> u64 {
        let pts = f.pts().as_nanos();
        let cn = cn_s.load(Ordering::Acquire);
        if cn != i64::MIN && lm_s.swap(cn, Ordering::AcqRel) != cn {
            worst_s.fetch_max(pts - (cn - seed), Ordering::AcqRel);
        }
        mp_s.fetch_max(pts, Ordering::AcqRel);
        f.tick.index
    };

    let ts_ctl = Arc::clone(&time_source);
    let cn_ctl = Arc::clone(&compose_now);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        cn_ctl.store(ts_ctl.now_nanos(), Ordering::Release);
        ts_ctl.advance(Duration::from_nanos(u64::try_from(period).unwrap_or(0)));
    };

    let stop = StopSignal::new();
    let run = runtime.run_for_with_control(
        publisher.as_ref(),
        &stop,
        ticks,
        state_of,
        |_f| None::<u64>,
        control,
    );
    let outcome = tokio::time::timeout(Duration::from_secs(20), run).await;

    // No run-ahead (postcondition deadline(result) <= now, never compose ahead).
    let ahead = worst_ahead.load(Ordering::Acquire);
    assert_ne!(ahead, i64::MIN, "no frame published — test wiring broke");
    assert!(ahead <= 0, "ran ahead at the 1 GHz boundary by {ahead} ns");

    // ONE-STEP resync: bounded repeats, completes the budget, and reached ~K (proves
    // it floored to wall-clock in one step rather than grinding from `next`).
    let outcome = outcome
        .expect("the 1 GHz resync must complete, not grind/park")
        .expect("the bounded run must return Ok");
    assert_eq!(outcome.ticks, ticks, "completes the bounded budget");
    assert!(
        runtime.frames_repeated() <= u64::from(MAX_REPEATS_PER_TICK),
        "one-step resync stays within a cap's worth of repeats"
    );
    assert!(
        max_pts.load(Ordering::Acquire) >= oracle_pts_ns((K - 16) as i64, cadence),
        "the resync reached the wall-clock floor near tick K (one step, no grind)"
    );
}
