//! Off-hot-path placement-controller tests (GPU-5b, ADR-0018 §4).
//!
//! Prove the closed-loop re-placement contract over **injected** `DeviceLoad`
//! sequences — pure, deterministic, no hardware:
//!
//! - a **transient** spike never migrates while a **sustained** overload does
//!   (EWMA + dwell);
//! - a **pin** always wins — a pinned pipeline never migrates off its device;
//! - a running pipeline is never fragmented unless no single GPU fits (a split
//!   is only ever proposed after re-selection rejects);
//! - anti-storm damping (cooldown + per-GPU budget + min-gain) bounds migration
//!   frequency;
//! - on a single-GPU host the controller never migrates (zero behaviour change);
//! - the controller only *proposes* — `observe` is a synchronous pure call that
//!   returns a value and cannot stall the engine (inv #1/#10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_core::pixel::PixelFormat;
use multiview_core::time::Rational;
use multiview_core::traits::BackendKind;
use multiview_engine::{
    MigrationPlan, PlacementController, PlacementControllerConfig, PlacementProposal, ShedReason,
};
use multiview_hal::cost::{CostBudget, TileLoad};
use multiview_hal::load::{DeviceId, DeviceLoad, Vendor};
use multiview_hal::select::{GpuCandidate, Pins, PipelineDemand, StageCaps};
use multiview_hal::{Capability, Resolution, Stage};

fn nv(id: &str, index: u32) -> DeviceId {
    DeviceId::new(Vendor::Nvidia, id, index)
}

fn cadence() -> Rational {
    Rational::new(30, 1)
}

/// A capability supporting up to 4K NV12 on a stage.
fn cap(stage: Stage) -> Capability {
    Capability::new(
        BackendKind::Cuda,
        stage,
        Resolution::UHD4K,
        vec![PixelFormat::Nv12],
    )
}

fn full_caps() -> StageCaps {
    StageCaps::new(
        cap(Stage::Decode),
        cap(Stage::Composite),
        cap(Stage::Encode),
    )
}

fn generous_budget() -> CostBudget {
    CostBudget::new(1000.0, 1000.0, 1000.0)
}

/// A small 1080p single-tile pipeline at 30 fps that opens an encode session.
fn demand_1080p() -> PipelineDemand {
    PipelineDemand::new(
        cadence(),
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD1080),
            TileLoad::new(Stage::Composite, Resolution::HD1080),
            TileLoad::new(Stage::Encode, Resolution::HD1080),
        ],
        Resolution::HD1080,
        PixelFormat::Nv12,
        1_000_000,
        true,
    )
}

fn candidate(id: &str, index: u32) -> GpuCandidate {
    GpuCandidate {
        device_id: nv(id, index),
        stage_caps: full_caps(),
        budget: generous_budget(),
    }
}

/// A load snapshot with a given VRAM used/total (the dominant share signal).
fn load_vram(id: &str, index: u32, used: u64, total: u64) -> DeviceLoad {
    let mut load = DeviceLoad::unknown(nv(id, index));
    load.vram_used_bytes = Some(used);
    load.vram_total_bytes = Some(total);
    load
}

/// VRAM fraction `frac` (in hundredths, `0..=100`) of a 12 GB card, as a byte
/// pair. Integer math only — no float `as` cast (a denied conversion).
fn vram_pct(id: &str, index: u32, pct: u64) -> DeviceLoad {
    let total = 12_000_000_000_u64;
    let used = total.saturating_mul(pct.min(100)) / 100;
    load_vram(id, index, used, total)
}

/// A config tuned for fast, deterministic tests: a 2-tick dwell, light EWMA
/// smoothing so a sustained high resolves quickly, no cooldown unless a test
/// sets one, generous per-GPU budget unless a test tightens it.
fn test_config() -> PlacementControllerConfig {
    let mut c = PlacementControllerConfig::new_default();
    c.ewma_alpha = 0.6; // resolve a sustained high within a couple of ticks
    c.dwell_ticks = 2;
    c.migration_cooldown_ticks = 0;
    c.per_gpu_budget = 10;
    c.budget_window_ticks = 1000;
    c.min_gain = 0.1;
    c
}

#[test]
fn no_overload_holds_forever() {
    // Two idle GPUs; the controlled pipeline on GPU-a. A long run of low load
    // must never propose anything but Hold.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    let loads = vec![vram_pct("GPU-a", 0, 20), vram_pct("GPU-b", 1, 20)];
    for _ in 0..50 {
        assert_eq!(ctl.observe(&loads), PlacementProposal::Hold);
    }
    assert_eq!(ctl.current_device(), &nv("GPU-a", 0));
}

#[test]
fn transient_spike_never_migrates() {
    // GPU-a is normally idle; a SINGLE-tick spike to 0.99 then back to idle must
    // be absorbed by the EWMA + dwell and never trigger a migration.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    let idle = vec![vram_pct("GPU-a", 0, 15), vram_pct("GPU-b", 1, 15)];
    let spike = vec![vram_pct("GPU-a", 0, 99), vram_pct("GPU-b", 1, 15)];

    // Warm up idle.
    for _ in 0..5 {
        assert_eq!(ctl.observe(&idle), PlacementProposal::Hold);
    }
    // One spike tick, then idle resumes.
    let p = ctl.observe(&spike);
    assert_eq!(
        p,
        PlacementProposal::Hold,
        "a single spike is not sustained"
    );
    for _ in 0..10 {
        assert_eq!(ctl.observe(&idle), PlacementProposal::Hold);
    }
    assert_eq!(
        ctl.current_device(),
        &nv("GPU-a", 0),
        "a transient never moves the pipeline"
    );
}

#[test]
fn sustained_overload_with_a_better_home_migrates() {
    // GPU-a stays pinned-high (0.95) for many ticks while GPU-b is idle (0.15):
    // a sustained overload with a materially-better home must MIGRATE to GPU-b,
    // make-before-break, IDR-aligned.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    let hot = vec![vram_pct("GPU-a", 0, 95), vram_pct("GPU-b", 1, 15)];

    let mut migrated = None;
    for _ in 0..10 {
        if let PlacementProposal::Migrate(plan) = ctl.observe(&hot) {
            migrated = Some(plan);
            break;
        }
    }
    let MigrationPlan {
        from,
        to,
        gain,
        idr_aligned,
        ..
    } = migrated.expect("a sustained overload with a better home migrates");
    assert_eq!(from, nv("GPU-a", 0));
    assert_eq!(to, nv("GPU-b", 1));
    assert!(
        gain >= 0.1,
        "the migration clears the min-gain gate: {gain}"
    );
    assert!(idr_aligned, "the cutover is IDR-aligned make-before-break");
    // The controller now tracks the new home.
    assert_eq!(ctl.current_device(), &nv("GPU-b", 1));
}

#[test]
fn sustained_overload_without_a_better_home_sheds() {
    // BOTH GPUs are equally hot (0.92): there is no materially-better home, so a
    // sustained overload must SHED locally (the imbalance can't be cured by
    // moving) — never migrate into an equally-loaded GPU.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    // Both over the headroom ceiling -> select_device rejects the other GPU ->
    // no better home -> shed.
    let hot = vec![vram_pct("GPU-a", 0, 92), vram_pct("GPU-b", 1, 92)];

    let mut shed = false;
    for _ in 0..10 {
        match ctl.observe(&hot) {
            PlacementProposal::Shed {
                reason: ShedReason::NoBetterHome,
            } => {
                shed = true;
                break;
            }
            PlacementProposal::Migrate(_) => panic!("must not migrate into an equally-hot GPU"),
            _ => {}
        }
    }
    assert!(shed, "a whole-host overload sheds locally");
    assert_eq!(
        ctl.current_device(),
        &nv("GPU-a", 0),
        "no migration occurred"
    );
}

#[test]
fn a_pin_always_wins_never_migrates() {
    // GPU-a is pinned AND overloaded while GPU-b is idle. The pin is absolute:
    // the pipeline must SHED (reason Pinned), never migrate off the pinned GPU.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::pin_pipeline(nv("GPU-a", 0)),
        nv("GPU-a", 0),
    );
    let hot = vec![vram_pct("GPU-a", 0, 97), vram_pct("GPU-b", 1, 10)];

    let mut saw_pinned_shed = false;
    for _ in 0..15 {
        match ctl.observe(&hot) {
            PlacementProposal::Migrate(_) => panic!("a pinned pipeline must never migrate"),
            PlacementProposal::Shed {
                reason: ShedReason::Pinned,
            } => saw_pinned_shed = true,
            _ => {}
        }
    }
    assert!(
        saw_pinned_shed,
        "the pin forces a local shed, not a migration"
    );
    assert_eq!(ctl.current_device(), &nv("GPU-a", 0));
}

#[test]
fn anti_storm_cooldown_bounds_migration_frequency() {
    // After a migration, a cooldown must forbid an immediate second migration
    // even under continued overload — the pipeline sheds during the cooldown.
    let mut config = test_config();
    config.migration_cooldown_ticks = 20;
    let candidates = vec![
        candidate("GPU-a", 0),
        candidate("GPU-b", 1),
        candidate("GPU-c", 2),
    ];
    let mut ctl = PlacementController::new(
        config,
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    // GPU-a hot, GPU-b idle (the first migration target), GPU-c idle.
    let hot_a = vec![
        vram_pct("GPU-a", 0, 95),
        vram_pct("GPU-b", 1, 15),
        vram_pct("GPU-c", 2, 15),
    ];

    // Drive to the first migration onto GPU-b.
    let mut first = None;
    for _ in 0..10 {
        if let PlacementProposal::Migrate(p) = ctl.observe(&hot_a) {
            first = Some(p);
            break;
        }
    }
    assert!(first.is_some(), "first migration happens");
    assert_eq!(ctl.current_device(), &nv("GPU-b", 1));

    // Now GPU-b (the new home) goes hot while GPU-c stays idle: a second
    // migration is justified by load, but the cooldown must suppress it — the
    // controller sheds instead for the cooldown window.
    let hot_b = vec![
        vram_pct("GPU-a", 0, 15),
        vram_pct("GPU-b", 1, 96),
        vram_pct("GPU-c", 2, 15),
    ];
    let mut saw_antistorm_shed = false;
    for _ in 0..10 {
        match ctl.observe(&hot_b) {
            PlacementProposal::Migrate(_) => {
                panic!("the cooldown must suppress an immediate second migration")
            }
            PlacementProposal::Shed {
                reason: ShedReason::AntiStorm,
            } => saw_antistorm_shed = true,
            _ => {}
        }
    }
    assert!(
        saw_antistorm_shed,
        "during cooldown a better home is suppressed -> AntiStorm shed"
    );
    assert_eq!(ctl.current_device(), &nv("GPU-b", 1), "still on GPU-b");
}

#[test]
fn per_gpu_budget_bounds_migrations_into_one_gpu() {
    // With a per-GPU budget of 1 over a wide window, a GPU that has already been
    // a migration target once cannot be a target again — the controller sheds
    // (AntiStorm) rather than re-targeting the budget-exhausted GPU.
    let mut config = test_config();
    config.migration_cooldown_ticks = 0;
    config.per_gpu_budget = 1;
    config.budget_window_ticks = 10_000;
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        config,
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    // First: a->b migration (GPU-a hot, GPU-b idle).
    let hot_a = vec![vram_pct("GPU-a", 0, 95), vram_pct("GPU-b", 1, 12)];
    let mut migrated = false;
    for _ in 0..10 {
        if let PlacementProposal::Migrate(p) = ctl.observe(&hot_a) {
            assert_eq!(p.to, nv("GPU-b", 1));
            migrated = true;
            break;
        }
    }
    assert!(migrated, "first migration onto GPU-b");

    // Now GPU-b goes hot and GPU-a is idle again — the only candidate is GPU-a,
    // but both GPU-a and GPU-b have each already been touched once, exhausting
    // the budget=1. A migration back is suppressed -> AntiStorm shed.
    let hot_b = vec![vram_pct("GPU-a", 0, 12), vram_pct("GPU-b", 1, 96)];
    let mut suppressed = false;
    for _ in 0..10 {
        match ctl.observe(&hot_b) {
            PlacementProposal::Migrate(_) => panic!("the per-GPU budget must suppress this move"),
            PlacementProposal::Shed {
                reason: ShedReason::AntiStorm,
            } => suppressed = true,
            _ => {}
        }
    }
    assert!(
        suppressed,
        "budget-exhausted GPUs suppress further migration"
    );
}

#[test]
fn min_gain_gate_suppresses_a_marginal_migration() {
    // GPU-a is over the band (sustained overload raised) at 0.91, but GPU-b is
    // only marginally better at 0.88 — below the 0.1 min-gain. The controller
    // must NOT migrate for a marginal improvement; it sheds (NoBetterHome).
    let mut config = test_config();
    config.min_gain = 0.1;
    // Raise the headroom ceiling so the marginally-better GPU-b is still a viable
    // (non-rejected) candidate; the min-gain gate is what must suppress the move.
    config.select_policy.headroom_ceiling = 0.95;
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        config,
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    let hot = vec![vram_pct("GPU-a", 0, 91), vram_pct("GPU-b", 1, 88)];

    let mut saw_no_better = false;
    for _ in 0..12 {
        match ctl.observe(&hot) {
            PlacementProposal::Migrate(_) => panic!("a sub-min-gain move must be suppressed"),
            PlacementProposal::Shed {
                reason: ShedReason::NoBetterHome,
            } => saw_no_better = true,
            _ => {}
        }
    }
    assert!(
        saw_no_better,
        "a marginal home does not justify a migration"
    );
}

#[test]
fn single_gpu_host_never_migrates() {
    // One GPU only. Even under a long sustained overload there is no other
    // candidate to migrate to: the controller may shed but never migrates —
    // zero placement behaviour beyond the degradation loop (ADR-0018 consequence).
    let candidates = vec![candidate("GPU-solo", 0)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-solo", 0),
    );
    let hot = vec![vram_pct("GPU-solo", 0, 97)];
    for _ in 0..30 {
        match ctl.observe(&hot) {
            PlacementProposal::Migrate(_) => panic!("a single-GPU host can never migrate"),
            PlacementProposal::Split(_) => panic!("a single-GPU host can never split"),
            _ => {}
        }
    }
    assert_eq!(ctl.current_device(), &nv("GPU-solo", 0));
}

#[test]
fn observe_is_a_pure_synchronous_value_call() {
    // Inv #1/#10 re-assert: the controller's step is a plain synchronous function
    // returning a value — there is no async, no channel, no lock in the public
    // surface. Calling it in a tight loop with adversarial loads can never block
    // (this test would deadlock/hang if `observe` ever awaited). We assert it
    // returns promptly across a hostile oscillating-load sequence.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    // A pathological oscillator: alternate which GPU is hot every tick. The
    // anti-flap EWMA/dwell must keep this from storming, and every call returns.
    for i in 0..1000 {
        let loads = if i % 2 == 0 {
            vec![vram_pct("GPU-a", 0, 99), vram_pct("GPU-b", 1, 5)]
        } else {
            vec![vram_pct("GPU-a", 0, 5), vram_pct("GPU-b", 1, 99)]
        };
        // Just exercising the call; the value is unimportant, the point is it
        // returns synchronously without blocking.
        let _ = ctl.observe(&loads);
    }
    // The current device is always a real candidate (never fragmented to nowhere).
    let here = ctl.current_device().clone();
    assert!(here == nv("GPU-a", 0) || here == nv("GPU-b", 1));
}

#[test]
fn unknown_load_for_the_current_device_never_fabricates_overload() {
    // The current device reports NO usable signal (a fully-blind probe). A blind
    // device must never fabricate an overload, so the controller holds.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    let blind = vec![
        DeviceLoad::unknown(nv("GPU-a", 0)),
        vram_pct("GPU-b", 1, 15),
    ];
    for _ in 0..30 {
        assert_eq!(
            ctl.observe(&blind),
            PlacementProposal::Hold,
            "a blind current device never fabricates an overload"
        );
    }
}
