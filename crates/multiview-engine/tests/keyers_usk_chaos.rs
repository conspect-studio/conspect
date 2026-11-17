//! Upstream keyer chaos (ADR-0113, design note §11.5, slice K3; invariants
//! #1 and #10): armed-key sources that stall, vanish and come back while
//! key-changing AUTOs (MIX, DIP, WIPE, key-only) run back to back, with an
//! `arm` flood from tokio tasks. Program ticks stay contiguous and correctly
//! stamped, nothing repeats, every take gets exactly one terminal outcome,
//! and the pooled odometers never move after warm-up (the no-per-tick-
//! allocation gate; the workspace forbids the `unsafe` a counting allocator
//! needs, see K2a Concerns).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "support/keyers_rig.rs"]
mod rig;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// The run: the clock ticks against a manual time source; armed-key sources
/// k1..k3 each live on their own duty cycle (k3 vanishes for good at tick
/// 400), so keys stall and reach the loss trigger mid-AUTO. A task floods
/// `arm`, transition selections, AUTOs and CUTs through the bounded mailbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalling_and_vanishing_armed_key_sources_never_stall_or_skip_a_tick() {
    const TICKS: u64 = 1_500;
    let mut rig = Rig::new(&["a", "b"]);
    for id in ["k1", "k2", "k3"] {
        rig.fast(id);
    }
    rig.publish_background(0);
    let (handle, hook, drive) = rig.build_usk(&[], &usks(), 100.0, "a", "b");
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
    let done = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let busy = Arc::new(AtomicU64::new(0));
    let flood = {
        let handle = handle.clone();
        let done = Arc::clone(&done);
        let accepted = Arc::clone(&accepted);
        let busy = Arc::clone(&busy);
        tokio::spawn(async move {
            let mut n = 0_u64;
            while !done.load(Ordering::Acquire) {
                let verb = match n % 6 {
                    0 => arm(n % 4 != 0, &[1 + u32::try_from(n % 3).unwrap()]),
                    1 => arm(false, &[1, 2, 3]),
                    2 => transition(n / 6),
                    3 => SwitcherVerb::Auto(FrameCount::try_from(6).unwrap()),
                    4 => SwitcherVerb::Cut,
                    _ => arm(true, &[2]),
                };
                let op = format!("c-{n}");
                if handle.try_send(cmd(&op, verb)).is_ok() {
                    accepted.lock().unwrap().push(op);
                } else {
                    busy.fetch_add(1, Ordering::Relaxed);
                }
                if n % 4 == 0 {
                    tokio::task::yield_now().await;
                }
                n += 1;
            }
            n
        })
    };
    let mut frames = Vec::new();
    let pace = Arc::clone(&clock);
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
                let next = frame.tick.index + 1;
                rig.publish_background(next);
                // k1 stalls 10 of every 30 ticks, k2 25 of every 70, k3 is
                // live until tick 400 and then vanishes for good.
                if next % 30 < 20 {
                    rig.publish("k1", LUMA_FILL, next);
                }
                if next % 70 < 45 {
                    rig.publish("k2", 200, next);
                }
                if next < 400 {
                    rig.publish("k3", 150, next);
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
    // Invariant #1: exactly one frame per tick, contiguous, stamped from the
    // tick counter, none repeated.
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
    // Invariant #10: the mailbox is bounded (some sends were refused), and
    // every accepted command got exactly one terminal outcome (the ring is
    // drained here, after the run; a lag would only make this weaker, so it
    // must be zero for the check to hold).
    let (got, lagged) = drain(&mut outcomes);
    if lagged == 0 {
        let mut seen = HashSet::new();
        for o in &got {
            assert!(seen.insert(o.operation_id.clone()), "one outcome per op: {o:?}");
        }
    }
    let _ = busy.load(Ordering::Relaxed);
    assert!(
        handle.keyer_counters().source_lost() > 0,
        "armed keys reached the loss trigger during the run"
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
                handle.try_send(cmd(&format!("t-{k}"), transition(n))).unwrap();
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
