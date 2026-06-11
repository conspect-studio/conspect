//! Acceptance tests for perf-class weighting (Tier-2 gap P1b).
//!
//! These prove the inv-#1 guarantee the feature exists for: a weak GPU (a 2016
//! Pascal Quadro P2000) must resolve to a [`PerfClass`] whose **composite**
//! ceiling is *below* a 4K30 composite demand, so the existing budget gate
//! ([`multiview_hal::Planner::admit`]) rejects that GPU for a 4K composite while
//! it still admits a strong GPU (an Ada RTX 4060) — turning the previously inert
//! uniform-budget gate into a real cadence-sustain check.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_hal::capability::{Resolution, Stage};
use multiview_hal::cost::TileLoad;
use multiview_hal::perf::{PerfClass, PerfSignals, DEFAULT_PERF_CLASS};
use multiview_hal::planner::{Plan, Planner};

/// A 4K canvas composited at 30 fps, in megapixels/sec, computed by the SAME
/// `stage_load_mpps` the planner uses — not a hand-typed magic number.
fn composite_4k30_mpps() -> f64 {
    let plan = Plan::new(
        Rational::new(30, 1),
        vec![TileLoad::new(Stage::Composite, Resolution::UHD4K)],
    );
    plan.stage_load_mpps(Stage::Composite)
}

/// Resolve a perf class purely from a device name (no NVML signals): the
/// name-substring table path (signal priority 3).
fn by_name(name: &str) -> PerfClass {
    PerfClass::for_device(&PerfSignals::from_name(name))
}

#[test]
fn rtx_4060_admits_a_4k30_composite() {
    // The Ada 4060 is a strong GPU: its composite ceiling must EXCEED a 4K30
    // composite demand so the budget gate admits it.
    let demand = composite_4k30_mpps();
    let budget = by_name("NVIDIA GeForce RTX 4060").stage_budgets();
    assert!(
        budget.composite_mpps > demand,
        "4060 composite ceiling {} must exceed the 4K30 demand {demand}",
        budget.composite_mpps
    );

    // And the real budget gate must ADMIT a 4K30 composite-only plan.
    let planner = Planner::new(budget).expect("4060 budget is valid");
    let plan = Plan::new(
        Rational::new(30, 1),
        vec![TileLoad::new(Stage::Composite, Resolution::UHD4K)],
    );
    assert!(
        planner.admit(&plan).is_ok(),
        "the 4060 budget gate must admit a 4K30 composite"
    );
}

#[test]
fn p2000_composite_ceiling_is_below_4k30() {
    // The inv-#1 proof: the 2016 Pascal P2000 cannot sustain a 4K30 composite,
    // so its composite ceiling must be strictly BELOW the demand — and the real
    // budget gate must REJECT a 4K30 composite plan on it.
    let demand = composite_4k30_mpps();
    let budget = by_name("Quadro P2000").stage_budgets();
    assert!(
        budget.composite_mpps < demand,
        "P2000 composite ceiling {} must be below the 4K30 demand {demand} (inv #1)",
        budget.composite_mpps
    );

    let planner = Planner::new(budget).expect("P2000 budget is valid");
    let plan = Plan::new(
        Rational::new(30, 1),
        vec![TileLoad::new(Stage::Composite, Resolution::UHD4K)],
    );
    let admit = planner.admit(&plan);
    assert!(
        admit.is_err(),
        "the P2000 budget gate MUST reject a 4K30 composite (it cannot sustain cadence)"
    );
}

#[test]
fn case_insensitive_name_matching() {
    // The arch table is matched case-insensitively: lower/upper/mixed all hit.
    let a = by_name("quadro p2000").stage_budgets();
    let b = by_name("QUADRO P2000").stage_budgets();
    let c = by_name("Quadro P2000").stage_budgets();
    assert_eq!(a, b);
    assert_eq!(b, c);
}

#[test]
fn unknown_name_falls_back_to_a_finite_conservative_default() {
    // An unknown GPU must resolve to the conservative DEFAULT — never infinite
    // (the inv-#1 guard: an unknown GPU is never handed an unbounded budget and
    // thus never trusted to sustain an arbitrarily heavy stage).
    let pc = by_name("Some Unreleased GPU 9999");
    assert_eq!(pc, DEFAULT_PERF_CLASS);
    let budget = pc.stage_budgets();
    assert!(budget.decode_mpps.is_finite());
    assert!(budget.composite_mpps.is_finite());
    assert!(budget.encode_mpps.is_finite());
    // The default is a conservative ~1080p60 composite tier: it must NOT exceed
    // a 4K30 composite demand (a too-generous default would re-introduce the bug).
    assert!(
        budget.composite_mpps < composite_4k30_mpps(),
        "the conservative default must not claim it can sustain 4K30 composite"
    );
}

#[test]
fn cpu_software_tier_is_representable_but_weak() {
    // The software fallback tier exists (so a CPU target is representable) but is
    // expensive: its ceilings are the lowest of all and finite.
    let cpu = PerfClass::cpu();
    let budget = cpu.stage_budgets();
    assert!(budget.decode_mpps.is_finite() && budget.decode_mpps > 0.0);
    assert!(budget.composite_mpps.is_finite() && budget.composite_mpps > 0.0);
    assert!(budget.encode_mpps.is_finite() && budget.encode_mpps > 0.0);
    // The CPU tier composite ceiling is below the conservative GPU default.
    assert!(
        budget.composite_mpps < DEFAULT_PERF_CLASS.stage_budgets().composite_mpps,
        "the CPU tier must be weaker than the conservative GPU default"
    );
}

#[test]
fn nvml_cores_clock_path_scales_from_the_anchor() {
    // Signal priority 1: when NVML reports cores x clock, the class scales
    // linearly from the calibrated anchor. The anchor itself (the 4060's
    // cores x clock) must reproduce ~the 4060 table ceilings.
    let anchor = PerfClass::for_device(&PerfSignals::from_nvml(
        Some("NVIDIA GeForce RTX 4060"),
        Some(3072),
        Some(2460),
        None,
    ));
    let table = by_name("NVIDIA GeForce RTX 4060");
    let a = anchor.stage_budgets();
    let t = table.stage_budgets();
    assert!(
        (a.composite_mpps - t.composite_mpps).abs() < 1.0,
        "the anchor cores x clock must reproduce the 4060 composite ceiling: anchor={} table={}",
        a.composite_mpps,
        t.composite_mpps
    );

    // A P2000-class cores x clock (1024 x ~1480) scaled from the anchor must also
    // land BELOW the 4K30 composite line (the NVML path agrees with the table).
    let p2000_like = PerfClass::for_device(&PerfSignals::from_nvml(
        Some("Quadro P2000"),
        Some(1024),
        Some(1480),
        None,
    ));
    assert!(
        p2000_like.stage_budgets().composite_mpps < composite_4k30_mpps(),
        "a Pascal-class cores x clock must scale below 4K30 composite"
    );
}

#[test]
fn cores_clock_outranks_a_richer_signal_monotonically() {
    // A strictly-higher (cores x clock) device never yields a strictly-lower
    // per-stage ceiling than a weaker one (monotonicity of the scaling).
    let strong = PerfClass::for_device(&PerfSignals::from_nvml(
        Some("X"),
        Some(4096),
        Some(2500),
        None,
    ));
    let weak = PerfClass::for_device(&PerfSignals::from_nvml(
        Some("X"),
        Some(1024),
        Some(1400),
        None,
    ));
    let s = strong.stage_budgets();
    let w = weak.stage_budgets();
    assert!(s.decode_mpps >= w.decode_mpps);
    assert!(s.composite_mpps >= w.composite_mpps);
    assert!(s.encode_mpps >= w.encode_mpps);
}

#[test]
fn architecture_fallback_when_no_cores_or_name_match() {
    // Signal priority 2: no usable cores x clock and an unknown name, but a known
    // architecture string resolves a coarse per-generation tier (finite, never
    // infinite). Ada (the 4060's gen) outranks Pascal (the P2000's gen).
    let ada = PerfClass::for_device(&PerfSignals::from_nvml(
        Some("Totally Unlisted Card"),
        None,
        None,
        Some("Ada"),
    ));
    let pascal = PerfClass::for_device(&PerfSignals::from_nvml(
        Some("Totally Unlisted Card"),
        None,
        None,
        Some("Pascal"),
    ));
    assert!(ada.stage_budgets().composite_mpps.is_finite());
    assert!(pascal.stage_budgets().composite_mpps.is_finite());
    assert!(
        ada.stage_budgets().composite_mpps > pascal.stage_budgets().composite_mpps,
        "Ada must outrank Pascal in the architecture fallback"
    );
    // Pascal's coarse tier composite ceiling is below 4K30 (consistent with the
    // P2000 being unable to sustain it).
    assert!(pascal.stage_budgets().composite_mpps < composite_4k30_mpps());
}
