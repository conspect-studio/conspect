//! Tests for the pure availability / error-second reporting counters.
//!
//! These model the G.826-style availability accounting an NMS expects from a
//! broadcast monitoring point: total in-service time, accumulated alarmed time,
//! and errored / severely-errored seconds, plus the derived availability ratio.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::PerceivedSeverity;
use multiview_telemetry::availability::AvailabilityCounters;
use proptest::prelude::*;

#[test]
fn fresh_counters_are_all_zero_and_fully_available() {
    let c = AvailabilityCounters::new();
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 0);
    assert_eq!(snap.alarm_seconds, 0);
    assert_eq!(snap.error_seconds, 0);
    assert_eq!(snap.severely_errored_seconds, 0);
    // With no observed time at all, availability is defined as 1.0 (fully up).
    assert!((snap.availability_ratio() - 1.0).abs() < 1e-12);
}

#[test]
fn ticking_clear_seconds_advances_only_uptime() {
    let c = AvailabilityCounters::new();
    c.tick(PerceivedSeverity::Cleared);
    c.tick(PerceivedSeverity::Cleared);
    c.tick(PerceivedSeverity::Cleared);
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 3);
    assert_eq!(snap.alarm_seconds, 0);
    assert_eq!(snap.error_seconds, 0);
    assert_eq!(snap.severely_errored_seconds, 0);
    assert!((snap.availability_ratio() - 1.0).abs() < 1e-12);
}

#[test]
fn an_active_alarm_second_counts_as_alarmed_and_errored() {
    let c = AvailabilityCounters::new();
    // Any active (non-Cleared) severity makes the second an errored second.
    c.tick(PerceivedSeverity::Warning);
    c.tick(PerceivedSeverity::Minor);
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 2);
    assert_eq!(snap.alarm_seconds, 2);
    assert_eq!(snap.error_seconds, 2);
    // Warning/Minor are not service-affecting, so not severely errored.
    assert_eq!(snap.severely_errored_seconds, 0);
}

#[test]
fn major_and_critical_seconds_are_severely_errored() {
    let c = AvailabilityCounters::new();
    c.tick(PerceivedSeverity::Major);
    c.tick(PerceivedSeverity::Critical);
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 2);
    assert_eq!(snap.alarm_seconds, 2);
    assert_eq!(snap.error_seconds, 2);
    // Major and Critical are service-affecting => severely errored seconds.
    assert_eq!(snap.severely_errored_seconds, 2);
}

#[test]
fn indeterminate_is_an_alarm_second_but_not_severely_errored() {
    let c = AvailabilityCounters::new();
    c.tick(PerceivedSeverity::Indeterminate);
    let snap = c.snapshot();
    assert_eq!(snap.alarm_seconds, 1);
    assert_eq!(snap.error_seconds, 1);
    assert_eq!(snap.severely_errored_seconds, 0);
}

#[test]
fn availability_ratio_is_unavailable_seconds_over_uptime() {
    let c = AvailabilityCounters::new();
    // 8 clear + 2 severely-errored => availability = (10 - 2) / 10 = 0.8.
    for _ in 0..8 {
        c.tick(PerceivedSeverity::Cleared);
    }
    c.tick(PerceivedSeverity::Critical);
    c.tick(PerceivedSeverity::Major);
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 10);
    assert_eq!(snap.severely_errored_seconds, 2);
    assert!(
        (snap.availability_ratio() - 0.8).abs() < 1e-12,
        "availability {} should be 0.8",
        snap.availability_ratio()
    );
}

#[test]
fn tick_n_advances_all_relevant_counters_by_n() {
    let c = AvailabilityCounters::new();
    c.tick_n(PerceivedSeverity::Major, 5);
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 5);
    assert_eq!(snap.alarm_seconds, 5);
    assert_eq!(snap.error_seconds, 5);
    assert_eq!(snap.severely_errored_seconds, 5);
}

#[test]
fn counters_are_clonable_handles_sharing_storage() {
    // The counters are a cheap shared handle (like the metrics registry), so a
    // clone observes the same accumulated state.
    let c = AvailabilityCounters::new();
    let other = c.clone();
    c.tick(PerceivedSeverity::Major);
    other.tick(PerceivedSeverity::Cleared);
    let snap = c.snapshot();
    assert_eq!(snap.uptime_seconds, 2, "both clones write the same storage");
    assert_eq!(snap.alarm_seconds, 1);
    assert_eq!(snap.severely_errored_seconds, 1);
}

fn severities() -> impl Strategy<Value = PerceivedSeverity> {
    prop_oneof![
        Just(PerceivedSeverity::Cleared),
        Just(PerceivedSeverity::Indeterminate),
        Just(PerceivedSeverity::Warning),
        Just(PerceivedSeverity::Minor),
        Just(PerceivedSeverity::Major),
        Just(PerceivedSeverity::Critical),
    ]
}

proptest! {
    /// Across any sequence of ticks the counters never exceed uptime, the
    /// severely-errored subset never exceeds the errored subset, and the derived
    /// availability ratio always lies in `[0, 1]`.
    #[test]
    fn counter_subsets_and_ratio_bounds(seq in proptest::collection::vec(severities(), 0..200)) {
        let c = AvailabilityCounters::new();
        for sev in &seq {
            c.tick(*sev);
        }
        let snap = c.snapshot();
        prop_assert_eq!(snap.uptime_seconds, u64::try_from(seq.len()).unwrap());
        prop_assert!(snap.alarm_seconds <= snap.uptime_seconds);
        prop_assert!(snap.error_seconds <= snap.uptime_seconds);
        prop_assert!(snap.severely_errored_seconds <= snap.error_seconds);
        let ratio = snap.availability_ratio();
        prop_assert!((0.0..=1.0).contains(&ratio), "ratio {ratio} out of range");
    }
}
