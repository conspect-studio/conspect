//! Tests for the CONSPECT **local-metrics retention feed** (engine-seam S5): the
//! pure classifier that maps live engine [`Event`]s onto the consent-independent
//! [`RetentionStore`], plus the end-to-end seam (a simulated reconnect / incident
//! event published on the engine broadcast lands in the store).
//!
//! These prove the feed is genuinely recording from the running system's event
//! stream — not a dead data structure — by exercising the same broadcast +
//! lagged-skip pattern the engine→control alarm ingest uses (invariant #10: the
//! feed only ever reads the drop-oldest broadcast; it can never back-pressure the
//! engine).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::metrics_retention::{classify, ingest_step, IngestStep, RetentionUpdate};
use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;
use multiview_engine::EnginePublisher;
use multiview_events::{
    AlarmTransition, Alert, AlertSeverity, Event, GpuMetrics, GpuVendor, HealthWarning,
    LifecycleState, ShedLoad, ShedReason as WireShedReason, ShedScope, SystemMetrics, TileState,
    WarningCode, WarningSeverity,
};
use multiview_telemetry::retention::{
    IncidentKind, RetentionStore, RetentionWindow, ShedReason as StoreShedReason,
};

type Publisher = EnginePublisher<serde_json::Value, Event>;

fn system_metrics(cpu: f32, gpu: f32) -> SystemMetrics {
    SystemMetrics {
        cpu_util: cpu,
        mem_used_bytes: None,
        mem_total_bytes: None,
        self_cpu_util: None,
        self_mem_used_bytes: None,
        gpus: vec![GpuMetrics {
            id: "gpu-0".to_owned(),
            vendor: GpuVendor::Nvidia,
            name: None,
            compute_util: gpu,
            mem_used_bytes: 0,
            mem_total_bytes: 0,
            encoder_util: None,
            decoder_util: None,
            encoder_sessions: None,
            encoder_session_ceiling: None,
            self_compute_util: None,
            self_encoder_util: None,
            self_decoder_util: None,
            self_mem_used_bytes: None,
            self_encoder_sessions: None,
        }],
        program_fps: Some(60.0),
        sampled_hz: 1,
    }
}

#[test]
fn classifies_system_metrics_as_utilisation() {
    let event = Event::SystemMetrics(system_metrics(0.42, 0.55));
    let update = classify(&event).expect("system metrics is a utilisation sample");
    match update {
        RetentionUpdate::Utilisation(sample) => {
            assert!((sample.cpu_util - 0.42).abs() < 1e-4, "cpu carried through");
            // The GPU busy fraction is the max over the device list.
            assert_eq!(sample.gpu_util, Some(0.55_f32.into()));
            assert_eq!(sample.program_fps, Some(60.0_f32.into()));
        }
        other => panic!("expected Utilisation, got {other:?}"),
    }
}

#[test]
fn classifies_reconnecting_tile_state_as_reconnect() {
    let event = Event::TileState(TileState {
        from: LifecycleState::Live,
        to: LifecycleState::Reconnecting,
        input: Some("cam-7".to_owned()),
        trigger: "state_change".to_owned(),
    });
    let update = classify(&event).expect("a reconnecting transition is a reconnect");
    match update {
        RetentionUpdate::Reconnect { input_id, .. } => assert_eq!(input_id, "cam-7"),
        other => panic!("expected Reconnect, got {other:?}"),
    }
}

#[test]
fn non_reconnect_tile_transitions_are_not_recorded() {
    // A Live<->Stale transition is not a reconnect; it must not be classified as
    // one (it would pollute the reconnect history with non-reconnect events).
    let event = Event::TileState(TileState {
        from: LifecycleState::Live,
        to: LifecycleState::Stale,
        input: Some("cam-7".to_owned()),
        trigger: "state_change".to_owned(),
    });
    assert!(
        classify(&event).is_none(),
        "a non-reconnecting tile transition is not a retention update"
    );
}

#[test]
fn tile_state_without_an_input_is_ignored() {
    // A reconnect with no bound input cannot be attributed to a source.
    let event = Event::TileState(TileState {
        from: LifecycleState::Live,
        to: LifecycleState::Reconnecting,
        input: None,
        trigger: "state_change".to_owned(),
    });
    assert!(classify(&event).is_none());
}

#[test]
fn classifies_input_signal_loss_alarm_as_input_flap_incident() {
    let record = AlarmRecord::new(
        AlarmId::new("a-1"),
        AlarmKind::SignalLoss,
        PerceivedSeverity::Major,
        AlarmScope::Probe {
            id: "cam-2".to_owned(),
        },
        MediaTime::from_nanos(1),
    );
    let event = Event::AlarmRaised(AlarmTransition::new(record));
    let update = classify(&event).expect("a signal-loss alarm is an incident marker");
    match update {
        RetentionUpdate::Incident { kind, subject } => {
            assert_eq!(kind, IncidentKind::InputFlap);
            assert_eq!(subject, "cam-2");
        }
        other => panic!("expected Incident, got {other:?}"),
    }
}

#[test]
fn cleared_alarms_are_not_incident_markers() {
    // Only a raised/updated active alarm marks an incident; a clear is not an
    // incident occurrence.
    let record = AlarmRecord::new(
        AlarmId::new("a-2"),
        AlarmKind::SignalLoss,
        PerceivedSeverity::Cleared,
        AlarmScope::Probe {
            id: "cam-2".to_owned(),
        },
        MediaTime::from_nanos(1),
    );
    let event = Event::AlarmCleared(AlarmTransition::new(record));
    assert!(
        classify(&event).is_none(),
        "an alarm clear is not an incident"
    );
}

#[test]
fn classifies_health_warning_as_incident() {
    let event = Event::HealthWarningRaised(HealthWarning {
        code: WarningCode::GpuPresentNoVulkanAdapter,
        severity: WarningSeverity::Warning,
        subsystem: "compositor".to_owned(),
        message: "gpu present but software adapter resolved".to_owned(),
        remediation: "install the Vulkan loader".to_owned(),
        since: 1,
        active: true,
    });
    let update = classify(&event).expect("an active health warning is an incident marker");
    assert!(matches!(update, RetentionUpdate::Incident { .. }));
}

#[test]
fn classifies_shed_load_as_a_shed_update() {
    // The §7.2 shed-load category now has a live producer: a `shed.load` event
    // classifies into the store's Shed arm, mapping the wire reason onto the
    // store's reason faithfully.
    let event = Event::ShedLoad(ShedLoad {
        reason: WireShedReason::EncoderOverload,
        scope: ShedScope::Program,
        level: 1,
        dropped: 5,
    });
    let update = classify(&event).expect("a shed.load is a shed retention update");
    match update {
        RetentionUpdate::Shed { reason } => {
            assert_eq!(reason, StoreShedReason::EncoderOverload);
        }
        other => panic!("expected Shed, got {other:?}"),
    }
}

#[test]
fn shed_reason_maps_every_wire_reason() {
    // Every wire reason must map onto a store reason (no fabrication, no panic):
    // the classifier is the seam that lands a real shed in the store's category.
    for (wire, store) in [
        (WireShedReason::Pinned, StoreShedReason::Pinned),
        (WireShedReason::DisplayBound, StoreShedReason::DisplayBound),
        (WireShedReason::NoBetterHome, StoreShedReason::NoBetterHome),
        (WireShedReason::AntiStorm, StoreShedReason::AntiStorm),
        (
            WireShedReason::EncoderOverload,
            StoreShedReason::EncoderOverload,
        ),
    ] {
        let event = Event::ShedLoad(ShedLoad {
            reason: wire,
            scope: ShedScope::Shared,
            level: 0,
            dropped: 0,
        });
        match classify(&event).expect("shed.load classifies") {
            RetentionUpdate::Shed { reason } => assert_eq!(reason, store),
            other => panic!("expected Shed for {wire:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn end_to_end_a_published_shed_lands_in_the_store() {
    // The shed seam, exercised end-to-end: publish a `shed.load` on the engine's
    // outbound broadcast and pump one ingest step; the shed must appear in the
    // consent-independent store's shed category. This closes the §7.2 gap.
    let publisher: Arc<Publisher> = Arc::new(EnginePublisher::new(16));
    let store = Arc::new(RetentionStore::new());
    let mut sub = publisher.subscribe();

    let now = 77_000_000_u64;
    publisher.publish_event(Event::ShedLoad(ShedLoad {
        reason: WireShedReason::NoBetterHome,
        scope: ShedScope::Input {
            id: "cam-9".to_owned(),
        },
        level: 2,
        dropped: 11,
    }));

    let step = ingest_step(&mut sub, store.as_ref(), now).await;
    assert_eq!(step, IngestStep::Applied);

    let window = store.shed_window(now, RetentionWindow::LastHour);
    assert_eq!(window.len(), 1, "the published shed landed in the store");
    assert_eq!(window[0].reason, StoreShedReason::NoBetterHome);
}

#[test]
fn unrelated_events_are_not_classified() {
    // A control-plane alert with no retention category is ignored.
    let event = Event::AlertRaised(Alert {
        key: "k".to_owned(),
        severity: AlertSeverity::Info,
        title: "hello".to_owned(),
        detail: None,
        active: true,
    });
    assert!(classify(&event).is_none());
    assert!(classify(&Event::Ping).is_none());
}

#[tokio::test]
async fn end_to_end_a_published_reconnect_lands_in_the_store() {
    // The seam, exercised: publish a reconnecting tile-state on the engine's
    // outbound broadcast and pump one ingest step; the reconnect must appear in
    // the consent-independent store. This proves the feed records from the live
    // event stream, not a dead structure.
    let publisher: Arc<Publisher> = Arc::new(EnginePublisher::new(16));
    let store = Arc::new(RetentionStore::new());
    let mut sub = publisher.subscribe();

    let now = 12_345_678_u64;
    publisher.publish_event(Event::TileState(TileState {
        from: LifecycleState::Reconnecting,
        to: LifecycleState::Reconnecting,
        input: Some("cam-live".to_owned()),
        trigger: "state_change".to_owned(),
    }));

    let step = ingest_step(&mut sub, store.as_ref(), now).await;
    assert_eq!(step, IngestStep::Applied);

    let window = store.reconnect_window(now, RetentionWindow::LastHour);
    assert_eq!(
        window.len(),
        1,
        "the published reconnect landed in the store"
    );
    assert_eq!(window[0].input_id, "cam-live");
}

#[tokio::test]
async fn end_to_end_system_metrics_lands_as_utilisation() {
    let publisher: Arc<Publisher> = Arc::new(EnginePublisher::new(16));
    let store = Arc::new(RetentionStore::new());
    let mut sub = publisher.subscribe();

    let now = 50_000_000_u64;
    publisher.publish_event(Event::SystemMetrics(system_metrics(0.30, 0.20)));
    let step = ingest_step(&mut sub, store.as_ref(), now).await;
    assert_eq!(step, IngestStep::Applied);

    let summary = store
        .utilisation_summary(now, RetentionWindow::LastHour)
        .expect("a sample was recorded");
    assert_eq!(summary.samples, 1);
}
