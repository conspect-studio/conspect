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
    KeyerReason, REFUSE_KEYER_CAPACITY, REFUSE_KEYER_CLEAN, REFUSE_KEYER_DEFINITION,
    REFUSE_KEYER_ON_AIR_PROTECTED, REFUSE_KEYER_SET_DEADLINE, REFUSE_KEYER_STALE,
};
use multiview_engine::switcher_control::SwitcherVerb;
use rig::{cmd, drain, dsk, probe, pts, step, tick, Rig, LUMA_A, LUMA_B, LUMA_FILL, SIDE};
use std::collections::HashMap;
use std::sync::Arc;

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
    assert!(!handle.clean_gate().clean_refused());
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let def = handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap();
    handle
        .try_send(cmd(
            "set",
            handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), 0, def, false),
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
            handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), 1, def, false),
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
            handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), 0, def.clone(), false),
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
            handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), 0, def, true),
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
            handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), 0, def, false),
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
                handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), keyer, def, false),
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

/// Review H6: a committed `keyer_set` tick builds no `KeyerDefinitions` on
/// the clock (the odometer stays at the attach build).
#[test]
fn a_definition_changing_tick_builds_no_readback_on_the_clock() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let builds = hook.driver.definition_builds();
    let def = handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap();
    handle
        .try_send(cmd(
            "set",
            handle.keyer_set_verb(&handle.keyer_definitions().unwrap(), 0, def, false),
        ))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    assert!(drain(&mut outcomes).0[0].completed);
    assert_eq!(handle.keyer_definitions().unwrap().version, 1);
    assert_eq!(
        hook.driver.definition_builds(),
        builds,
        "the committed tick built no readback"
    );
}

/// Review H6: the readback a `keyer_set` carries is prepared against the
/// committed definitions it was admitted on; a second set admitted on the
/// same base is refused `keyer-definitions-changed` at the tick (nothing
/// changes), and the published readback is exactly the first one's `Arc`.
#[test]
fn a_keyer_set_admitted_on_a_stale_base_is_refused() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 9.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let base = handle.keyer_definitions().unwrap();
    let first = handle.keyer_set_verb(
        &base,
        0,
        handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap(),
        false,
    );
    let multiview_engine::switcher_control::SwitcherVerb::KeyerSet { readback, .. } = &first else {
        panic!("a keyer_set verb");
    };
    let prepared = std::sync::Arc::clone(&readback.0);
    let second = handle.keyer_set_verb(
        &base,
        1,
        handle.resolve_keyer(&dsk("dsk2", "gfx2", None, 2)).unwrap(),
        false,
    );
    handle.try_send(cmd("first", first)).unwrap();
    handle.try_send(cmd("second", second)).unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let outcomes = drain(&mut outcomes).0;
    let by = |op: &str| outcomes.iter().find(|o| o.operation_id == op).unwrap();
    assert!(by("first").completed);
    assert_eq!(by("second").error.as_deref(), Some(REFUSE_KEYER_STALE));
    let published = handle.keyer_definitions().unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&published, &prepared),
        "the tick published the prepared readback"
    );
    assert!(published.defs[1].is_none());
}

/// Review H4: a `keyer_set` drained at or after its deadline tick (the
/// clock's last composed tick at admission + `KEYER_SET_DEADLINE_TICKS`) is
/// refused and never applies, like a held take past
/// `TAKE_PRIME_DEADLINE_TICKS`; one drained the tick before still applies.
#[test]
fn a_keyer_set_drained_past_its_deadline_never_applies() {
    use multiview_engine::switcher_control::KEYER_SET_DEADLINE_TICKS;
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 9.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let verb = |id: &str, slot: u8, z: i32| {
        handle.keyer_set_verb(
            &handle.keyer_definitions().unwrap(),
            slot,
            handle.resolve_keyer(&dsk(id, "gfx", None, z)).unwrap(),
            false,
        )
    };
    let late = verb("dsk1", 0, 1);
    handle.try_send(cmd("late", late)).unwrap();
    // The clock skips to the deadline tick: drained there, refused.
    fresh(&rig, KEYER_SET_DEADLINE_TICKS);
    step(&mut hook, &mut drive, KEYER_SET_DEADLINE_TICKS);
    let outcome = drain(&mut outcomes).0;
    assert_eq!(outcome[0].error.as_deref(), Some(REFUSE_KEYER_SET_DEADLINE));
    assert!(!handle.keyer_definitions().unwrap().any());
    assert!(hook.driver.keyers().is_empty());
    // Admitted now (deadline = this tick + the bound), drained one tick
    // before its deadline: applied.
    let now = KEYER_SET_DEADLINE_TICKS;
    handle.try_send(cmd("in-time", verb("dsk1", 0, 1))).unwrap();
    let last = now + KEYER_SET_DEADLINE_TICKS - 1;
    fresh(&rig, last);
    step(&mut hook, &mut drive, last);
    assert!(drain(&mut outcomes).0[0].completed);
    assert!(handle.keyer_definitions().unwrap().any());
}

/// Review H3 backstop: the tick refuses a `keyer_set` defining a new keyer
/// while the router holds a Clean reservation (whatever the caller did),
/// applies nothing, and leaves no DSK reservation; a change to an existing
/// keyer is not a definition and is unaffected. Once Clean is released the
/// same verb defines the keyer and the gate's committed bit is set.
#[test]
fn the_tick_refuses_a_new_keyer_while_clean_is_held() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 9.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let gate = handle.clean_gate();
    let clean = gate.try_reserve_clean().unwrap();
    let verb = || {
        handle.keyer_set_verb(
            &handle.keyer_definitions().unwrap(),
            0,
            handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap(),
            false,
        )
    };
    handle.try_send(cmd("while-clean", verb())).unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let outcome = drain(&mut outcomes).0;
    assert_eq!(outcome[0].error.as_deref(), Some(REFUSE_KEYER_CLEAN));
    assert!(!handle.keyer_definitions().unwrap().any());
    assert_eq!(gate.dsk_reservations(), 0);
    assert!(!gate.defined());
    drop(clean);
    handle.try_send(cmd("after", verb())).unwrap();
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    assert!(drain(&mut outcomes).0[0].completed);
    assert!(gate.defined(), "the commit marks the DSK defined");
    assert_eq!(
        gate.dsk_reservations(),
        0,
        "the tick's reservation is released"
    );
    assert!(gate.try_reserve_clean().is_none());
}

/// Review H3 backstop: a rejected frame that restores away a new keyer
/// drops the tick's DSK reservation and never marks the gate defined.
#[test]
fn a_rejected_definition_releases_the_tick_reservation() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 9.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let gate = handle.clean_gate();
    let verb = handle.keyer_set_verb(
        &handle.keyer_definitions().unwrap(),
        0,
        handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap(),
        false,
    );
    handle.try_send(cmd("set", verb)).unwrap();
    hook.driver.drain_at_tick(tick(1));
    assert_eq!(gate.dsk_reservations(), 1, "held across the compose");
    hook.driver.reject_composed_tick(tick(1), "compose failed");
    assert_eq!(gate.dsk_reservations(), 0);
    assert!(!gate.defined());
    assert!(gate.try_reserve_clean().is_some());
    assert_eq!(
        drain(&mut outcomes).0[0].error.as_deref(),
        Some("compose failed")
    );
}

/// Review J3: the tick-side `keyer_set` refusals (`keyer-set-deadline`,
/// `clean-unavailable-with-dsk`) follow the switcher's existing per-command
/// outcome pattern: exactly one outcome when the command is drained, and
/// nothing on any later steady tick (no refusal is re-published per tick
/// while a command is held; these verbs are never held).
#[test]
fn a_tick_refusal_publishes_once_per_command_and_never_on_steady_ticks() {
    use multiview_engine::switcher_control::KEYER_SET_DEADLINE_TICKS;
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 9.0, "a", "b");
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let gate = handle.clean_gate();
    let clean = gate.try_reserve_clean().unwrap();
    let verb = || {
        handle.keyer_set_verb(
            &handle.keyer_definitions().unwrap(),
            0,
            handle.resolve_keyer(&dsk("dsk1", "gfx", None, 1)).unwrap(),
            false,
        )
    };
    handle.try_send(cmd("late", verb())).unwrap();
    handle.try_send(cmd("while-clean", verb())).unwrap();
    // Both share the admission base, so both are drained at their deadline
    // tick: one `keyer-set-deadline` outcome each, never re-published.
    let at = KEYER_SET_DEADLINE_TICKS;
    fresh(&rig, at);
    step(&mut hook, &mut drive, at);
    let first = drain(&mut outcomes).0;
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(first
        .iter()
        .all(|o| !o.completed && o.error.as_deref() == Some(REFUSE_KEYER_SET_DEADLINE)));
    // A fresh command refused by the Clean backstop: one outcome.
    handle.try_send(cmd("clean", verb())).unwrap();
    fresh(&rig, at + 1);
    step(&mut hook, &mut drive, at + 1);
    let second = drain(&mut outcomes).0;
    assert_eq!(second.len(), 1, "{second:?}");
    assert_eq!(second[0].error.as_deref(), Some(REFUSE_KEYER_CLEAN));
    // Steady ticks with no command publish nothing, Clean still held.
    for index in at + 2..at + 12 {
        fresh(&rig, index);
        step(&mut hook, &mut drive, index);
    }
    assert!(drain(&mut outcomes).0.is_empty(), "a steady tick published");
    drop(clean);
}

/// Issue #464 regression (merge with K4): `sync_live_stores` must republish
/// the shared `stores` `LatestState` (the boot map overlaid with the drive's
/// live stores), not just the driver's private `live_stores`, so
/// `SwitcherControl::resolve_keyer` (used by `keyer_set`) can resolve a
/// source added live and a removed-then-re-added id resolves to its NEW
/// store, not the stale boot one.
#[test]
fn sync_live_stores_republishes_the_shared_view_resolve_keyer_reads() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[], 3.0, "a", "b");
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);

    // Before any live sync, "late" has no store anywhere: resolving a keyer
    // fill against it fails honestly.
    assert!(
        handle
            .resolve_keyer(&dsk("dsk-late", "late", None, 1))
            .is_err(),
        "late is not registered yet"
    );

    // A live add: a brand-new source joins the drive's store set (not the
    // boot map). The drive is the source of truth the hook resyncs from.
    let late_store = Arc::new(multiview_framestore::TileStore::with_defaults("late"));
    late_store.publish(Rig::solid(LUMA_FILL, SIDE, SIDE), pts(1));
    let mut live = HashMap::new();
    live.insert("late".to_owned(), Arc::clone(&late_store));
    // A re-added id: "gfx" gets a brand-new Arc, distinct from the boot one.
    let new_gfx = Arc::new(multiview_framestore::TileStore::with_defaults("gfx"));
    new_gfx.publish(Rig::solid(LUMA_B, SIDE, SIDE), pts(1));
    live.insert("gfx".to_owned(), Arc::clone(&new_gfx));
    hook.driver.sync_live_stores(&live);

    // "late" now resolves: a keyer fill on a live-added source is buildable.
    let late_def = handle
        .resolve_keyer(&dsk("dsk-late", "late", None, 1))
        .expect("resolve_keyer must see the live-added store after sync");
    assert!(Arc::ptr_eq(&late_def.0.fill, &late_store));

    // "gfx" resolves to the NEW store, not the stale boot `Arc`.
    let gfx_def = handle
        .resolve_keyer(&dsk("dsk-gfx", "gfx", None, 2))
        .expect("resolve_keyer must see gfx");
    assert!(
        Arc::ptr_eq(&gfx_def.0.fill, &new_gfx),
        "resolve_keyer must resolve the re-added id to its NEW store, not the boot one"
    );
    assert!(
        !Arc::ptr_eq(&gfx_def.0.fill, rig.store("gfx")),
        "the resolved fill must not be the stale boot store"
    );
}
