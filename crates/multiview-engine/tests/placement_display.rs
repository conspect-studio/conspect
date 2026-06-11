//! Display-bound scanout-affinity placement tests (DEV-B2, ADR-0044 §3 /
//! display-out.md §3).
//!
//! Prove the **hard** scanout-affinity constraint over **injected** `DeviceLoad`
//! sequences — pure, deterministic, no hardware:
//!
//! - a pipeline whose composite feeds a local display sink is **affinity-pinned
//!   to the connector-owning GPU**: under a sustained overload that would
//!   otherwise migrate, the controller MUST refuse to migrate (it sheds locally
//!   instead), because moving composite off the scanout GPU would force the
//!   per-frame GPU→host→GPU copy ADR-0018 forbids;
//! - it likewise never splits the composite off the display GPU;
//! - on a single-GPU host the constraint is trivially satisfied (the display
//!   device IS the only device), and behaviour is unchanged;
//! - the constraint is distinct from an operator pin — it carries its own shed
//!   reason — and composite can still shed locally on the display GPU.
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
    PlacementController, PlacementControllerConfig, PlacementProposal, ShedReason,
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

/// A small 1080p single-tile pipeline at 30 fps. `locality` is the scanout
/// affinity set the display sink declares (empty = no display sink).
fn demand_1080p(locality: Vec<DeviceId>) -> PipelineDemand {
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
    .with_sink_locality(locality)
}

fn candidate(id: &str, index: u32) -> GpuCandidate {
    GpuCandidate {
        device_id: nv(id, index),
        stage_caps: full_caps(),
        budget: generous_budget(),
    }
}

fn load_vram(id: &str, index: u32, used: u64, total: u64) -> DeviceLoad {
    let mut load = DeviceLoad::unknown(nv(id, index));
    load.vram_used_bytes = Some(used);
    load.vram_total_bytes = Some(total);
    load
}

/// VRAM fraction `pct` (`0..=100`) of a 12 GB card. Integer math only.
fn vram_pct(id: &str, index: u32, pct: u64) -> DeviceLoad {
    let total = 12_000_000_000_u64;
    let used = total.saturating_mul(pct.min(100)) / 100;
    load_vram(id, index, used, total)
}

fn test_config() -> PlacementControllerConfig {
    let mut c = PlacementControllerConfig::new_default();
    c.ewma_alpha = 0.6;
    c.dwell_ticks = 2;
    c.migration_cooldown_ticks = 0;
    c.per_gpu_budget = 10;
    c.budget_window_ticks = 1000;
    c.min_gain = 0.1;
    c
}

#[test]
fn display_bound_pipeline_never_migrates_off_the_scanout_gpu() {
    // THE load-bearing test. Two GPUs: GPU-display owns the connector and runs
    // the composite that scans out; GPU-spare is idle. GPU-display is driven to a
    // SUSTAINED overload that — absent the affinity constraint — would migrate the
    // island to GPU-spare. Because the pipeline is display-bound to GPU-display,
    // the controller MUST refuse to migrate (a migration would move composite off
    // the scanout GPU, forcing the per-frame GPU->host->GPU copy ADR-0018 forbids)
    // and shed locally instead, with the dedicated DisplayBound reason.
    let candidates = vec![candidate("GPU-display", 0), candidate("GPU-spare", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(vec![nv("GPU-display", 0)]),
        candidates,
        Pins::none(),
        nv("GPU-display", 0),
    );
    // GPU-display pinned-hot, GPU-spare idle: a sustained overload with a clearly
    // better home — the move is suppressed ONLY by the display affinity.
    let hot = vec![vram_pct("GPU-display", 0, 96), vram_pct("GPU-spare", 1, 12)];

    let mut saw_display_bound_shed = false;
    for _ in 0..20 {
        match ctl.observe(&hot) {
            PlacementProposal::Migrate(_) => {
                panic!("a display-bound pipeline must NEVER migrate off its scanout GPU")
            }
            PlacementProposal::Split(_) => {
                panic!("a display-bound pipeline must NEVER split composite off its scanout GPU")
            }
            PlacementProposal::Shed {
                reason: ShedReason::DisplayBound,
            } => saw_display_bound_shed = true,
            _ => {}
        }
    }
    assert!(
        saw_display_bound_shed,
        "the scanout affinity forces a local shed, never a migration"
    );
    assert_eq!(
        ctl.current_device(),
        &nv("GPU-display", 0),
        "composite stays on the connector-owning GPU"
    );
}

#[test]
fn display_bound_is_reported_by_the_controller() {
    // The controller exposes that it is display-bound and to which device, so the
    // control plane / telemetry can surface the hard affinity.
    let candidates = vec![candidate("GPU-display", 0), candidate("GPU-spare", 1)];
    let ctl = PlacementController::new(
        test_config(),
        demand_1080p(vec![nv("GPU-display", 0)]),
        candidates,
        Pins::none(),
        nv("GPU-display", 0),
    );
    assert!(ctl.is_display_bound_here());

    // A pipeline with no display sink is not display-bound.
    let candidates2 = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let ctl2 = PlacementController::new(
        test_config(),
        demand_1080p(vec![]),
        candidates2,
        Pins::none(),
        nv("GPU-a", 0),
    );
    assert!(!ctl2.is_display_bound_here());
}

#[test]
fn non_display_pipeline_still_migrates_normally() {
    // Control: the SAME hot/idle scenario WITHOUT a display sink must migrate as
    // before — proving the refusal is caused by the affinity, not a regression of
    // the migration path.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(vec![]), // no display sink
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );
    let hot = vec![vram_pct("GPU-a", 0, 96), vram_pct("GPU-b", 1, 12)];

    let mut migrated = false;
    for _ in 0..20 {
        if let PlacementProposal::Migrate(plan) = ctl.observe(&hot) {
            assert_eq!(plan.from, nv("GPU-a", 0));
            assert_eq!(plan.to, nv("GPU-b", 1));
            migrated = true;
            break;
        }
    }
    assert!(
        migrated,
        "without a display sink the same overload migrates"
    );
}

#[test]
fn display_bound_single_gpu_host_is_trivially_satisfied() {
    // The display-node / thin-client tier: one GPU, which owns the connector. The
    // affinity is trivially satisfied (display device IS the only device). The
    // controller may shed under overload but never migrates or splits.
    let candidates = vec![candidate("GPU-solo", 0)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(vec![nv("GPU-solo", 0)]),
        candidates,
        Pins::none(),
        nv("GPU-solo", 0),
    );
    assert!(ctl.is_display_bound_here());
    let hot = vec![vram_pct("GPU-solo", 0, 97)];
    for _ in 0..30 {
        match ctl.observe(&hot) {
            PlacementProposal::Migrate(_) => panic!("a single-GPU display host can never migrate"),
            PlacementProposal::Split(_) => panic!("a single-GPU display host can never split"),
            _ => {}
        }
    }
    assert_eq!(ctl.current_device(), &nv("GPU-solo", 0));
}

#[test]
fn display_bound_holds_when_not_overloaded() {
    // No overload: a display-bound pipeline simply Holds, exactly like any other —
    // the affinity only bites on the migrate/split decision, never fabricating a
    // shed.
    let candidates = vec![candidate("GPU-display", 0), candidate("GPU-spare", 1)];
    let mut ctl = PlacementController::new(
        test_config(),
        demand_1080p(vec![nv("GPU-display", 0)]),
        candidates,
        Pins::none(),
        nv("GPU-display", 0),
    );
    let calm = vec![vram_pct("GPU-display", 0, 25), vram_pct("GPU-spare", 1, 25)];
    for _ in 0..40 {
        assert_eq!(ctl.observe(&calm), PlacementProposal::Hold);
    }
}
