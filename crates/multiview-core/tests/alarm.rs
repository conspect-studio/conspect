//! Integration tests for the X.733 alarm vocabulary (`alarm` module).
//!
//! These pin the broadcast monitoring foundation (broadcast-multiviewer brief
//! §4): the ITU-T X.733 perceived-severity ordering used for probe -> tile ->
//! group -> system roll-up, the alarm/probe taxonomy, acknowledgement state,
//! and the `AlarmRecord` value type. The severity ordering and roll-up `max`
//! are the load-bearing invariants and are property-tested.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::{
    AckState, AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity,
};
use multiview_core::time::MediaTime;
use proptest::prelude::*;

/// The six X.733 perceived-severity values in strictly ascending order.
const ASCENDING: [PerceivedSeverity; 6] = [
    PerceivedSeverity::Cleared,
    PerceivedSeverity::Indeterminate,
    PerceivedSeverity::Warning,
    PerceivedSeverity::Minor,
    PerceivedSeverity::Major,
    PerceivedSeverity::Critical,
];

#[test]
fn severity_total_order_is_cleared_lowest_critical_highest() {
    for pair in ASCENDING.windows(2) {
        assert!(pair[0] < pair[1], "{:?} should be < {:?}", pair[0], pair[1]);
    }
    assert_eq!(PerceivedSeverity::Cleared, *ASCENDING.first().unwrap());
    assert_eq!(PerceivedSeverity::Critical, *ASCENDING.last().unwrap());
}

#[test]
fn severity_default_is_cleared() {
    assert_eq!(PerceivedSeverity::default(), PerceivedSeverity::Cleared);
}

#[test]
fn cleared_severity_is_not_active() {
    assert!(!PerceivedSeverity::Cleared.is_active());
    assert!(PerceivedSeverity::Indeterminate.is_active());
    assert!(PerceivedSeverity::Warning.is_active());
    assert!(PerceivedSeverity::Critical.is_active());
}

#[test]
fn rollup_of_empty_is_cleared() {
    assert_eq!(
        PerceivedSeverity::rollup(std::iter::empty()),
        PerceivedSeverity::Cleared
    );
}

#[test]
fn rollup_takes_the_worst_severity() {
    let s = PerceivedSeverity::rollup([
        PerceivedSeverity::Warning,
        PerceivedSeverity::Critical,
        PerceivedSeverity::Minor,
    ]);
    assert_eq!(s, PerceivedSeverity::Critical);
}

#[test]
fn rollup_of_all_cleared_is_cleared() {
    let s = PerceivedSeverity::rollup([PerceivedSeverity::Cleared, PerceivedSeverity::Cleared]);
    assert_eq!(s, PerceivedSeverity::Cleared);
}

#[test]
fn ack_state_default_is_unacked() {
    assert_eq!(AckState::default(), AckState::Unacked);
    assert!(!AckState::default().is_acked());
}

#[test]
fn ack_state_acked_carries_who_and_when() {
    let ack = AckState::acked("operator-1", MediaTime::from_nanos(42));
    assert!(ack.is_acked());
    match ack {
        AckState::Acked { who, when } => {
            assert_eq!(who, "operator-1");
            assert_eq!(when, MediaTime::from_nanos(42));
        }
        other => panic!("expected Acked, got {other:?}"),
    }
}

#[test]
fn alarm_record_construction_and_clear() {
    let rec = AlarmRecord::new(
        AlarmId::new("a1"),
        AlarmKind::Black,
        PerceivedSeverity::Major,
        AlarmScope::Probe {
            id: "probe-7".to_owned(),
        },
        MediaTime::from_nanos(1_000),
    );
    assert_eq!(rec.kind, AlarmKind::Black);
    assert_eq!(rec.severity, PerceivedSeverity::Major);
    assert!(rec.is_active());
    assert!(!rec.latched);
    assert_eq!(rec.ack, AckState::Unacked);
    assert_eq!(rec.dwell, MediaTime::ZERO);

    // A record raised at Cleared severity is not active.
    let cleared = AlarmRecord::new(
        AlarmId::new("a2"),
        AlarmKind::Freeze,
        PerceivedSeverity::Cleared,
        AlarmScope::System,
        MediaTime::ZERO,
    );
    assert!(!cleared.is_active());
}

#[test]
fn alarm_record_round_trips_via_json() {
    let rec = AlarmRecord::new(
        AlarmId::new("a3"),
        AlarmKind::Silence,
        PerceivedSeverity::Critical,
        AlarmScope::Tile { index: 3 },
        MediaTime::from_nanos(9_999),
    );
    let json = serde_json::to_string(&rec).unwrap();
    let back: AlarmRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(rec, back);
}

#[test]
fn alarm_kind_serializes_tagged_not_untagged() {
    // Tagged enums (per repo conventions) serialize the variant name as a
    // string for these unit-like variants.
    let json = serde_json::to_string(&AlarmKind::LoudnessViolation).unwrap();
    assert!(json.contains("LoudnessViolation"), "json was: {json}");
}

#[test]
fn alarm_scope_round_trips_each_variant() {
    for scope in [
        AlarmScope::Probe { id: "p".to_owned() },
        AlarmScope::Tile { index: 1 },
        AlarmScope::Group {
            name: "g".to_owned(),
        },
        AlarmScope::System,
    ] {
        let json = serde_json::to_string(&scope).unwrap();
        let back: AlarmScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, back);
    }
}

fn severity_strategy() -> impl Strategy<Value = PerceivedSeverity> {
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
    /// Roll-up always equals the maximum severity in the set (X.733 semantics).
    #[test]
    fn prop_rollup_equals_max(set in prop::collection::vec(severity_strategy(), 0..16)) {
        let expected = set.iter().copied().max().unwrap_or(PerceivedSeverity::Cleared);
        prop_assert_eq!(PerceivedSeverity::rollup(set), expected);
    }

    /// The ordering is total and antisymmetric: ordering matches index in the
    /// ascending table for any pair.
    #[test]
    fn prop_order_matches_rank(a in 0usize..6, b in 0usize..6) {
        let sa = ASCENDING[a];
        let sb = ASCENDING[b];
        prop_assert_eq!(sa.cmp(&sb), a.cmp(&b));
    }
}
