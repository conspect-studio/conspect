//! Acceptance-soak analyzer (DEV-C4): the pure pass/fail logic the soak harness
//! runs over a captured metrics series. These tests pin the percentile maths,
//! the per-source threshold boundary, and the invariant-#1 chaos assertion
//! (output cadence never stalls across a PTP/WS kill window).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::clock::{
    ClockSourceLabel, CHRONY_OFFSET_P99_MAX_NS, PTP_OFFSET_P99_MAX_NS,
};
use multiview_telemetry::soak::{
    cadence_uninterrupted, evaluate_offset, p99_abs_offset_ns, SoakReport,
};

#[test]
fn p99_is_the_nearest_rank_of_absolute_offset() {
    // 100 samples 1..=100 ns: nearest-rank p99 (ceil(0.99*100)=99th) = 99.
    let samples: Vec<i64> = (1..=100).collect();
    assert_eq!(p99_abs_offset_ns(&samples), Some(99));
}

#[test]
fn p99_uses_absolute_value_so_sign_does_not_hide_drift() {
    // A symmetric spread: the 99th-percentile of |x| must see the -100 tail.
    let mut samples: Vec<i64> = (1..=99).map(|n| -n).collect();
    samples.push(-100);
    assert_eq!(p99_abs_offset_ns(&samples), Some(99));
}

#[test]
fn p99_of_an_empty_series_is_none() {
    assert_eq!(p99_abs_offset_ns(&[]), None);
}

#[test]
fn ptp_offset_exactly_at_the_threshold_passes_but_one_ns_over_fails() {
    // A series whose 99th-pct lands exactly on the PTP bound (100_000 ns) passes;
    // one ns over the bound at the 99th-pct fails. Boundary is inclusive-pass.
    // (Every sample sits at the bound so the 99th-rank value IS the bound — the
    // single-outlier-in-99-zeros shape would instead be tolerated, see below.)
    let at: Vec<i64> = vec![PTP_OFFSET_P99_MAX_NS; 100];
    let v = evaluate_offset(ClockSourceLabel::Ptp, &at).unwrap();
    assert_eq!(v.p99_abs_ns, PTP_OFFSET_P99_MAX_NS);
    assert_eq!(v.threshold_ns, PTP_OFFSET_P99_MAX_NS);
    assert!(v.pass, "p99 == threshold must pass");

    let over: Vec<i64> = vec![PTP_OFFSET_P99_MAX_NS + 1; 100];
    assert!(!evaluate_offset(ClockSourceLabel::Ptp, &over).unwrap().pass);
}

#[test]
fn the_99th_percentile_tolerates_the_worst_one_percent() {
    // The SLO is "99th-pct |offset| ≤ bound", so the worst 1% may exceed it. 99
    // samples at the bound + 1 huge outlier still passes (the outlier is the
    // 100th/top-1% sample, above the 99th-rank); but if >1% exceed, it fails.
    let mut tolerated: Vec<i64> = vec![PTP_OFFSET_P99_MAX_NS; 99];
    tolerated.push(50_000_000); // a single 50 ms spike — within the top 1%
    assert!(
        evaluate_offset(ClockSourceLabel::Ptp, &tolerated)
            .unwrap()
            .pass
    );

    let mut breached: Vec<i64> = vec![0; 98];
    breached.push(PTP_OFFSET_P99_MAX_NS + 1);
    breached.push(PTP_OFFSET_P99_MAX_NS + 1); // 2% exceed → 99th-rank exceeds
    assert!(
        !evaluate_offset(ClockSourceLabel::Ptp, &breached)
            .unwrap()
            .pass
    );
}

#[test]
fn chrony_uses_the_looser_millisecond_bound() {
    // 800 µs at the 99th-pct: fails the 100 µs PTP bound, passes the 1 ms chrony
    // bound. Same series, different per-source threshold.
    let s: Vec<i64> = vec![800_000; 100];
    assert!(!evaluate_offset(ClockSourceLabel::Ptp, &s).unwrap().pass);
    let v = evaluate_offset(ClockSourceLabel::System, &s).unwrap();
    assert_eq!(v.threshold_ns, CHRONY_OFFSET_P99_MAX_NS);
    assert!(v.pass);
}

#[test]
fn cadence_uninterrupted_holds_when_every_window_advanced_at_least_the_floor() {
    // Output-tick counts sampled each wall interval; expected ≥30 ticks/sample.
    let ticks = [0_u64, 30, 60, 90, 120, 150];
    assert!(cadence_uninterrupted(&ticks, 30));
}

#[test]
fn cadence_uninterrupted_fails_on_a_stall_across_the_kill_window() {
    // The PTP/WS kill lands between samples 2 and 3: a healthy node free-runs
    // (ticks keep advancing); a stalled output clock shows a flat delta and must
    // be caught (inv #1 — the output never falters even when timing is killed).
    let stalled = [0_u64, 30, 60, 60, 90, 120];
    assert!(!cadence_uninterrupted(&stalled, 30));
}

#[test]
fn a_soak_report_passes_only_when_every_leg_and_the_cadence_pass() {
    let ptp: Vec<i64> = vec![0; 100];
    let ticks = [0_u64, 30, 60, 90];
    let mut report = SoakReport::default();
    report.add_offset(evaluate_offset(ClockSourceLabel::Ptp, &ptp).unwrap());
    report.set_cadence(cadence_uninterrupted(&ticks, 30));
    assert!(report.passed());

    // One failing leg sinks the whole report (99th-pct over the bound).
    let bad: Vec<i64> = vec![PTP_OFFSET_P99_MAX_NS + 1; 100];
    let mut report2 = SoakReport::default();
    report2.add_offset(evaluate_offset(ClockSourceLabel::Ptp, &bad).unwrap());
    report2.set_cadence(true);
    assert!(!report2.passed());
}
