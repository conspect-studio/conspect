use multiview_cli::source_event_service::*;
use multiview_core::source_event::*;
use std::time::{Duration, Instant};

fn observation(source: &str, epoch: u64) -> SourceEventObservation {
    SourceEventObservation {
        key: SourceEventKey {
            source_id: source.into(),
            epoch,
            origin: EventOrigin::Scte35,
            event_id: "cue".into(),
        },
        revision: 0,
        boundary: EventBoundary::Start {
            pts_ns: Some(10),
            duration_ns: None,
        },
        correlation_id: None,
        asset_id: None,
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}
#[test]
fn upstream_degradation_is_immediate_and_generation_scoped() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let view = service.registry();
    let old = service.register("a", 1).unwrap();
    old.mark_degraded();
    assert!(view.snapshot("a").unwrap().degraded);
    let new = service.register("a", 2).unwrap();
    old.mark_degraded();
    assert!(!view.snapshot("a").unwrap().degraded);
    old.retire();
    assert!(view.snapshot("a").is_some());
    new.retire();
    assert!(view.snapshot("a").is_none());
    assert_eq!(new.try_observe(observation("a", 2)), SubmitOutcome::Retired);
    drop(new);
    service.shutdown().unwrap();
}

#[test]
fn ownership_replacement_drop_and_shutdown() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let view = service.registry();
    let old = service.register("a", 1).unwrap();
    assert_eq!(
        old.try_observe(observation("b", 1)),
        SubmitOutcome::WrongSource
    );
    assert_eq!(
        old.try_observe(observation("a", 2)),
        SubmitOutcome::WrongEpoch
    );
    assert_eq!(old.try_observe(observation("a", 1)), SubmitOutcome::Queued);
    wait(|| view.snapshot("a").is_some_and(|s| s.events.len() == 1));
    let retained = view.snapshot("a").unwrap();
    let new = service.register("a", 2).unwrap();
    assert_eq!(old.try_observe(observation("a", 1)), SubmitOutcome::Retired);
    assert!(view.snapshot("a").unwrap().events.is_empty());
    assert_eq!(retained.epoch, 1); // already acquired immutable views cannot be revoked
    assert_eq!(new.try_observe(observation("a", 2)), SubmitOutcome::Queued);
    wait(|| {
        view.snapshot("a")
            .is_some_and(|s| s.epoch == 2 && s.events.len() == 1)
    });
    drop(new);
    assert!(view.snapshot("a").is_none());
    service.shutdown().unwrap();
    assert!(view.is_stopped());
}
#[test]
fn bounded_registry_and_replay_floor() {
    let service = SourceEventService::new(ServiceLimits {
        max_sources: 1,
        ..ServiceLimits::default()
    })
    .unwrap();
    let p = service.register("a", 4).unwrap();
    assert!(matches!(
        service.register("b", 1),
        Err(RegisterError::Capacity)
    ));
    drop(p);
    assert!(matches!(
        service.register("a", 4),
        Err(RegisterError::StaleEpoch)
    ));
    assert!(matches!(
        service.register("a", 3),
        Err(RegisterError::StaleEpoch)
    ));
    service.shutdown().unwrap();
}
#[test]
fn overflow_is_immediately_fail_closed() {
    let service = SourceEventService::new(ServiceLimits {
        queue_capacity: 0,
        ..ServiceLimits::default()
    })
    .unwrap();
    let view = service.registry();
    let p = service.register("a", 1).unwrap();
    wait(|| p.try_observe(observation("a", 1)) == SubmitOutcome::Overflow);
    let s = view.snapshot("a").unwrap();
    assert!(s.degraded);
    assert!(s.queue_overflow > 0);
    service.shutdown().unwrap();
    assert_eq!(p.try_observe(observation("a", 1)), SubmitOutcome::Retired);
    assert!(view.snapshot("a").is_none());
}
#[test]
fn queued_old_generation_never_contaminates_replacement() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let view = service.registry();
    let old = service.register("a", 1).unwrap();
    let writer = std::thread::spawn(move || {
        for revision in 0..100 {
            let mut o = observation("a", 1);
            o.revision = revision;
            if old.try_observe(o) == SubmitOutcome::Retired {
                break;
            }
        }
    });
    let new = service.register("a", 2).unwrap();
    writer.join().unwrap();
    assert_eq!(new.try_observe(observation("a", 2)), SubmitOutcome::Queued);
    wait(|| view.snapshot("a").is_some_and(|s| !s.events.is_empty()));
    for _ in 0..1000 {
        let s = view.snapshot("a").unwrap();
        assert_eq!(s.epoch, 2);
        assert!(!s.degraded);
        assert!(s.events.iter().all(|e| e.key.epoch == 2));
    }
    service.shutdown().unwrap();
}
#[test]
fn capacity_and_invalid_payload_degrade_without_eviction() {
    let service = SourceEventService::new(ServiceLimits {
        tracker: TrackerLimits {
            max_events: 1,
            ..TrackerLimits::default()
        },
        ..ServiceLimits::default()
    })
    .unwrap();
    let view = service.registry();
    let p = service.register("a", 1).unwrap();
    assert_eq!(p.try_observe(observation("a", 1)), SubmitOutcome::Queued);
    let mut second = observation("a", 1);
    second.key.event_id = "other".into();
    assert_eq!(p.try_observe(second), SubmitOutcome::Queued);
    wait(|| {
        view.snapshot("a")
            .is_some_and(|s| s.diagnostics.capacity_exceeded == 1)
    });
    let s = view.snapshot("a").unwrap();
    assert!(s.degraded);
    assert_eq!(s.events.len(), 1);
    assert_eq!(s.events[0].key.event_id, "cue");
    let mut huge = observation("a", 1);
    huge.asset_id = Some("x".repeat(1025));
    assert_eq!(p.try_observe(huge), SubmitOutcome::Invalid);
    service.shutdown().unwrap();
}
#[test]
fn drop_service_stops_worker_with_live_producer() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let view = service.registry();
    let p = service.register("a", 1).unwrap();
    drop(service);
    wait(|| view.worker_finished());
    assert!(view.snapshot("a").is_none());
    assert_eq!(p.try_observe(observation("a", 1)), SubmitOutcome::Retired);
}

#[test]
fn delayed_snapshot_requires_current_epoch_and_live_owner() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let view = service.registry();
    let controller = service.registration_controller();
    let old = controller.register("a", 10).unwrap();
    let acquired = view.snapshot("a").unwrap();
    assert!(view.is_current(&acquired));
    let replacement = controller.clone().register("a", 11).unwrap();
    assert!(!view.is_current(&acquired));
    assert_eq!(acquired.epoch, 10);
    drop(old);
    let current = view.snapshot("a").unwrap();
    assert!(view.is_current(&current));
    drop(replacement);
    assert!(!view.is_current(&current));
    drop(service);
    assert!(matches!(
        controller.register("a", 12),
        Err(RegisterError::Stopped)
    ));
    wait(|| view.worker_finished());
}

#[test]
fn concurrent_controllers_serialize_epoch_checks_and_preserve_sources() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let controller = service.registration_controller();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let writers: Vec<_> = (0..2)
        .map(|_| {
            let controller = controller.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                controller.register("a", 7)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(RegisterError::StaleEpoch)))
            .count(),
        1
    );
    let other = controller.register("b", 1).unwrap();
    assert_eq!(service.registry().snapshot("a").unwrap().epoch, 7);
    assert_eq!(service.registry().snapshot("b").unwrap().epoch, 1);
    drop(other);
    assert!(service.registry().snapshot("a").is_some());
    service.shutdown().unwrap();
}
