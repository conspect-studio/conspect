//! Tests for the PTP **reference tracker** — the lock-state / clock-class state
//! machine (`Freerun -> Acquiring -> Locked -> Holdover`) driven by the offset
//! servo over **injected** `(offset, delay)` samples plus a monotonic sample
//! timebase. Pure-Rust default build (no PTP NIC): every input is injected, so
//! the whole state machine, the staleness/holdover timing, and the badge mapping
//! are deterministically exercised here.
//!
//! Invariant #1 is re-asserted: the reference tracker only *informs* the
//! wall-clock badge; it never paces the output clock. The dedicated tick test
//! lives in `ptp_no_pacing.rs`; this file drives the state machine itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_engine::ptp::{
    LockState, PtpSample, ReferenceConfig, ReferenceStatus, ReferenceTracker, ServoConfig,
};

/// A tracker config that locks after 3 in-tolerance samples, treats a sample
/// older than 2 s as stale (-> holdover), and abandons the reference (->
/// freerun) after 5 s of holdover. Lock tolerance is 50 us of offset.
fn cfg() -> ReferenceConfig {
    ReferenceConfig {
        servo: ServoConfig {
            // Light smoothing so the injected offsets land near the estimate
            // quickly and the in-tolerance counter advances deterministically.
            alpha_recip: 2,
            step_threshold_ns: 1_000_000,
            delay_outlier_pct: 0,
        },
        lock_tolerance_ns: 50_000,
        lock_samples: 3,
        stale_after_ns: 2_000_000_000,
        holdover_window_ns: 5_000_000_000,
    }
}

#[test]
fn starts_in_freerun() {
    let t = ReferenceTracker::new(cfg());
    assert_eq!(t.state(), LockState::Freerun);
    // Freerun reports no usable offset estimate.
    assert!(!t.status().disciplined);
}

#[test]
fn first_sample_moves_to_acquiring_not_locked() {
    let mut t = ReferenceTracker::new(cfg());
    // One good sample: the servo anchors, but a single sample is not yet a lock.
    t.observe(PtpSample::new(0, 1_000), 0);
    assert_eq!(t.state(), LockState::Acquiring);
    assert!(!t.status().disciplined);
}

#[test]
fn locks_after_enough_in_tolerance_samples() {
    let mut t = ReferenceTracker::new(cfg());
    // Three consecutive in-tolerance samples (offset stays within 50 us of the
    // estimate) reach `lock_samples == 3` -> Locked.
    t.observe(PtpSample::new(0, 1_000), 0);
    assert_eq!(t.state(), LockState::Acquiring);
    t.observe(PtpSample::new(1_000, 1_000), 100_000_000);
    assert_eq!(t.state(), LockState::Acquiring);
    t.observe(PtpSample::new(2_000, 1_000), 200_000_000);
    assert_eq!(t.state(), LockState::Locked);
    let s = t.status();
    assert!(s.disciplined);
    assert_eq!(s.state, LockState::Locked);
    // The reported offset is the servo's smoothed estimate.
    assert_eq!(s.offset_ns, t.servo_offset_ns());
}

#[test]
fn large_out_of_tolerance_sample_resets_acquisition() {
    let mut t = ReferenceTracker::new(cfg());
    t.observe(PtpSample::new(0, 1_000), 0);
    t.observe(PtpSample::new(1_000, 1_000), 100_000_000);
    assert_eq!(t.state(), LockState::Acquiring);
    // A sample 200 us off the estimate (> 50 us tolerance) is in-band for the
    // servo (< 1 ms step threshold) but out of *lock* tolerance: it must reset
    // the consecutive-in-tolerance counter, so we stay Acquiring (never lock on
    // this run of three).
    t.observe(PtpSample::new(200_000, 1_000), 200_000_000);
    assert_eq!(t.state(), LockState::Acquiring);
}

#[test]
fn stale_samples_drop_locked_to_holdover() {
    let mut t = ReferenceTracker::new(cfg());
    // Reach Locked.
    t.observe(PtpSample::new(0, 1_000), 0);
    t.observe(PtpSample::new(1_000, 1_000), 100_000_000);
    t.observe(PtpSample::new(2_000, 1_000), 200_000_000);
    assert_eq!(t.state(), LockState::Locked);
    // No new sample; 3 s of wall time passes (> 2 s stale window): the tracker
    // coasts into Holdover on the last good estimate.
    let off_before = t.servo_offset_ns();
    t.tick(3_200_000_000);
    assert_eq!(t.state(), LockState::Holdover);
    // Holdover still reports the last good offset and is still "disciplined".
    assert!(t.status().disciplined);
    assert_eq!(t.servo_offset_ns(), off_before);
}

#[test]
fn holdover_expires_to_freerun() {
    let mut t = ReferenceTracker::new(cfg());
    t.observe(PtpSample::new(0, 1_000), 0);
    t.observe(PtpSample::new(1_000, 1_000), 100_000_000);
    t.observe(PtpSample::new(2_000, 1_000), 200_000_000);
    assert_eq!(t.state(), LockState::Locked);
    // 3 s -> holdover, then a further span past the 5 s holdover window from the
    // last good sample -> the reference is abandoned (Freerun).
    t.tick(3_200_000_000);
    assert_eq!(t.state(), LockState::Holdover);
    t.tick(8_000_000_000);
    assert_eq!(t.state(), LockState::Freerun);
    assert!(!t.status().disciplined);
}

#[test]
fn a_fresh_sample_in_holdover_reacquires() {
    let mut t = ReferenceTracker::new(cfg());
    t.observe(PtpSample::new(0, 1_000), 0);
    t.observe(PtpSample::new(1_000, 1_000), 100_000_000);
    t.observe(PtpSample::new(2_000, 1_000), 200_000_000);
    t.tick(3_200_000_000);
    assert_eq!(t.state(), LockState::Holdover);
    // A new in-tolerance sample arrives while still in holdover: discipline
    // resumes. It must re-run acquisition (one fresh sample is Acquiring, not an
    // instant re-lock).
    t.observe(PtpSample::new(2_100, 1_000), 3_300_000_000);
    assert_eq!(t.state(), LockState::Acquiring);
}

#[test]
fn rejected_outlier_does_not_advance_lock() {
    // With the servo's delay-outlier guard on, a rejected sample must not count
    // toward the lock streak nor refresh staleness.
    let mut c = cfg();
    c.servo.delay_outlier_pct = 200; // reject delays > 2x running average
    let mut t = ReferenceTracker::new(c);
    t.observe(PtpSample::new(0, 10_000), 0); // anchor, avg delay 10us
    t.observe(PtpSample::new(1_000, 10_000), 100_000_000);
    assert_eq!(t.state(), LockState::Acquiring);
    // A delay spike (10x average) is rejected by the servo; it must not advance
    // toward lock.
    let accepted = t.observe(PtpSample::new(2_000, 100_000), 200_000_000);
    assert!(!accepted, "delay outlier must be rejected");
    assert_eq!(t.state(), LockState::Acquiring);
    // The next two good samples complete the (re-counted) streak.
    t.observe(PtpSample::new(2_000, 10_000), 300_000_000);
    t.observe(PtpSample::new(3_000, 10_000), 400_000_000);
    assert_eq!(t.state(), LockState::Locked);
}

#[test]
fn status_snapshot_is_consistent() {
    let mut t = ReferenceTracker::new(cfg());
    t.observe(PtpSample::new(0, 1_000), 0);
    t.observe(PtpSample::new(1_000, 1_000), 100_000_000);
    t.observe(PtpSample::new(2_000, 1_000), 200_000_000);
    let s: ReferenceStatus = t.status();
    assert_eq!(s.state, t.state());
    assert_eq!(s.offset_ns, t.servo_offset_ns());
    assert_eq!(s.frequency_ppb, t.servo_frequency_ppb());
    assert_eq!(s.accepted, t.accepted());
    assert!(s.disciplined);
}
