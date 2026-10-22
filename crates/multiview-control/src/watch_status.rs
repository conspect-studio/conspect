//! The config-file watch status surface (ADR-W020).
//!
//! [`ConfigWatchStatus`] is the shared slot the CLI's config-file watcher
//! records into and `GET /api/v1/config/watch-status` reads from: whether a
//! watcher is active, the watched path, the last applied/rejected loads, and
//! the restart-pending section names. Plain control-plane state guarded by a
//! `Mutex` no engine code ever touches (invariant #10); the default value is
//! the honest "not watched" state a store-only deployment reports.

use std::collections::BTreeSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One recorded watch event: when it happened and what it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WatchStamp {
    /// When the load was applied/rejected, as Unix milliseconds (UTC).
    pub at_ms: i64,
    /// What happened: an applied-change summary, or the rejection reason.
    pub detail: String,
}

/// The body of `GET /api/v1/config/watch-status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WatchStatusBody {
    /// Whether a config-file watcher is running for this process.
    pub active: bool,
    /// The watched config file path (absent when no watcher was ever
    /// started; kept after a stop so the status names what *was* watched).
    pub path: Option<String>,
    /// How many file changes have been successfully applied since start.
    pub applied_count: u64,
    /// The most recent successfully applied load, if any.
    pub last_applied: Option<WatchStamp>,
    /// The most recent rejected (invalid) load, if any.
    pub last_rejected: Option<WatchStamp>,
    /// Section names changed on disk that only apply on restart (sorted,
    /// deduplicated; latched until restart — ADR-W020).
    pub restart_pending: Vec<String>,
}

/// The interior state behind the shared status slot.
#[derive(Debug, Default)]
struct Inner {
    active: bool,
    path: Option<String>,
    applied_count: u64,
    last_applied: Option<WatchStamp>,
    last_rejected: Option<WatchStamp>,
    restart_pending: BTreeSet<String>,
}

/// The shared config-file watch status slot (see the module docs).
#[derive(Debug, Default)]
pub struct ConfigWatchStatus {
    inner: Mutex<Inner>,
}

impl ConfigWatchStatus {
    /// A fresh, inactive ("not watched") status.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the interior, recovering from a poisoned lock (a panicked recorder
    /// must not wedge the read-only status endpoint).
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Mark the watcher active over `path`.
    pub fn mark_active(&self, path: &str) {
        let mut inner = self.lock();
        inner.active = true;
        inner.path = Some(path.to_owned());
    }

    /// Mark the watcher inactive (stopped, shut down, or its task died). The
    /// path is kept so the status still names what *was* watched.
    pub fn mark_inactive(&self) {
        self.lock().active = false;
    }

    /// Record a successfully applied file load.
    pub fn record_applied(&self, at_ms: i64, detail: &str) {
        let mut inner = self.lock();
        inner.applied_count = inner.applied_count.saturating_add(1);
        inner.last_applied = Some(WatchStamp {
            at_ms,
            detail: detail.to_owned(),
        });
    }

    /// Record a rejected (invalid) file load.
    pub fn record_rejected(&self, at_ms: i64, detail: &str) {
        self.lock().last_rejected = Some(WatchStamp {
            at_ms,
            detail: detail.to_owned(),
        });
    }

    /// Add restart-pending section names (latched until restart; deduplicated).
    pub fn add_restart_pending<I>(&self, sections: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.lock().restart_pending.extend(sections);
    }

    /// A point-in-time snapshot for the status endpoint (and tests).
    #[must_use]
    pub fn snapshot(&self) -> WatchStatusBody {
        let inner = self.lock();
        WatchStatusBody {
            active: inner.active,
            path: inner.path.clone(),
            applied_count: inner.applied_count,
            last_applied: inner.last_applied.clone(),
            last_rejected: inner.last_rejected.clone(),
            restart_pending: inner.restart_pending.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::ConfigWatchStatus;

    #[test]
    fn defaults_to_the_not_watched_state() {
        let status = ConfigWatchStatus::new();
        let snap = status.snapshot();
        assert!(!snap.active);
        assert!(snap.path.is_none());
        assert_eq!(snap.applied_count, 0);
        assert!(snap.last_applied.is_none() && snap.last_rejected.is_none());
        assert!(snap.restart_pending.is_empty());
    }

    #[test]
    fn records_applied_rejected_and_pending_sections() {
        let status = ConfigWatchStatus::new();
        status.mark_active("/etc/multiview.toml");
        status.record_applied(10, "first");
        status.record_applied(20, "second");
        status.record_rejected(30, "broken");
        status.add_restart_pending(["outputs".to_owned(), "canvas".to_owned()]);
        status.add_restart_pending(["outputs".to_owned()]);
        let snap = status.snapshot();
        assert!(snap.active);
        assert_eq!(snap.path.as_deref(), Some("/etc/multiview.toml"));
        assert_eq!(snap.applied_count, 2);
        assert_eq!(snap.last_applied.unwrap().detail, "second");
        assert_eq!(snap.last_rejected.unwrap().at_ms, 30);
        // Sorted + deduplicated (latched).
        assert_eq!(snap.restart_pending, vec!["canvas", "outputs"]);
    }
}
