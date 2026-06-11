//! The device **driver** registry (DEV-A4, ADR-M009): the latest-wins cache of
//! each driver's enumerated [`SourceCandidate`]s and [`OutputTarget`]s.
//!
//! A driver actor enumerates a device's facets and publishes them here; the
//! `GET /devices/{id}/source-candidates` and `/output-targets` routes (DEV-A3)
//! read them. Like [`DeviceStatusRegistry`](super::registry::DeviceStatusRegistry)
//! this is plain control-plane state behind a `Mutex`; the lock guards only this
//! map and is never held by the engine, so it cannot back-pressure the engine
//! (invariant #10). A device with no driver — or whose driver has not yet
//! enumerated — has no entry, and the routes fall back to the honest-empty list.

use std::collections::HashMap;
use std::sync::Mutex;

use super::projection::{OutputTarget, SourceCandidate};

/// One device's driver-enumerated projections (the source/output facets).
#[derive(Debug, Clone, Default)]
struct DriverFacets {
    /// The source candidates the source facet enumerated.
    source_candidates: Vec<SourceCandidate>,
    /// The output targets the output facet enumerated.
    output_targets: Vec<OutputTarget>,
}

/// The latest-wins per-device driver projection cache the A3 facet routes read.
#[derive(Debug, Default)]
pub struct DeviceDriverRegistry {
    /// device id → enumerated facets (latest-wins).
    facets: Mutex<HashMap<String, DriverFacets>>,
}

impl DeviceDriverRegistry {
    /// A fresh, empty driver registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in one
    /// driver task must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, DriverFacets>> {
        match self.facets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Replace `device_id`'s enumerated source candidates (latest-wins).
    pub fn set_source_candidates(&self, device_id: &str, candidates: Vec<SourceCandidate>) {
        let mut guard = self.lock();
        guard
            .entry(device_id.to_owned())
            .or_default()
            .source_candidates = candidates;
    }

    /// Replace `device_id`'s enumerated output targets (latest-wins).
    pub fn set_output_targets(&self, device_id: &str, targets: Vec<OutputTarget>) {
        let mut guard = self.lock();
        guard
            .entry(device_id.to_owned())
            .or_default()
            .output_targets = targets;
    }

    /// The source candidates a driver enumerated for `device_id` (empty when no
    /// driver has enumerated yet — the route's honest-empty fallback).
    #[must_use]
    pub fn source_candidates(&self, device_id: &str) -> Vec<SourceCandidate> {
        self.lock()
            .get(device_id)
            .map(|f| f.source_candidates.clone())
            .unwrap_or_default()
    }

    /// The output targets a driver enumerated for `device_id` (empty when no
    /// driver has enumerated yet — the route's honest-empty fallback).
    #[must_use]
    pub fn output_targets(&self, device_id: &str) -> Vec<OutputTarget> {
        self.lock()
            .get(device_id)
            .map(|f| f.output_targets.clone())
            .unwrap_or_default()
    }

    /// Drop a device's enumerated facets (the device was removed).
    pub fn forget(&self, device_id: &str) {
        self.lock().remove(device_id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::DeviceDriverRegistry;
    use crate::devices::projection::{OutputTarget, SourceCandidate};

    #[test]
    fn empty_until_a_driver_enumerates() {
        let reg = DeviceDriverRegistry::new();
        assert!(reg.source_candidates("dev-a").is_empty());
        assert!(reg.output_targets("dev-a").is_empty());
    }

    #[test]
    fn set_then_read_is_latest_wins() {
        let reg = DeviceDriverRegistry::new();
        reg.set_source_candidates(
            "dev-a",
            vec![SourceCandidate {
                id: "main".to_owned(),
                kind: "rtsp".to_owned(),
                url: Some("rtsp://[fd00::1]:8554/main/av".to_owned()),
                unverified: false,
            }],
        );
        assert_eq!(reg.source_candidates("dev-a").len(), 1);
        // Latest-wins: a fresh enumeration replaces the prior list.
        reg.set_source_candidates("dev-a", Vec::new());
        assert!(reg.source_candidates("dev-a").is_empty());

        reg.set_output_targets(
            "dev-a",
            vec![OutputTarget {
                id: "slot-0".to_owned(),
                kind: "rtsp".to_owned(),
                label: None,
            }],
        );
        assert_eq!(reg.output_targets("dev-a").len(), 1);
    }

    #[test]
    fn forget_drops_the_facets() {
        let reg = DeviceDriverRegistry::new();
        reg.set_output_targets(
            "dev-a",
            vec![OutputTarget {
                id: "slot-0".to_owned(),
                kind: "srt".to_owned(),
                label: None,
            }],
        );
        reg.forget("dev-a");
        assert!(reg.output_targets("dev-a").is_empty());
    }
}
