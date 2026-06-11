//! The Conspect **startup gate** (S1, ADR-0050 §5): block creating a NEW engine
//! instance when the ladder is at the block-new-instance rung — while a RUNNING
//! engine continues untouched.
//!
//! THE SACRED CONSTRAINT (invariant #1): enforcement degrades conveniences only;
//! it NEVER stops a running program. These tests prove both halves: a new start
//! is refused with the spec's exact reason, AND an already-built/running engine
//! keeps emitting one frame per tick across the gate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::run::{start_gate, RunError, SoftwareEngine};
use multiview_config::MultiviewConfig;
use multiview_engine::{CooperativePacer, ManualTimeSource};
use multiview_licence::EnforcementLevel;

/// A small 1x1 software config that builds + runs deterministically.
fn small_config() -> MultiviewConfig {
    let toml = r##"
schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/gate.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    let cfg = MultiviewConfig::load_from_toml(toml).expect("parse");
    cfg.validate().expect("validate");
    cfg
}

/// At `block-new-instance` the gate refuses a NEW start with the spec's exact
/// reason — running ones untouched.
#[test]
fn block_new_instance_refuses_a_new_start_with_the_exact_reason() {
    let err = start_gate(Some(EnforcementLevel::BlockNewInstance))
        .expect_err("block-new-instance must refuse a NEW start");
    let RunError::LeaseExpired(reason) = &err else {
        panic!("expected RunError::LeaseExpired, got {err:?}");
    };
    // Match the brief's copy verbatim (the operator + portal read the same words).
    assert_eq!(
        reason,
        "Lease expired — new engine instances won't start; running ones untouched"
    );
}

/// Every softer rung (and no-plane / no-lease) allows a new start.
#[test]
fn softer_rungs_allow_a_new_start() {
    assert!(start_gate(None).is_ok(), "no plane / no lease → allowed");
    for level in [
        EnforcementLevel::Active,
        EnforcementLevel::Warning,
        EnforcementLevel::ConfigLocked,
        EnforcementLevel::Watermark,
        EnforcementLevel::UnlicensedBuild,
    ] {
        assert!(
            start_gate(Some(level)).is_ok(),
            "{level:?} must allow a new start (only block-new-instance refuses)"
        );
    }
}

/// `SoftwareEngine::build_gated` refuses to CREATE a new engine at the hardest
/// rung — the gate runs BEFORE the engine is built, so no engine exists to stop.
#[test]
fn build_gated_refuses_at_block_new_instance() {
    let cfg = small_config();
    let err = SoftwareEngine::build_gated(&cfg, Some(EnforcementLevel::BlockNewInstance))
        .err()
        .expect("build_gated refuses at block-new-instance");
    assert!(matches!(err, RunError::LeaseExpired(_)));
}

/// THE NEVER-OFF-AIR PROOF: a RUNNING engine is untouched by the hardest rung.
/// We build an engine while compliant, then drive it to N frames while the
/// ladder sits at `block-new-instance` — the output still emits exactly one
/// frame per tick and never falters. The gate only governs *new* builds; a
/// running engine never re-enters the gate.
#[tokio::test]
async fn a_running_engine_is_untouched_by_block_new_instance() {
    let cfg = small_config();
    // Built while compliant.
    let mut engine =
        SoftwareEngine::build_gated(&cfg, Some(EnforcementLevel::Active)).expect("build compliant");

    // Now the ladder hardens to block-new-instance. A NEW build would be refused…
    assert!(SoftwareEngine::build_gated(&cfg, Some(EnforcementLevel::BlockNewInstance)).is_err());

    // …but the ALREADY-BUILT engine drives to completion, one frame per tick,
    // never faltered — the hardest rung cannot touch a running program.
    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for(time, CooperativePacer, 30)
        .await
        .expect("the running engine drives regardless of the ladder");
    assert_eq!(report.frames, 30, "one frame per tick");
    assert_eq!(report.ticks, 30);
    assert!(
        !report.faltered,
        "a running engine never falters at any ladder rung"
    );
}
