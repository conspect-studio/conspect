//! `keyer_set` at one tick (ADR-0113 §10, design note §11.7, slice K4): a
//! class-1 definition change resolved off the clock, rechecked at the tick
//! (budget, unique id/z, the on-air guard), restored by a rejected frame and
//! published as committed definitions only on change.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/keyers_rig.rs"]
mod rig;

use multiview_engine::keyer::{
    KeyerReason, REFUSE_KEYER_CAPACITY, REFUSE_KEYER_DEFINITION, REFUSE_KEYER_ON_AIR_PROTECTED,
};
use multiview_engine::switcher_control::SwitcherVerb;
use rig::{cmd, drain, dsk, probe, step, tick, Rig, LUMA_A, LUMA_B, LUMA_FILL, SIDE};

fn rig() -> Rig {
    Rig::new(&["a", "b", "gfx", "gfx2", "matte"])
}

fn fresh(rig: &Rig, index: u64) {
    rig.publish_background(index);
    rig.publish("gfx", LUMA_FILL, index);
    rig.publish("gfx2", LUMA_B, index);
}

#[test]
fn keyer_set_defines_a_new_keyer_at_one_tick_and_publishes_definitions_on_change() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let mut events = handle.keyer_events();
    let before = handle.keyer_definitions().unwrap();
    assert!(!before.any());
    assert!(!handle
        .dsk_defined_cell()
        .load(std::sync::atomic::Ordering::Acquire));
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let def = handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap();
    handle
        .try_send(cmd(
            "set",
            SwitcherVerb::KeyerSet {
                keyer: 0,
                def,
                allow_on_air: false,
            },
        ))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let outcome = drain(&mut outcomes).0;
    assert_eq!((outcome[0].tick, outcome[0].completed), (1, true));
    let defs = handle.keyer_definitions().unwrap();
    assert_eq!(defs.version, 1);
    assert_eq!(defs.find("dsk1").unwrap().0, 0);
    assert_eq!(&*defs.defs[0].as_ref().unwrap().object_id, "main/dsk1");
    let event = drain(&mut events).0;
    assert_eq!(event[0].reason, KeyerReason::Definition);
    assert!(!event[0].on_air, "a definition never puts a keyer on air");
    // Definitions are published only on change: a quiet tick keeps the Arc.
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    assert!(std::sync::Arc::ptr_eq(
        &defs,
        &handle.keyer_definitions().unwrap()
    ));
    // The new keyer is a full keyer: CUT puts it on.
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    fresh(&rig, 3);
    assert!(probe(&step(&mut hook, &mut drive, 3)).abs_diff(LUMA_FILL) < 8);
}

#[test]
fn keyer_set_over_budget_is_refused_at_the_tick_and_changes_nothing() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    // A second full-canvas DSK with a key costs 3.0: over the 3.0 budget.
    let def = handle
        .resolve_keyer(&dsk("dsk2", "gfx2", Some("matte"), 2))
        .unwrap();
    handle
        .try_send(cmd(
            "set",
            SwitcherVerb::KeyerSet {
                keyer: 1,
                def,
                allow_on_air: false,
            },
        ))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let outcome = drain(&mut outcomes).0;
    assert_eq!(outcome[0].error.as_deref(), Some(REFUSE_KEYER_CAPACITY));
    let defs = handle.keyer_definitions().unwrap();
    assert_eq!(defs.version, 0);
    assert!(defs.defs[1].is_none());
    assert_eq!(hook.driver.keyers().len(), 1);
}

#[test]
fn changing_the_fill_of_an_on_air_keyer_needs_allow() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    drain(&mut outcomes);
    let def = handle.resolve_keyer(&dsk("dsk1", "gfx2", None, 1)).unwrap();
    handle
        .try_send(cmd(
            "protect",
            SwitcherVerb::KeyerSet {
                keyer: 0,
                def: def.clone(),
                allow_on_air: false,
            },
        ))
        .unwrap();
    fresh(&rig, 2);
    let frame = step(&mut hook, &mut drive, 2);
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8, "still the old fill");
    let outcome = drain(&mut outcomes).0;
    assert_eq!(
        outcome[0].error.as_deref(),
        Some(REFUSE_KEYER_ON_AIR_PROTECTED)
    );
    handle
        .try_send(cmd(
            "allow",
            SwitcherVerb::KeyerSet {
                keyer: 0,
                def,
                allow_on_air: true,
            },
        ))
        .unwrap();
    fresh(&rig, 3);
    let frame = step(&mut hook, &mut drive, 3);
    assert!(
        probe(&frame).abs_diff(LUMA_B) < 8,
        "the new fill is keyed on the applying tick"
    );
    assert!(drain(&mut outcomes).0[0].completed);
    assert!(handle.snapshot().unwrap().keyers[0].unwrap().on_air);
}

#[test]
fn a_rejected_frame_restores_the_previous_definition() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.rate = 30;
    let def = handle.resolve_keyer(&cfg).unwrap();
    handle
        .try_send(cmd(
            "set",
            SwitcherVerb::KeyerSet {
                keyer: 0,
                def,
                allow_on_air: false,
            },
        ))
        .unwrap();
    hook.driver.drain_at_tick(tick(1));
    assert_eq!(hook.driver.keyers().def(0).unwrap().rate.get(), 30);
    hook.driver.reject_composed_tick(tick(1), "compose failed");
    assert_eq!(hook.driver.keyers().def(0).unwrap().rate.get(), 15);
    assert_eq!(hook.driver.keyers().def_version(), 0);
    let outcome = drain(&mut outcomes).0;
    assert_eq!(outcome[0].error.as_deref(), Some("compose failed"));
    assert_eq!(handle.keyer_definitions().unwrap().version, 0);
    let _ = (LUMA_A, SIDE);
}

#[test]
fn a_definition_never_replaces_a_different_keyer_or_shares_a_z() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 9.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    for (op, keyer, cfg) in [
        ("other-id", 0, dsk("dsk9", "gfx2", None, 5)),
        ("same-z", 1, dsk("dsk2", "gfx2", None, 1)),
        ("dup-id", 1, dsk("dsk1", "gfx2", None, 2)),
    ] {
        let def = handle.resolve_keyer(&cfg).unwrap();
        handle
            .try_send(cmd(
                op,
                SwitcherVerb::KeyerSet {
                    keyer,
                    def,
                    allow_on_air: false,
                },
            ))
            .unwrap();
    }
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let outcomes = drain(&mut outcomes).0;
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes
        .iter()
        .all(|o| o.error.as_deref() == Some(REFUSE_KEYER_DEFINITION)));
    assert_eq!(handle.keyer_definitions().unwrap().version, 0);
}
