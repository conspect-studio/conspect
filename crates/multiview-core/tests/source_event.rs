use multiview_core::source_event::*;

fn key() -> SourceEventKey {
    SourceEventKey {
        source_id: "source".into(),
        epoch: 7,
        origin: EventOrigin::Scte35,
        event_id: "42".into(),
    }
}
fn obs(boundary: EventBoundary, revision: u64) -> SourceEventObservation {
    SourceEventObservation {
        key: key(),
        revision,
        boundary,
        correlation_id: None,
        asset_id: None,
    }
}
fn clock(position_ns: i64) -> PresentationClock {
    PresentationClock::Fresh {
        source_id: "source".into(),
        epoch: 7,
        position_ns,
    }
}
fn tracker() -> SourceEventTracker {
    SourceEventTracker::new(TrackerLimits::default())
}

#[test]
fn early_end_and_exact_media_projection() {
    let mut t = tracker();
    t.observe(obs(
        EventBoundary::End {
            pts_ns: Some(9_007_199_254_741_003),
        },
        9,
    ));
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: Some(9_007_199_254_741_001),
            duration_ns: None,
        },
        1,
    ));
    let e = t.get(&key()).unwrap();
    assert_eq!(
        e.project(&clock(9_007_199_254_741_000)).lifecycle,
        EventLifecycle::Announced
    );
    assert_eq!(
        e.project(&clock(9_007_199_254_741_001)).lifecycle,
        EventLifecycle::Active
    );
    assert_eq!(
        e.project(&clock(9_007_199_254_741_003)).lifecycle,
        EventLifecycle::Ended
    );
    assert_eq!(
        e.project(&clock(9_007_199_254_741_003)).end_confidence,
        EndConfidence::Confirmed
    );
}

#[test]
fn duration_is_expected_not_confirmed_and_clock_loss_is_uncertain() {
    let mut t = tracker();
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: Some(10),
            duration_ns: Some(5),
        },
        1,
    ));
    let e = t.get(&key()).unwrap();
    let p = e.project(&clock(15));
    assert_eq!(p.lifecycle, EventLifecycle::Uncertain);
    assert_eq!(p.expected_end_ns, Some(15));
    assert_eq!(p.end_confidence, EndConfidence::Expected);
    assert_eq!(
        e.project(&PresentationClock::Unknown).lifecycle,
        EventLifecycle::Uncertain
    );
    assert_eq!(
        e.project(&PresentationClock::Stale).lifecycle,
        EventLifecycle::Uncertain
    );
    assert_eq!(
        e.project(&PresentationClock::Fresh {
            source_id: "source".into(),
            epoch: 8,
            position_ns: 12
        })
        .lifecycle,
        EventLifecycle::Superseded
    );
}

#[test]
fn revisions_duplicates_cancel_and_marker_absence() {
    let mut t = tracker();
    let start = obs(
        EventBoundary::Start {
            pts_ns: Some(10),
            duration_ns: None,
        },
        2,
    );
    assert_eq!(t.observe(start.clone()), ObservationOutcome::Applied);
    assert_eq!(t.observe(start), ObservationOutcome::Duplicate);
    assert_eq!(
        t.observe(obs(
            EventBoundary::Start {
                pts_ns: Some(20),
                duration_ns: None
            },
            1
        )),
        ObservationOutcome::Stale
    );
    assert_eq!(
        t.observe(obs(
            EventBoundary::Start {
                pts_ns: Some(20),
                duration_ns: None
            },
            2
        )),
        ObservationOutcome::Conflict
    );
    t.observe(obs(EventBoundary::MarkerAbsent, 3));
    assert_eq!(
        t.get(&key()).unwrap().project(&clock(100)).lifecycle,
        EventLifecycle::Active
    );
    t.observe(obs(EventBoundary::Cancel, 4));
    assert_eq!(
        t.get(&key()).unwrap().project(&clock(100)).lifecycle,
        EventLifecycle::Cancelled
    );
}

#[test]
fn origins_remain_distinct_and_storage_is_bounded() {
    let mut t = SourceEventTracker::new(TrackerLimits {
        max_events: 2,
        max_history: 1,
        max_string_bytes: 16,
    });
    let first = obs(
        EventBoundary::Start {
            pts_ns: Some(1),
            duration_ns: None,
        },
        1,
    );
    t.observe(first.clone());
    let mut other = first.clone();
    other.key.origin = EventOrigin::VendorMarker {
        vendor: "vendor".into(),
    };
    other.boundary = EventBoundary::MarkerObserved;
    t.observe(other);
    assert_eq!(t.events().count(), 2);
    assert_eq!(t.history().len(), 1);
    let mut overflow = first.clone();
    overflow.key.event_id = "third".into();
    assert_eq!(t.observe(overflow), ObservationOutcome::CapacityExceeded);
    let mut oversized = first;
    oversized.asset_id = Some("a".repeat(17));
    assert_eq!(t.observe(oversized), ObservationOutcome::Invalid);
    assert_eq!(t.diagnostics().capacity_exceeded, 1);
    assert_eq!(t.diagnostics().history_dropped, 1);
}

#[test]
fn missing_timestamps_and_overflow_never_fabricate_timing() {
    let mut t = tracker();
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: None,
            duration_ns: None,
        },
        1,
    ));
    assert_eq!(
        t.get(&key()).unwrap().project(&clock(100)).lifecycle,
        EventLifecycle::Uncertain
    );
    assert_eq!(
        t.observe(obs(
            EventBoundary::Start {
                pts_ns: Some(i64::MAX),
                duration_ns: Some(1)
            },
            2
        )),
        ObservationOutcome::Invalid
    );
    assert_eq!(
        t.observe(obs(
            EventBoundary::Start {
                pts_ns: Some(0),
                duration_ns: Some(-1)
            },
            2
        )),
        ObservationOutcome::Invalid
    );
}

#[test]
fn vendor_presence_never_confirms_a_break_and_asset_identity_is_not_occurrence() {
    let mut t = tracker();
    let mut marker = obs(EventBoundary::MarkerObserved, 1);
    marker.key.origin = EventOrigin::VendorMarker {
        vendor: "ssai".into(),
    };
    marker.asset_id = Some("reused-caid".into());
    assert_eq!(t.observe(marker.clone()), ObservationOutcome::Applied);
    marker.boundary = EventBoundary::MarkerAbsent;
    t.observe(marker.clone());
    assert_eq!(
        t.get(&marker.key).unwrap().project(&clock(100)).lifecycle,
        EventLifecycle::Observed
    );
    marker.boundary = EventBoundary::Start {
        pts_ns: Some(0),
        duration_ns: None,
    };
    assert_eq!(t.observe(marker.clone()), ObservationOutcome::Invalid);
    marker.boundary = EventBoundary::End { pts_ns: Some(100) };
    assert_eq!(t.observe(marker.clone()), ObservationOutcome::Invalid);
    marker.boundary = EventBoundary::MarkerObserved;
    marker.key.event_id = "next-occurrence".into();
    assert_eq!(t.observe(marker), ObservationOutcome::Applied);
    assert_eq!(t.events().count(), 2);
}

#[test]
fn zero_limits_and_foreign_clock_are_safe() {
    let mut t = SourceEventTracker::new(TrackerLimits {
        max_events: 0,
        max_history: 0,
        max_string_bytes: 16,
    });
    assert_eq!(
        t.observe(obs(EventBoundary::Cancel, 1)),
        ObservationOutcome::CapacityExceeded
    );
    assert!(t.history().is_empty());
    let mut t = tracker();
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: Some(0),
            duration_ns: None,
        },
        1,
    ));
    let foreign = PresentationClock::Fresh {
        source_id: "other".into(),
        epoch: 7,
        position_ns: 100,
    };
    assert_eq!(
        t.get(&key()).unwrap().project(&foreign).lifecycle,
        EventLifecycle::Uncertain
    );
}

#[test]
fn inverted_boundaries_are_uncertain() {
    let mut t = tracker();
    t.observe(obs(EventBoundary::End { pts_ns: Some(9) }, 1));
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: Some(10),
            duration_ns: None,
        },
        1,
    ));
    for now in [0, 9, 10, 20] {
        let p = t.get(&key()).unwrap().project(&clock(now));
        assert_eq!(p.lifecycle, EventLifecycle::Uncertain);
        assert_eq!(p.end_confidence, EndConfidence::Unknown);
        assert_eq!(p.confirmed_end_ns, None);
        assert_eq!(p.expected_end_ns, None);
    }
    t.observe(obs(EventBoundary::Cancel, 2));
    assert_eq!(
        t.get(&key()).unwrap().project(&clock(20)).lifecycle,
        EventLifecycle::Cancelled
    );
    assert_eq!(
        t.get(&key())
            .unwrap()
            .project(&PresentationClock::Unknown)
            .lifecycle,
        EventLifecycle::Cancelled
    );
}

#[test]
fn epoch_supersedes_malformed_intervals_and_allows_pruning() {
    let mut t = tracker();
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: Some(100),
            duration_ns: None,
        },
        1,
    ));
    t.observe(obs(EventBoundary::End { pts_ns: Some(90) }, 1));
    let next_epoch = PresentationClock::Fresh {
        source_id: "source".into(),
        epoch: 8,
        position_ns: 0,
    };
    assert_eq!(
        t.get(&key()).unwrap().project(&next_epoch).lifecycle,
        EventLifecycle::Superseded
    );
    assert_eq!(t.prune_terminal(&next_epoch, |_| true), 1);
    assert!(t.get(&key()).is_none());
}

#[test]
fn epoch_floor_reclaims_capacity_and_rejects_replay() {
    let mut t = SourceEventTracker::new(TrackerLimits {
        max_events: 1,
        max_history: 4,
        max_string_bytes: 16,
    });
    let old = obs(
        EventBoundary::Start {
            pts_ns: Some(0),
            duration_ns: None,
        },
        1,
    );
    t.observe(old.clone());
    assert_eq!(t.advance_epoch_floor("source", 8), Ok(1));
    assert!(t.history().is_empty());
    assert_eq!(t.observe(old.clone()), ObservationOutcome::Stale);
    assert_eq!(
        t.advance_epoch_floor("source", 7),
        Err(ObservationOutcome::Stale)
    );
    assert_eq!(t.observe(old.clone()), ObservationOutcome::Stale);
    let mut new = old;
    new.key.epoch = 8;
    assert_eq!(t.observe(new), ObservationOutcome::Applied);
    assert_eq!(
        t.advance_epoch_floor("other", 1),
        Err(ObservationOutcome::CapacityExceeded)
    );
}

#[test]
fn terminal_pruning_requires_explicit_owner_selection() {
    let mut t = tracker();
    t.observe(obs(
        EventBoundary::Start {
            pts_ns: Some(0),
            duration_ns: None,
        },
        1,
    ));
    assert_eq!(t.prune_terminal(&clock(100), |_| true), 0);
    t.observe(obs(EventBoundary::End { pts_ns: Some(10) }, 1));
    assert_eq!(t.prune_terminal(&clock(100), |_| false), 0);
    assert_eq!(t.prune_terminal(&clock(100), |_| true), 1);
    assert_eq!(t.events().count(), 0);
    assert_eq!(t.history().len(), 2);
}

#[test]
fn serde_is_tagged_and_exact() {
    let o = obs(
        EventBoundary::Start {
            pts_ns: Some(i64::MAX - 1),
            duration_ns: None,
        },
        1,
    );
    let json = serde_json::to_string(&o).unwrap();
    assert!(json.contains("\"kind\":\"start\""));
    assert_eq!(
        serde_json::from_str::<SourceEventObservation>(&json).unwrap(),
        o
    );
}
