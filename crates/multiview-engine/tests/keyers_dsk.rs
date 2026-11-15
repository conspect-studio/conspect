//! Downstream keyer frame coordination (ADR-0113 §5, design note §11.3, slice
//! K2a): CUT, AUTO, TIE on CUT, MIX, DIP and WIPE, abort and reject atomicity, a held
//! background take, `on_at_start`, tally and the snapshot. Real hook, CPU
//! drive, exact tick PTS.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

#[path = "support/keyers_rig.rs"]
mod rig;

use multiview_core::switcher::{kernel, FrameCount, Progress, TransitionDescriptor};
use multiview_core::time::Rational;
use multiview_engine::keyer::{KeyerReason, TallyRole};
use multiview_engine::switcher_control::{
    SwitcherVerb, REFUSE_LOST_DURING_AUTO, TAKE_PRIME_DEADLINE_TICKS,
};
use rig::{cmd, drain, dsk, probe, step, weight_of, Rig, LUMA_A, LUMA_B, LUMA_FILL};

fn rig() -> Rig {
    Rig::new(&["a", "b", "gfx", "matte", "cold"])
}

fn keep_fresh(rig: &Rig, index: u64) {
    rig.publish_background(index);
    rig.publish("gfx", LUMA_FILL, index);
}

#[test]
fn a_dsk_cut_lands_on_the_next_tick_and_off_is_immediate() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let mut events = handle.keyer_events();
    keep_fresh(&rig, 0);
    assert!(probe(&step(&mut hook, &mut drive, 0)).abs_diff(LUMA_A) < 8);
    handle
        .try_send(cmd("on", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    keep_fresh(&rig, 1);
    let frame = step(&mut hook, &mut drive, 1);
    assert!(
        probe(&frame).abs_diff(LUMA_FILL) < 8,
        "the fill is keyed on tick 1"
    );
    let outcome = drain(&mut outcomes).0;
    assert_eq!(outcome.len(), 1);
    assert_eq!((outcome[0].tick, outcome[0].completed), (1, true));
    let snapshot = handle.snapshot().unwrap();
    assert!(snapshot.keyers[0].unwrap().on_air);
    assert!(snapshot.tally.program.contains(TallyRole::Fill(0)));
    let event = drain(&mut events).0;
    assert_eq!(event.len(), 1);
    assert_eq!(
        (event[0].tick, event[0].on_air, event[0].reason),
        (1, true, KeyerReason::Operator)
    );

    handle
        .try_send(cmd(
            "off",
            SwitcherVerb::KeyerCut {
                keyer: 0,
                on: false,
            },
        ))
        .unwrap();
    // Going off is never gated: it lands on the next tick.
    rig.publish_background(2);
    let frame = step(&mut hook, &mut drive, 2);
    assert!(probe(&frame).abs_diff(LUMA_A) < 8);
    assert!(
        hook.driver.keyers().last_good(0).is_none(),
        "the pair is released off air"
    );
    assert!(!handle
        .snapshot()
        .unwrap()
        .tally
        .program
        .contains(TallyRole::Fill(0)));
}

#[test]
fn a_dsk_auto_of_n_frames_shows_weight_k_over_n_at_tick_k() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let n = 8_u32;
    let rate = FrameCount::try_from(n).unwrap();
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
    let snapshots_before = handle.snapshot().map(|s| s.tick);
    let mut snapshot_changes = 0;
    let mut last = snapshots_before;
    let mut previous_luma = 0_u8;
    for k in 0..=u64::from(n) {
        keep_fresh(&rig, k);
        let frame = step(&mut hook, &mut drive, k);
        let expected = if k < u64::from(n) {
            kernel::opacity(Progress::new(u32::try_from(k).unwrap(), rate))
        } else {
            1.0
        };
        let weight = weight_of(&drive, 0).unwrap_or(0.0);
        assert!(
            (weight - expected).abs() < 1e-6,
            "tick {k}: {weight} vs {expected}"
        );
        // The blend is in linear light, so the picture is monotonic in the
        // weight rather than linear in code values.
        let luma = probe(&frame);
        assert!(
            luma >= previous_luma,
            "tick {k}: {luma} after {previous_luma}"
        );
        previous_luma = luma;
        let now = handle.snapshot().map(|s| s.tick);
        if now != last {
            snapshot_changes += 1;
            last = now;
        }
    }
    // Snapshot changes: the first frame (ramp starts), tick 1 (the fill
    // starts contributing to the tally) and the terminal tick. Never per
    // ramp tick.
    assert!(snapshot_changes <= 3, "{snapshot_changes} snapshot changes");
    let outcome = drain(&mut outcomes).0;
    assert_eq!(outcome.len(), 1, "one outcome, at the terminal tick");
    assert_eq!(
        (outcome[0].tick, outcome[0].completed),
        (u64::from(n), true)
    );
    assert!(handle.snapshot().unwrap().keyers[0].unwrap().ramp.is_none());
}

#[test]
fn a_tied_dsk_flips_on_the_cut_tick() {
    let rig = rig();
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.tie = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    keep_fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    assert!(
        handle
            .snapshot()
            .unwrap()
            .tally
            .preview
            .contains(TallyRole::Fill(0)),
        "a tied keyer going on is PREVIEW-tallied"
    );
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    keep_fresh(&rig, 1);
    let frame = step(&mut hook, &mut drive, 1);
    assert!(
        probe(&frame).abs_diff(LUMA_FILL) < 8,
        "tied DSK on with the CUT"
    );
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert!(snapshot.keyers[0].unwrap().on_air);
    // A second CUT takes it off on that tick.
    handle.try_send(cmd("cut2", SwitcherVerb::Cut)).unwrap();
    keep_fresh(&rig, 2);
    let frame = step(&mut hook, &mut drive, 2);
    assert!(probe(&frame).abs_diff(LUMA_A) < 8);
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
}

#[test]
fn a_tied_dsk_follows_a_mix_and_flips_on_the_terminal_tick() {
    let rig = rig();
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.tie = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let n = 4_u32;
    let rate = FrameCount::try_from(n).unwrap();
    handle
        .try_send(cmd("mix", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..u64::from(n) {
        keep_fresh(&rig, k);
        step(&mut hook, &mut drive, k);
        let p = kernel::opacity(Progress::new(u32::try_from(k).unwrap(), rate));
        let weight = weight_of(&drive, 0).unwrap_or(0.0);
        assert!(
            (weight - p).abs() < 1e-6,
            "going on follows p at {k}: {weight}"
        );
        assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    }
    keep_fresh(&rig, u64::from(n));
    step(&mut hook, &mut drive, u64::from(n));
    assert!(
        handle.snapshot().unwrap().keyers[0].unwrap().on_air,
        "flips on the terminal tick"
    );
    assert_eq!(weight_of(&drive, 0), Some(1.0));
    // And back off over 1 − p.
    handle
        .try_send(cmd("mix2", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..u64::from(n) {
        let index = u64::from(n) + 1 + k;
        keep_fresh(&rig, index);
        step(&mut hook, &mut drive, index);
        let p = kernel::opacity(Progress::new(u32::try_from(k).unwrap(), rate));
        let weight = weight_of(&drive, 0).unwrap_or(0.0);
        assert!(
            (weight - (1.0 - p)).abs() < 1e-6,
            "going off follows 1-p at {k}: {weight}"
        );
    }
    let end = 2 * u64::from(n) + 1;
    keep_fresh(&rig, end);
    step(&mut hook, &mut drive, end);
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    assert_eq!(weight_of(&drive, 0), None);
}

/// ADR-0113 §4 / design note §3.2 on the ADR-0112 DIP: a tied keyer going on
/// draws the incoming contribution `i` (0 until the switch point), going off
/// `1 − d`, from the same `dip_weights` the transition tiles use, and flips on
/// the terminal tick.
#[test]
fn a_tied_dsk_follows_a_dip_and_flips_on_the_terminal_tick() {
    let rig = rig();
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.tie = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let n = 4_u32;
    let half = Rational::new(1, 2);
    let dip: TransitionDescriptor = serde_json::from_value(serde_json::json!({
        "kind": "dip",
        "rate": n,
        "switch_point": {"num": half.num, "den": half.den},
    }))
    .unwrap();
    handle
        .try_send(cmd("set", SwitcherVerb::SetTransition(dip)))
        .unwrap();
    let rate = FrameCount::try_from(n).unwrap();
    handle
        .try_send(cmd("dip", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..u64::from(n) {
        keep_fresh(&rig, k);
        step(&mut hook, &mut drive, k);
        let (_, incoming) = kernel::dip_weights(u32::try_from(k).unwrap(), n, half);
        let weight = weight_of(&drive, 0).unwrap_or(0.0);
        assert!(
            (weight - incoming).abs() < 1e-6,
            "going on follows i at {k}: {weight}"
        );
        assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    }
    keep_fresh(&rig, u64::from(n));
    step(&mut hook, &mut drive, u64::from(n));
    assert!(
        handle.snapshot().unwrap().keyers[0].unwrap().on_air,
        "flips on the terminal tick"
    );
    assert_eq!(weight_of(&drive, 0), Some(1.0));
    handle
        .try_send(cmd("dip2", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..u64::from(n) {
        let index = u64::from(n) + 1 + k;
        keep_fresh(&rig, index);
        step(&mut hook, &mut drive, index);
        let (d, _) = kernel::dip_weights(u32::try_from(k).unwrap(), n, half);
        let weight = weight_of(&drive, 0).unwrap_or(0.0);
        assert!(
            (weight - (1.0 - d)).abs() < 1e-6,
            "going off follows 1-d at {k}: {weight}"
        );
    }
    let end = 2 * u64::from(n) + 1;
    keep_fresh(&rig, end);
    step(&mut hook, &mut drive, end);
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    assert_eq!(weight_of(&drive, 0), None);
}

/// K2a deviation (design note §14, K3): a tied keyer on a WIPE keeps its
/// state through the flight (weight 0 going on, 1 going off) and flips on
/// the terminal tick, a CUT at the transition end. The per-pixel `m(x, y)`
/// envelope lands with K3.
#[test]
fn a_tied_dsk_on_a_wipe_cuts_at_the_terminal_tick() {
    let rig = rig();
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.tie = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let n = 4_u32;
    let wipe: TransitionDescriptor = serde_json::from_value(serde_json::json!({
        "kind": "wipe",
        "pattern": "linear_edge",
        "rate": n,
    }))
    .unwrap();
    handle
        .try_send(cmd("set", SwitcherVerb::SetTransition(wipe)))
        .unwrap();
    let rate = FrameCount::try_from(n).unwrap();
    handle
        .try_send(cmd("wipe", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..u64::from(n) {
        keep_fresh(&rig, k);
        step(&mut hook, &mut drive, k);
        assert_eq!(
            weight_of(&drive, 0),
            None,
            "stays off during the wipe at {k}"
        );
        assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    }
    keep_fresh(&rig, u64::from(n));
    step(&mut hook, &mut drive, u64::from(n));
    assert!(
        handle.snapshot().unwrap().keyers[0].unwrap().on_air,
        "flips on the terminal tick"
    );
    assert_eq!(weight_of(&drive, 0), Some(1.0));
    handle
        .try_send(cmd("wipe2", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..u64::from(n) {
        let index = u64::from(n) + 1 + k;
        keep_fresh(&rig, index);
        step(&mut hook, &mut drive, index);
        assert_eq!(
            weight_of(&drive, 0),
            Some(1.0),
            "stays fully on during the wipe at {k}"
        );
        assert!(handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    }
    let end = 2 * u64::from(n) + 1;
    keep_fresh(&rig, end);
    step(&mut hook, &mut drive, end);
    assert!(!handle.snapshot().unwrap().keyers[0].unwrap().on_air);
    assert_eq!(weight_of(&drive, 0), None);
}

#[test]
fn an_aborted_auto_restores_the_tied_keyer() {
    // SW2-02: PVW loses its fresh frame before the terminal tick.
    let rig = rig();
    let mut cfg = dsk("dsk1", "gfx", None, 1);
    cfg.tie = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let rate = FrameCount::try_from(20).unwrap();
    handle
        .try_send(cmd("mix", SwitcherVerb::Auto(rate)))
        .unwrap();
    for k in 0..=20 {
        rig.publish("a", LUMA_A, k);
        rig.publish("gfx", LUMA_FILL, k);
        if k == 0 {
            rig.publish("b", LUMA_B, 0); // b goes stale after tick 12
        }
        step(&mut hook, &mut drive, k);
    }
    let refused = drain(&mut outcomes)
        .0
        .into_iter()
        .find(|o| o.operation_id == "mix")
        .unwrap();
    assert_eq!(refused.error.as_deref(), Some(REFUSE_LOST_DURING_AUTO));
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "a");
    assert!(
        !snapshot.keyers[0].unwrap().on_air,
        "the tied keyer kept its state"
    );
    assert_eq!(weight_of(&drive, 0), None);
}

#[test]
fn a_rejected_frame_restores_keyer_flags_and_refuses_the_staged_op() {
    let rig = rig();
    // A cell id the drive does not know: the CUT's rebind fails, so the
    // frame is rejected (the `failed_rebind` pattern of switcher_hook.rs).
    let (handle, mut hook, mut drive) =
        rig.build_with_cell(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b", "missing");
    let mut outcomes = handle.outcomes();
    let mut events = handle.keyer_events();
    keep_fresh(&rig, 0);
    handle
        .try_send(cmd("key", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    // The first frame binds the cell (`last_program` is None), which fails.
    step(&mut hook, &mut drive, 0);
    let refused = drain(&mut outcomes).0;
    let key = refused.iter().find(|o| o.operation_id == "key").unwrap();
    assert!(!key.completed && key.error.is_some(), "{key:?}");
    assert!(
        !hook.driver.keyers().flags(0).unwrap().on_air,
        "flags restored"
    );
    assert!(
        drain(&mut events).0.is_empty(),
        "no event for a rejected frame"
    );
    assert_eq!(
        weight_of(&drive, 0),
        None,
        "nothing keyed on the rejected state"
    );
}

#[test]
fn a_held_background_take_does_not_block_a_keyer_cut() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "cold");
    let mut outcomes = handle.outcomes();
    keep_fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    // PVW `cold` never publishes: the CUT is held for 50 ticks.
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    keep_fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    handle
        .try_send(cmd("key", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    keep_fresh(&rig, 2);
    let frame = step(&mut hook, &mut drive, 2);
    assert!(
        probe(&frame).abs_diff(LUMA_FILL) < 8,
        "the DSK went on while the CUT is held"
    );
    let outcome = drain(&mut outcomes).0;
    let key = outcome.iter().find(|o| o.operation_id == "key").unwrap();
    assert!(key.completed);
    assert!(
        outcome.iter().all(|o| o.operation_id != "cut"),
        "the CUT is still held"
    );
    for k in 3..=1 + TAKE_PRIME_DEADLINE_TICKS {
        keep_fresh(&rig, k);
        step(&mut hook, &mut drive, k);
    }
    let cut = drain(&mut outcomes)
        .0
        .into_iter()
        .find(|o| o.operation_id == "cut")
        .unwrap();
    assert!(cut.error.is_some());
}

#[test]
fn on_at_start_goes_on_at_the_first_fresh_tick_without_an_outcome() {
    let rig = rig();
    let mut cfg = dsk("bug", "gfx", None, 1);
    cfg.on_at_start = true;
    let (handle, mut hook, mut drive) = rig.build(&[cfg], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let mut events = handle.keyer_events();
    for k in 0..5 {
        rig.publish_background(k);
        step(&mut hook, &mut drive, k);
        assert_eq!(weight_of(&drive, 0), None, "no fill yet at {k}");
    }
    keep_fresh(&rig, 5);
    let frame = step(&mut hook, &mut drive, 5);
    assert!(probe(&frame).abs_diff(LUMA_FILL) < 8);
    let event = drain(&mut events).0;
    assert_eq!(event.len(), 1);
    assert_eq!(
        (event[0].tick, event[0].reason),
        (5, KeyerReason::OnAtStart)
    );
    assert!(
        drain(&mut outcomes).0.is_empty(),
        "the boot intent has no operation"
    );
}

#[test]
fn a_keyer_command_for_an_undefined_index_is_refused() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    handle
        .try_send(cmd(
            "bad",
            SwitcherVerb::KeyerTie {
                keyer: 1,
                tie: true,
            },
        ))
        .unwrap();
    keep_fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let outcome = drain(&mut outcomes).0;
    assert!(outcome[0]
        .error
        .as_deref()
        .unwrap()
        .starts_with("unknown-keyer"));
}

#[test]
fn a_fill_and_key_pair_is_program_tallied_and_draws_in_z_order() {
    let rig = rig();
    let defs = [
        dsk("dsk1", "gfx", Some("matte"), 2),
        dsk("dsk2", "b", None, 1),
    ];
    // Two keyers need a larger budget than the default.
    let (handle, mut hook, mut drive) = rig.build(&defs, 10.0, "a", "b");
    for keyer in 0..2 {
        handle
            .try_send(cmd(
                &format!("k{keyer}"),
                SwitcherVerb::KeyerCut { keyer, on: true },
            ))
            .unwrap();
    }
    keep_fresh(&rig, 0);
    rig.publish("matte", 235, 0);
    step(&mut hook, &mut drive, 0);
    let draws: Vec<u8> = drive
        .keyer_draws()
        .iter()
        .flatten()
        .map(|d| d.keyer)
        .collect();
    assert_eq!(
        draws,
        vec![1, 0],
        "ascending z: dsk2 (z 1) below dsk1 (z 2)"
    );
    let tally = handle.snapshot().unwrap().tally;
    for role in [
        TallyRole::Program,
        TallyRole::Fill(0),
        TallyRole::Key(0),
        TallyRole::Fill(1),
    ] {
        assert!(tally.program.contains(role), "{role:?}");
    }
    assert!(
        !tally.program.contains(TallyRole::Key(1)),
        "a self key has no key role"
    );
}

/// Review F4: the PVW role is PROGRAM-tallied only on a frame where the
/// transition's incoming tile contributes: never at MIX `p = 0`, never on a
/// DIP's outgoing-only (colour) ticks before the switch point.
fn preview_tally_follows_the_incoming_tile(dip: bool) {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let n = 8_u32;
    let half = Rational::new(1, 2);
    if dip {
        let descriptor: TransitionDescriptor = serde_json::from_value(serde_json::json!({
            "kind": "dip",
            "rate": n,
            "switch_point": {"num": half.num, "den": half.den},
        }))
        .unwrap();
        handle
            .try_send(cmd("set", SwitcherVerb::SetTransition(descriptor)))
            .unwrap();
    }
    let rate = FrameCount::try_from(n).unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate)))
        .unwrap();
    let mut pre = 0;
    for k in 0..u64::from(n) {
        keep_fresh(&rig, k);
        step(&mut hook, &mut drive, k);
        let elapsed = u32::try_from(k).unwrap();
        let incoming = if dip {
            kernel::dip_weights(elapsed, n, half).1
        } else {
            kernel::opacity(Progress::new(elapsed, rate))
        };
        let tally = hook.driver.keyers().tally();
        assert_eq!(
            tally.program.contains(TallyRole::Preview),
            incoming > 0.0,
            "tick {k}: incoming weight {incoming}"
        );
        if incoming <= 0.0 {
            pre += 1;
        }
    }
    assert!(pre >= 1, "the run covers a tick with no incoming picture");
    if dip {
        assert!(pre > 1, "the DIP's colour-only ticks are covered");
    }
}

#[test]
fn preview_is_not_program_tallied_at_mix_p_zero() {
    preview_tally_follows_the_incoming_tile(false);
}

#[test]
fn preview_is_not_program_tallied_before_a_dip_switch_point() {
    preview_tally_follows_the_incoming_tile(true);
}

/// Review F5: a keyer AUTO rate override is bounded like the configured rate
/// (1..=300) and an over-limit one is refused before any state change.
#[test]
fn a_keyer_auto_rate_override_is_bounded() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build(&[dsk("dsk1", "gfx", None, 1)], 3.0, "a", "b");
    let mut outcomes = handle.outcomes();
    let mut index = 0;
    for (op, frames) in [("301", 301_u32), ("max", u32::MAX)] {
        handle
            .try_send(cmd(
                op,
                SwitcherVerb::KeyerAuto {
                    keyer: 0,
                    on: true,
                    rate: Some(FrameCount::try_from(frames).unwrap()),
                },
            ))
            .unwrap();
        keep_fresh(&rig, index);
        step(&mut hook, &mut drive, index);
        index += 1;
        let outcome = drain(&mut outcomes).0;
        assert_eq!(outcome.len(), 1, "{op}: {outcome:?}");
        assert_eq!(outcome[0].tick, index - 1);
        assert!(
            outcome[0]
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("keyer-rate")),
            "{op}: {outcome:?}"
        );
        let flags = hook.driver.keyers().flags(0).unwrap();
        assert!(flags.ramp.is_none() && !flags.on_air && flags.pending.is_none());
        assert_eq!(weight_of(&drive, 0), None);
    }
    // The limit itself is accepted and ramps.
    handle
        .try_send(cmd(
            "300",
            SwitcherVerb::KeyerAuto {
                keyer: 0,
                on: true,
                rate: Some(FrameCount::try_from(300).unwrap()),
            },
        ))
        .unwrap();
    keep_fresh(&rig, index);
    step(&mut hook, &mut drive, index);
    let ramp = hook.driver.keyers().flags(0).unwrap().ramp.unwrap();
    assert_eq!(
        (ramp.start, ramp.rate.get(), ramp.to_on),
        (index, 300, true)
    );
    assert!(
        drain(&mut outcomes).0.is_empty(),
        "in flight, no outcome yet"
    );
}
