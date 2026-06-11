//! Penalty-box state-machine tests (ADR-MV001): a sustained alarm boxes a tile
//! (returning a layout action), a sustained recovery restores it, and neither
//! flaps — all over an injected `MediaTime`, never blocking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_engine::alarm::penalty_box::{
    PenaltyAction, PenaltyBox, PenaltyConfig, PenaltyState,
};
use proptest::prelude::*;

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

#[test]
fn boxes_only_after_sustain_and_returns_action() {
    let cfg = PenaltyConfig::new(3, ms(500), ms(500)).promote(9);
    let mut pb = PenaltyBox::new(cfg);
    assert_eq!(pb.state(), PenaltyState::Normal);

    // Alarm active at t=0: arming, no action yet.
    assert_eq!(pb.observe(true, ms(0)), PenaltyAction::None);
    assert!(matches!(pb.state(), PenaltyState::Arming { .. }));
    assert!(!pb.is_boxed());

    // Still within sustain.
    assert_eq!(pb.observe(true, ms(499)), PenaltyAction::None);

    // Sustain elapsed: box it, with auto-promote spare.
    assert_eq!(
        pb.observe(true, ms(500)),
        PenaltyAction::PenaltyBox {
            tile: 3,
            promote: Some(9)
        }
    );
    assert_eq!(pb.state(), PenaltyState::Boxed);
    assert!(pb.is_boxed());
}

#[test]
fn alarm_clearing_before_sustain_does_not_box() {
    let mut pb = PenaltyBox::new(PenaltyConfig::new(0, ms(500), ms(500)));
    assert_eq!(pb.observe(true, ms(0)), PenaltyAction::None);
    // Clears before sustain: back to Normal, never boxed.
    assert_eq!(pb.observe(false, ms(200)), PenaltyAction::None);
    assert_eq!(pb.state(), PenaltyState::Normal);
}

#[test]
fn restores_only_after_release_and_returns_action() {
    let mut pb = PenaltyBox::new(PenaltyConfig::new(2, ms(0), ms(300)));
    // Sustain 0: boxes immediately.
    assert!(matches!(
        pb.observe(true, ms(0)),
        PenaltyAction::PenaltyBox { tile: 2, .. }
    ));
    // Alarm clears at t=1000: enters Releasing, still boxed.
    assert_eq!(pb.observe(false, ms(1000)), PenaltyAction::None);
    assert!(matches!(pb.state(), PenaltyState::Releasing { .. }));
    assert!(pb.is_boxed());
    // Within release window.
    assert_eq!(pb.observe(false, ms(1299)), PenaltyAction::None);
    // Release elapsed: restore.
    assert_eq!(
        pb.observe(false, ms(1300)),
        PenaltyAction::Restore { tile: 2 }
    );
    assert_eq!(pb.state(), PenaltyState::Normal);
    assert!(!pb.is_boxed());
}

#[test]
fn alarm_returning_within_release_snaps_back_to_boxed() {
    let mut pb = PenaltyBox::new(PenaltyConfig::new(1, ms(0), ms(300)));
    pb.observe(true, ms(0));
    pb.observe(false, ms(100));
    assert!(matches!(pb.state(), PenaltyState::Releasing { .. }));
    // Fault returns mid-release: snap back to Boxed, no Restore emitted.
    assert_eq!(pb.observe(true, ms(150)), PenaltyAction::None);
    assert_eq!(pb.state(), PenaltyState::Boxed);
}

#[test]
fn no_promote_spare_means_promote_none() {
    let mut pb = PenaltyBox::new(PenaltyConfig::new(7, ms(0), ms(0)));
    assert_eq!(
        pb.observe(true, ms(0)),
        PenaltyAction::PenaltyBox {
            tile: 7,
            promote: None
        }
    );
}

#[test]
fn backwards_clock_does_not_shorten_sustain() {
    let mut pb = PenaltyBox::new(PenaltyConfig::new(0, ms(500), ms(0)));
    pb.observe(true, ms(1000));
    // Backwards step: elapsed clamps to 0, still arming.
    assert_eq!(pb.observe(true, ms(900)), PenaltyAction::None);
    assert!(matches!(pb.state(), PenaltyState::Arming { .. }));
}

// Property: at most one PenaltyBox and one Restore over a full active→inactive
// cycle that satisfies both dwells; the box action fires only after sustain and
// the restore only after release.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn one_box_one_restore_per_cycle(
        sustain_ms in 0_i64..400,
        release_ms in 0_i64..400,
        active_ms in 1_i64..800,
        inactive_ms in 1_i64..800,
        step_ms in 1_i64..40,
    ) {
        let cfg = PenaltyConfig::new(0, ms(sustain_ms), ms(release_ms));
        let mut pb = PenaltyBox::new(cfg);

        // Observe at every `step`, then force an observation exactly at the window
        // boundary so a deadline that falls between steps is still evaluated while
        // the run is still in effect (the machine can only act when observed).
        let mut boxes = 0_u32;
        let mut box_time: Option<i64> = None;
        let active_end = active_ms;
        let mut t = 0_i64;
        while t < active_end {
            if let PenaltyAction::PenaltyBox { tile, .. } = pb.observe(true, ms(t)) {
                prop_assert_eq!(tile, 0);
                boxes += 1;
                box_time = Some(t);
            }
            t += step_ms;
        }
        if let PenaltyAction::PenaltyBox { tile, .. } = pb.observe(true, ms(active_end)) {
            prop_assert_eq!(tile, 0);
            boxes += 1;
            box_time = Some(active_end);
        }

        if active_ms >= sustain_ms {
            prop_assert_eq!(boxes, 1, "exactly one box when active >= sustain");
            prop_assert!(box_time.unwrap() >= sustain_ms);
        } else {
            prop_assert_eq!(boxes, 0, "no box when active < sustain");
        }

        if !pb.is_boxed() {
            return Ok(());
        }

        let mut restores = 0_u32;
        let recover_start = active_end + 1;
        let recover_end = recover_start + inactive_ms;
        let mut restore_time: Option<i64> = None;
        let mut t = recover_start;
        while t < recover_end {
            if let PenaltyAction::Restore { tile } = pb.observe(false, ms(t)) {
                prop_assert_eq!(tile, 0);
                restores += 1;
                restore_time = Some(t);
            }
            t += step_ms;
        }
        if let PenaltyAction::Restore { tile } = pb.observe(false, ms(recover_end)) {
            prop_assert_eq!(tile, 0);
            restores += 1;
            restore_time = Some(recover_end);
        }

        if inactive_ms >= release_ms {
            prop_assert_eq!(restores, 1, "exactly one restore when inactive >= release");
            prop_assert!(restore_time.unwrap() - recover_start >= release_ms);
        }
    }
}
