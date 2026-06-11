//! The in-memory device **status** registry (ADR-M008 §2.1): the latest-wins
//! runtime snapshot for each adopted device.
//!
//! This is runtime state, **never** persisted or exported (config-as-code is
//! the durable desired state — §7.3). It is the conflated `device.status` lane's
//! backing store: the broadcaster writes the newest status here (latest-wins)
//! and the realtime session pump re-snapshots from here on resume, because the
//! conflated status is **excluded from the lossless replay ring** (ADR-RT007).
//! `GET /devices/{id}/status` reads it as a cold-snapshot fallback.
//!
//! It is plain control-plane state behind a `Mutex`; the lock guards only this
//! map and is never held by the engine, so it cannot back-pressure the engine
//! (invariant #10).

use std::collections::HashMap;
use std::sync::Mutex;

use multiview_events::{DeviceState, DeviceStatus};

use super::state_machine::DeviceLifecycle;

/// The latest runtime status of every device in the registry, keyed by device
/// id. Latest-wins: a newer status supersedes the prior value entirely.
#[derive(Debug, Default)]
pub struct DeviceStatusRegistry {
    /// device id → (lifecycle, latest conflated status snapshot).
    statuses: Mutex<HashMap<String, DeviceRuntime>>,
}

/// One device's runtime: its lifecycle state machine plus the latest conflated
/// status snapshot the broadcaster published.
#[derive(Debug, Clone)]
struct DeviceRuntime {
    lifecycle: DeviceLifecycle,
    status: DeviceStatus,
}

impl DeviceStatusRegistry {
    /// A fresh, empty status registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, DeviceRuntime>> {
        match self.statuses.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Register a newly-adopted device in its start state (`ADOPTING`), if it is
    /// not already present. No-op for an already-tracked device, so adoption is
    /// idempotent (config re-apply on boot — §7.3).
    pub fn ensure(&self, device_id: &str) {
        let mut guard = self.lock();
        guard.entry(device_id.to_owned()).or_insert_with(|| {
            let lifecycle = DeviceLifecycle::new();
            DeviceRuntime {
                lifecycle,
                status: DeviceStatus::new(device_id, lifecycle.state()),
            }
        });
    }

    /// Drop a device's runtime status (it was removed from the registry).
    pub fn forget(&self, device_id: &str) {
        self.lock().remove(device_id);
    }

    /// Replace a device's conflated status snapshot (latest-wins) and sync the
    /// lifecycle state it carries. Creates the entry if absent.
    pub fn set_status(&self, status: DeviceStatus) {
        let mut guard = self.lock();
        let entry = guard
            .entry(status.device_id.clone())
            .or_insert_with(|| DeviceRuntime {
                lifecycle: DeviceLifecycle::in_state(status.state),
                status: status.clone(),
            });
        entry.lifecycle = DeviceLifecycle::in_state(status.state);
        entry.status = status;
    }

    /// The latest conflated status snapshot for `device_id`, if tracked.
    #[must_use]
    pub fn snapshot(&self, device_id: &str) -> Option<DeviceStatus> {
        self.lock().get(device_id).map(|r| r.status.clone())
    }

    /// The current lifecycle state for `device_id`, if tracked.
    #[must_use]
    pub fn state(&self, device_id: &str) -> Option<DeviceState> {
        self.lock().get(device_id).map(|r| r.lifecycle.state())
    }

    /// Every device's latest status snapshot, id-sorted — the `$snapshot` a
    /// freshly-subscribing realtime client rebuilds its device cache from
    /// (ADR-RT003 / ADR-RT007).
    #[must_use]
    pub fn snapshot_all(&self) -> Vec<DeviceStatus> {
        let guard = self.lock();
        let mut out: Vec<DeviceStatus> = guard.values().map(|r| r.status.clone()).collect();
        out.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::DeviceStatusRegistry;
    use multiview_events::{DeviceState, DeviceStatus};

    #[test]
    fn ensure_seeds_adopting_and_is_idempotent() {
        let reg = DeviceStatusRegistry::new();
        reg.ensure("dev-a");
        assert_eq!(reg.state("dev-a"), Some(DeviceState::Adopting));
        // A second ensure does not reset a device whose status moved on.
        reg.set_status(DeviceStatus::new("dev-a", DeviceState::Online));
        reg.ensure("dev-a");
        assert_eq!(reg.state("dev-a"), Some(DeviceState::Online));
    }

    #[test]
    fn set_status_is_latest_wins() {
        let reg = DeviceStatusRegistry::new();
        reg.set_status(DeviceStatus::new("dev-a", DeviceState::Online));
        reg.set_status(DeviceStatus::new("dev-a", DeviceState::Degraded));
        assert_eq!(reg.snapshot("dev-a").unwrap().state, DeviceState::Degraded);
    }

    #[test]
    fn snapshot_all_is_id_sorted() {
        let reg = DeviceStatusRegistry::new();
        reg.ensure("dev-z");
        reg.ensure("dev-a");
        let ids: Vec<String> = reg
            .snapshot_all()
            .into_iter()
            .map(|s| s.device_id)
            .collect();
        assert_eq!(ids, vec!["dev-a".to_owned(), "dev-z".to_owned()]);
    }

    #[test]
    fn forget_drops_the_runtime() {
        let reg = DeviceStatusRegistry::new();
        reg.ensure("dev-a");
        reg.forget("dev-a");
        assert_eq!(reg.state("dev-a"), None);
    }
}
