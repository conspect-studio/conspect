//! The **telemetry-consent** record + the **diagnostics-snapshot** store
//! (Conspect, ADR-0052 §2/§3/§4, the brief §7.1/§10, spec §4.2/§11).
//!
//! # Two pipes — keep them apart (ADR-0052 §1)
//!
//! Multiview makes exactly **two** machine→Aperim contacts and they are **never**
//! co-mingled:
//!
//! * the **mandatory licensing heartbeat** — the keep-alive that holds an
//!   entitlement lease live. It lives under `/api/v1/licensing/` and is owned by
//!   [`crate::routes::licence`]; its consent is *implicit in running the official
//!   build* and is **not** in this document.
//! * the **opt-in product telemetry** pipe — anonymised daily analytics. It lives
//!   under `/api/v1/telemetry/`, is **off by default**, revocable, and governed by
//!   the single consent record this module owns.
//!
//! This module is the **telemetry** pipe's local record only. Nothing here reads,
//! writes, or references the licensing heartbeat; the separation is a hard
//! operator directive and is pinned by the route tests
//! (`telemetry_schema_diagnostics::telemetry_and_diagnostics_routes_are_advertised`).
//!
//! # Consent record + last-writer-wins (ADR-0052 §2)
//!
//! Telemetry consent is a single versioned document — `{ enabled, changed_at,
//! actor }` — recorded **on the machine**. Concurrent edits (machine UI vs the
//! later portal mirror) resolve **last-writer-wins by `changed_at`**: the simplest
//! correct rule for a single boolean preference where staleness, not merge-loss,
//! is the only risk. A write whose timestamp is **strictly later** than the
//! incumbent's is applied; an earlier-or-equal write is rejected (so a delayed
//! portal mirror can never clobber a fresher local choice, and replay is
//! idempotent). The portal mirror is the later O1 transport; the local record +
//! LWW semantics are complete now and pinned by test.
//!
//! # Consent gates nothing locally (ADR-0052)
//!
//! The consent record governs **only** the (future, O1-gated) outbound daily
//! telemetry pipe. It gates **no** local UI/API surface — staying off costs none
//! of the local product. No control-plane handler consults consent except the
//! daily pipe itself (which does not run yet); this is pinned by
//! `telemetry_schema_diagnostics::consent_gates_no_local_route`.
//!
//! # Isolation (invariant #10)
//!
//! The consent record and the snapshot store are ordinary control-plane state
//! (`Mutex`-guarded, no engine handle, never on the hot loop). A wedged client of
//! these surfaces can never back-pressure the engine. The diagnostics bundle is
//! assembled by **reading** the consent-independent local metrics retention store
//! ([`multiview_telemetry::retention::RetentionStore`], ADR-0053) off the engine.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use multiview_core::time::MediaTime;

/// Who recorded a telemetry-consent change.
///
/// The local machine UI/API or the (later, O1-gated) portal mirror. Serialised
/// lower-case so the slug is stable across the machine and the portal.
/// `#[non_exhaustive]` so a future actor is additive on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ConsentActor {
    /// The machine itself — a local UI/API edit.
    Local,
    /// The Aperim portal mirror (the later O1 transport).
    Portal,
}

impl ConsentActor {
    /// The stable, lower-case label for this actor.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ConsentActor::Local => "local",
            ConsentActor::Portal => "portal",
        }
    }
}

/// The immutable, in-memory telemetry-consent record (ADR-0052 §2).
///
/// `enabled` is the current consent for the **outbound daily telemetry pipe**
/// (off by default); `changed_at` is the media-timeline instant the record was
/// last written (the LWW key); `actor` is who wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsentRecord {
    /// Whether the outbound daily telemetry pipe is consented (default `false`).
    pub enabled: bool,
    /// The instant this record was last written — the last-writer-wins key.
    pub changed_at: MediaTime,
    /// Who wrote this record.
    pub actor: ConsentActor,
}

impl Default for ConsentRecord {
    /// The secure, opt-in default: **off**, never written (instant zero), by the
    /// machine. A fresh machine has never consented — including on the free tier
    /// (ADR-0052 §1).
    fn default() -> Self {
        Self {
            enabled: false,
            changed_at: MediaTime::ZERO,
            actor: ConsentActor::Local,
        }
    }
}

/// The shared telemetry-consent state held in [`crate::AppState`].
///
/// A `Mutex` over the single [`ConsentRecord`]; the critical section is a
/// compare-and-overwrite only (never held across an `.await`), so it cannot
/// back-pressure the engine (invariant #10). Cloned cheaply behind an `Arc`.
#[derive(Debug, Default)]
pub struct ConsentState {
    record: Mutex<ConsentRecord>,
}

impl ConsentState {
    /// A fresh consent state at the secure default (off, opt-in).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the record, recovering a poisoned lock rather than propagating a panic
    /// (a panic in another request must not wedge the consent surface — inv #10).
    fn lock(&self) -> std::sync::MutexGuard<'_, ConsentRecord> {
        self.record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current consent record (a cheap copy).
    #[must_use]
    pub fn record(&self) -> ConsentRecord {
        *self.lock()
    }

    /// Apply a consent write under **last-writer-wins** by `changed_at`.
    ///
    /// The write takes effect **iff** its `changed_at` is **strictly later** than
    /// the incumbent's (so a delayed/stale write can never clobber a fresher one,
    /// and an equal-timestamp replay is idempotent). Returns `true` when the write
    /// was applied, `false` when it was rejected as stale.
    pub fn apply(&self, enabled: bool, changed_at: MediaTime, actor: ConsentActor) -> bool {
        let mut guard = self.lock();
        if changed_at.as_nanos() > guard.changed_at.as_nanos() {
            *guard = ConsentRecord {
                enabled,
                changed_at,
                actor,
            };
            true
        } else {
            false
        }
    }
}

/// The lifecycle status of a diagnostics snapshot.
///
/// Snapshots are assembled synchronously on request (the local retention buffer
/// is in-memory), so a returned bundle is always `Ready`; the status is modelled
/// explicitly so the §4.2 one-button flow and the SPA can render a stable shape,
/// and so a future asynchronous composer (large bundles, attachment export) can
/// add a `Pending` arm without a breaking change. `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SnapshotStatus {
    /// The bundle is assembled and readable.
    Ready,
}

/// A bounded, in-memory store of assembled diagnostics snapshots, keyed by id.
///
/// Control-plane-only, `Mutex`-guarded, drop-oldest past a small cap (a support
/// bundle is requested rarely; we keep the most recent few). It holds **no**
/// engine handle and is never on the hot loop, so it cannot back-pressure the
/// engine (invariant #10).
#[derive(Debug)]
pub struct DiagnosticsSnapshotStore {
    capacity: usize,
    inner: Mutex<SnapshotInner>,
}

#[derive(Debug, Default)]
struct SnapshotInner {
    /// Assembled bundles keyed by id.
    bundles: HashMap<String, serde_json::Value>,
    /// Insertion order of ids, for drop-oldest eviction past the cap.
    order: Vec<String>,
}

/// The default cap on retained diagnostics snapshots (oldest evicted past this).
/// Bounded so repeated requests cannot grow control-plane memory (invariant #10).
pub const DEFAULT_SNAPSHOT_CAPACITY: usize = 16;

impl Default for DiagnosticsSnapshotStore {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_SNAPSHOT_CAPACITY)
    }
}

impl DiagnosticsSnapshotStore {
    /// A fresh, empty store at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty store retaining at most `capacity` newest snapshots.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(SnapshotInner::default()),
        }
    }

    /// Lock the inner state, recovering a poisoned lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, SnapshotInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Store an assembled bundle under `id` (drop-oldest past the cap).
    pub fn put(&self, id: impl Into<String>, bundle: serde_json::Value) {
        let id = id.into();
        let mut guard = self.lock();
        if guard.bundles.insert(id.clone(), bundle).is_none() {
            guard.order.push(id);
        }
        while guard.order.len() > self.capacity {
            let evict = guard.order.remove(0);
            guard.bundles.remove(&evict);
        }
    }

    /// Fetch a previously-assembled bundle by `id`, or `None` when unknown.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<serde_json::Value> {
        self.lock().bundles.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{ConsentActor, ConsentState, DiagnosticsSnapshotStore, SnapshotStatus};
    use multiview_core::time::MediaTime;

    #[test]
    fn default_consent_is_off_local_at_zero() {
        let record = ConsentState::new().record();
        assert!(!record.enabled, "opt-in default is off");
        assert_eq!(record.actor, ConsentActor::Local);
        assert_eq!(record.changed_at, MediaTime::ZERO);
    }

    #[test]
    fn lww_strictly_later_write_wins() {
        let state = ConsentState::new();
        assert!(state.apply(true, MediaTime::from_nanos(10), ConsentActor::Portal));
        assert!(state.apply(false, MediaTime::from_nanos(20), ConsentActor::Local));
        let r = state.record();
        assert!(!r.enabled);
        assert_eq!(r.actor, ConsentActor::Local);
    }

    #[test]
    fn lww_earlier_or_equal_write_is_rejected() {
        let state = ConsentState::new();
        assert!(state.apply(true, MediaTime::from_nanos(20), ConsentActor::Local));
        // Strictly-earlier loses.
        assert!(!state.apply(false, MediaTime::from_nanos(10), ConsentActor::Portal));
        // Equal-timestamp loses (incumbent kept — idempotent under replay).
        assert!(!state.apply(false, MediaTime::from_nanos(20), ConsentActor::Portal));
        assert!(state.record().enabled);
    }

    #[test]
    fn actor_labels_are_stable() {
        assert_eq!(ConsentActor::Local.label(), "local");
        assert_eq!(ConsentActor::Portal.label(), "portal");
    }

    #[test]
    fn snapshot_store_round_trips_and_is_bounded() {
        let store = DiagnosticsSnapshotStore::with_capacity(2);
        store.put("a", serde_json::json!({ "n": 1 }));
        store.put("b", serde_json::json!({ "n": 2 }));
        store.put("c", serde_json::json!({ "n": 3 }));
        assert!(store.get("a").is_none(), "oldest evicted past the cap");
        assert_eq!(store.get("b").expect("b kept")["n"], 2);
        assert_eq!(store.get("c").expect("c kept")["n"], 3);
    }

    #[test]
    fn snapshot_status_serialises_lowercase() {
        let json = serde_json::to_value(SnapshotStatus::Ready).expect("serialize");
        assert_eq!(json, serde_json::json!("ready"));
    }
}
