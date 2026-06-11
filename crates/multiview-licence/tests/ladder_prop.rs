//! Property tests for the ladder state engine: the day-boundary mapping is
//! exact and the program-on-air invariant holds for EVERY input.
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
use multiview_licence::{HardwareClass, ACTIVATION_WINDOW_DAYS, LEASE_FULL_DAYS, LEASE_GRACE_DAYS};
use proptest::prelude::*;

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn online_input(days_after_grant: i64) -> LadderInput {
    let granted = epoch();
    LadderInput {
        lease: Lease::new_full(
            "p-serial".to_owned(),
            granted,
            LeaseSource::Online,
            ACTIVATION_WINDOW_DAYS,
        ),
        now: granted + Duration::days(days_after_grant),
        licensed_class: HardwareClass::Standard,
        detected_class: HardwareClass::Standard,
        gpu_limit: 4,
        gpu_in_use: 1,
        evaluation_started_at: None,
    }
}

proptest! {
    /// For a healthy (matching class, in-limit GPU) online lease, the state is a
    /// pure function of whole-days-past-expiry, with the exact boundaries.
    #[test]
    fn time_phase_boundaries_are_exact(days in 0_i64..400) {
        let st = compute_ladder_state(&online_input(days)).state;
        let past = days - LEASE_FULL_DAYS;
        let expected = if past <= 0 {
            LadderState::Compliant
        } else if past <= LEASE_GRACE_DAYS {
            LadderState::Grace
        } else if past <= 45 {
            LadderState::LapsedSoft
        } else {
            LadderState::LapsedHard
        };
        prop_assert_eq!(st, expected);
    }

    /// The program is on air for EVERY input — the never-off-air promise.
    #[test]
    fn always_on_air(
        days in 0_i64..500,
        gpu_limit in 0_u32..8,
        gpu_in_use in 0_u32..16,
        mismatch in any::<bool>(),
    ) {
        let mut input = online_input(days);
        input.gpu_limit = gpu_limit;
        input.gpu_in_use = gpu_in_use;
        if mismatch {
            input.detected_class = HardwareClass::Datacenter;
        }
        let st = compute_ladder_state(&input);
        prop_assert!(st.program_stays_on_air());
    }

    /// Class mismatch ALWAYS wins over the time-based phase (most actionable).
    #[test]
    fn class_mismatch_dominates(days in 0_i64..500) {
        let mut input = online_input(days);
        input.detected_class = HardwareClass::Edge; // != Standard
        let st = compute_ladder_state(&input).state;
        prop_assert_eq!(st, LadderState::ClassMismatch);
    }

    /// over_gpu wins over a time phase, but NOT over a class mismatch.
    #[test]
    fn over_gpu_precedence(days in 0_i64..500) {
        let mut input = online_input(days);
        input.gpu_limit = 1;
        input.gpu_in_use = 5; // over
        // No class mismatch → over_gpu reported.
        prop_assert_eq!(compute_ladder_state(&input).state, LadderState::OverGpu);
        // Add a class mismatch → class_mismatch wins.
        input.detected_class = HardwareClass::Datacenter;
        prop_assert_eq!(compute_ladder_state(&input).state, LadderState::ClassMismatch);
    }
}
