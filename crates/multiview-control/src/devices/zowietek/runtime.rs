//! The runtime registry of **spawned** device poller actors (DEV-A4, ADR-M009).
//!
//! This is the seam that turns the typed driver + poller into *running* control-
//! plane tasks. The control plane holds one [`DevicePollerRegistry`] on its
//! [`AppState`](crate::state::AppState); the adopt route and boot-seed ask it to
//! **start** a poller for a `driver = zowietek` device, the delete route asks it
//! to **stop** one, and the `set-mode` route asks it to **dispatch** a
//! convergence to the running actor.
//!
//! ## The transport seam stays socket-free by construction
//!
//! Building a real poller needs a concrete [`ZowietekTransport`](super::client::ZowietekTransport)
//! — the reqwest-backed one behind the off-by-default `zowietek` feature. Rather
//! than make the whole control plane generic over the transport, the registry
//! delegates the spawn to a [`DevicePollerFactory`]: an object-safe seam whose
//! concrete impl owns the transport type internally and hands back a
//! transport-erased [`PollerHandle`]. The default factory
//! ([`NoPollerFactory`]) spawns **nothing** (the projection routes stay honestly
//! empty — exactly today's behaviour), so the default build pulls no socket; the
//! binary installs the reqwest-backed factory only when the `zowietek` feature
//! is on. Tests inject a scripted factory, so the whole runtime path is driven
//! socket-free.
//!
//! ## Deterministic start/stop
//!
//! `start` is check-then-spawn under the registry lock and `stop` tombstones
//! the id and **joins** the aborted task, so a delete that races an adopt
//! deterministically wins: after `stop` returns no poller for that id is
//! running or can get started (a late `start` is rejected before any task is
//! spawned), and no ghost `device_status`/driver entries can be re-created. A
//! later legitimate re-adopt clears the tombstone (the create route does this
//! before its store insert) and starts fresh. See [`DevicePollerRegistry`].
//!
//! ## Isolation (invariant #10)
//!
//! Every handle here is control-plane: the registry is a `Mutex`-guarded map the
//! engine never touches, dispatch to an actor is a bounded non-blocking
//! `try_send`, and stopping an actor aborts its task. Nothing can back-pressure
//! the engine.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use multiview_config::Device;

use super::poller::{PollerControl, PollerHandle};
use crate::devices::broadcaster::DeviceBroadcaster;
use crate::devices::cast::store::CastSessionStore;
use crate::devices::driver_registry::DeviceDriverRegistry;
use crate::state::AckClock;

/// The control-plane handles a [`DevicePollerFactory`] needs to wire a freshly
/// spawned poller into the running control plane: the broadcaster it publishes
/// through and the driver registry it enumerates facets into.
///
/// (The status registry the poller reflects into is the broadcaster's own
/// registry — [`DeviceBroadcaster::registry`] — so it is not threaded
/// separately.)
#[derive(Clone)]
pub struct PollerWiring {
    /// The broadcaster the poller publishes lifecycle/status/mode events through.
    pub broadcaster: DeviceBroadcaster,
    /// The driver registry the poller enumerates source/output facets into (read
    /// by the projection routes).
    pub drivers: Arc<DeviceDriverRegistry>,
    /// The ephemeral cast-session store the cast driver stamps started-at
    /// into at its LOAD-accept moment (DEV-D3.1). A no-op for ids the store
    /// does not track (saved cast devices run the same driver but have no
    /// ephemeral record); other drivers ignore it entirely.
    pub cast_sessions: Arc<CastSessionStore>,
    /// The control plane's injectable clock (the same `AckClock` the audit
    /// log stamps with — Unix nanoseconds by default) the cast driver reads
    /// the started-at stamp from.
    pub clock: AckClock,
}

/// An object-safe seam that spawns a poller actor for one managed device.
///
/// The concrete impl owns the transport type internally (so the control plane
/// stays transport-agnostic) and returns a transport-erased [`PollerHandle`], or
/// [`None`] when it does not manage this device (wrong driver, no live
/// transport, missing address). The default [`NoPollerFactory`] always returns
/// [`None`]; the binary's reqwest-backed factory (feature `zowietek`) returns a
/// live handle for a `zowietek` device.
pub trait DevicePollerFactory: Send + Sync {
    /// Spawn a supervised poller for `device`, wired through `wiring`, or
    /// [`None`] when this factory does not manage it.
    fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle>;
}

/// The default factory: spawns **nothing**.
///
/// In the default build there is no live device transport (the reqwest backend
/// is behind the off-by-default `zowietek` feature), so no poller is spawned and
/// the projection routes stay honestly empty — exactly the pre-DEV-A4 behaviour.
/// The binary swaps in the reqwest-backed factory when the feature is on.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPollerFactory;

impl DevicePollerFactory for NoPollerFactory {
    fn spawn(&self, _device: &Device, _wiring: &PollerWiring) -> Option<PollerHandle> {
        None
    }
}

/// Composes several driver factories into one (DEV-D2): the registry holds a
/// single factory, so a deployment with more than one live driver (`zowietek`
/// **and** `cast`) installs a composite whose `spawn` asks each member in
/// order and takes the **first** one that manages the device. Members decline
/// by driver kind, so order only matters if two members claim the same driver
/// — which no build does.
pub struct CompositePollerFactory {
    members: Vec<Arc<dyn DevicePollerFactory>>,
}

impl CompositePollerFactory {
    /// Build a composite over `members`, asked in order.
    #[must_use]
    pub fn new(members: Vec<Arc<dyn DevicePollerFactory>>) -> Self {
        Self { members }
    }
}

impl DevicePollerFactory for CompositePollerFactory {
    fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle> {
        self.members
            .iter()
            .find_map(|member| member.spawn(device, wiring))
    }
}

/// The runtime registry of spawned poller actors, keyed by device id.
///
/// Holds the [`DevicePollerFactory`] (how to spawn one) and the live
/// [`PollerHandle`]s (the running tasks + their control channels). All methods
/// are control-plane only and never touch the engine (invariant #10).
///
/// ## Deterministic start/stop (no ghost pollers)
///
/// `start` and `stop` are serialized through one lock, and a [`stop`]
/// **tombstones** the id: a `start` for a tombstoned id is rejected *before any
/// task is spawned* (check-then-spawn under the lock), so a delete's `stop`
/// that interleaves with an adopt's in-flight `start` deterministically wins —
/// after `stop` returns, no poller for that id is running or can get started,
/// and nothing can re-create the `device_status`/driver entries the delete
/// forgets. A later legitimate re-adopt clears the tombstone via
/// [`clear_tombstone`] **before** its store insert (see that method for why the
/// ordering makes the clear safe), then starts fresh.
///
/// [`stop`]: DevicePollerRegistry::stop
/// [`clear_tombstone`]: DevicePollerRegistry::clear_tombstone
pub struct DevicePollerRegistry {
    factory: Arc<dyn DevicePollerFactory>,
    inner: Mutex<RegistryInner>,
}

/// The lock-guarded registry state: the live handles plus the tombstoned ids.
#[derive(Default)]
struct RegistryInner {
    /// The running poller actors, keyed by device id.
    handles: HashMap<String, PollerHandle>,
    /// Ids whose poller was stopped (the device deleted): a `start` for a
    /// tombstoned id is rejected until a fresh create clears it.
    tombstones: std::collections::HashSet<String>,
}

impl std::fmt::Debug for DevicePollerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePollerRegistry")
            .field("running", &self.running_count())
            .finish()
    }
}

impl Default for DevicePollerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DevicePollerRegistry {
    /// A registry with the **no-op** factory (the default build: no live device
    /// transport, so no poller is ever spawned).
    #[must_use]
    pub fn new() -> Self {
        Self::with_factory(Arc::new(NoPollerFactory))
    }

    /// A registry with an explicit spawn `factory` (the binary's reqwest-backed
    /// factory behind the `zowietek` feature, or a test's scripted factory).
    #[must_use]
    pub fn with_factory(factory: Arc<dyn DevicePollerFactory>) -> Self {
        Self {
            factory,
            inner: Mutex::new(RegistryInner::default()),
        }
    }

    /// Lock the registry state, recovering from a poisoned lock (a panic in one
    /// request must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The number of currently-running poller actors.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.lock().handles.len()
    }

    /// Whether a poller is currently running for `device_id`.
    #[must_use]
    pub fn is_running(&self, device_id: &str) -> bool {
        self.lock().handles.contains_key(device_id)
    }

    /// Start (or restart) a supervised poller for `device`, wiring it through
    /// `wiring`. A no-op when the factory does not manage this device (e.g. the
    /// default no-op factory, a non-`zowietek` driver, or a missing address).
    ///
    /// **Rejected for a tombstoned id** (a [`stop`](DevicePollerRegistry::stop)
    /// for this id happened and no fresh create has
    /// [cleared](DevicePollerRegistry::clear_tombstone) it): a delete's stop
    /// that completed while this start was still in flight must win — no ghost
    /// poller. The tombstone check **and** the factory spawn run under the one
    /// registry lock (check-then-spawn), so no task is ever spawned for a
    /// tombstoned id and a concurrent `stop` is fully ordered against this
    /// start: either it tombstones first (this start spawns nothing) or it runs
    /// after the insert (it removes and joins the fresh poller). The factory
    /// spawn is brief, synchronous, control-plane-only work, so holding the
    /// lock across it trades a moment of start/stop serialization for
    /// determinism (invariant #10 untouched — the engine never takes this lock).
    ///
    /// Idempotent by id: an existing poller for the same device id is replaced
    /// (the previous handle's drop aborts its task), so an adopt/edit
    /// re-converges on a fresh task rather than leaking the old one.
    ///
    /// Returns `true` when a poller was spawned.
    pub fn start(&self, device: &Device, wiring: &PollerWiring) -> bool {
        let mut guard = self.lock();
        if guard.tombstones.contains(&device.id) {
            // The device was stopped/deleted after this start began: reject
            // before spawning anything — the delete wins, no ghost poller.
            return false;
        }
        let Some(handle) = self.factory.spawn(device, wiring) else {
            return false;
        };
        // Replace-by-id: dropping the previous handle aborts its task.
        let _previous = guard.handles.insert(device.id.clone(), handle);
        true
    }

    /// Stop the poller for `device_id` (the device was removed): tombstone the
    /// id (so an in-flight `start` for it is rejected — see
    /// [`start`](DevicePollerRegistry::start)), abort its task, and **await the
    /// task's termination** before returning. A no-op when none is running
    /// (still tombstones, so a late racing start cannot resurrect it).
    ///
    /// Awaiting the aborted task's join (outside the lock) guarantees that once
    /// `stop` returns the actor can no longer publish anything — the delete
    /// route's subsequent `device_status`/driver `forget`s cannot be raced by a
    /// final in-flight publish, so no ghost entries survive the delete.
    pub async fn stop(&self, device_id: &str) {
        let handle = {
            let mut guard = self.lock();
            guard.tombstones.insert(device_id.to_owned());
            guard.handles.remove(device_id)
        };
        if let Some(handle) = handle {
            let task = handle.into_join_handle();
            task.abort();
            // The join completes with a cancellation error — expected; what
            // matters is that the task is fully terminated when we return.
            let _ = task.await;
        }
    }

    /// Stop the poller/session for `device_id` **gracefully** (DEV-D2): like
    /// [`stop`](DevicePollerRegistry::stop) — tombstone first, then tear the
    /// task down and await its termination — but instead of aborting
    /// immediately, the actor gets a `grace` window to **exit voluntarily**
    /// (the cast actor's [`PollerControl::StopCast`] teardown sends the
    /// receiver `STOP` that actually clears the TV, then returns). An actor
    /// that ignores the window is aborted when it elapses, so the call is
    /// bounded: after it returns no task for this id is running, exactly the
    /// `stop` post-condition. Dispatch the voluntary-exit command **before**
    /// calling this (the route does), or the grace window just elapses.
    pub async fn stop_graceful(&self, device_id: &str, grace: std::time::Duration) {
        let handle = {
            let mut guard = self.lock();
            guard.tombstones.insert(device_id.to_owned());
            guard.handles.remove(device_id)
        };
        if let Some(handle) = handle {
            let mut task = handle.into_join_handle();
            // Voluntary-exit join, bounded by the grace window. The join
            // result is irrelevant (a clean return or a cancellation both mean
            // "terminated"); what matters is that the task is gone on return.
            if tokio::time::timeout(grace, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }

    /// Clear the tombstone for `device_id` — a **fresh legitimate create** is
    /// beginning. The create route calls this **before** its resource-store
    /// insert; that ordering is what keeps the tombstone sound: any delete's
    /// tombstone is set *after* its store delete, which required some create's
    /// store insert, which this clear *precedes* — so a clear can never erase
    /// the tombstone of a delete that raced the same create, and only a create
    /// that starts entirely after the delete (a real re-adopt) starts with a
    /// clean slate. A no-op when the id is not tombstoned.
    pub fn clear_tombstone(&self, device_id: &str) {
        let _was_tombstoned = self.lock().tombstones.remove(device_id);
    }

    /// Dispatch a control command to the running poller for `device_id` without
    /// blocking (the `set-mode` route's dispatch path).
    ///
    /// Returns `true` when the command was enqueued to a running actor; `false`
    /// when no poller is running for that device, or its control channel is full
    /// (drop-newest — the route never blocks on the actor, invariant #10).
    pub fn dispatch(&self, device_id: &str, command: PollerControl) -> bool {
        self.lock()
            .handles
            .get(device_id)
            .is_some_and(|handle| handle.try_dispatch(command))
    }
}

/// The reqwest-backed poller factory — the live device transport, behind the
/// off-by-default `zowietek` feature (DEV-A4, ADR-M009 §3.1).
///
/// Builds a [`ReqwestTransport`](super::client::ReqwestTransport) to a
/// `zowietek` device's management address, wraps it in a
/// [`ZowietekDriver`](super::ZowietekDriver) + [`ZowietekPoller`](super::poller::ZowietekPoller),
/// and spawns the supervised actor. The binary installs this factory on the
/// poller registry only when the `zowietek` feature is on, so the default build
/// stays socket-free.
///
/// Credentials are resolved by the binary's secret-store seam (1Password etc.)
/// from the device's `auth.secret_ref` and supplied per device through a
/// resolver closure, so plaintext credentials never enter this crate's config
/// model — only the resolved `(username, password)` reach the transport at spawn.
#[cfg(feature = "zowietek")]
pub struct ReqwestPollerFactory {
    /// The HTTP request timeout for each device round-trip.
    timeout: std::time::Duration,
    /// Resolves a device's `auth.secret_ref` to `(username, password)`. Returns
    /// `None` when no credential is available (the poller is then not spawned for
    /// that device — it cannot log in). Boxed so the binary injects any resolver
    /// (1Password, env, file) without this crate depending on a secret store.
    #[allow(clippy::type_complexity)]
    resolve_credentials: Box<dyn Fn(&Device) -> Option<(String, String)> + Send + Sync>,
    /// The outbound-dial SSRF policy the transport screens each resolved dial IP
    /// against (SEC-02, CWE-918).
    dial_policy: Arc<multiview_config::device::net_guard::DialPolicy>,
}

#[cfg(feature = "zowietek")]
impl ReqwestPollerFactory {
    /// Build a reqwest-backed factory with `timeout` per request, a
    /// `resolve_credentials` closure mapping a device to its `(username,
    /// password)` (resolved from `auth.secret_ref` by the binary's secret store),
    /// and the outbound-dial SSRF `dial_policy` the transport enforces.
    #[must_use]
    pub fn new(
        timeout: std::time::Duration,
        resolve_credentials: impl Fn(&Device) -> Option<(String, String)> + Send + Sync + 'static,
        dial_policy: Arc<multiview_config::device::net_guard::DialPolicy>,
    ) -> Self {
        Self {
            timeout,
            resolve_credentials: Box::new(resolve_credentials),
            dial_policy,
        }
    }
}

#[cfg(feature = "zowietek")]
impl DevicePollerFactory for ReqwestPollerFactory {
    fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle> {
        use multiview_config::DeviceDriver;

        // Only manage `zowietek` devices; other drivers are not ours.
        if device.driver != DeviceDriver::Zowietek {
            return None;
        }
        // A zowietek device requires a management address (config validation
        // already enforces this); without one we cannot reach it.
        let address = device.address.as_deref()?;
        // Resolve credentials from the device's secret_ref via the injected
        // resolver; without a credential the device cannot be logged into.
        let (username, password) = (self.resolve_credentials)(device)?;

        let transport = match super::client::ReqwestTransport::new(
            address,
            self.timeout,
            Arc::clone(&self.dial_policy),
        ) {
            Ok(transport) => Arc::new(transport),
            Err(err) => {
                tracing::warn!(
                    device = %device.id,
                    error = %err,
                    "zowietek transport build failed; no poller spawned"
                );
                return None;
            }
        };
        let driver = super::ZowietekDriver::new(
            &device.id,
            transport,
            wiring.broadcaster.clone(),
            Arc::clone(&wiring.drivers),
            &username,
            &password,
        );
        // The management host the source facet addresses: the authority of the
        // device address (host[:port] without the scheme), bracket-preserving so
        // an IPv6 literal stays a valid URL host.
        let host = management_host(address);
        let config = device
            .reconnect
            .map_or_else(super::poller::PollerConfig::default, |policy| {
                super::poller::PollerConfig::from_reconnect_policy(policy)
            });
        let poller = super::poller::ZowietekPoller::new(
            &device.id,
            driver,
            Arc::clone(wiring.broadcaster.registry()),
            &host,
            config,
        )
        // Thread the config-as-code desired_mode: the poller converges the
        // device onto it whenever adopt/reconnect reaches ONLINE.
        .with_desired_mode(device.desired_mode.clone());
        Some(poller.spawn())
    }
}

/// Extract the management host (authority without scheme or path) from a device
/// management address, for addressing the served RTSP mounts.
///
/// `http://[fd00:db8::42]:80/` → `[fd00:db8::42]`; `http://host:8080` → `host`.
/// Bracket-preserving so an IPv6 literal stays a valid URL host. Defensive: an
/// address with no scheme/authority is returned trimmed, never panics.
#[cfg(feature = "zowietek")]
fn management_host(address: &str) -> String {
    // Strip the scheme (`scheme://`) if present.
    let after_scheme = address.split_once("://").map_or(address, |(_, rest)| rest);
    // Take the authority (up to the first `/`).
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    // Strip a trailing `:port` — but NOT the colons inside a bracketed IPv6
    // literal. For `[v6]:port` the port follows the closing bracket; for
    // `host:port` it follows the host.
    if let Some(close) = authority.rfind(']') {
        // Bracketed IPv6: keep through the closing bracket (drop any `:port`).
        authority.get(..=close).unwrap_or(authority).to_owned()
    } else if let Some(colon) = authority.rfind(':') {
        authority.get(..colon).unwrap_or(authority).to_owned()
    } else {
        authority.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use multiview_config::{Device, DeviceDriver};

    use super::{DevicePollerFactory, DevicePollerRegistry, NoPollerFactory, PollerWiring};

    fn device(id: &str) -> Device {
        // Minimal valid zowietek device; the runtime registry only reads id /
        // driver / address.
        serde_json::from_value(serde_json::json!({
            "id": id,
            "driver": "zowietek",
            "address": "http://[fd00:db8::42]"
        }))
        .expect("a valid device")
    }

    #[tokio::test]
    async fn no_poller_factory_starts_nothing() {
        let reg = DevicePollerRegistry::new();
        let engine = Arc::new(multiview_engine::EnginePublisher::new(8));
        let status = Arc::new(crate::devices::DeviceStatusRegistry::new());
        let wiring = PollerWiring {
            broadcaster: crate::devices::DeviceBroadcaster::new(engine, status),
            drivers: Arc::new(crate::devices::DeviceDriverRegistry::new()),
            cast_sessions: Arc::new(crate::devices::cast::store::CastSessionStore::new()),
            clock: Arc::new(|| multiview_core::time::MediaTime::from_nanos(0)),
        };
        let dev = device("dev-a");
        assert_eq!(dev.driver, DeviceDriver::Zowietek);
        // The default no-op factory spawns nothing — the projection routes stay
        // honestly empty, exactly the pre-DEV-A4 behaviour.
        assert!(!reg.start(&dev, &wiring));
        assert!(!reg.is_running("dev-a"));
        assert_eq!(reg.running_count(), 0);
        // Dispatch to a non-running poller is a clean false (never a block).
        assert!(!reg.dispatch(
            "dev-a",
            super::PollerControl::SetMode {
                mode: "decoder".to_owned()
            }
        ));
        // Stop aborts nothing when none is running (it still tombstones, so a
        // racing late start cannot resurrect a deleted device).
        reg.stop("dev-a").await;
        assert!(!reg.start(&dev, &wiring));
    }

    #[test]
    fn no_factory_reports_no_managed_device() {
        let factory = NoPollerFactory;
        let engine = Arc::new(multiview_engine::EnginePublisher::new(8));
        let status = Arc::new(crate::devices::DeviceStatusRegistry::new());
        let wiring = PollerWiring {
            broadcaster: crate::devices::DeviceBroadcaster::new(engine, status),
            drivers: Arc::new(crate::devices::DeviceDriverRegistry::new()),
            cast_sessions: Arc::new(crate::devices::cast::store::CastSessionStore::new()),
            clock: Arc::new(|| multiview_core::time::MediaTime::from_nanos(0)),
        };
        assert!(factory.spawn(&device("dev-a"), &wiring).is_none());
    }
}
