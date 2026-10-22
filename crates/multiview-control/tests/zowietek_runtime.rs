//! The runtime [`DevicePollerRegistry`] start/stop lifecycle (DEV-A4 review
//! fix): a delete's `stop` and a racing adopt's `start` must be deterministic —
//! after a stop, **no** poller for that id can be running or get started (a
//! tombstone rejects the late start before any task is spawned), no ghost
//! `device_status`/driver entries are re-created, and a later legitimate
//! re-adopt (which clears the tombstone, as the create route does before its
//! store insert) still works. Pollers are real actors over the scripted
//! transport — the whole path is socket-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Test prose names lifecycle states in plain caps (ONLINE/ADOPTING) for
    // readability; not API items needing backticks.
    clippy::doc_markdown
)]

use std::sync::{Arc, Barrier};
use std::time::Duration;

use multiview_config::Device;
use multiview_control::devices::zowietek::client::ScriptedTransport;
use multiview_control::devices::zowietek::poller::PollerConfig;
use multiview_control::devices::zowietek::ZowietekDriver;
use multiview_control::devices::{
    DeviceBroadcaster, DeviceDriverRegistry, DevicePollerFactory, DevicePollerRegistry,
    DeviceStatusRegistry, PollerHandle, PollerWiring, ZowietekPoller,
};
use multiview_engine::EnginePublisher;
use multiview_events::DeviceState;
use serde_json::json;

/// The control-plane wiring + the registries the assertions read.
fn wiring() -> (
    PollerWiring,
    Arc<DeviceStatusRegistry>,
    Arc<DeviceDriverRegistry>,
) {
    let engine = Arc::new(EnginePublisher::<serde_json::Value, multiview_events::Event>::new(64));
    let status = Arc::new(DeviceStatusRegistry::new());
    let drivers = Arc::new(DeviceDriverRegistry::new());
    let wiring = PollerWiring {
        broadcaster: DeviceBroadcaster::new(engine, Arc::clone(&status)),
        drivers: Arc::clone(&drivers),
        cast_sessions: std::sync::Arc::new(
            multiview_control::devices::cast::store::CastSessionStore::new(),
        ),
        clock: std::sync::Arc::new(|| multiview_core::time::MediaTime::from_nanos(0)),
    };
    (wiring, status, drivers)
}

/// A minimal valid `zowietek` device document.
fn device(id: &str) -> Device {
    serde_json::from_value(json!({
        "id": id,
        "driver": "zowietek",
        "address": "http://[fd00:db8::42]"
    }))
    .expect("a valid device")
}

/// Build a real poller actor over a scripted transport (login + probe scripted;
/// unscripted polls fall back to benign successes, so the actor stays ONLINE).
fn scripted_poller(device_id: &str, wiring: &PollerWiring) -> PollerHandle {
    let transport = ScriptedTransport::new();
    transport.push(
        "system",
        multiview_control::devices::zowietek::client::ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "uuid": "u", "type": 0 } }),
        ),
    );
    transport.push(
        "venc",
        multiview_control::devices::zowietek::client::ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    let driver = ZowietekDriver::new(
        device_id,
        Arc::new(transport),
        wiring.broadcaster.clone(),
        Arc::clone(&wiring.drivers),
        "admin",
        "admin",
    );
    let poller = ZowietekPoller::new(
        device_id,
        driver,
        Arc::clone(wiring.broadcaster.registry()),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    );
    poller.spawn()
}

/// A factory that spawns a real scripted poller for every device.
struct ScriptedFactory;

impl DevicePollerFactory for ScriptedFactory {
    fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle> {
        Some(scripted_poller(&device.id, wiring))
    }
}

/// A factory that rendezvouses inside `spawn` so a test can deterministically
/// interleave a concurrent `stop` with an in-flight `start` (the
/// start-before-insert window the old registry left open).
struct GatedScriptedFactory {
    /// The test rendezvouses here once `spawn` has been entered.
    entered: Arc<Barrier>,
    /// `spawn` proceeds (spawning the real poller) after this rendezvous.
    proceed: Arc<Barrier>,
}

impl DevicePollerFactory for GatedScriptedFactory {
    fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle> {
        self.entered.wait();
        self.proceed.wait();
        Some(scripted_poller(&device.id, wiring))
    }
}

/// The delete-vs-adopt race outcome (the start-before-insert window): the
/// delete's `stop` completes while the racing adopt's `start` has not yet
/// inserted its handle. The late `start` must be **rejected** — no task spawned,
/// no poller running, and no ghost `device_status`/driver entries re-created
/// after the delete's `forget`s.
#[tokio::test]
async fn late_start_after_stop_is_rejected_and_recreates_nothing() {
    let (wiring, status, drivers) = wiring();
    let reg = DevicePollerRegistry::with_factory(Arc::new(ScriptedFactory));

    // The delete route's sequence for a device whose racing create has not yet
    // inserted its poller: stop (removes nothing — the window), then forget.
    reg.stop("dev-a").await;
    status.forget("dev-a");
    drivers.forget("dev-a");

    // The racing create's start arrives AFTER the delete finished: it must be
    // rejected outright (tombstoned id) — not insert a live ghost poller.
    assert!(
        !reg.start(&device("dev-a"), &wiring),
        "a start for a stopped/deleted id is rejected (tombstone)"
    );
    assert!(!reg.is_running("dev-a"), "no ghost poller is running");

    // Nothing was spawned, so nothing can re-create the runtime entries the
    // delete just forgot — even after a settle delay.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        status.snapshot("dev-a").is_none(),
        "no ghost device_status entry was re-created after delete"
    );
    assert!(
        drivers.source_candidates("dev-a").is_empty(),
        "no ghost driver facet entries were re-created after delete"
    );
}

/// `stop` awaits the aborted actor's termination: once `stop` returns, the
/// poller task is fully gone, so the delete's subsequent `forget`s cannot race
/// a final in-flight publish (no ghost entries survive the delete).
#[tokio::test]
async fn stop_joins_the_running_poller_so_forget_cannot_race_it() {
    let (wiring, status, drivers) = wiring();
    let reg = DevicePollerRegistry::with_factory(Arc::new(ScriptedFactory));
    assert!(reg.start(&device("dev-a"), &wiring), "the poller starts");
    assert!(reg.is_running("dev-a"));

    // Let the actor adopt and publish (it rides to ONLINE on the script).
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if status.state("dev-a") == Some(DeviceState::Online) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the spawned poller adopts to ONLINE");

    // The delete route: stop (awaits the task's termination), THEN forget.
    reg.stop("dev-a").await;
    assert!(!reg.is_running("dev-a"), "no poller survives a stop");
    status.forget("dev-a");
    drivers.forget("dev-a");

    // The task terminated before the forgets ran, so nothing republishes.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        status.snapshot("dev-a").is_none(),
        "no status entry re-appeared after stop + forget"
    );
    assert!(
        drivers.source_candidates("dev-a").is_empty(),
        "no driver facet entries re-appeared after stop + forget"
    );
}

/// A later **legitimate** re-adopt of a deleted id still works: the create
/// route clears the tombstone before its store insert, after which `start`
/// spawns a fresh poller that adopts to ONLINE.
#[tokio::test]
async fn readopt_after_delete_clears_the_tombstone_and_starts_fresh() {
    let (wiring, status, _drivers) = wiring();
    let reg = DevicePollerRegistry::with_factory(Arc::new(ScriptedFactory));

    // Adopt, then delete (stop tombstones the id).
    assert!(reg.start(&device("dev-a"), &wiring));
    reg.stop("dev-a").await;
    status.forget("dev-a");
    assert!(!reg.is_running("dev-a"));

    // The fresh create: clear the tombstone (as the route does before its store
    // insert), then start — the re-adopt must spawn a live poller again.
    reg.clear_tombstone("dev-a");
    assert!(
        reg.start(&device("dev-a"), &wiring),
        "a fresh create after delete starts a new poller"
    );
    assert!(reg.is_running("dev-a"), "the re-adopted poller is running");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if status.state("dev-a") == Some(DeviceState::Online) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the re-adopted poller adopts to ONLINE");
}

/// The interleaving exercised concurrently: a `stop` issued while a `start` is
/// **inside the factory spawn** (the exact window the old registry lost the
/// race in) must still end with no running poller — the registry serializes the
/// start decision against the stop, whichever order the lock resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_start_and_stop_leave_no_running_poller() {
    let (wiring, status, drivers) = wiring();
    let entered = Arc::new(Barrier::new(2));
    let proceed = Arc::new(Barrier::new(2));
    let reg = Arc::new(DevicePollerRegistry::with_factory(Arc::new(
        GatedScriptedFactory {
            entered: Arc::clone(&entered),
            proceed: Arc::clone(&proceed),
        },
    )));

    // The adopt's start, parked inside the factory spawn window.
    let start = tokio::task::spawn_blocking({
        let reg = Arc::clone(&reg);
        let wiring = wiring.clone();
        move || reg.start(&device("dev-a"), &wiring)
    });
    // Rendezvous: the start is now inside the spawn window.
    tokio::task::spawn_blocking({
        let entered = Arc::clone(&entered);
        move || entered.wait()
    })
    .await
    .expect("rendezvous with the in-flight start");

    // The delete's stop, issued mid-window.
    let stop = tokio::spawn({
        let reg = Arc::clone(&reg);
        async move { reg.stop("dev-a").await }
    });
    // Give the stop a moment to reach the registry, then release the start.
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::task::spawn_blocking({
        let proceed = Arc::clone(&proceed);
        move || proceed.wait()
    })
    .await
    .expect("release the in-flight start");

    let _started = start.await.expect("the start task completes");
    stop.await.expect("the stop task completes");

    // Whichever order the lock resolved, the stop wins: no poller survives.
    assert!(
        !reg.is_running("dev-a"),
        "no poller survives a stop, regardless of start/stop interleaving"
    );
    // And the delete's forgets cannot be raced by a survivor.
    status.forget("dev-a");
    drivers.forget("dev-a");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        status.snapshot("dev-a").is_none(),
        "no ghost status entry after the interleaved delete"
    );
}
