use multiview_cli::{
    source_event_ingest::IngestController,
    source_event_service::{RegisterError, ServiceLimits, SourceEventService},
};
use multiview_core::time::MediaTime;
use multiview_framestore::tile::SourceFrameTime;

#[test]
fn reconnect_and_replacement_fence_old_capabilities() {
    let service = SourceEventService::new(ServiceLimits::default()).expect("worker");
    let owner = IngestController::new(service.registration_controller(), 1);
    let first = owner.activate("a", 70).expect("activate");
    let c1 = first.connect().expect("connect");
    let (one, _p1) = c1.register(9).expect("mapping");
    let c2 = first.connect().expect("reconnect");
    assert!(matches!(c1.register(10), Err(RegisterError::StaleEpoch)));
    let (two, _p2) = c2.register(0).expect("new mapping starts at zero");
    assert!(two.clock.connection_epoch > one.clock.connection_epoch);
    assert!(two.service_epoch > one.service_epoch);
    assert_ne!(two.service_epoch, two.lifecycle);
    let replacement = owner.activate("a", 71).expect("replacement");
    assert!(matches!(first.connect(), Err(RegisterError::StaleEpoch)));
    assert!(matches!(c2.register(1), Err(RegisterError::StaleEpoch)));
    let (three, _p3) = replacement
        .connect()
        .expect("connect")
        .register(0)
        .expect("register");
    assert!(three.service_epoch > two.service_epoch);
    assert!(three.clock.connection_epoch > two.clock.connection_epoch);
    assert!(matches!(
        owner.activate("b", 1),
        Err(RegisterError::Capacity)
    ));
    service.shutdown().expect("join");
}

#[test]
fn equal_numeric_contexts_cannot_match_another_sources_evidence() {
    let service = SourceEventService::new(ServiceLimits::default()).expect("worker");
    let owner = IngestController::new(service.registration_controller(), 2);
    let a = owner.activate("a", 77).expect("activate a");
    let b = owner.activate("b", 77).expect("activate b");
    let (a_context, _a_producer) = a
        .connect()
        .expect("connect a")
        .register(4)
        .expect("register a");
    let (b_context, _b_producer) = b
        .connect()
        .expect("connect b")
        .register(4)
        .expect("register b");
    assert_eq!(a_context, b_context);
    let frame = SourceFrameTime {
        pts: MediaTime::ZERO,
        generation: a_context.lifecycle,
        event_clock: Some(a_context.clock),
    };
    let a_evidence = service.registry().snapshot("a").expect("a snapshot");
    let b_evidence = service.registry().snapshot("b").expect("b snapshot");
    assert!(a.context_reader().matches(&a_evidence, frame));
    assert!(b.context_reader().matches(&b_evidence, frame));
    assert!(!a.context_reader().matches(&b_evidence, frame));
    assert!(!b.context_reader().matches(&a_evidence, frame));
    service.shutdown().expect("join");
}

#[test]
fn exact_selected_frame_identity_and_retirement() {
    let service = SourceEventService::new(ServiceLimits::default()).expect("worker");
    let owner = IngestController::new(service.registration_controller(), 1);
    let source = owner.activate("a", 77).expect("activate");
    let reader = source.context_reader();
    let connection = source.connect().expect("connect");
    let (context, _producer) = connection.register(4).expect("register");
    let frame = SourceFrameTime {
        pts: MediaTime::ZERO,
        generation: 77,
        event_clock: Some(context.clock),
    };
    let evidence = service.registry().snapshot("a").expect("snapshot");
    assert!(reader.matches(&evidence, frame));
    assert!(!reader.matches(
        &evidence,
        SourceFrameTime {
            generation: context.service_epoch,
            ..frame
        }
    ));
    assert!(!reader.matches(
        &evidence,
        SourceFrameTime {
            event_clock: None,
            ..frame
        }
    ));
    assert!(matches!(
        connection.register(4),
        Err(RegisterError::StaleEpoch)
    ));
    let (_next, _next_producer) = connection.register(5).expect("mapping advance");
    assert!(!reader.matches(&evidence, frame));
    source.retire();
    assert!(matches!(
        connection.register(6),
        Err(RegisterError::StaleEpoch)
    ));
    assert!(matches!(source.connect(), Err(RegisterError::StaleEpoch)));
    let replacement = owner.activate("a", 78).expect("reactivate");
    assert!(replacement.connect().is_ok());
    service.shutdown().expect("join");
}
