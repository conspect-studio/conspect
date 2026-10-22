//! The cast driver's runtime wiring (DEV-D2): the [`CastSessionFactory`]
//! plugs into the SAME `DevicePollerRegistry`/factory + tombstone machinery
//! the zowietek driver uses (DEV-A4) — composed via the
//! [`CompositePollerFactory`], honouring the delete-tombstone race
//! protections for cast ids too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_config::Device;
use multiview_control::devices::cast::media::{CastDelivery, CastMediaTarget, HlsSegmentFormat};
use multiview_control::devices::cast::runtime::CastSessionFactory;
use multiview_control::devices::cast::session::{
    CastSessionConfig, ScriptedChannel, ScriptedConnector, ScriptedInbound,
};
use multiview_control::devices::{
    CompositePollerFactory, DeviceBroadcaster, DeviceDriverRegistry, DevicePollerFactory,
    DevicePollerRegistry, DeviceStatusRegistry, PollerHandle, PollerWiring,
};
use multiview_control::EngineStateSnapshot;
use multiview_engine::EnginePublisher;
use multiview_events::Event;

/// The control-plane wiring + the status registry the assertions read.
fn wiring() -> (PollerWiring, Arc<DeviceStatusRegistry>) {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let status = Arc::new(DeviceStatusRegistry::new());
    let wiring = PollerWiring {
        broadcaster: DeviceBroadcaster::new(engine, Arc::clone(&status)),
        drivers: Arc::new(DeviceDriverRegistry::new()),
        cast_sessions: std::sync::Arc::new(
            multiview_control::devices::cast::store::CastSessionStore::new(),
        ),
        clock: std::sync::Arc::new(|| multiview_core::time::MediaTime::from_nanos(0)),
    };
    (wiring, status)
}

/// A delivery map carrying one HLS rendition for `out-a`.
fn delivery() -> Arc<CastDelivery> {
    let mut d = CastDelivery::new();
    d.insert(
        "out-a",
        CastMediaTarget {
            url: "http://192.0.2.7:8080/hls/out-a/a.m3u8".to_owned(),
            format: HlsSegmentFormat::MpegTs,
        },
    );
    Arc::new(d)
}

/// A device document parsed from JSON (the route/store path's shape).
fn device(body: serde_json::Value) -> Device {
    serde_json::from_value(body).expect("a valid device document")
}

/// A factory over a connector whose every connect is refused (no scripted
/// channels): good enough to observe *whether* an actor was spawned.
fn refused_factory() -> CastSessionFactory<ScriptedConnector> {
    CastSessionFactory::new(
        Arc::new(ScriptedConnector::new(vec![])),
        delivery(),
        CastSessionConfig::test_fast(),
    )
}

#[test]
fn factory_only_manages_cast_devices() {
    let (wiring, _status) = wiring();
    let factory = refused_factory();
    // A zowietek device is not ours.
    assert!(factory
        .spawn(
            &device(serde_json::json!({
                "id": "dev-z", "driver": "zowietek", "address": "http://[fd00:db8::42]"
            })),
            &wiring,
        )
        .is_none());
    // A displaynode is not ours either.
    assert!(factory
        .spawn(
            &device(serde_json::json!({ "id": "dev-n", "driver": "displaynode" })),
            &wiring,
        )
        .is_none());
}

#[test]
fn factory_spawns_for_a_cast_device_with_an_output_assignment() {
    let (wiring, _status) = wiring();
    let factory = refused_factory();
    let handle = factory.spawn(
        &device(serde_json::json!({
            "id": "dev-c",
            "driver": "cast",
            "address": "[2001:db8::20]:8009",
            "display": { "assign": { "output": "out-a" } }
        })),
        &wiring,
    );
    assert!(
        handle.is_some(),
        "a cast device with a rendition gets a session actor"
    );
}

#[test]
fn factory_resolves_the_program_assignment_to_the_first_rendition() {
    let (wiring, _status) = wiring();
    let factory = refused_factory();
    let handle = factory.spawn(
        &device(serde_json::json!({
            "id": "dev-c",
            "driver": "cast",
            "address": "192.0.2.20",
            "display": { "assign": { "program": true } }
        })),
        &wiring,
    );
    assert!(
        handle.is_some(),
        "program assign casts the first HLS rendition"
    );
}

#[test]
fn factory_spawns_nothing_without_a_resolvable_rendition() {
    let (wiring, _status) = wiring();
    let factory = refused_factory();
    // No display assignment: nothing to cast — honestly no actor (the device
    // record rides ADOPTING; the operator names a rendition to cast).
    assert!(factory
        .spawn(
            &device(serde_json::json!({
                "id": "dev-c", "driver": "cast", "address": "192.0.2.20"
            })),
            &wiring,
        )
        .is_none());
    // A wall head is not an HLS rendition (ADR-M011).
    assert!(factory
        .spawn(
            &device(serde_json::json!({
                "id": "dev-c", "driver": "cast", "address": "192.0.2.20",
                "display": { "assign": { "wall_head": "head-l" } }
            })),
            &wiring,
        )
        .is_none());
    // An output id with no HLS mount resolves to nothing.
    assert!(factory
        .spawn(
            &device(serde_json::json!({
                "id": "dev-c", "driver": "cast", "address": "192.0.2.20",
                "display": { "assign": { "output": "not-hls" } }
            })),
            &wiring,
        )
        .is_none());
}

#[test]
fn composite_factory_dispatches_to_the_first_managing_member() {
    /// A factory that never manages anything.
    struct Never;
    impl DevicePollerFactory for Never {
        fn spawn(&self, _device: &Device, _wiring: &PollerWiring) -> Option<PollerHandle> {
            None
        }
    }
    let (wiring, _status) = wiring();
    let composite = CompositePollerFactory::new(vec![Arc::new(Never), Arc::new(refused_factory())]);
    let handle = composite.spawn(
        &device(serde_json::json!({
            "id": "dev-c", "driver": "cast", "address": "192.0.2.20",
            "display": { "assign": { "output": "out-a" } }
        })),
        &wiring,
    );
    assert!(handle.is_some(), "the cast member manages the cast device");
    assert!(composite
        .spawn(
            &device(serde_json::json!({ "id": "dev-n", "driver": "displaynode" })),
            &wiring,
        )
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_tombstone_is_honoured_for_cast_ids() {
    let (wiring, status) = wiring();
    let registry = DevicePollerRegistry::with_factory(Arc::new(refused_factory()));
    let dev = device(serde_json::json!({
        "id": "cast-session-1", "driver": "cast", "address": "192.0.2.20",
        "display": { "assign": { "output": "out-a" } }
    }));

    // Start, then stop: the id is tombstoned and a late `start` is rejected
    // BEFORE any task is spawned — the DEV-A4 delete-vs-adopt determinism,
    // reused verbatim for cast session ids.
    assert!(registry.start(&dev, &wiring));
    assert!(registry.is_running("cast-session-1"));
    registry.stop("cast-session-1").await;
    assert!(!registry.is_running("cast-session-1"));
    assert!(!registry.start(&dev, &wiring), "tombstoned id is rejected");

    // A fresh create clears the tombstone and starts cleanly.
    registry.clear_tombstone("cast-session-1");
    assert!(registry.start(&dev, &wiring));
    registry.stop("cast-session-1").await;
    let _ = status; // (status registry unused in this lifecycle check)
}

#[tokio::test(start_paused = true)]
async fn stop_graceful_joins_a_voluntary_exit_after_the_receiver_stop() {
    use multiview_control::devices::cast::protocol::{CastFrame, NS_RECEIVER};

    let (wiring, _status) = wiring();

    // An actor that exits voluntarily on StopCast (the scripted session
    // establishes, then idles): stop_graceful's grace window lets it run its
    // teardown (the receiver STOP) instead of aborting it mid-send.
    let established = CastFrame {
        namespace: NS_RECEIVER.to_owned(),
        source: "receiver-0".to_owned(),
        destination: "sender-0".to_owned(),
        payload: serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [{
                "appId": "CC1AD845", "sessionId": "s-1", "transportId": "t-1"
            }] }
        })
        .to_string(),
    };
    let (ch, sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(established),
        ScriptedInbound::Hang,
    ]);
    let factory = CastSessionFactory::new(
        Arc::new(ScriptedConnector::new(vec![ch])),
        delivery(),
        CastSessionConfig::test_fast(),
    );
    let registry = DevicePollerRegistry::with_factory(Arc::new(factory));
    let dev = device(serde_json::json!({
        "id": "cast-session-2", "driver": "cast", "address": "192.0.2.20",
        "display": { "assign": { "output": "out-a" } }
    }));
    assert!(registry.start(&dev, &wiring));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(registry.dispatch(
        "cast-session-2",
        multiview_control::devices::PollerControl::StopCast
    ));
    registry
        .stop_graceful("cast-session-2", Duration::from_secs(2))
        .await;
    assert!(!registry.is_running("cast-session-2"));
    assert!(
        sent.lock()
            .expect("sent log")
            .iter()
            .any(|f| f.payload.contains("\"STOP\"")),
        "the graceful stop let the actor STOP the receiver app"
    );
    // The tombstone protects this id exactly like a plain stop.
    assert!(!registry.start(&dev, &wiring));
}

#[tokio::test(start_paused = true)]
async fn stop_graceful_aborts_an_actor_that_ignores_the_grace_window() {
    use multiview_control::devices::PollerControl;

    /// A factory whose "actor" never exits on its own (a pending task): the
    /// grace window must elapse and the abort must still tear it down.
    struct Wedged;
    impl DevicePollerFactory for Wedged {
        fn spawn(&self, _device: &Device, _wiring: &PollerWiring) -> Option<PollerHandle> {
            let (tx, _rx) = tokio::sync::mpsc::channel::<PollerControl>(1);
            Some(PollerHandle::new(
                tx,
                tokio::spawn(std::future::pending::<()>()),
            ))
        }
    }

    let (wiring, _status) = wiring();
    let registry = DevicePollerRegistry::with_factory(Arc::new(Wedged));
    let dev = device(serde_json::json!({
        "id": "cast-session-3", "driver": "cast", "address": "192.0.2.20",
        "display": { "assign": { "output": "out-a" } }
    }));
    assert!(registry.start(&dev, &wiring));
    registry
        .stop_graceful("cast-session-3", Duration::from_millis(100))
        .await;
    assert!(!registry.is_running("cast-session-3"));
    assert!(!registry.start(&dev, &wiring), "tombstoned after the abort");
}
