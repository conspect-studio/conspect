//! Health-warning persistence: the [`WarningRepository`] trait + its in-memory
//! default (ADR-0035 SA-0).
//!
//! The control plane mirrors the engine's health warnings into a store so the
//! read-only `GET /api/v1/health` endpoint can list the active warnings (a clear,
//! actionable banner for the operator — e.g. "GPU present but compositing fell
//! back to CPU; here is the fix"). The store is fed by the **engine event
//! subscription** ([`crate::warning_ingest`]): each `health.warning.raised` /
//! `health.warning.cleared` transition [`upsert`](WarningRepository::upsert)s the
//! carried [`HealthWarning`], coalescing on its stable [`WarningCode`] key so a
//! re-raised **latched** warning does not stack into a second entry.
//!
//! This is a deliberate, smaller sibling of [`crate::alarm_store`]: warnings carry
//! a `code` + `remediation` and need no `ETag`/ack (they are read-only operator
//! signals, not acknowledgeable alarms), so the repository is list/get/upsert only.
//! It is control-plane state and never sits on the engine's data plane, so however
//! it synchronises internally it cannot back-pressure the engine (invariant #10).
use std::collections::HashMap;
use std::sync::Mutex;

use multiview_events::HealthWarning;

use crate::error::ControlResult;

/// The resource collection name used in problem documents and not-found errors.
pub const WARNING_KIND: &str = "health-warning";

/// A filter over the health-warning list endpoint.
///
/// All fields are optional; an absent field does not constrain the result. The
/// filter is applied purely by [`WarningRepository::list`] implementations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WarningFilter {
    /// When `Some(true)` keep only **active** warnings; `Some(false)` keep only
    /// cleared ones; `None` keeps both. The REST endpoint defaults to active-only.
    pub active: Option<bool>,
}

impl WarningFilter {
    /// Keep only active warnings (the default the REST endpoint applies).
    #[must_use]
    pub const fn active_only() -> Self {
        Self { active: Some(true) }
    }

    /// Whether `warning` satisfies every set predicate of this filter.
    #[must_use]
    pub fn matches(&self, warning: &HealthWarning) -> bool {
        if let Some(want_active) = self.active {
            if warning.active != want_active {
                return false;
            }
        }
        true
    }
}

/// Persistence for the control plane's health-warning mirror.
///
/// Implementations are control-plane state only and never sit on the engine's
/// data plane, so however they synchronise internally they cannot back-pressure
/// the engine (invariant #10).
pub trait WarningRepository: Send + Sync + 'static {
    /// List the stored warnings matching `filter`, in a stable code-sorted order.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`](crate::error::ControlError::Repository) on a
    /// backing-store fault.
    fn list(&self, filter: &WarningFilter) -> ControlResult<Vec<HealthWarning>>;

    /// Insert or update the stored copy of an engine-published health warning.
    ///
    /// Coalesces on the warning's [`key`](HealthWarning::key) (its
    /// [`WarningCode`](multiview_events::WarningCode)) so a re-raised **latched**
    /// warning updates the single entry rather than stacking — emitting the same
    /// warning twice yields ONE active entry. The engine is authoritative for
    /// warning state, so this write path is lossy and carries no precondition.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`](crate::error::ControlError::Repository) on a
    /// backing-store fault.
    fn upsert(&self, warning: HealthWarning) -> ControlResult<()>;
}

/// An in-memory [`WarningRepository`] backed by a `Mutex<HashMap>` keyed by the
/// warning's [`WarningCode`](multiview_events::WarningCode) string.
///
/// The default, fully-tested store. Its lock guards only control-plane state — it
/// is never held by the engine — so it cannot back-pressure the engine.
#[derive(Debug, Default)]
pub struct InMemoryWarningStore {
    warnings: Mutex<HashMap<&'static str, HealthWarning>>,
}

impl InMemoryWarningStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<&'static str, HealthWarning>> {
        match self.warnings.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl WarningRepository for InMemoryWarningStore {
    fn list(&self, filter: &WarningFilter) -> ControlResult<Vec<HealthWarning>> {
        let guard = self.lock();
        let mut out: Vec<HealthWarning> = guard
            .values()
            .filter(|w| filter.matches(w))
            .cloned()
            .collect();
        // Stable, deterministic order by the wire code so the list never flickers.
        out.sort_by(|a, b| a.code.as_str().cmp(b.code.as_str()));
        Ok(out)
    }

    fn upsert(&self, warning: HealthWarning) -> ControlResult<()> {
        // Coalesce on the stable code key: a re-raised latched warning replaces
        // the single entry rather than stacking (the latched/idempotent property).
        self.lock().insert(warning.key(), warning);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_events::{HealthWarning, WarningCode, WarningSeverity};

    use super::{InMemoryWarningStore, WarningFilter, WarningRepository};

    fn warning(active: bool) -> HealthWarning {
        HealthWarning {
            code: WarningCode::GpuPresentNoVulkanAdapter,
            severity: WarningSeverity::Warning,
            subsystem: "compositor".to_owned(),
            message: "msg".to_owned(),
            remediation: "fix".to_owned(),
            since: 1,
            active,
        }
    }

    #[test]
    fn upsert_coalesces_on_code_so_a_re_raise_does_not_stack() {
        let store = InMemoryWarningStore::new();
        store.upsert(warning(true)).unwrap();
        store.upsert(warning(true)).unwrap();
        let all = store.list(&WarningFilter::default()).unwrap();
        assert_eq!(all.len(), 1, "the same code coalesces to one entry");
    }

    #[test]
    fn list_active_only_excludes_cleared() {
        let store = InMemoryWarningStore::new();
        store.upsert(warning(true)).unwrap();
        assert_eq!(store.list(&WarningFilter::active_only()).unwrap().len(), 1);
        // Clearing the same code (active=false) removes it from the active list.
        store.upsert(warning(false)).unwrap();
        assert!(store
            .list(&WarningFilter::active_only())
            .unwrap()
            .is_empty());
        // ...but it is still in the store (both).
        assert_eq!(store.list(&WarningFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn empty_store_lists_nothing() {
        let store = InMemoryWarningStore::new();
        assert!(store
            .list(&WarningFilter::active_only())
            .unwrap()
            .is_empty());
    }
}
