//! Enforcement-ladder state-engine tests — the day-boundary behaviour of the
//! pure-data ladder (ADR-0050 §4 / §6; the brief §2.2, §6.2, §12).
//!
//! These assert the EXACT constants and the EXACT day boundaries: a portal that
//! shows "35 days" and a machine that enforces 30 is a support incident
//! (ADR-0050 §4), so the boundaries are nailed down here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use chrono::{DateTime, Duration, Utc};
use multiview_licence::ladder::{compute_ladder_state, LadderInput, LadderState};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::{
    HardwareClass, ACTIVATION_WINDOW_DAYS, LEASE_FULL_DAYS, LEASE_GRACE_DAYS, LEASE_HARD_DAYS,
};

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

/// Build an online lease granted at `granted`, with the standard 35-day term.
fn online_lease(granted: DateTime<Utc>) -> Lease {
    Lease::new_full(
        "serial-0001".to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    )
}

/// Standard compliant input: matching hardware class, GPU within limit, online.
fn input_at(lease: Lease, now: DateTime<Utc>) -> LadderInput {
    LadderInput {
        lease,
        now,
        licensed_class: HardwareClass::Standard,
        detected_class: HardwareClass::Standard,
        gpu_limit: 4,
        gpu_in_use: 1,
        evaluation_started_at: None,
    }
}

#[test]
fn constants_are_exact() {
    // The portals depend on these byte-for-byte (ADR-0050 §4 / brief §2.2).
    assert_eq!(LEASE_FULL_DAYS, 35);
    assert_eq!(LEASE_GRACE_DAYS, 14);
    assert_eq!(LEASE_HARD_DAYS, 90);
    assert_eq!(ACTIVATION_WINDOW_DAYS, 31);
}

#[test]
fn compliant_within_lease() {
    let granted = epoch();
    let lease = online_lease(granted);
    // One day after grant — comfortably inside the 35-day term.
    let now = granted + Duration::days(1);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::Compliant);
}

#[test]
fn compliant_on_the_last_lease_day() {
    let granted = epoch();
    let lease = online_lease(granted);
    // Exactly at expiry boundary (35 days) is still compliant; the next instant
    // past expiry begins grace.
    let now = granted + Duration::days(LEASE_FULL_DAYS);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::Compliant);
}

#[test]
fn grace_begins_just_past_expiry() {
    let granted = epoch();
    let lease = online_lease(granted);
    // One day past the 35-day lease → grace.
    let now = granted + Duration::days(LEASE_FULL_DAYS + 1);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::Grace);
}

#[test]
fn grace_at_the_last_grace_day() {
    let granted = epoch();
    let lease = online_lease(granted);
    // 14 days past expiry is the LAST grace day.
    let now = granted + Duration::days(LEASE_FULL_DAYS + LEASE_GRACE_DAYS);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::Grace);
}

#[test]
fn lapsed_soft_at_fifteen_days_past_expiry() {
    let granted = epoch();
    let lease = online_lease(granted);
    // 15 days past expiry (grace +1) → lapsed_soft. Blocks NEW instances (data).
    let now = granted + Duration::days(LEASE_FULL_DAYS + LEASE_GRACE_DAYS + 1);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::LapsedSoft);
    assert!(
        st.blocks_new_instances(),
        "lapsed_soft must block NEW instances (data only)"
    );
}

#[test]
fn lapsed_soft_at_forty_five_days_past_expiry() {
    let granted = epoch();
    let lease = online_lease(granted);
    // 45 days past expiry is the LAST lapsed_soft day.
    let now = granted + Duration::days(LEASE_FULL_DAYS + 45);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::LapsedSoft);
}

#[test]
fn lapsed_hard_past_forty_five_days() {
    let granted = epoch();
    let lease = online_lease(granted);
    // 46 days past expiry → lapsed_hard: watermark + config-lock (data only).
    let now = granted + Duration::days(LEASE_FULL_DAYS + 46);
    let st = compute_ladder_state(&input_at(lease, now));
    assert_eq!(st.state, LadderState::LapsedHard);
    assert!(
        st.watermark(),
        "lapsed_hard must request a watermark (data)"
    );
    assert!(
        st.config_locked(),
        "lapsed_hard must request a config-lock (data)"
    );
}

#[test]
fn never_off_air_on_every_rung() {
    // The product promise: NO ladder state ever requests stopping output.
    // This is data — the type cannot even express "stop output".
    let granted = epoch();
    let lease = online_lease(granted);
    for extra in [0_i64, 1, 14, 15, 45, 46, 200] {
        let now = granted + Duration::days(LEASE_FULL_DAYS + extra);
        let st = compute_ladder_state(&input_at(lease.clone(), now));
        assert!(
            st.program_stays_on_air(),
            "state {:?} must keep program on air",
            st.state
        );
    }
}

#[test]
fn evaluation_clean_before_day_thirty_one() {
    let granted = epoch();
    let lease = Lease::new_full(
        "eval-serial".to_owned(),
        granted,
        LeaseSource::File,
        ACTIVATION_WINDOW_DAYS,
    );
    let eval_start = granted;
    let mut input = input_at(lease, granted + Duration::days(30));
    input.evaluation_started_at = Some(eval_start);
    let st = compute_ladder_state(&input);
    assert_eq!(st.state, LadderState::Evaluation);
    assert!(
        !st.watermark(),
        "evaluation before day 31 must NOT watermark"
    );
}

#[test]
fn evaluation_watermark_from_day_thirty_one() {
    let granted = epoch();
    let lease = Lease::new_full(
        "eval-serial".to_owned(),
        granted,
        LeaseSource::File,
        ACTIVATION_WINDOW_DAYS,
    );
    let eval_start = granted;
    let mut input = input_at(lease, granted + Duration::days(31));
    input.evaluation_started_at = Some(eval_start);
    let st = compute_ladder_state(&input);
    assert_eq!(st.state, LadderState::Evaluation);
    assert!(
        st.watermark(),
        "evaluation watermark must engage from day 31"
    );
}

#[test]
fn over_gpu_when_usage_exceeds_limit() {
    let granted = epoch();
    let lease = online_lease(granted);
    let mut input = input_at(lease, granted + Duration::days(1));
    input.gpu_limit = 2;
    input.gpu_in_use = 3; // over the limit
    let st = compute_ladder_state(&input);
    assert_eq!(st.state, LadderState::OverGpu);
    assert!(st.program_stays_on_air());
}

#[test]
fn over_gpu_at_exactly_the_limit_is_not_over() {
    let granted = epoch();
    let lease = online_lease(granted);
    let mut input = input_at(lease, granted + Duration::days(1));
    input.gpu_limit = 2;
    input.gpu_in_use = 2; // exactly at the limit — NOT over
    let st = compute_ladder_state(&input);
    assert_eq!(st.state, LadderState::Compliant);
}

#[test]
fn class_mismatch_when_detected_differs_from_licensed() {
    let granted = epoch();
    let lease = online_lease(granted);
    let mut input = input_at(lease, granted + Duration::days(1));
    input.licensed_class = HardwareClass::Standard;
    input.detected_class = HardwareClass::Datacenter;
    let st = compute_ladder_state(&input);
    assert_eq!(st.state, LadderState::ClassMismatch);
    assert!(st.program_stays_on_air());
}

#[test]
fn class_mismatch_takes_precedence_over_lapse() {
    // A mismatched class on a long-lapsed lease still reports class_mismatch —
    // it is the more specific, more actionable reason.
    let granted = epoch();
    let lease = online_lease(granted);
    let mut input = input_at(lease, granted + Duration::days(LEASE_FULL_DAYS + 100));
    input.detected_class = HardwareClass::Datacenter;
    let st = compute_ladder_state(&input);
    assert_eq!(st.state, LadderState::ClassMismatch);
}

#[test]
fn offline_lease_uses_ninety_day_term() {
    // An offline (file) lease is granted the 90-day hard term, not 35d.
    let granted = epoch();
    let lease = Lease::new_offline("offline-serial".to_owned(), granted, ACTIVATION_WINDOW_DAYS);
    assert_eq!(lease.source, LeaseSource::File);
    // 89 days in, still compliant under the 90-day offline term.
    let now = granted + Duration::days(89);
    let st = compute_ladder_state(&input_at(lease.clone(), now));
    assert_eq!(st.state, LadderState::Compliant);
    // The expiry is exactly LEASE_HARD_DAYS from grant.
    assert_eq!(lease.expires_at, granted + Duration::days(LEASE_HARD_DAYS));
}
