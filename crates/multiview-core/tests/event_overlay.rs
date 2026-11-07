use multiview_core::event_overlay::*;
use multiview_core::source_event::*;

fn key() -> SourceEventKey {
    SourceEventKey {
        source_id: "camera".into(),
        epoch: 1,
        origin: EventOrigin::Scte35,
        event_id: "break".into(),
    }
}
#[allow(clippy::panic)] // Test fixture failure must fail the test, never fabricate evidence.
fn event(boundaries: Vec<EventBoundary>) -> SourceEvent {
    let mut tracker = SourceEventTracker::new(TrackerLimits::default());
    for boundary in boundaries {
        tracker.observe(SourceEventObservation {
            key: key(),
            revision: 1,
            boundary,
            correlation_id: None,
            asset_id: None,
        });
    }
    match tracker.get(&key()) {
        Some(event) => event.clone(),
        None => panic!("fixture event must be retained"),
    }
}
fn start() -> EventBoundary {
    EventBoundary::Start {
        pts_ns: Some(100),
        duration_ns: Some(50),
    }
}
fn clock(position_ns: i64) -> PresentationClock {
    PresentationClock::Fresh {
        source_id: "camera".into(),
        epoch: 1,
        position_ns,
    }
}
fn binding() -> EventOverlayBinding {
    EventOverlayBinding {
        source_id: "camera".into(),
        event_id: Some("break".into()),
        origin: None,
        policy: EventOverlayPolicy {
            warning_lead_ns: 10,
            terminal_hold_ns: 5,
            uncertain_hold_ns: 7,
        },
    }
}
fn project(
    e: &SourceEvent,
    c: &PresentationClock,
    now_ns: u64,
    since_ns: u64,
) -> Option<EventOverlay> {
    project_event_overlay(
        e,
        c,
        &binding(),
        OverlayObservationTime {
            now_ns,
            lifecycle_since_ns: since_ns,
        },
    )
}
#[test]
fn warning_and_start_follow_exact_source_time() {
    let e = event(vec![start()]);
    assert!(project(&e, &clock(89), 0, 0).is_none());
    let warning = project(&e, &clock(90), 0, 0).unwrap();
    assert_eq!(warning.label, OverlayLabel::Upcoming);
    assert_eq!(
        warning.timer,
        Some(OverlayTimer {
            kind: OverlayTimerKind::UntilStart,
            nanoseconds: "10".into()
        })
    );
    let active = project(&e, &clock(100), 0, 0).unwrap();
    assert_eq!(active.label, OverlayLabel::Active);
    assert_eq!(active.timer.unwrap().nanoseconds, "50");
}
#[test]
fn expected_end_is_uncertain_not_confirmed_and_expires() {
    let e = event(vec![start()]);
    let ended = project(&e, &clock(150), 20, 20).unwrap();
    assert_eq!(ended.lifecycle, EventLifecycle::Uncertain);
    assert_eq!(ended.label, OverlayLabel::Overrun);
    assert_eq!(ended.timer.unwrap().nanoseconds, "0");
    let overrun = project(&e, &clock(153), 23, 20).unwrap();
    assert_eq!(overrun.timer.unwrap().kind, OverlayTimerKind::Overrun);
    assert!(project(&e, &clock(157), 27, 20).is_none());
}
#[test]
fn explicit_end_holds_then_clears_using_monotonic_not_media_time() {
    let e = event(vec![start(), EventBoundary::End { pts_ns: Some(140) }]);
    let active = project(&e, &clock(130), 1000, 0).unwrap();
    assert_eq!(active.timer.unwrap().nanoseconds, "10");
    assert_eq!(
        project(&e, &clock(140), 1000, 1000).unwrap().label,
        OverlayLabel::Ended
    );
    assert!(project(&e, &clock(140), 1004, 1000).is_some());
    assert!(project(&e, &clock(140), 1005, 1000).is_none());
}
#[test]
fn cancellation_and_epoch_change_clear_immediately() {
    let e = event(vec![start(), EventBoundary::Cancel]);
    assert!(project(&e, &PresentationClock::Stale, 0, 0).is_none());
    let e = event(vec![start()]);
    let c = PresentationClock::Fresh {
        source_id: "camera".into(),
        epoch: 2,
        position_ns: 100,
    };
    assert!(project(&e, &c, 0, 0).is_none());
}
#[test]
fn stale_clock_never_fabricates_countdown_and_expiry_does_not_reset() {
    let e = event(vec![start()]);
    let stale = project(&e, &PresentationClock::Stale, 30, 30).unwrap();
    assert_eq!(stale.label, OverlayLabel::Uncertain);
    assert!(stale.timer.is_none());
    assert!(project(&e, &PresentationClock::Unknown, 36, 30).is_some());
    assert!(project(&e, &PresentationClock::Stale, 37, 30).is_none());
}
#[test]
fn selection_checks_source_event_and_origin() {
    let e = event(vec![start()]);
    for b in [
        EventOverlayBinding {
            source_id: "other".into(),
            ..binding()
        },
        EventOverlayBinding {
            event_id: Some("other".into()),
            ..binding()
        },
        EventOverlayBinding {
            origin: Some(EventOrigin::External),
            ..binding()
        },
    ] {
        assert!(project_event_overlay(
            &e,
            &clock(100),
            &b,
            OverlayObservationTime {
                now_ns: 0,
                lifecycle_since_ns: 0
            }
        )
        .is_none());
    }
}
#[test]
fn unknown_duration_uses_elapsed_not_invented_remaining() {
    let e = event(vec![EventBoundary::Start {
        pts_ns: Some(100),
        duration_ns: None,
    }]);
    let timer = project(&e, &clock(120), 0, 0).unwrap().timer.unwrap();
    assert_eq!(timer.kind, OverlayTimerKind::Elapsed);
    assert_eq!(timer.nanoseconds, "20");
}
#[test]
fn vendor_evidence_has_no_break_timer_even_with_explicit_timestamps() {
    let mut e = event(vec![start()]);
    e.key.origin = EventOrigin::VendorMarker {
        vendor: "vendor".into(),
    };
    let overlay = project(&e, &clock(120), 0, 0).unwrap();
    assert_eq!(overlay.label, OverlayLabel::Observed);
    assert!(overlay.timer.is_none());
}
#[test]
fn full_signed_range_has_exact_web_safe_magnitude() {
    let e = event(vec![EventBoundary::Start {
        pts_ns: Some(i64::MIN),
        duration_ns: None,
    }]);
    let overlay = project(&e, &clock(i64::MAX), 0, 0).unwrap();
    assert_eq!(
        overlay.timer.as_ref().unwrap().nanoseconds,
        u64::MAX.to_string()
    );
    let json = serde_json::to_value(&overlay).unwrap();
    assert_eq!(json["timer"]["nanoseconds"], "18446744073709551615");
    assert_eq!(
        serde_json::from_value::<EventOverlay>(json).unwrap(),
        overlay
    );
}
#[test]
fn contradictory_end_never_becomes_a_countdown() {
    let e = event(vec![start(), EventBoundary::End { pts_ns: Some(90) }]);
    let overlay = project(&e, &clock(160), 0, 0).unwrap();
    assert_eq!(overlay.label, OverlayLabel::Uncertain);
    assert!(overlay.timer.is_none());
    let cancelled = event(vec![
        start(),
        EventBoundary::End { pts_ns: Some(90) },
        EventBoundary::Cancel,
    ]);
    assert!(project(&cancelled, &clock(160), 0, 0).is_none());
}
#[test]
fn marker_absence_clears_presence_without_claiming_end() {
    let e = event(vec![
        EventBoundary::MarkerObserved,
        EventBoundary::MarkerAbsent,
    ]);
    assert_eq!(e.project(&clock(100)).lifecycle, EventLifecycle::Observed);
    assert!(project(&e, &clock(100), 0, 0).is_none());
}
#[test]
fn absent_marker_never_resurrects_when_clock_is_lost() {
    for origin in [
        EventOrigin::Scte35,
        EventOrigin::VendorMarker { vendor: "v".into() },
    ] {
        let mut e = event(vec![
            EventBoundary::MarkerObserved,
            EventBoundary::MarkerAbsent,
        ]);
        e.key.origin = origin;
        for hold in [0, 7] {
            let mut b = binding();
            b.policy.uncertain_hold_ns = hold;
            for c in [
                clock(100),
                PresentationClock::Stale,
                PresentationClock::Unknown,
            ] {
                assert!(project_event_overlay(
                    &e,
                    &c,
                    &b,
                    OverlayObservationTime {
                        now_ns: 0,
                        lifecycle_since_ns: 0
                    }
                )
                .is_none());
                assert_ne!(e.project(&c).lifecycle, EventLifecycle::Ended);
            }
        }
    }
}
#[test]
fn zero_holds_clear_immediately() {
    let mut b = binding();
    b.policy.terminal_hold_ns = 0;
    b.policy.uncertain_hold_ns = 0;
    let e = event(vec![start(), EventBoundary::End { pts_ns: Some(140) }]);
    for c in [clock(140), PresentationClock::Stale] {
        assert!(project_event_overlay(
            &e,
            &c,
            &b,
            OverlayObservationTime {
                now_ns: 1,
                lifecycle_since_ns: 1
            }
        )
        .is_none());
    }
}
#[test]
fn epoch_identity_is_web_safe_above_number_precision() {
    let mut e = event(vec![start()]);
    e.key.epoch = u64::MAX;
    let c = PresentationClock::Fresh {
        source_id: "camera".into(),
        epoch: u64::MAX,
        position_ns: 120,
    };
    let overlay = project(&e, &c, 0, 0).unwrap();
    let json = serde_json::to_value(&overlay).unwrap();
    assert_eq!(json["key"]["epoch"], "18446744073709551615");
    assert_eq!(
        serde_json::from_value::<EventOverlay>(json).unwrap(),
        overlay
    );
}
#[test]
fn invalid_monotonic_times_fail_closed() {
    let e = event(vec![start()]);
    assert!(project(&e, &PresentationClock::Stale, 9, 10).is_none());
}
