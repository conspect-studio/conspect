//! Chaos: a **slow per-tick probe** must not starve the control hook or slip the
//! output clock (#143, invariants #1 + #10; design note
//! `docs/architecture/probe-starvation.md`).
//!
//! Found while fixing #137: FaultDetector black/freeze probes running inside the
//! `state_of` projection took longer than a tick period. The ADR-T018 cadence-hold
//! loop called `state_of` for EVERY last-good repeat and re-read wall-clock after
//! each one, so a projection costing more than one period made the catch-up burst
//! chase a moving target: it never caught up, and the control hook (the command
//! drain) plus fresh compose ran only when the `MAX_REPEATS_PER_TICK` resync cap
//! finally fired.
//!
//! These tests drive the REAL [`EngineRuntime`] with a [`ManualTimeSource`] +
//! [`CooperativePacer`] (no real sleeps — deterministic on a loaded host). A
//! probe's cost is simulated by advancing the manual clock inside `state_of`.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    OutputClock, StopSignal, TimeSource, MAX_REPEATS_BETWEEN_CONTROL,
};
use multiview_framestore::TileStore;

/// 25 fps → an exact 40 ms period, so whole-period advances land on deadlines.
const PERIOD_NS: u64 = 40_000_000;
/// The injected probe costs three tick periods (the acceptance criterion).
const PROBE_COST_NS: u64 = 3 * PERIOD_NS;

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
                width: 16,
                height: 16,
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

/// The worst number of emitted ticks between two consecutive control-hook calls
/// (and from the last call to the end of the run), given the emitted count at each.
fn worst_control_gap(calls_at_emitted: &[u64], total: u64) -> u64 {
    let mut worst = 0;
    let mut prev = 0;
    for &at in calls_at_emitted.iter().chain(std::iter::once(&total)) {
        worst = worst.max(at.saturating_sub(prev));
        prev = at;
    }
    worst
}

/// A probe that honours the `repeat` flag (the production FaultDetector wiring):
/// it costs 3× the tick period on every FRESH frame for the first `SLOW_TICKS`
/// emitted ticks, then drops to one period. The output must hold exactly one
/// contiguous frame per tick against wall-clock, drain commands within a bounded
/// number of ticks, and return to fresh-every-tick once the cost drops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_probe_holds_one_frame_per_tick_drains_commands_and_recovers() {
    const TICKS: u64 = 400;
    const SLOW_TICKS: u64 = 300;
    let cadence = Rational::new(25, 1);
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(),
        ts,
        CooperativePacer,
    );
    let seed = runtime.seed_nanos();

    // (index, repeat) for every published frame.
    let log = Arc::new(Mutex::new(Vec::<(u64, bool)>::new()));
    let log_s = Arc::clone(&log);
    let ts_probe = Arc::clone(&time_source);
    let state_of = move |f: &CompositedFrame| -> u64 {
        let mut log = log_s.lock().unwrap();
        let emitted = log.len() as u64;
        log.push((f.tick.index, f.repeat));
        drop(log);
        if !f.repeat {
            // The probe: expensive while "slow", one period once the cost drops.
            let cost = if emitted < SLOW_TICKS {
                PROBE_COST_NS
            } else {
                PERIOD_NS
            };
            ts_probe.advance(Duration::from_nanos(cost));
        }
        f.tick.index
    };

    // The control hook = the command drain. Record how many ticks had been
    // emitted at each call: a command posted at any tick is drained at the next.
    let calls = Arc::new(Mutex::new(Vec::<u64>::new()));
    let calls_ctl = Arc::clone(&calls);
    let log_ctl = Arc::clone(&log);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        let emitted = log_ctl.lock().unwrap().len() as u64;
        calls_ctl.lock().unwrap().push(emitted);
    };

    let stop = StopSignal::new();
    let run = runtime.run_for_with_control(
        publisher.as_ref(),
        &stop,
        TICKS,
        state_of,
        |_f| None::<u64>,
        control,
    );
    let outcome = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the run must not park or spin")
        .expect("the run returns Ok");
    assert_eq!(outcome.ticks, TICKS);

    let log = log.lock().unwrap();
    // frames == ticks: one contiguous frame per tick index, no resync gap.
    assert_eq!(log.len() as u64, TICKS);
    for (i, &(index, _)) in log.iter().enumerate() {
        assert_eq!(
            index, i as u64,
            "frames must be contiguous (one per tick, no resync skip)"
        );
    }
    // ...over wall-clock: the last emitted index tracks wall-clock, not probe speed.
    let wall_index = (time_source.now_nanos() - seed) / PERIOD_NS as i64;
    let last_index = log.last().unwrap().0 as i64;
    assert!(
        last_index + 4 >= wall_index,
        "output slipped behind wall-clock: last {last_index}, wall {wall_index}"
    );
    // The slow phase really was overloaded (repeats held cadence).
    let slow_repeats = log
        .iter()
        .take(SLOW_TICKS as usize)
        .filter(|&&(_, r)| r)
        .count();
    assert!(slow_repeats > 0, "the 3x probe must force cadence repeats");

    // Commands drain within a bounded number of ticks.
    let calls = calls.lock().unwrap();
    let gap = worst_control_gap(&calls, TICKS);
    assert!(
        gap <= u64::from(MAX_REPEATS_BETWEEN_CONTROL) + 1,
        "control hook starved for {gap} ticks (bound {})",
        u64::from(MAX_REPEATS_BETWEEN_CONTROL) + 1
    );

    // Recovery: once the cost drops, the tail is fresh every tick again.
    assert!(
        log.iter().rev().take(50).all(|&(_, r)| !r),
        "cadence must recover to fresh-every-tick once the probe cost drops"
    );
}

/// Worst case: a projection that IGNORES the `repeat` flag and costs 3× the tick
/// period on EVERY frame, repeats included. One frame per tick is physically
/// impossible here (each publish costs three periods), so the engine's duty is
/// the inv #1/#10 floor: the control hook still runs within a bounded number of
/// emitted ticks, pts stay strictly increasing, the run never parks, and cadence
/// recovers once the cost drops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecimated_slow_projection_cannot_starve_the_control_hook() {
    const TICKS: u64 = 600;
    let cadence = Rational::new(25, 1);
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(),
        ts,
        CooperativePacer,
    );

    let slow = Arc::new(AtomicBool::new(true));
    let log = Arc::new(Mutex::new(Vec::<(i64, bool)>::new()));
    let log_s = Arc::clone(&log);
    let ts_probe = Arc::clone(&time_source);
    let slow_s = Arc::clone(&slow);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s.lock().unwrap().push((f.pts().as_nanos(), f.repeat));
        if slow_s.load(Ordering::Acquire) {
            ts_probe.advance(Duration::from_nanos(PROBE_COST_NS));
        } else if !f.repeat {
            ts_probe.advance(Duration::from_nanos(PERIOD_NS));
        }
        f.tick.index
    };

    let calls = Arc::new(Mutex::new(Vec::<u64>::new()));
    let calls_ctl = Arc::clone(&calls);
    let log_ctl = Arc::clone(&log);
    let slow_ctl = Arc::clone(&slow);
    let drained = Arc::new(AtomicU64::new(0));
    let drained_ctl = Arc::clone(&drained);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        let emitted = log_ctl.lock().unwrap().len() as u64;
        calls_ctl.lock().unwrap().push(emitted);
        // The cost drops after 300 emitted ticks (the recovery phase).
        if emitted >= 300 {
            slow_ctl.store(false, Ordering::Release);
        }
        drained_ctl.fetch_add(1, Ordering::AcqRel);
    };

    let stop = StopSignal::new();
    let run = runtime.run_for_with_control(
        publisher.as_ref(),
        &stop,
        TICKS,
        state_of,
        |_f| None::<u64>,
        control,
    );
    let outcome = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the run must not park or spin")
        .expect("the run returns Ok");
    assert_eq!(outcome.ticks, TICKS);

    let calls = calls.lock().unwrap();
    let gap = worst_control_gap(&calls, TICKS);
    assert!(
        gap <= u64::from(MAX_REPEATS_BETWEEN_CONTROL) + 1,
        "an undecimated slow projection starved the control hook for {gap} ticks (bound {})",
        u64::from(MAX_REPEATS_BETWEEN_CONTROL) + 1
    );
    assert!(drained.load(Ordering::Acquire) > 0);

    let log = log.lock().unwrap();
    for w in log.windows(2) {
        assert!(w[1].0 > w[0].0, "pts must stay strictly increasing");
    }
    assert!(
        log.iter().rev().take(50).all(|&(_, r)| !r),
        "cadence must recover to fresh-every-tick once the projection cost drops"
    );
}
