//! The conflating broadcaster + session-pump ring rule (ADR-RT007), the
//! highest-risk part of DEV-A3.
//!
//! The `devices` topic is the one mixed-cadence lane: `device.status` /
//! `timing.status` are **conflated, latest-wins** telemetry excluded from the
//! lossless replay ring, while `device.adopted`/`.removed`/`.mode`/`.error`/
//! `.sync` are lossless lifecycle events that replay after a gap. The session
//! pump applies `topic.is_high_rate() || event.is_conflated()` to decide
//! ring-exclusion per event type. These tests prove:
//!
//! 1. resume-after-gap replays lifecycle events losslessly;
//! 2. resume-after-gap does NOT replay stale conflated status — the client
//!    re-snapshots instead;
//! 3. the broadcaster never back-pressures the engine (invariant #10): every
//!    publish is non-blocking even when a slow client never drains.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_control::devices::{DeviceBroadcaster, DeviceStatusRegistry};
use multiview_control::SessionStream;
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, Event};

type Publisher = EnginePublisher<serde_json::Value, Event>;

#[tokio::test]
async fn resume_after_gap_replays_lifecycle_losslessly() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(DeviceStatusRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&registry));

    // A live subscriber so events buffer for resume.
    let sub = engine.subscribe();

    // Three lifecycle events the operator must never lose.
    let s_adopt = broadcaster.adopted("dev-a", multiview_config::DeviceDriver::Zowietek, None);
    let _s_mode = broadcaster.mode_started("dev-a", "encoder");
    let s_removed = broadcaster.removed("dev-a");
    assert!(s_removed > s_adopt);

    // The client had already observed the adopt (resume strictly after it).
    let mut session = SessionStream::new(sub, "sess-resume", Some(s_adopt));

    // The adopt is skipped (already observed); mode + removed replay losslessly.
    assert_eq!(session.next_delta().await.unwrap(), None, "adopt skipped");

    let d_mode = session.next_delta().await.unwrap().expect("mode replays");
    assert!(
        matches!(d_mode.envelope.payload, Event::DeviceMode(_)),
        "device.mode replays after the gap, got {:?}",
        d_mode.envelope.payload
    );

    let d_removed = session
        .next_delta()
        .await
        .unwrap()
        .expect("removed replays");
    assert!(
        matches!(d_removed.envelope.payload, Event::DeviceRemoved(_)),
        "device.removed replays after the gap, got {:?}",
        d_removed.envelope.payload
    );
}

#[tokio::test]
async fn resume_after_gap_excludes_conflated_status_from_the_ring() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(DeviceStatusRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&registry));

    let sub = engine.subscribe();

    // A lifecycle anchor the client has already seen, then a flurry of status
    // samples (conflated) interleaved with a lossless lifecycle event.
    let s_adopt = broadcaster.adopted("dev-a", multiview_config::DeviceDriver::Zowietek, None);
    let _s1 = broadcaster.status("dev-a", DeviceState::Online);
    let _s2 = broadcaster.status("dev-a", DeviceState::Degraded);
    let _s3 = broadcaster.status("dev-a", DeviceState::Online);
    let _s_err = broadcaster.error("dev-a", "decode stalled");

    let mut session = SessionStream::new(sub, "sess-resume", Some(s_adopt));

    // Drain every replayed delta. The conflated `device.status` samples in the
    // gap are EXCLUDED from the lossless replay (re-snapshot heals them); only
    // the lossless lifecycle event (`device.error`) is delivered.
    let mut delivered: Vec<&'static str> = Vec::new();
    for _ in 0..8 {
        if let Some(frame) = session.next_delta().await.unwrap() {
            match frame.envelope.payload {
                Event::DeviceStatus(_) => delivered.push("device.status"),
                Event::DeviceError(_) => delivered.push("device.error"),
                Event::DeviceAdopted(_) => delivered.push("device.adopted"),
                _ => delivered.push("other"),
            }
        }
    }
    assert!(
        !delivered.contains(&"device.status"),
        "conflated device.status must NOT replay from the gap ring; got {delivered:?}"
    );
    assert!(
        delivered.contains(&"device.error"),
        "lossless device.error must replay after the gap; got {delivered:?}"
    );

    // The fresh snapshot the resuming client rebuilds from carries the LATEST
    // conflated status (latest-wins), not a stale gap sample.
    let snap = registry
        .snapshot("dev-a")
        .expect("a snapshot for the live device");
    assert_eq!(
        snap.state,
        DeviceState::Online,
        "the re-snapshot heals status to the latest value"
    );
}

#[tokio::test]
async fn fresh_connection_delivers_a_device_snapshot_then_live_status_deltas() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(DeviceStatusRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&registry));

    // Seed a status before the client connects.
    broadcaster.status("dev-a", DeviceState::Online);

    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-fresh", None);

    // A fresh client gets the device-registry snapshot of current statuses.
    let snap = session
        .devices_snapshot_frame(&registry, engine.state.sequence())
        .expect("a device snapshot when statuses exist");
    match &snap.envelope.payload {
        Event::DeviceStatus(status) => {
            assert_eq!(status.device_id, "dev-a");
            assert_eq!(status.state, DeviceState::Online);
        }
        other => panic!("expected a device.status snapshot, got {other:?}"),
    }

    // A live status sample after connect flows through as a delta.
    broadcaster.status("dev-a", DeviceState::Degraded);
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("a live status delta");
    assert!(matches!(d.envelope.payload, Event::DeviceStatus(_)));
}

#[tokio::test]
async fn broadcaster_never_back_pressures_a_slow_client() {
    // A tiny ring; publish far more than capacity while nothing drains. Every
    // publish must return promptly (non-blocking) — the invariant #10 property
    // for the control-plane device broadcaster.
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(4));
    let registry = Arc::new(DeviceStatusRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&registry));
    let _sub = engine.subscribe();

    let pump = async {
        for i in 0..1000 {
            broadcaster.status("dev-a", DeviceState::Online);
            if i % 50 == 0 {
                broadcaster.error("dev-a", "transient");
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), pump)
        .await
        .expect("the device broadcaster must never block on a slow consumer");
}
