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

use multiview_config::{KeyerLossPolicy, KeyerSize};
use multiview_core::switcher::FrameCount;
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
