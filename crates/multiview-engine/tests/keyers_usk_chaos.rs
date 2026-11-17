//! Upstream keyer chaos (ADR-0113, design note §11.5, slice K3; invariants
//! #1 and #10), restructured after review K3-INV-2 into deterministic
//! scenarios with count/bound assertions: armed-key sources stalling and
//! vanishing DURING a key-changing AUTO (clock-driven, the loss asserted
//! in flight); a bounded `arm`/take flood whose accepted operation ids are
//! compared exactly with the terminal outcomes; a stalled outcome reader's
//! counted overflow; and the pooled odometers over 6 000 key-changing ticks
//! (the no-per-tick-allocation gate; the workspace forbids the `unsafe` a
//! counting allocator needs, see K2a Concerns).
#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "support/keyers_rig.rs"]
mod rig;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_core::switcher::{ArmedSet, FrameCount, TransitionDescriptor};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::keyer::{DrawRole, TallyRole};
use multiview_engine::switcher_control::SwitcherVerb;
use multiview_engine::{
    CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal, TimeSource,
};
use rig::{cmd, drain, step, usk, Rig, LUMA_FILL, SIDE, TICK_NS};

fn arm(background: bool, keys: &[u32]) -> SwitcherVerb {
    let mut set = ArmedSet::default();
    if background {
        set = set.with_background();
    }
    for &k in keys {
        set = set.with_key(k);
    }
    SwitcherVerb::Arm(Arc::new(set))
}

fn transition(i: u64) -> SwitcherVerb {
    let json = match i % 3 {
        0 => serde_json::json!({"kind": "mix", "rate": 6}),
        1 => serde_json::json!({"kind": "dip", "rate": 6,
            "switch_point": {"num": 1, "den": 2}}),
        _ => serde_json::json!({"kind": "wipe", "pattern": "circle", "rate": 6,
            "softness": 0.1, "border_width": 0.05, "border_color": "#000000"}),
    };
    SwitcherVerb::SetTransition(serde_json::from_value::<TransitionDescriptor>(json).unwrap())
}

fn usks() -> Vec<multiview_config::UpstreamKeyerConfig> {
    vec![
        usk("usk1", "k1", 1, Some((0, 0, 32, 32))),
        usk("usk2", "k2", 2, None),
        usk("usk3", "k3", 3, Some((32, 32, 32, 32))),
    ]
}

/// Review K3-INV-2, scenario 1 (deterministic, clock-driven): a
/// key-changing AUTO is in flight when the armed keys' sources stall and
/// then vanish DURING it. The runtime's clock ticks against a manual time
/// source; the frame callback publishes sources by tick index, so the
/// sequence is the same on any runner. Each cycle: arm usk1 (going on) and
/// usk3 (going off), start a 40-tick AUTO at phase 0, stop k1 at phase 5 and
/// k3 at phase 8 (both reach the 6-tick loss ladder mid-flight), let the
/// AUTO end, then bring them back. Asserted: every loss happened while a
/// transition was in flight (counted in the frame callback from the
/// committed snapshot), Program ticks are contiguous with PTS from the tick
/// counter, and no frame repeats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_key_sources_vanishing_during_an_auto_never_stall_or_skip_a_tick() {
    const CYCLE: u64 = 100;
    const CYCLES: u64 = 8;
    const TICKS: u64 = CYCLE * CYCLES;
    let mut rig = Rig::new(&["a", "b"]);
    for id in ["k1", "k2", "k3"] {
        rig.fast(id);
    }
    rig.publish_background(0);
    for id in ["k1", "k3"] {
        rig.publish(id, LUMA_FILL, 0);
    }
    let (handle, hook, drive) = rig.build_usk(&[], &usks(), 100.0, "a", "b");
    // usk3 starts on air so each AUTO takes one key on and one off.
    let mut outcomes = handle.outcomes();
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
    let counters = handle.keyer_counters();
    let mut frames = Vec::new();
    let mut lost_in_flight = 0_u64;
    let mut lost_outside = 0_u64;
    let mut flights = 0_u64;
    let mut last_lost = counters.source_lost();
    let pace = Arc::clone(&clock);
    let control = handle.clone();
    // Commands are enqueued from the frame callback at fixed phases, so the
    // drain order is deterministic: at most 3 per tick, far below the
    // mailbox (64) and the per-tick drain (16).
    let send = move |op: String, verb: SwitcherVerb| {
        control.try_send(cmd(&op, verb)).unwrap();
    };
    send("setup-arm".into(), arm(false, &[3]));
    send("setup-mix".into(), transition(0));
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                frames.push((frame.tick.index, frame.tick.pts));
                pace.advance(std::time::Duration::from_nanos(
                    u64::try_from(TICK_NS).unwrap(),
                ));
                let k = frame.tick.index;
                // The loss counter moved on this committed frame: where was
                // the M/E?
                let lost = counters.source_lost();
                if lost > last_lost {
                    if handle.snapshot().is_some_and(|s| s.in_flight) {
                        lost_in_flight += lost - last_lost;
                    } else {
                        lost_outside += lost - last_lost;
                    }
                    last_lost = lost;
                }
                if handle.snapshot().is_some_and(|s| s.in_flight) && k % CYCLE == 1 {
                    flights += 1;
                }
                let next = k + 1;
                rig.publish_background(next);
                let phase = next % CYCLE;
                // Warm-up: put usk3 on before the first cycle.
                if next == 1 {
                    send("setup-cut".into(), SwitcherVerb::Cut);
                }
                // k1 / k3 are live outside the stall window [5, 60).
                if !(5..60).contains(&phase) {
                    rig.publish("k1", LUMA_FILL, next);
                }
                if !(8..60).contains(&phase) {
                    rig.publish("k3", 150, next);
                }
                match phase {
                    // Re-arm after any loss took a key off: usk1 on, usk3 off
                    // (or back on, whatever their state; both toggle).
                    90 => send(format!("arm-{next}"), arm(false, &[1, 3])),
                    0 if next >= CYCLE => send(
                        format!("auto-{next}"),
                        SwitcherVerb::Auto(FrameCount::try_from(40).unwrap()),
                    ),
                    _ => {}
                }
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    assert_eq!(u64::try_from(frames.len()).unwrap(), TICKS);
    for (i, (index, pts)) in frames.iter().enumerate() {
        assert_eq!(*index, u64::try_from(i).unwrap(), "contiguous ticks");
        assert_eq!(
            *pts,
            MediaTime::from_nanos(i64::try_from(*index).unwrap() * TICK_NS),
            "PTS from the tick counter"
        );
    }
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    println!("flights={flights} lost_in_flight={lost_in_flight} lost_outside={lost_outside}");
    assert!(flights >= CYCLES - 2, "AUTOs were in flight: {flights}");
    assert!(
        lost_in_flight >= CYCLES - 2,
        "armed keys were lost during the active transition ({lost_in_flight} in flight, \
         {lost_outside} outside)"
    );
    // Every operation got exactly one terminal outcome (the ring holds them
    // all: a few per cycle, far below 256).
    let (got, lagged) = drain(&mut outcomes);
    assert_eq!(lagged, 0);
    let mut seen = HashSet::new();
    for o in &got {
        assert!(
            seen.insert(o.operation_id.clone()),
            "one outcome per op: {o:?}"
        );
    }
    assert!(
        seen.iter().filter(|op| op.starts_with("auto-")).count()
            >= usize::try_from(CYCLES - 2).unwrap()
    );
}

/// Review K3-INV-2, scenario 2: a bounded flood of `arm` / AUTO / CUT from
/// 8 tasks while the clock runs. The producers stop before a bounded drain,
/// then every ACCEPTED operation id has exactly one terminal outcome and
/// every refused send was a full mailbox (counted, never accepted). The
/// outcome stream is drained by a reader task every tick, so the ring never
/// overflows here (asserted: zero lag).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bounded_arm_and_take_flood_accounts_for_every_operation() {
    const PER_TASK: u64 = 400;
    const TASKS: u64 = 8;
    let rig = Rig::new(&["a", "b", "k1", "k2", "k3"]);
    rig.publish_background(0);
    let (handle, mut hook, mut drive) = rig.build_usk(&[], &usks(), 100.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let accepted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let busy = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for t in 0..TASKS {
        let handle = handle.clone();
        let accepted = Arc::clone(&accepted);
        let busy = Arc::clone(&busy);
        tasks.push(tokio::spawn(async move {
            for i in 0..PER_TASK {
                let n = t * PER_TASK + i;
                let verb = match n % 5 {
                    0 => arm(n % 2 == 0, &[1 + u32::try_from(n % 3).unwrap()]),
                    1 => transition(n),
                    2 => SwitcherVerb::Auto(FrameCount::try_from(3).unwrap()),
                    3 => SwitcherVerb::Cut,
                    _ => arm(true, &[]),
                };
                let op = format!("f-{n}");
                if handle.try_send(cmd(&op, verb)).is_ok() {
                    accepted.lock().unwrap().push(op);
                } else {
                    busy.fetch_add(1, Ordering::Relaxed);
                }
                if i % 8 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }
    // The clock drains while the producers run, then a bounded drain after.
    let mut got = Vec::new();
    let mut lagged = 0;
    let mut k = 0_u64;
    let mut ticks = Vec::new();
    while tasks.iter().any(|t| !t.is_finished()) {
        rig.publish_background(k);
        for id in ["k1", "k2", "k3"] {
            rig.publish(id, LUMA_FILL, k);
        }
        ticks.push(step(&mut hook, &mut drive, k).tick.index);
        let (items, lag) = drain(&mut outcomes);
        got.extend(items);
        lagged += lag;
        k += 1;
        tokio::task::yield_now().await;
    }
    for t in tasks {
        t.await.unwrap();
    }
    // Bounded drain: the mailbox holds at most 64 commands, drained 16 per
    // tick; a held take resolves within its 50-tick deadline and an AUTO of
    // 3 ticks completes; 80 ticks bound all of it.
    for _ in 0..80 {
        rig.publish_background(k);
        for id in ["k1", "k2", "k3"] {
            rig.publish(id, LUMA_FILL, k);
        }
        ticks.push(step(&mut hook, &mut drive, k).tick.index);
        let (items, lag) = drain(&mut outcomes);
        got.extend(items);
        lagged += lag;
        k += 1;
    }
    assert!(
        ticks.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    assert_eq!(lagged, 0, "the per-tick reader never lags");
    let accepted = accepted.lock().unwrap().clone();
    let busy = busy.load(Ordering::Relaxed);
    assert_eq!(
        u64::try_from(accepted.len()).unwrap() + busy,
        TASKS * PER_TASK,
        "every send was accepted or refused busy"
    );
    let mut outcome_ids = HashSet::new();
    for o in &got {
        assert!(
            outcome_ids.insert(o.operation_id.clone()),
            "one outcome per op: {o:?}"
        );
    }
    let accepted: HashSet<String> = accepted.into_iter().collect();
    assert_eq!(
        outcome_ids, accepted,
        "terminal outcomes are exactly the accepted operations"
    );
}

/// Review K3-INV-2, scenario 3: a reader that never reads while more
/// outcomes than the ring holds are produced sees exactly the ring (256)
/// and a counted lag for the rest; nothing blocks the clock.
#[test]
fn a_stalled_outcome_reader_sees_the_ring_and_a_counted_lag() {
    let rig = Rig::new(&["a", "b", "k1", "k2", "k3"]);
    let (handle, mut hook, mut drive) = rig.build_usk(&[], &usks(), 100.0, "a", "b");
    let mut stalled = handle.outcomes();
    let mut produced = 0_u64;
    for k in 0..60_u64 {
        // 10 refused arms per tick (key 4 is not defined): one outcome each.
        for i in 0..10 {
            handle
                .try_send(cmd(&format!("x-{k}-{i}"), arm(false, &[4])))
                .unwrap();
            produced += 1;
        }
        rig.publish_background(k);
        step(&mut hook, &mut drive, k);
    }
    assert!(produced > 256);
    let (items, lagged) = drain(&mut stalled);
    assert_eq!(items.len(), 256, "the ring is retained");
    assert_eq!(
        u64::try_from(items.len()).unwrap() + lagged,
        produced,
        "the rest is a counted lag"
    );
}

/// 6 000 ticks of key-changing AUTOs (MIX, DIP, WIPE with a border, a
/// key-only one) with stalling key sources: after one warm-up cycle the
/// drive's pooled scratch, the compositor's band/scene scratch and the DIP
/// colour cache never grow, and the tally names a USK on Program exactly
/// when its draw is visible.
#[test]
fn key_changing_transitions_never_grow_the_pools() {
    const TICKS: u64 = 6_000;
    const CYCLE: u64 = 60;
    let rig = Rig::new(&["a", "b", "k1", "k2", "k3"]);
    let (handle, mut hook, mut drive) = rig.build_usk(&[], &usks(), 100.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let mut baseline = None;
    let mut grouped = 0_u64;
    for k in 0..TICKS {
        let phase = k % CYCLE;
        let n = k / CYCLE;
        match phase {
            0 => {
                handle
                    .try_send(cmd(&format!("t-{k}"), transition(n)))
                    .unwrap();
                let background = n % 4 != 3;
                handle
                    .try_send(cmd(&format!("arm-{k}"), arm(background, &[1, 2, 3])))
                    .unwrap();
                handle
                    .try_send(cmd(
                        &format!("auto-{k}"),
                        SwitcherVerb::Auto(FrameCount::try_from(8).unwrap()),
                    ))
                    .unwrap();
            }
            30 => handle
                .try_send(cmd(&format!("cut-{k}"), SwitcherVerb::Cut))
                .unwrap(),
            _ => {}
        }
        rig.publish_background(k);
        rig.publish("k1", LUMA_FILL, k);
        if phase % 20 < 15 {
            rig.publish("k2", 200, k);
        }
        rig.publish("k3", 150, k);
        step(&mut hook, &mut drive, k);
        if drive
            .keyer_draws()
            .iter()
            .flatten()
            .any(|d| d.role != DrawRole::Top)
        {
            grouped += 1;
        }
        let tally = hook.driver.keyers().tally();
        for d in drive.keyer_draws().iter().flatten() {
            if d.role == DrawRole::Top {
                assert!(tally.program.contains(TallyRole::Fill(d.keyer)), "tick {k}");
            }
        }
        let (_, lagged) = drain(&mut outcomes);
        assert_eq!(lagged, 0);
        if k == 4 * CYCLE {
            // Every transition kind and the key-only path ran once.
            baseline = Some((
                drive.scratch_reservations(),
                drive.slate_builds(),
                drive.transition_fill_builds(),
                hook.driver.definition_builds(),
            ));
        }
        if let Some(b) = baseline {
            assert_eq!(
                (
                    drive.scratch_reservations(),
                    drive.slate_builds(),
                    drive.transition_fill_builds(),
                    hook.driver.definition_builds()
                ),
                b,
                "tick {k}"
            );
        }
    }
    assert!(grouped > 100, "key-changing frames composed: {grouped}");
    assert_eq!(handle.keyer_counters().events_dropped(), 0);
    let _ = SIDE;
}
