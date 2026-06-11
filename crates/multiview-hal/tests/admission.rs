#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: empty-plan utilization is exactly 0.0 by construction (0.0/0.0
    // path returns a literal 0.0), so an exact comparison is correct here.
    clippy::float_cmp
)]
//! Admission control: accept / reject a proposed plan at budget boundaries,
//! in megapixels/sec, per engine.

use multiview_core::time::Rational;
use multiview_hal::{CostBudget, Error, Plan, Planner, Resolution, Stage, TileLoad};

// 1080p = 1920x1080 = 2_073_600 px = 2.0736 Mpix; at 30 fps = 62.208 Mpix/s.
const ONE_1080P_30_MPPS: f64 = 62.208;

fn planner_with(decode: f64, composite: f64, encode: f64) -> Planner {
    Planner::new(CostBudget::new(decode, composite, encode)).unwrap()
}

#[test]
fn tile_load_is_resolution_megapixels_times_fps() {
    let load = TileLoad::new(Stage::Decode, Resolution::HD1080);
    let mpps = load.megapixels_per_sec(Rational::FPS_30);
    assert!(
        (mpps - ONE_1080P_30_MPPS).abs() < 1e-9,
        "expected {ONE_1080P_30_MPPS}, got {mpps}"
    );
}

#[test]
fn ntsc_cadence_uses_exact_rational_not_float() {
    // 29.97 == 30000/1001; 1080p tile -> 2.0736 * (30000/1001) Mpix/s.
    let load = TileLoad::new(Stage::Decode, Resolution::HD1080);
    let mpps = load.megapixels_per_sec(Rational::FPS_29_97);
    let expected = 2.0736 * (30000.0 / 1001.0);
    assert!((mpps - expected).abs() < 1e-6, "got {mpps}");
    // And it is strictly below the integer-30 figure (proving 1001 was used).
    assert!(mpps < ONE_1080P_30_MPPS);
}

#[test]
fn plan_exactly_at_budget_is_admitted() {
    // Two 1080p30 decode tiles == 124.416 Mpix/s decode load.
    let plan = Plan::new(
        Rational::FPS_30,
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD1080),
            TileLoad::new(Stage::Decode, Resolution::HD1080),
        ],
    );
    let budget = 2.0 * ONE_1080P_30_MPPS;
    let planner = planner_with(budget, 1000.0, 1000.0);

    let admission = planner.admit(&plan).expect("at-budget plan must admit");
    // The decode stage is at exactly 100% utilization.
    let util = admission.for_stage(Stage::Decode).utilization();
    assert!((util - 1.0).abs() < 1e-9, "utilization {util}");
}

#[test]
fn plan_one_epsilon_over_budget_is_rejected_on_the_offending_stage() {
    let plan = Plan::new(
        Rational::FPS_30,
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD1080),
            TileLoad::new(Stage::Decode, Resolution::HD1080),
        ],
    );
    // Budget just below the 2-tile requirement.
    let budget = 2.0 * ONE_1080P_30_MPPS - 0.001;
    let planner = planner_with(budget, 1000.0, 1000.0);

    let err = planner.admit(&plan).unwrap_err();
    match err {
        Error::BudgetExceeded {
            stage,
            requested_mpps,
            budget_mpps,
        } => {
            assert_eq!(stage, Stage::Decode);
            assert!(requested_mpps > budget_mpps);
            assert!((budget_mpps - budget).abs() < 1e-9);
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
}

#[test]
fn each_engine_is_budgeted_independently() {
    // Decode + composite fit, but encode is over its own budget.
    let plan = Plan::new(
        Rational::FPS_30,
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD720),
            TileLoad::new(Stage::Composite, Resolution::HD1080),
            TileLoad::new(Stage::Encode, Resolution::UHD4K),
        ],
    );
    // Generous decode/composite, tiny encode budget.
    let planner = planner_with(1000.0, 1000.0, 1.0);

    let err = planner.admit(&plan).unwrap_err();
    match err {
        Error::BudgetExceeded { stage, .. } => assert_eq!(stage, Stage::Encode),
        other => panic!("expected encode BudgetExceeded, got {other:?}"),
    }

    // Raising only the encode budget admits the same plan.
    let planner = planner_with(1000.0, 1000.0, 1000.0);
    assert!(planner.admit(&plan).is_ok());
}

#[test]
fn empty_plan_admits_with_zero_utilization() {
    let plan = Plan::new(Rational::FPS_30, vec![]);
    let planner = planner_with(0.0, 0.0, 0.0);
    let admission = planner.admit(&plan).expect("empty plan must admit");
    assert_eq!(admission.for_stage(Stage::Decode).utilization(), 0.0);
}

#[test]
fn zero_budget_rejects_any_nonzero_load() {
    let plan = Plan::new(
        Rational::FPS_30,
        vec![TileLoad::new(Stage::Decode, Resolution::HD720)],
    );
    let planner = planner_with(0.0, 1000.0, 1000.0);
    let usage = planner.evaluate(&plan).for_stage(Stage::Decode);
    assert!(usage.utilization().is_infinite());
    assert!(!usage.fits());
    assert!(planner.admit(&plan).is_err());
}

#[test]
fn larger_tiles_cost_proportionally_more() {
    // A 4K tile must cost ~4x a 1080p tile (3840*2160 / (1920*1080) = 4).
    let cadence = Rational::FPS_30;
    let uhd = TileLoad::new(Stage::Decode, Resolution::UHD4K).megapixels_per_sec(cadence);
    let hd = TileLoad::new(Stage::Decode, Resolution::HD1080).megapixels_per_sec(cadence);
    assert!((uhd / hd - 4.0).abs() < 1e-9, "ratio {}", uhd / hd);
}
