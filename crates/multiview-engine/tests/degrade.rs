//! Admission + degradation control-loop tests — invariant #9.
//!
//! Prove the ladder sheds in the documented cheapest-impact-first order, that
//! tile-only rungs are exhausted BEFORE the program output everyone sees is
//! touched, that program-output validity is preserved under simulated overload,
//! and that hysteresis prevents oscillation (no flapping).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_core::time::Rational;
use multiview_engine::ControlLoop;
use multiview_hal::cost::TileLoad;
use multiview_hal::degradation::{DegradationAction, LadderMove, MAX_LEVEL};
use multiview_hal::planner::Plan;
use multiview_hal::{CostBudget, HysteresisConfig, Resolution, Stage};
use proptest::prelude::*;

fn loop_with_budget() -> ControlLoop {
    // Generous budget so admission succeeds; pressure is driven explicitly.
    ControlLoop::new(CostBudget::new(2000.0, 2000.0, 2000.0)).unwrap()
}

#[test]
fn admission_rejects_an_over_budget_plan() {
    // A tiny decode budget; a 4K@60 tile blows it.
    let cl = ControlLoop::new(CostBudget::new(1.0, 2000.0, 2000.0)).unwrap();
    let plan = Plan::new(
        Rational::FPS_60,
        vec![TileLoad::new(Stage::Decode, Resolution::UHD4K)],
    );
    assert!(
        cl.admit(&plan).is_err(),
        "over-budget decode must be rejected"
    );
}

#[test]
fn admission_accepts_a_fitting_plan() {
    let cl = loop_with_budget();
    let plan = Plan::new(
        Rational::FPS_30,
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD1080),
            TileLoad::new(Stage::Composite, Resolution::HD1080),
            TileLoad::new(Stage::Encode, Resolution::HD1080),
        ],
    );
    assert!(cl.admit(&plan).is_ok());
}

#[test]
fn ladder_sheds_in_documented_cheapest_impact_first_order() {
    let mut cl = loop_with_budget();
    assert_eq!(cl.level(), 0, "starts at full quality");

    // Sustained high pressure -> step down one rung per tick, in LADDER order.
    let mut applied = Vec::new();
    for _ in 0..MAX_LEVEL {
        let step = cl.step(1.0); // well above the `high` threshold (0.9)
        assert_eq!(step.mv, LadderMove::Down);
        // The newest action is the last entry of the active set.
        applied.push(*step.active.last().unwrap());
    }
    assert_eq!(
        applied,
        DegradationAction::LADDER.to_vec(),
        "actions applied in exactly the documented order"
    );
    // At MAX_LEVEL, more pressure holds (nothing left to shed).
    assert_eq!(cl.step(1.0).mv, LadderMove::Hold);
    assert_eq!(cl.level(), MAX_LEVEL);
}

#[test]
fn tiles_are_shed_before_program_output_is_touched() {
    let mut cl = loop_with_budget();
    // The first three rungs (tile resolution, tile fps, simpler scaler) are
    // tile/shared-only; `affects_program()` must be false until rung 4.
    for expected_rung in 0..MAX_LEVEL {
        let step = cl.step(1.0);
        let crosses_program = DegradationAction::LADDER[expected_rung].affects_program();
        assert_eq!(
            step.affects_program(),
            // Once we have applied any program-affecting rung it stays true.
            DegradationAction::LADDER[..=expected_rung]
                .iter()
                .any(|a| a.affects_program())
        );
        if expected_rung < 3 {
            assert!(
                !step.affects_program(),
                "rung {expected_rung} must NOT affect program output yet"
            );
            assert!(!crosses_program);
        }
    }
}

#[test]
fn hysteresis_prevents_oscillation() {
    // Tight cooldown so the test is short but still asserts the anti-flap dwell.
    let cfg = HysteresisConfig::try_new(0.7, 0.9, 5).unwrap();
    let mut cl = ControlLoop::with_hysteresis(CostBudget::new(100.0, 100.0, 100.0), cfg).unwrap();

    // One spike sheds one rung.
    assert_eq!(cl.step(0.95).mv, LadderMove::Down);
    assert_eq!(cl.level(), 1);

    // Immediately-low readings must NOT recover during the cooldown window: a
    // naive controller would flap Down/Up/Down. Ours holds.
    for tick in 0..5 {
        let mv = cl.step(0.1).mv;
        assert_eq!(
            mv,
            LadderMove::Hold,
            "must hold during cooldown (tick {tick})"
        );
        assert_eq!(cl.level(), 1);
    }
    // After the dwell elapses, a low reading finally recovers ONE rung.
    assert_eq!(cl.step(0.1).mv, LadderMove::Up);
    assert_eq!(cl.level(), 0);

    // Pressure inside the band (between low and high) always holds — no flap.
    cl.step(0.95); // down to 1
    for _ in 0..20 {
        assert_eq!(cl.step(0.8).mv, LadderMove::Hold);
    }
}

#[test]
fn program_output_validity_is_preserved_throughout_overload() {
    // Under sustained overload the controller may climb the whole ladder, but
    // the active action set is always a valid prefix of LADDER and the level is
    // always within bounds — i.e. the plan never becomes nonsensical, so program
    // output stays producible (the drive loop always has a valid plan to honour).
    let mut cl = loop_with_budget();
    for _ in 0..1000 {
        let step = cl.step(1.0);
        assert!(step.level <= MAX_LEVEL);
        // active is the level-length prefix of the ladder.
        assert_eq!(step.active.len(), step.level);
        assert_eq!(step.active, &DegradationAction::LADDER[..step.level]);
    }
    assert_eq!(cl.level(), MAX_LEVEL);
}

#[test]
fn pressure_is_derived_from_worst_stage_utilization() {
    // decode budget 10, composite/encode 1000; a plan that loads decode to 50%
    // and composite to 5% -> worst utilization is decode's 0.5.
    let cl = ControlLoop::new(CostBudget::new(10.0, 1000.0, 1000.0)).unwrap();
    // HD1080 @ 30 == 2.0736 Mpix * 30 ~= 62.2 Mpix/s. Decode budget 10 -> >1,
    // clamps to 1.0 (saturated).
    let plan = Plan::new(
        Rational::FPS_30,
        vec![TileLoad::new(Stage::Decode, Resolution::HD1080)],
    );
    let p = cl.pressure_from_plan(&plan);
    assert!((0.0..=1.0).contains(&p));
    assert_eq!(p, 1.0, "over-budget decode saturates pressure");

    // A light plan yields proportional, in-range pressure.
    let cl2 = ControlLoop::new(CostBudget::new(1000.0, 1000.0, 1000.0)).unwrap();
    let light = Plan::new(
        Rational::FPS_30,
        vec![TileLoad::new(Stage::Decode, Resolution::HD720)],
    );
    let p2 = cl2.pressure_from_plan(&light);
    assert!(p2 > 0.0 && p2 < 0.1, "light load -> low pressure, got {p2}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// For ANY sequence of pressure readings, the level never leaves bounds and
    /// the active set is always a valid ladder prefix — the controller can never
    /// produce a corrupt plan no matter how noisy the signal.
    #[test]
    fn prop_level_always_bounded_and_prefix(
        readings in prop::collection::vec(0.0_f64..=1.0, 1..200)
    ) {
        let mut cl = loop_with_budget();
        for r in readings {
            let step = cl.step(r);
            prop_assert!(step.level <= MAX_LEVEL);
            prop_assert_eq!(step.active.len(), step.level);
            prop_assert_eq!(step.active, &DegradationAction::LADDER[..step.level]);
        }
    }

    /// A non-finite pressure reading (bad sensor) never moves the plan.
    #[test]
    fn prop_bad_sensor_reading_holds(level_seed in 0_usize..=MAX_LEVEL) {
        let mut cl = loop_with_budget();
        // Climb to an arbitrary level first.
        for _ in 0..level_seed {
            cl.step(1.0);
        }
        let before = cl.level();
        let step = cl.step(f64::NAN);
        prop_assert_eq!(step.mv, LadderMove::Hold);
        prop_assert_eq!(cl.level(), before);
    }
}
