#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Degradation ladder ordering + hysteresis (no flapping).

use multiview_hal::degradation::{
    actions_at_level, DegradationAction, Hysteresis, HysteresisConfig, LadderMove, MAX_LEVEL,
};
use multiview_hal::{CostBudget, Planner};

#[test]
fn ladder_is_cheapest_impact_first_and_in_documented_order() {
    use DegradationAction::*;
    // The five PREVIEW rungs are topmost (cheapest to shed), in the exact order
    // preview-subsystem.md §8 mandates — shed focus WHEP, then grid fps, then
    // grid res, then off-air cue decoders, then suspend preview entirely — ALL
    // before any program tile/output lever moves (PRV-4, ADR-P001).
    assert_eq!(
        DegradationAction::LADDER,
        [
            ShedFocusWhep,
            DropPreviewGridFps,
            DropPreviewGridRes,
            DropOffAirCueDecoders,
            SuspendPreviewEntirely,
            DropTileResolution,
            DropTileFps,
            SimplerScaler,
            FasterEncoderPreset,
            LowerOutputBitrate,
            LowerOutputFps,
            LowerOutputResolution,
            ShedTiles,
        ]
    );
    // Rungs are strictly increasing in order.
    for window in DegradationAction::LADDER.windows(2) {
        assert!(window[0].rung() < window[1].rung());
    }
}

#[test]
fn tile_levers_come_before_program_levers() {
    // Every action before FasterEncoderPreset must NOT affect the program;
    // FasterEncoderPreset onward MUST. This encodes invariant #9's ordering:
    // shed preview, then low-priority tiles/shared resources, before the program
    // everyone sees. (Preview rungs and tile levers are both pre-program.)
    for action in DegradationAction::LADDER {
        if action.rung() < DegradationAction::FasterEncoderPreset.rung() {
            assert!(
                !action.affects_program(),
                "{action:?} should be a pre-program (preview or tile) lever"
            );
        } else {
            assert!(
                action.affects_program(),
                "{action:?} should be a program-affecting lever"
            );
        }
    }
    // The last rung (total shed) is the most drastic.
    assert_eq!(
        DegradationAction::LADDER[MAX_LEVEL - 1],
        DegradationAction::ShedTiles
    );
}

#[test]
fn actions_at_level_is_cumulative_and_clamped() {
    assert!(actions_at_level(0).is_empty());
    // Level 1 sheds the single cheapest rung — preview focus WHEP, not a tile.
    assert_eq!(actions_at_level(1), &[DegradationAction::ShedFocusWhep]);
    assert_eq!(
        actions_at_level(3),
        &[
            DegradationAction::ShedFocusWhep,
            DegradationAction::DropPreviewGridFps,
            DegradationAction::DropPreviewGridRes,
        ]
    );
    assert_eq!(actions_at_level(MAX_LEVEL).len(), MAX_LEVEL);
    // Over-clamping is saturating, never a panic / index error.
    assert_eq!(actions_at_level(MAX_LEVEL + 99).len(), MAX_LEVEL);
}

#[test]
fn preview_rungs_are_the_topmost_cheapest_rungs() {
    use DegradationAction::*;
    // The five preview rungs occupy levels 0..5 — strictly below every other
    // rung. `affects_preview()` distinguishes them; the helper
    // `first_non_preview_level()` agrees on the boundary.
    let preview = [
        ShedFocusWhep,
        DropPreviewGridFps,
        DropPreviewGridRes,
        DropOffAirCueDecoders,
        SuspendPreviewEntirely,
    ];
    for (level, action) in preview.iter().enumerate() {
        assert_eq!(action.rung(), level, "{action:?} must be rung {level}");
        assert!(
            action.affects_preview(),
            "{action:?} must be a preview rung"
        );
    }
    // The boundary helper points at the first NON-preview rung.
    assert_eq!(
        DegradationAction::first_non_preview_level(),
        preview.len(),
        "the first non-preview rung sits right after the preview block"
    );
    assert_eq!(
        DegradationAction::LADDER[DegradationAction::first_non_preview_level()],
        DropTileResolution,
        "the first non-preview rung is the cheapest tile lever"
    );
}

#[test]
fn preview_rungs_are_below_every_program_rung_and_never_affect_program() {
    // PRV-4 / ADR-P001: preview is shed BEFORE any program lever moves. Every
    // preview rung must precede every program-affecting rung AND must itself be
    // non-program-affecting.
    for action in DegradationAction::LADDER {
        if action.affects_preview() {
            assert!(
                !action.affects_program(),
                "{action:?} is preview — it must never touch the program output"
            );
            assert!(
                action.rung() < DegradationAction::first_program_level(),
                "preview rung {action:?} must sit below the first program rung"
            );
        }
    }
    // Every program-affecting rung sits at or above the first program level.
    for action in DegradationAction::LADDER {
        if action.affects_program() {
            assert!(action.rung() >= DegradationAction::first_program_level());
        }
    }
}

#[test]
fn climbing_pressure_sheds_all_preview_before_any_tile_or_program_lever() {
    // The whole point of PRV-4: under sustained overload the ladder sheds EVERY
    // preview rung before it touches a single tile/program lever. Drive the
    // controller up one rung per tick and assert the order.
    let mut h = Hysteresis::new(config());
    let first_non_preview = DegradationAction::first_non_preview_level();

    // Shedding the preview block: each Down move applies one preview rung, and
    // while preview rungs remain the program output is never touched.
    for _ in 0..first_non_preview {
        assert_eq!(h.observe(0.99), LadderMove::Down);
        let newest = *actions_at_level(h.level())
            .last()
            .expect("a rung was just applied");
        assert!(
            newest.affects_preview(),
            "rung {} ({newest:?}) must be a preview shed",
            h.level()
        );
        assert!(
            !actions_at_level(h.level())
                .iter()
                .any(|a| a.affects_program()),
            "no program rung may be active while preview rungs are still being shed"
        );
    }
    // Exactly the five preview rungs were shed, none program-affecting yet.
    assert_eq!(h.level(), first_non_preview);
    assert!(actions_at_level(h.level())
        .iter()
        .all(|a| a.affects_preview()));

    // The NEXT step finally reaches the first tile lever (still pre-program).
    assert_eq!(h.observe(0.99), LadderMove::Down);
    let newest = *actions_at_level(h.level())
        .last()
        .expect("a rung was just applied");
    assert_eq!(newest, DegradationAction::DropTileResolution);
    assert!(!newest.affects_preview());
    assert!(!newest.affects_program());
}

fn config() -> HysteresisConfig {
    HysteresisConfig::try_new(0.7, 0.9, 3).unwrap()
}

#[test]
fn high_pressure_steps_down_one_rung_at_a_time() {
    let mut h = Hysteresis::new(config());
    assert_eq!(h.level(), 0);
    // Sustained high pressure applies one rung per tick (proposes ONE step).
    for expected_level in 1..=MAX_LEVEL {
        assert_eq!(h.observe(0.95), LadderMove::Down);
        assert_eq!(h.level(), expected_level);
    }
    // At the bottom of the ladder, further high pressure holds (nothing left).
    assert_eq!(h.observe(0.99), LadderMove::Hold);
    assert_eq!(h.level(), MAX_LEVEL);
}

#[test]
fn inside_the_band_holds() {
    let mut h = Hysteresis::new(config());
    h.observe(0.95); // -> level 1
    assert_eq!(h.level(), 1);
    // 0.8 is between low (0.7) and high (0.9): hold.
    assert_eq!(h.observe(0.8), LadderMove::Hold);
    assert_eq!(h.level(), 1);
}

#[test]
fn recovery_requires_cooldown_to_elapse_first() {
    let mut h = Hysteresis::new(config());
    assert_eq!(h.observe(0.95), LadderMove::Down); // level 1, cooldown=3
    assert_eq!(h.cooldown_remaining(), 3);

    // Low pressure, but cooldown is active: must hold, decrementing cooldown.
    assert_eq!(h.observe(0.1), LadderMove::Hold);
    assert_eq!(h.observe(0.1), LadderMove::Hold);
    assert_eq!(h.observe(0.1), LadderMove::Hold);
    assert_eq!(h.cooldown_remaining(), 0);
    assert_eq!(h.level(), 1, "must not recover while cooling down");

    // Now the cooldown has elapsed: recover one rung.
    assert_eq!(h.observe(0.1), LadderMove::Up);
    assert_eq!(h.level(), 0);
}

#[test]
fn does_not_flap_on_oscillating_pressure_around_a_single_threshold() {
    // A signal hovering at one value (e.g. 0.85, inside the band) must NEVER
    // move the level — the hysteresis band defeats single-threshold flapping.
    let mut h = Hysteresis::new(config());
    h.observe(0.95); // level 1
    for _ in 0..100 {
        assert_eq!(h.observe(0.85), LadderMove::Hold);
    }
    assert_eq!(h.level(), 1);
}

#[test]
fn fast_oscillation_across_band_cannot_thrash_the_level() {
    // Alternating just-below-low / just-above-high readings would flap a naive
    // controller every tick. With the recovery cooldown, downs are prompt but
    // ups are gated, so the level cannot bounce 0<->1 each tick.
    let mut h = Hysteresis::new(config());
    let mut downs = 0_u32;
    let mut ups = 0_u32;
    let mut pressure = 0.95;
    for _ in 0..40 {
        match h.observe(pressure) {
            LadderMove::Down => downs += 1,
            LadderMove::Up => ups += 1,
            LadderMove::Hold => {}
        }
        // flip the pressure each tick.
        pressure = if pressure > 0.9 { 0.1 } else { 0.95 };
    }
    // A naive flapper would record ~20 downs and ~20 ups. The cooldown forces
    // far fewer recoveries than a per-tick bounce.
    assert!(
        ups < downs,
        "ups={ups} downs={downs} (cooldown must gate recovery)"
    );
    assert!(h.level() <= MAX_LEVEL);
}

#[test]
fn non_finite_pressure_never_moves_the_level() {
    let mut h = Hysteresis::new(config());
    h.observe(0.95); // level 1
    assert_eq!(h.observe(f64::NAN), LadderMove::Hold);
    assert_eq!(h.observe(f64::INFINITY), LadderMove::Hold);
    assert_eq!(h.level(), 1);
}

#[test]
fn invalid_hysteresis_config_is_rejected() {
    // low >= high.
    assert!(HysteresisConfig::try_new(0.9, 0.7, 3).is_err());
    assert!(HysteresisConfig::try_new(0.5, 0.5, 3).is_err());
    // out of range.
    assert!(HysteresisConfig::try_new(-0.1, 0.9, 3).is_err());
    assert!(HysteresisConfig::try_new(0.1, 1.5, 3).is_err());
    // non-finite.
    assert!(HysteresisConfig::try_new(f64::NAN, 0.9, 3).is_err());
    // a valid band is accepted.
    assert!(HysteresisConfig::try_new(0.7, 0.9, 3).is_ok());
}

#[test]
fn planner_next_step_drives_the_ladder_through_admission_seam() {
    let mut planner = Planner::new(CostBudget::new(100.0, 100.0, 100.0)).unwrap();
    assert_eq!(planner.degradation_level(), 0);
    // Drive pressure high a few ticks: the planner sheds.
    assert_eq!(planner.next_step(0.99), LadderMove::Down);
    assert_eq!(planner.next_step(0.99), LadderMove::Down);
    assert_eq!(planner.degradation_level(), 2);
}
