//! Downstream keyer chaos (ADR-0113 Consequences, design note §11.5, slice
//! K2a; invariants #1 and #10). Assertions are on counts and bounds, never
//! on wall-clock percentiles, so they hold on a loaded CI runner.
//!
//! Counting-allocator gap: the workspace forbids `unsafe_code` on every
//! target and a `GlobalAlloc` needs `unsafe`, so the no-per-tick-allocation
//! gate here is the drive's pooled odometers (`scratch_reservations`,
//! `slate_builds`) plus the keyer runtime's fixed arrays (recorded under
//! Concerns in the PR).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::float_cmp
)]
// reason: a chaos test asserts many counts over one run; splitting it hides
// the single-run relationship between the flood and the clock.

#[path = "support/keyers_rig.rs"]
mod rig;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use multiview_core::switcher::FrameCount;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::isolation::TryRecvError;
use multiview_engine::keyer::TallyRole;
use multiview_engine::switcher_control::{SwitcherVerb, COMMAND_CAPACITY, MAX_COMMANDS_PER_TICK};
use multiview_engine::{
    CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal, TimeSource,
};
use rig::{cmd, drain, dsk, step, Rig, LUMA_FILL, TICK_NS};

const FLOOD_TASKS: u64 = 8;
const FLOOD_COMMANDS: u64 = 10_000;

fn flood_verb(n: u64) -> SwitcherVerb {
    let keyer = u8::try_from(n % 2).unwrap();
    match n % 5 {
        0 => SwitcherVerb::KeyerCut { keyer, on: true },
        1 => SwitcherVerb::KeyerCut { keyer, on: false },
        2 => SwitcherVerb::KeyerAuto {
            keyer,
            on: n % 3 == 0,
            rate: Some(FrameCount::try_from(3).unwrap()),
        },
        3 => SwitcherVerb::KeyerTie {
            keyer,
            tie: n % 4 == 0,
        },
        // An undefined keyer: refused at once.
        _ => SwitcherVerb::KeyerCut { keyer: 7, on: true },
    }
}

/// The flood of the stalled-consumer test: going-on commands, ties and an
/// undefined keyer, but never "off". Reason: `flood_verb` sends an "off" to
/// each keyer every few commands, so with 16 commands drained per tick a
/// keyer is taken off air on the first tick its sources die, before the
/// configured `reconnecting` trigger (lag >= 4 ticks with the fast
/// thresholds) is reached, and the loss path this test exists to exercise
/// never runs. Going-on commands keep keyers engaged across the death (and
/// are held, then refused at +50 or landed on recovery), so the run drops
/// keyers at the trigger while consumers are stalled.
fn stalled_flood_verb(n: u64) -> SwitcherVerb {
    let keyer = u8::try_from(n % 2).unwrap();
    match n % 4 {
        0 => SwitcherVerb::KeyerCut { keyer, on: true },
        1 => SwitcherVerb::KeyerAuto {
            keyer,
            on: true,
            rate: Some(FrameCount::try_from(3).unwrap()),
        },
        2 => SwitcherVerb::KeyerTie {
            keyer,
            tie: n % 3 == 0,
        },
        _ => SwitcherVerb::KeyerCut { keyer: 7, on: true },
    }
}

/// 10 000 keyer commands from 8 tokio tasks against the real runtime: the
/// clock emits one contiguous, correctly stamped frame per tick, never
/// repeats, the mailbox stays bounded (full = a counted 503), at most 16
/// commands apply per tick, and every accepted op gets exactly one outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keyer_command_flood_never_moves_the_output_clock() {
    const TICKS: u64 = 1_500;
    let rig = Rig::new(&["a", "b", "gfx", "matte"]);
    rig.publish_background(0);
    rig.publish("gfx", LUMA_FILL, 0);
    rig.publish("matte", 235, 0);
    let defs = [
        dsk("dsk1", "gfx", Some("matte"), 1),
        dsk("dsk2", "gfx", None, 2),
    ];
    let (handle, hook, drive) = rig.build(&defs, 10.0, "a", "b");
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut outcomes = handle.outcomes();

    let accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let busy = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for task in 0..FLOOD_TASKS {
        let handle = handle.clone();
        let accepted = Arc::clone(&accepted);
        let busy = Arc::clone(&busy);
        tasks.push(tokio::spawn(async move {
            let per_task = FLOOD_COMMANDS / FLOOD_TASKS;
            for i in 0..per_task {
                let n = task * per_task + i;
                let op = format!("k-{n}");
                if handle.try_send(cmd(&op, flood_verb(n))).is_ok() {
                    accepted.lock().unwrap().push(op);
                } else {
                    // The 503 the control plane would answer.
                    busy.fetch_add(1, Ordering::Relaxed);
                }
                if i % 16 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    let mut indices = Vec::new();
    let mut stamps = Vec::new();
    let mut seen = HashSet::new();
    let mut per_tick = std::collections::HashMap::<u64, usize>::new();
    let mut lagged = 0_u64;
    let pace = Arc::clone(&clock);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                indices.push(frame.tick.index);
                stamps.push(frame.tick.pts);
                loop {
                    match outcomes.try_recv() {
                        Ok(item) => {
                            assert!(
                                seen.insert(item.event.operation_id.clone()),
                                "one outcome per op: {}",
                                item.event.operation_id
                            );
                            *per_tick.entry(item.event.tick).or_default() += 1;
                        }
                        Err(TryRecvError::Lagged(n)) => lagged += n,
                        Err(_) => break,
                    }
                }
                pace.advance(std::time::Duration::from_nanos(
                    u64::try_from(TICK_NS).unwrap(),
                ));
                let next = frame.tick.index + 1;
                rig.publish_background(next);
                rig.publish("gfx", LUMA_FILL, next);
                rig.publish("matte", 235, next);
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    for task in tasks {
        task.await.unwrap();
    }

    // Invariant #1: one frame per tick, contiguous, stamped from the tick.
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    for (index, pts) in indices.iter().zip(&stamps) {
        assert_eq!(
            *pts,
            MediaTime::from_nanos(i64::try_from(*index).unwrap() * TICK_NS),
            "PTS from the tick counter"
        );
    }
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");

    // Invariant #10: the flood met a bounded mailbox; refusals are counted.
    let accepted = accepted.lock().unwrap().clone();
    let busy = busy.load(Ordering::Relaxed);
    assert_eq!(
        u64::try_from(accepted.len()).unwrap() + busy,
        FLOOD_COMMANDS
    );
    assert!(busy > 0, "the flood filled the 64-slot mailbox");
    assert_eq!(lagged, 0, "a per-tick reader never lags the outcome ring");
    // Every accepted op is resolved, except at most the mailbox plus one
    // held command and one AUTO per keyer.
    let unresolved = accepted.iter().filter(|op| !seen.contains(*op)).count();
    assert!(
        unresolved <= COMMAND_CAPACITY + 4,
        "unresolved ops are bounded: {unresolved}"
    );
    // Each of the 16 drained commands yields at most two outcomes (a CUT
    // also refuses the AUTO it interrupts), plus one ramp terminal and one
    // held-command terminal per keyer.
    assert!(
        per_tick
            .values()
            .all(|&n| n <= 2 * MAX_COMMANDS_PER_TICK + 2 * 2),
        "bounded work per tick: {:?}",
        per_tick.values().max()
    );
    assert!(seen.iter().all(|op| op.starts_with("k-")));
}

/// Consumers that never read outcomes, keyer events or snapshots while
/// sources die and commands flood: retention stays bounded by the ring
/// capacity, a late reader sees a counted lag, Program stays contiguous.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_consumers_never_stall_the_clock_while_sources_die() {
    const TICKS: u64 = 1_200;
    let mut rig = Rig::new(&["a", "b"]);
    rig.fast("gfx");
    rig.fast("matte");
    let defs = [
        dsk("dsk1", "gfx", Some("matte"), 1),
        dsk("dsk2", "gfx", None, 2),
    ];
    let (handle, hook, drive) = rig.build(&defs, 10.0, "a", "b");
    // Subscribed, never read until the end.
    let mut stalled_outcomes = handle.outcomes();
    let mut stalled_events = handle.keyer_events();
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let stalled_snapshot = Arc::new(Mutex::new(None));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The flood runs until the clock stops, so it overlaps every tick.
    let flood = {
        let handle = handle.clone();
        let snapshot = Arc::clone(&stalled_snapshot);
        let done = Arc::clone(&done);
        tokio::spawn(async move {
            let mut n = 0_u64;
            while !done.load(Ordering::Acquire) {
                let _ = handle.try_send(cmd(&format!("s-{n}"), stalled_flood_verb(n)));
                if n == 100 {
                    // A reader that grabs one snapshot and holds it forever.
                    *snapshot.lock().unwrap() = handle.snapshot();
                }
                if n % 8 == 0 {
                    tokio::task::yield_now().await;
                }
                n += 1;
            }
            n
        })
    };
    let mut indices = Vec::new();
    let pace = Arc::clone(&clock);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                indices.push(frame.tick.index);
                pace.advance(std::time::Duration::from_nanos(
                    u64::try_from(TICK_NS).unwrap(),
                ));
                let next = frame.tick.index + 1;
                rig.publish_background(next);
                // The keyer sources live for 20 ticks out of every 60, so
                // keyers repeatedly reach the loss trigger and drop.
                if next % 60 < 20 {
                    rig.publish("gfx", LUMA_FILL, next);
                    rig.publish("matte", 235, next);
                }
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    done.store(true, Ordering::Release);
    let sent = flood.await.unwrap();
    assert!(sent > 0);
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    assert_eq!(repeated.load(Ordering::Acquire), 0);
    // Bounded retention. `EventSubscription::len` is tokio's
    // `broadcast::Receiver::len`: it counts every message not yet received,
    // INCLUDING ones the ring already overwrote (they surface as a `Lagged`
    // count), so it is not capped by the capacity and cannot express the
    // bound. The bound is what a drain actually yields: at most the ring
    // (256) retained, every other message accounted as a counted lag.
    let outcomes_pending = u64::try_from(stalled_outcomes.len()).unwrap();
    let events_pending = u64::try_from(stalled_events.len()).unwrap();
    let (buffered, outcome_lag) = drain(&mut stalled_outcomes);
    // Over 1 200 ticks of flooding the never-read ring overflows: the late
    // reader sees its drops as a counted lag, and at most the ring.
    assert!(buffered.len() <= 256, "{}", buffered.len());
    assert_eq!(
        u64::try_from(buffered.len()).unwrap() + outcome_lag,
        outcomes_pending
    );
    if sent >= 2_000 {
        assert!(
            outcome_lag > 0,
            "the stalled outcome reader saw a counted lag"
        );
    }
    let (events, events_lag) = drain(&mut stalled_events);
    assert!(events.len() <= 256, "{}", events.len());
    assert_eq!(
        u64::try_from(events.len()).unwrap() + events_lag,
        events_pending
    );
    assert!(!events.is_empty());
    assert!(
        handle.keyer_counters().source_lost() > 0,
        "keyers dropped at the trigger during the run"
    );
    // The held snapshot is just an old Arc; the live one moved on.
    let held = stalled_snapshot.lock().unwrap().clone();
    if let (Some(held), Some(now)) = (held, handle.snapshot()) {
        assert!(now.tick >= held.tick);
    }
}

/// 10 000 ticks of ramping keyers (an AUTO on/off loop) and a tied keyer on
/// a background MIX: after one warm-up cycle the pooled scratch and the
/// slate cache never reserve again, and the tally roles match the composed
/// keyer tiles on every tick.
#[test]
fn ramping_keyers_never_grow_the_scratch_pool_and_tally_matches_tiles() {
    const TICKS: u64 = 10_000;
    const CYCLE: u64 = 40;
    let rig = Rig::new(&["a", "b", "gfx", "matte"]);
    let mut tied = dsk("dsk2", "gfx", None, 2);
    tied.tie = true;
    let defs = [dsk("dsk1", "gfx", Some("matte"), 1), tied];
    let (handle, mut hook, mut drive) = rig.build(&defs, 10.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let rate = FrameCount::try_from(8).unwrap();
    let mut baseline = None;
    for k in 0..TICKS {
        match k % CYCLE {
            0 => handle
                .try_send(cmd(
                    &format!("on-{k}"),
                    SwitcherVerb::KeyerAuto {
                        keyer: 0,
                        on: true,
                        rate: Some(rate),
                    },
                ))
                .unwrap(),
            12 => handle
                .try_send(cmd(
                    &format!("off-{k}"),
                    SwitcherVerb::KeyerAuto {
                        keyer: 0,
                        on: false,
                        rate: Some(rate),
                    },
                ))
                .unwrap(),
            24 => handle
                .try_send(cmd(&format!("mix-{k}"), SwitcherVerb::Auto(rate)))
                .unwrap(),
            _ => {}
        }
        rig.publish_background(k);
        rig.publish("gfx", LUMA_FILL, k);
        rig.publish("matte", 235, k);
        step(&mut hook, &mut drive, k);
        // Tally roles match the keyer tiles actually composed this tick.
        let tally = hook.driver.keyers().tally();
        for keyer in 0..2_u8 {
            let drawn = drive
                .keyer_draws()
                .iter()
                .flatten()
                .any(|d| d.keyer == keyer && d.weight > 0.0);
            assert_eq!(
                tally.program.contains(TallyRole::Fill(keyer)),
                drawn,
                "tick {k}: keyer {keyer} tally vs tiles"
            );
        }
        assert_eq!(
            tally.program.contains(TallyRole::Key(0)),
            drive
                .keyer_draws()
                .iter()
                .flatten()
                .any(|d| d.keyer == 0 && d.key.is_some())
        );
        // The outcome ring is drained like the control-plane relay does.
        let (_, lagged) = drain(&mut outcomes);
        assert_eq!(lagged, 0);
        if k == CYCLE {
            // One full cycle (keyer AUTO on/off, background MIX, tied flip)
            // has sized every pool.
            baseline = Some((drive.scratch_reservations(), drive.slate_builds()));
        }
        if let Some((reservations, slates)) = baseline {
            assert_eq!(drive.scratch_reservations(), reservations, "tick {k}");
            assert_eq!(drive.slate_builds(), slates, "tick {k}");
        }
    }
    assert!(baseline.is_some());
    assert_eq!(handle.keyer_counters().events_dropped(), 0);
}
