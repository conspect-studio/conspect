//! The wait-free convenience flags the engine seams derive from a single
//! published [`EnforcementLevel`] (Conspect S1/S3, ADR-0050 §3/§5/§6).
//!
//! The cli publishes ONE `EnforcementLevel` (arc-swapped) and the engine seams
//! read exactly two cheap booleans off it: `watermark()` (S3) and
//! `blocks_new_instances()` (S1). These tests pin the level→flag mapping so the
//! engine surface stays "two booleans derived from data" and every level still
//! keeps the program on air.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc
)]

use multiview_licence::EnforcementLevel;

/// `watermark()` is true exactly for the levels the spec marks the canvas on:
/// `watermark`, `block-new-instance`, and `unlicensed-build` (the honest
/// source-build watermark). It is clean for `active`, `warning`, `config-locked`.
#[test]
fn watermark_flag_matches_the_spec_table() {
    assert!(
        !EnforcementLevel::Active.watermark(),
        "active: clean canvas"
    );
    assert!(
        !EnforcementLevel::Warning.watermark(),
        "warning: clean canvas"
    );
    assert!(
        !EnforcementLevel::ConfigLocked.watermark(),
        "config-locked: clean canvas (only reconfiguration is denied)"
    );
    assert!(
        EnforcementLevel::Watermark.watermark(),
        "watermark: marks the canvas"
    );
    assert!(
        EnforcementLevel::BlockNewInstance.watermark(),
        "block-new-instance: still marks the canvas (a harder rung than watermark)"
    );
    assert!(
        EnforcementLevel::UnlicensedBuild.watermark(),
        "unlicensed-build: an honest watermark (ADR-0050 §7)"
    );
}

/// `blocks_new_instances()` is true ONLY at the hardest rung. Every softer rung
/// allows a new start (S1 never fires until block-new-instance).
#[test]
fn blocks_new_instances_only_at_the_hardest_rung() {
    assert!(!EnforcementLevel::Active.blocks_new_instances());
    assert!(!EnforcementLevel::Warning.blocks_new_instances());
    assert!(!EnforcementLevel::ConfigLocked.blocks_new_instances());
    assert!(!EnforcementLevel::Watermark.blocks_new_instances());
    assert!(
        EnforcementLevel::BlockNewInstance.blocks_new_instances(),
        "only block-new-instance refuses a NEW engine instance"
    );
    assert!(
        !EnforcementLevel::UnlicensedBuild.blocks_new_instances(),
        "an unlicensed build is reported honestly but does not block a start (ADR-0050 §7)"
    );
}

/// `config_locked()` matches the spec table: locked from `config-locked` and on
/// the harder rungs (`watermark`, `block-new-instance`); never on the soft rungs.
#[test]
fn config_locked_flag_matches_the_spec_table() {
    assert!(!EnforcementLevel::Active.config_locked());
    assert!(!EnforcementLevel::Warning.config_locked());
    assert!(EnforcementLevel::ConfigLocked.config_locked());
    assert!(EnforcementLevel::Watermark.config_locked());
    assert!(EnforcementLevel::BlockNewInstance.config_locked());
    assert!(
        !EnforcementLevel::UnlicensedBuild.config_locked(),
        "an unlicensed build does not lock reconfiguration"
    );
}

/// THE SACRED PROPERTY: no enforcement level ever takes a running program off
/// air. Every level — including the hardest — answers `program_stays_on_air()`.
#[test]
fn every_level_keeps_the_program_on_air() {
    for level in [
        EnforcementLevel::Active,
        EnforcementLevel::Warning,
        EnforcementLevel::ConfigLocked,
        EnforcementLevel::Watermark,
        EnforcementLevel::BlockNewInstance,
        EnforcementLevel::UnlicensedBuild,
    ] {
        assert!(
            level.program_stays_on_air(),
            "{level:?} must keep the program on air — enforcement degrades conveniences only"
        );
    }
}
