//! The change **audit log** (M6 management hardening; broadcast brief §8 "RBAC
//! multi-user + audit log").
//!
//! Every successful mutation the control plane applies is recorded here:
//! **who** (the authenticated principal's key id), **what** (the action +
//! addressed object), and **when** (a media-timeline timestamp). The log is
//! **append-only** and **read-only over HTTP** — there is no route that mutates
//! it — so it is a faithful, tamper-resistant record of operator activity.
//!
//! Like every store in this crate, the audit log holds control-plane state only
//! and is never on the engine's data plane, so however it synchronizes
//! internally it cannot back-pressure the engine (invariant #10).
use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use multiview_core::time::MediaTime;

use crate::error::ControlResult;

/// The resource collection name used in problem documents and routes.
pub const AUDIT_KIND: &str = "audit";

/// The default cap on retained audit entries (oldest evicted past this).
///
/// The log is bounded so a long-running deployment cannot grow control-plane
/// memory without bound; the most recent activity is always retained.
pub const DEFAULT_AUDIT_CAPACITY: usize = 10_000;

/// The kind of mutation an audit entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuditAction {
    /// A resource was created.
    Create,
    /// A resource was replaced/updated.
    Update,
    /// A resource was deleted.
    Delete,
    /// An operational command was submitted (start/stop/swap, arm/take, …).
    Command,
    /// A configuration revision was rolled back.
    Rollback,
}

/// One immutable audit-log entry: who did what to which object, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AuditEntry {
    /// The authenticated principal that performed the mutation (its key id /
    /// JWT subject) — the **who**.
    pub actor: String,
    /// The kind of mutation — part of the **what**.
    pub action: AuditAction,
    /// The resource collection the object belongs to (e.g. `layout`).
    pub object_kind: String,
    /// The addressed object id — the rest of the **what**.
    pub object_id: String,
    /// The media-timeline timestamp (nanoseconds) the mutation was recorded —
    /// the **when**.
    pub at_nanos: i64,
    /// An optional structured detail (e.g. the new resource body, or command
    /// parameters). Never contains secrets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Append + query access to the change audit log.
///
/// The trait is deliberately query-only besides [`AuditLog::record`]: there is
/// no update or delete, so the log is append-only.
pub trait AuditLog: Send + Sync + 'static {
    /// Append an entry recording a successful mutation.
    ///
    /// `at` is the media-timeline timestamp (the control plane injects its
    /// clock, off the engine). Recording is infallible at the trait surface for
    /// the in-memory store; a persistent backend would surface I/O faults via
    /// [`AuditLog::list`].
    fn record(
        &self,
        actor: &str,
        action: AuditAction,
        object_kind: &str,
        object_id: &str,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    );

    /// List entries **newest-first**, optionally filtered to a single
    /// `object_id`.
    ///
    /// # Errors
    ///
    /// [`crate::error::ControlError::Repository`] on a backing-store fault.
    fn list(&self, object_id: Option<&str>) -> ControlResult<Vec<AuditEntry>>;
}

/// Alias matching the crate's `*Repository` naming for trait objects in state.
pub use AuditLog as AuditRepository;

/// An in-memory, bounded, append-only [`AuditLog`].
///
/// Backed by a `Mutex<VecDeque>` capped at a fixed capacity (oldest evicted).
/// The lock guards control-plane state only — never held by the engine — so it
/// cannot back-pressure the engine.
#[derive(Debug)]
pub struct InMemoryAuditLog {
    capacity: usize,
    entries: Mutex<VecDeque<AuditEntry>>,
}

impl Default for InMemoryAuditLog {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_AUDIT_CAPACITY)
    }
}

impl InMemoryAuditLog {
    /// A fresh, empty log at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty log retaining at most `capacity` newest entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Lock the inner deque, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<AuditEntry>> {
        match self.entries.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl AuditLog for InMemoryAuditLog {
    fn record(
        &self,
        actor: &str,
        action: AuditAction,
        object_kind: &str,
        object_id: &str,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    ) {
        let entry = AuditEntry {
            actor: actor.to_owned(),
            action,
            object_kind: object_kind.to_owned(),
            object_id: object_id.to_owned(),
            at_nanos: at.as_nanos(),
            detail,
        };
        let mut guard = self.lock();
        // Newest at the front so `list` is already reverse-chronological.
        guard.push_front(entry);
        while guard.len() > self.capacity {
            guard.pop_back();
        }
    }

    fn list(&self, object_id: Option<&str>) -> ControlResult<Vec<AuditEntry>> {
        let guard = self.lock();
        let out = guard
            .iter()
            .filter(|e| object_id.is_none_or(|id| e.object_id == id))
            .cloned()
            .collect();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{AuditAction, AuditLog, InMemoryAuditLog};
    use multiview_core::time::MediaTime;

    #[test]
    fn bounded_eviction_keeps_newest() {
        let log = InMemoryAuditLog::with_capacity(2);
        for i in 0..5 {
            log.record(
                "a",
                AuditAction::Create,
                "layout",
                &format!("id-{i}"),
                MediaTime::from_nanos(i),
                None,
            );
        }
        let all = log.list(None).unwrap();
        assert_eq!(all.len(), 2);
        // Newest first: id-4 then id-3.
        assert_eq!(all[0].object_id, "id-4");
        assert_eq!(all[1].object_id, "id-3");
    }

    #[test]
    fn filter_by_object_id() {
        let log = InMemoryAuditLog::new();
        log.record(
            "a",
            AuditAction::Create,
            "layout",
            "x",
            MediaTime::from_nanos(1),
            None,
        );
        log.record(
            "b",
            AuditAction::Update,
            "layout",
            "y",
            MediaTime::from_nanos(2),
            None,
        );
        assert_eq!(log.list(Some("x")).unwrap().len(), 1);
        assert_eq!(log.list(Some("y")).unwrap().len(), 1);
        assert!(log.list(Some("z")).unwrap().is_empty());
    }
}
