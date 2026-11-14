//! Downstream keyer fresh gate and loss policy (ADR-0113 §6-§7, design note
//! §11.4, slice K2a). Real hook, CPU drive, exact tick PTS. Stores marked
//! `fast` run the failure ladder in 2/4/6 ticks (stale/reconnecting/no
//! signal after the last frame).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

#[path = "support/keyers_rig.rs"]
mod rig;

use std::sync::Arc;

use multiview_config::{KeyerLossPolicy, KeyerLossTrigger, KeyerSize};
use multiview_core::switcher::{FrameCount, TransitionDescriptor};
use multiview_engine::keyer::{
    KeyerReason, REFUSE_KEYER_NO_FRESH_FRAME, REFUSE_KEYER_PENDING, REFUSE_KEYER_SOURCE_LOST,
};
use multiview_engine::switcher_control::{SwitcherVerb, TAKE_PRIME_DEADLINE_TICKS};
use rig::{cmd, drain, dsk, probe, step, weight_of, Rig, LUMA_A, LUMA_FILL, SIDE};

#[test]
fn a_fill_that_never_publishes_is_refused_at_exactly_the_deadline() {
    let rig = Rig::new(&["a", "b", "cold"]);
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "cold", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    rig.publish_background(0);
    step(&mut hook, &mut drive, 0);
    assert!(handle.snapshot().unwrap().keyers[0].unwrap().pending);
    // A second command for the same keyer while one is held is refused.
    handle
        .try_send(cmd(
            "again",
            SwitcherVerb::KeyerAuto {
                keyer: 0,
                on: true,
                rate: None,
            },
        ))
        .unwrap();
    let mut indices = Vec::new();
    for k in 1..=TAKE_PRIME_DEADLINE_TICKS + 5 {
        rig.publish_background(k);
        let frame = step(&mut hook, &mut drive, k);
        indices.push(frame.tick.index);
        assert!(
            probe(&frame).abs_diff(LUMA_A) < 8,
            "Program unchanged at {k}"
        );
    }
    assert!(indices.windows(2).all(|w| w[1] == w[0] + 1), "0 lost ticks");
    let all = drain(&mut outcomes).0;
    let again = all.iter().find(|o| o.operation_id == "again").unwrap();
    assert_eq!(
        (again.tick, again.error.as_deref()),
        (1, Some(REFUSE_KEYER_PENDING))
    );
    let on = all.iter().find(|o| o.operation_id == "on").unwrap();
    assert_eq!(
        on.tick, TAKE_PRIME_DEADLINE_TICKS,
        "refused at exactly tick +50"
    );
    assert_eq!(on.error.as_deref(), Some(REFUSE_KEYER_NO_FRESH_FRAME));
    assert_eq!(all.len(), 2, "exactly one outcome per op");
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().pending);
}

/// Shared body: one source of a fill + key pair dies mid-ramp.
fn death_mid_ramp(kill_fill: bool) {
    let mut rig = Rig::new(&["a", "b"]);
    rig.fast("gfx");
    rig.fast("matte");
    let (handle, mut hook, mut drive) =
        rig.build(&[dsk("dsk1", "gfx", Some("matte"), 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let mut events = handle.keyer_events();
    let rate = FrameCount::try_from(30).unwrap();
    handle
        .try_send(cmd(
            "auto",
            SwitcherVerb::KeyerAuto {
                keyer: 0,
                on: true,
                rate: Some(rate),
            },
        ))
        .unwrap();
    let last_both = 5_u64;
    for k in 0..=last_both {
        rig.publish_background(k);
        rig.publish("gfx", LUMA_FILL, k);
        rig.publish("matte", 235, k);
        step(&mut hook, &mut drive, k);
    }
    // From here one source stops; the other keeps publishing new frames.
    // The dead source's last frame still reads live for the hold window
    // (2 ticks), so the pair last updates at `last_both + 1`.
    let mut frozen = None;
    let mut lost_at = None;
    for k in last_both + 1..last_both + 20 {
        rig.publish_background(k);
        if kill_fill {
            rig.publish("matte", 235, k);
        } else {
            rig.publish("gfx", LUMA_FILL, k);
        }
        step(&mut hook, &mut drive, k);
        if k == last_both + 1 {
            frozen = hook.driver.keyers().last_good(0);
            continue;
        }
        let frozen = frozen.as_ref().unwrap();
        let flags = hook.driver.keyers().flags(0).unwrap();
        if flags.ramp.is_none() && !flags.on_air {
            lost_at = Some(k);
            break;
        }
        // Until the trigger, the stored pair is drawn, frozen together.
        let draw = drive.keyer_draws()[0]
            .as_ref()
            .expect("the frozen pair draws");
        assert!(Arc::ptr_eq(&draw.fill, &frozen.0), "fill frozen at {k}");
        assert!(
            Arc::ptr_eq(draw.key.as_ref().unwrap(), frozen.1.as_ref().unwrap()),
            "key frozen together at {k}"
        );
    }
    let lost_at = lost_at.expect("the keyer dropped at the trigger");
    // Reconnecting = a latched lag of 4 ticks (fast thresholds).
    assert_eq!(lost_at, last_both + 4, "dropped exactly at the trigger");
    let event = drain(&mut events)
        .0
        .into_iter()
        .find(|e| e.reason == KeyerReason::SourceLost)
        .unwrap();
    assert_eq!((event.tick, event.on_air), (lost_at, false));
    let refused = drain(&mut outcomes).0;
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].error.as_deref(), Some(REFUSE_KEYER_SOURCE_LOST));
    assert!(hook.driver.keyers().last_good(0).is_none(), "pair released");
    // It stays off even when the source comes back.
    for k in lost_at + 1..lost_at + 10 {
        rig.publish_background(k);
        rig.publish("gfx", LUMA_FILL, k);
        rig.publish("matte", 235, k);
        let frame = step(&mut hook, &mut drive, k);
        assert_eq!(weight_of(&drive, 0), None);
        assert!(probe(&frame).abs_diff(LUMA_A) < 8);
    }
    assert_eq!(handle.keyer_counters().source_lost(), 1);
}

#[test]
fn fill_only_death_mid_ramp_freezes_the_pair_then_drops_and_stays_off() {
    death_mid_ramp(true);
}

#[test]
fn key_only_death_mid_ramp_freezes_the_pair_then_drops_and_stays_off() {
    death_mid_ramp(false);
}

#[test]
fn hold_last_good_freezes_the_pair_and_marks_it_stale() {
    let mut rig = Rig::new(&["a", "b"]);
    rig.fast("gfx");
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.loss = KeyerLossPolicy::HoldLastGood;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let mut events = handle.keyer_events();
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    rig.publish_background(0);
    rig.publish("gfx", LUMA_FILL, 0);
    step(&mut hook, &mut drive, 0);
    let frozen = hook.driver.keyers().last_good(0).unwrap();
    for k in 1..20 {
        rig.publish_background(k);
        let frame = step(&mut hook, &mut drive, k);
        let draw = drive.keyer_draws()[0].as_ref().expect("held pair draws");
        assert!(Arc::ptr_eq(&draw.fill, &frozen.0));
        assert!(probe(&frame).abs_diff(LUMA_FILL) < 8);
    }
    let snapshot = handle.snapshot().unwrap();
    assert!(snapshot.keyers[0].unwrap().on_air);
    assert!(snapshot.keyers[0].unwrap().stale);
    let stale = drain(&mut events).0.into_iter().filter(|e| e.stale).count();
    assert_eq!(stale, 1, "stale is reported once, on the change");
    // Recovery clears stale.
    rig.publish("gfx", LUMA_FILL, 20);
    rig.publish_background(20);
    step(&mut hook, &mut drive, 20);
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().stale);
}

#[test]
fn a_slate_is_never_keyed() {
    // `NoSignal` reads have no frame (slates exist only in the drive's cell
    // sampler), so a keyer with no pair draws nothing, never a slate.
    let rig = Rig::new(&["a", "b", "cold"]);
    let mut cfg = dsk("dsk1", "cold", None, 1);
    cfg.loss = KeyerLossPolicy::HoldLastGood;
    cfg.on_at_start = true;
    let (_handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    for k in 0..30 {
        rig.publish_background(k);
        let frame = step(&mut hook, &mut drive, k);
        assert_eq!(weight_of(&drive, 0), None);
        assert!(probe(&frame).abs_diff(LUMA_A) < 8);
        assert!(hook.driver.keyers().last_good(0).is_none());
    }
}

#[test]
fn oversize_frames_are_non_live_and_counted() {
    let rig = Rig::new(&["a", "b", "gfx"]);
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.max_source = Some(KeyerSize { w: 32, h: 32 });
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    // A 64x64 fill over a 32x32 envelope can never pass the gate.
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    for k in 0..3 {
        rig.publish_background(k);
        rig.publish_sized("gfx", LUMA_FILL, k, SIDE, SIDE);
        step(&mut hook, &mut drive, k);
        assert_eq!(weight_of(&drive, 0), None);
    }
    assert!(handle.snapshot().unwrap().keyers[0].unwrap().pending);
    // In-envelope frames pass; a later oversize frame is counted and the
    // pair holds.
    rig.publish_background(3);
    rig.publish_sized("gfx", LUMA_FILL, 3, 32, 32);
    step(&mut hook, &mut drive, 3);
    assert!(drain(&mut outcomes).0[0].completed);
    let pair = hook.driver.keyers().last_good(0).unwrap();
    rig.publish_background(4);
    rig.publish_sized("gfx", LUMA_FILL, 4, SIDE, SIDE);
    step(&mut hook, &mut drive, 4);
    assert!(handle.keyer_counters().oversize_frames() >= 1);
    let held = drive.keyer_draws()[0].as_ref().unwrap();
    assert!(
        Arc::ptr_eq(&held.fill, &pair.0),
        "the in-envelope pair is drawn"
    );
}

#[test]
fn going_off_is_never_gated() {
    let rig = Rig::new(&["a", "b", "gfx"]);
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.loss = KeyerLossPolicy::HoldLastGood;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    rig.publish_background(0);
    rig.publish("gfx", LUMA_FILL, 0);
    step(&mut hook, &mut drive, 0);
    // The fill dies; AUTO off still starts at once and completes on time.
    let rate = FrameCount::try_from(5).unwrap();
    handle
        .try_send(cmd(
            "off",
            SwitcherVerb::KeyerAuto {
                keyer: 0,
                on: false,
                rate: Some(rate),
            },
        ))
        .unwrap();
    for k in 1..=6 {
        rig.publish_background(k);
        step(&mut hook, &mut drive, k);
    }
    let all = drain(&mut outcomes).0;
    let off = all.iter().find(|o| o.operation_id == "off").unwrap();
    assert_eq!(
        (off.tick, off.completed),
        (6, true),
        "started at 1, terminal at 1 + 5"
    );
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
}

/// Review F1: a tied `drop_to_off_air` DSK loses its fill during a background
/// AUTO (MIX or DIP, going on or going off), then the fill recovers before
/// the terminal tick. The loss policy applies throughout the flight (a DIP
/// going on draws weight 0 before its switch point and is still watched),
/// the loss cancels the keyer's participation, and neither the recovery nor
/// the terminal tick re-keys it: it stays off until an operator command.
fn tied_loss_during_auto(dip: bool, going_on: bool) {
    let mut rig = Rig::new(&["a", "b"]);
    rig.fast("gfx");
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.tie = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let mut events = handle.keyer_events();
    let n = 20_u32;
    if dip {
        let descriptor: TransitionDescriptor = serde_json::from_value(serde_json::json!({
            "kind": "dip",
            "rate": n,
            "switch_point": {"num": 1, "den": 2},
        }))
        .unwrap();
        handle
            .try_send(cmd("set", SwitcherVerb::SetTransition(descriptor)))
            .unwrap();
    }
    if !going_on {
        handle
            .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
            .unwrap();
    }
    rig.publish_background(0);
    rig.publish("gfx", LUMA_FILL, 0);
    step(&mut hook, &mut drive, 0);
    assert_eq!(
        hook.driver.keyers().flags(0).unwrap().on_air,
        !going_on,
        "starting state"
    );
    let rate = FrameCount::try_from(n).unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate)))
        .unwrap();
    let start = 1_u64;
    let terminal = start + u64::from(n);
    // The fill publishes for two ticks of the flight, dies (reconnecting 4
    // ticks after its last frame) and recovers well before the terminal
    // tick, before the DIP's switch point too.
    let dead = start + 2..start + 9;
    let mut lost_at = None;
    for k in start..=terminal + 5 {
        rig.publish_background(k);
        if !dead.contains(&k) {
            rig.publish("gfx", LUMA_FILL, k);
        }
        let frame = step(&mut hook, &mut drive, k);
        let flags = hook.driver.keyers().flags(0).unwrap();
        if lost_at.is_none() && handle.keyer_counters().source_lost() > 0 {
            lost_at = Some(k);
        }
        if lost_at.is_some() {
            assert_eq!(weight_of(&drive, 0), None, "no draw after the loss at {k}");
            assert!(!flags.on_air, "off stays off at {k}");
            if k >= terminal {
                assert!(
                    probe(&frame).abs_diff(rig::LUMA_B) < 8,
                    "only the new background at {k}"
                );
            }
        }
    }
    let lost_at = lost_at.expect("the tied keyer dropped at the trigger");
    assert!(
        lost_at < dead.end,
        "dropped while the fill was dead, not at the terminal tick ({lost_at})"
    );
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
    let events = drain(&mut events).0;
    let lost: Vec<_> = events
        .iter()
        .filter(|e| e.reason == KeyerReason::SourceLost)
        .collect();
    assert_eq!(lost.len(), 1, "{events:?}");
    assert_eq!((lost[0].tick, lost[0].on_air), (lost_at, false));
    assert!(
        events
            .iter()
            .all(|e| e.reason != KeyerReason::Tie || e.tick < lost_at),
        "no tie flip after the loss: {events:?}"
    );
    assert_eq!(handle.keyer_counters().source_lost(), 1);
}

#[test]
fn a_tied_dsk_lost_during_a_mix_going_on_stays_off() {
    tied_loss_during_auto(false, true);
}

#[test]
fn a_tied_dsk_lost_during_a_mix_going_off_stays_off() {
    tied_loss_during_auto(false, false);
}

#[test]
fn a_tied_dsk_lost_during_a_dip_going_on_stays_off() {
    tied_loss_during_auto(true, true);
}

#[test]
fn a_tied_dsk_lost_during_a_dip_going_off_stays_off() {
    tied_loss_during_auto(true, false);
}

/// Review F2 rig: a DSK on air from tick 0 on `gfx` (fast ladder, no-signal
/// trigger, so a frozen pair outlives the stale window), and a CUT onto `b`
/// whose Program rebind fails (the drive has no `b` store), so the frame
/// that carries it is rejected.
fn on_air_with_failing_cut() -> (
    Rig,
    multiview_engine::switcher_control::SwitcherControl,
    rig::Hook,
    multiview_engine::CompositorDrive<multiview_compositor::pipeline::Nv12Image>,
    Arc<multiview_compositor::pipeline::Nv12Image>,
) {
    let mut rig = Rig::new(&["a", "b"]);
    rig.fast("gfx");
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.loss_trigger = KeyerLossTrigger::NoSignal;
    let (handle, mut hook, mut drive) = rig.build_hiding(&[cfg], 3.0, "a", "b", "b");
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    rig.publish_background(0);
    let fill = rig.publish("gfx", LUMA_FILL, 0);
    let frame = step(&mut hook, &mut drive, 0);
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8, "on air at tick 0");
    (rig, handle, hook, drive, fill)
}

fn assert_committed(
    hook: &rig::Hook,
    drive: &multiview_engine::CompositorDrive<multiview_compositor::pipeline::Nv12Image>,
    fill: &Arc<multiview_compositor::pipeline::Nv12Image>,
    at: &str,
) {
    let draw = drive.keyer_draws()[0]
        .as_ref()
        .unwrap_or_else(|| panic!("the committed draw survives ({at})"));
    assert!(Arc::ptr_eq(&draw.fill, fill), "committed picture ({at})");
    assert_eq!(draw.weight, 1.0, "committed weight ({at})");
    let pair = hook
        .driver
        .keyers()
        .last_good(0)
        .unwrap_or_else(|| panic!("the last-good pair survives ({at})"));
    assert!(Arc::ptr_eq(&pair.0, fill), "committed pair ({at})");
    assert!(
        hook.driver.keyers().flags(0).unwrap().on_air,
        "on air ({at})"
    );
}

#[test]
fn a_rejected_take_keeps_an_on_air_dsk_in_the_frame() {
    let (rig, handle, mut hook, mut drive, _) = on_air_with_failing_cut();
    let mut outcomes = handle.outcomes();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    rig.publish_background(1);
    rig.publish("gfx", LUMA_FILL, 1);
    let committed = drive.keyer_draws()[0].as_ref().unwrap().fill.clone();
    let frame = step(&mut hook, &mut drive, 1);
    let cut = drain(&mut outcomes).0;
    assert!(cut.iter().any(|o| o.operation_id == "cut" && !o.completed));
    assert!(
        probe(&frame).abs_diff(LUMA_FILL) < 8,
        "the DSK pixels are unchanged on the rejected frame"
    );
    assert_committed(&hook, &drive, &committed, "rejected tick");
}

#[test]
fn a_rejected_take_keeps_a_frozen_dsk_pair_with_stale_sources() {
    let (rig, handle, mut hook, mut drive, fill) = on_air_with_failing_cut();
    // `gfx` stops: stale after 2 ticks, no signal after 6.
    for k in 1..3 {
        rig.publish_background(k);
        step(&mut hook, &mut drive, k);
    }
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    rig.publish_background(3);
    let frame = step(&mut hook, &mut drive, 3);
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8, "frozen pair drawn");
    assert_committed(&hook, &drive, &fill, "rejected tick");
    rig.publish_background(4);
    let frame = step(&mut hook, &mut drive, 4);
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8, "still drawn after");
    assert_committed(&hook, &drive, &fill, "next tick");
}

#[test]
fn a_rejected_tick_with_a_staged_off_restores_the_pair() {
    let (rig, handle, mut hook, mut drive, fill) = on_air_with_failing_cut();
    let mut outcomes = handle.outcomes();
    for k in 1..3 {
        rig.publish_background(k);
        step(&mut hook, &mut drive, k);
    }
    // The off is applied in the drain (releasing the pair), then the CUT's
    // rebind fails: the whole tick is rejected.
    handle
        .try_send(cmd(
            "off",
            SwitcherVerb::KeyerCut {
                keyer: 0,
                on: false,
            },
        ))
        .unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    rig.publish_background(3);
    let frame = step(&mut hook, &mut drive, 3);
    let refused = drain(&mut outcomes).0;
    let off = refused.iter().find(|o| o.operation_id == "off").unwrap();
    assert!(!off.completed && off.error.is_some(), "{off:?}");
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8);
    assert_committed(&hook, &drive, &fill, "rejected tick");
    // The sources are stale now: only the restored pair can draw.
    rig.publish_background(4);
    let frame = step(&mut hook, &mut drive, 4);
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8, "restored pair drawn");
    assert_committed(&hook, &drive, &fill, "next tick");
}

#[test]
fn a_failed_compose_restores_the_committed_keyer_draws() {
    use multiview_engine::runtime::FrameControl;
    let (rig, _handle, mut hook, mut drive, fill) = on_air_with_failing_cut();
    rig.publish_background(1);
    rig.publish("gfx", LUMA_FILL, 1);
    // Tick 1 resolves a new pair, then its compose misses its deadline.
    hook.before_compose(&mut drive, rig::tick(1));
    assert!(!Arc::ptr_eq(
        &drive.keyer_draws()[0].as_ref().unwrap().fill,
        &fill
    ));
    hook.on_compose_failed(&mut drive, rig::tick(1), "readback deadline");
    assert_committed(&hook, &drive, &fill, "failed compose");
}
