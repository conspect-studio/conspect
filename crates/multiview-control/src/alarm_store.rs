//! Alarm persistence: the [`AlarmRepository`] trait and its in-memory default.
//!
//! The control plane mirrors the engine's alarm state into a versioned store so
//! the REST API can list active/historical alarms and acknowledge them with
//! `ETag`/`If-Match` optimistic concurrency (ADR-W006). The store is fed two
//! ways:
//!
//! * by the **engine event subscription** ([`crate::alarm_ingest`]): each
//!   `alarm.raised` / `alarm.updated` / `alarm.cleared` / `alarm.acked`
//!   transition [`upsert`](AlarmRepository::upsert)s the carried
//!   [`AlarmRecord`]. This read path is lossy and isolation-safe — it can never
//!   back-pressure the engine (invariant #10);
//! * by an operator **acknowledgement** ([`AlarmRepository::acknowledge`]) from
//!   the REST API, which sets the [`AckState`] and bumps the version.
//!
//! The **tested default** is [`InMemoryAlarmStore`] (pure Rust, no native deps).
//! It is control-plane state only and never sits on the engine's data plane, so
//! however it synchronises internally it cannot back-pressure the engine.
use std::collections::HashMap;
use std::sync::Mutex;

use multiview_core::alarm::{AckState, AlarmId, AlarmRecord, PerceivedSeverity};
use multiview_core::time::MediaTime;

use crate::concurrency::Version;
use crate::error::{ControlError, ControlResult};

/// The resource collection name used in problem documents and not-found errors.
pub const ALARM_KIND: &str = "alarm";

/// A stored alarm together with its optimistic-concurrency version.
///
/// The `version` stamps the `ETag` and is matched by `If-Match` on an
/// acknowledge. It is bumped on every mutation (an engine-driven upsert that
/// changes the record, or an operator acknowledge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedAlarm {
    /// The optimistic-concurrency version.
    pub version: Version,
    /// The alarm record.
    pub record: AlarmRecord,
}

/// A filter over the alarm list endpoint.
///
/// All fields are optional; an absent field does not constrain the result. The
/// filter is applied purely by [`AlarmRepository::list`] implementations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlarmFilter {
    /// Keep only alarms at or above this severity (X.733 total order).
    pub min_severity: Option<PerceivedSeverity>,
    /// When `Some(true)` keep only **active** alarms; `Some(false)` keep only
    /// cleared/historical alarms; `None` keeps both.
    pub active: Option<bool>,
    /// Keep only alarms whose scope tag matches this string (e.g. `tile`,
    /// `probe`, `group`, `system`).
    pub scope_kind: Option<String>,
}

impl AlarmFilter {
    /// Whether `record` satisfies every set predicate of this filter.
    #[must_use]
    pub fn matches(&self, record: &AlarmRecord) -> bool {
        if let Some(min) = self.min_severity {
            if record.severity < min {
                return false;
            }
        }
        if let Some(want_active) = self.active {
            if record.is_active() != want_active {
                return false;
            }
        }
        if let Some(scope) = &self.scope_kind {
            if scope_tag(record) != scope.as_str() {
                return false;
            }
        }
        true
    }
}

/// The serde `kind` tag of an alarm scope (matches `AlarmScope`'s tagged form).
#[must_use]
pub fn scope_tag(record: &AlarmRecord) -> &'static str {
    use multiview_core::alarm::AlarmScope;
    match record.scope {
        AlarmScope::Probe { .. } => "probe",
        AlarmScope::Tile { .. } => "tile",
        AlarmScope::Group { .. } => "group",
        AlarmScope::System => "system",
        // `AlarmScope` is `#[non_exhaustive]`; an unknown future scope reports a
        // generic tag rather than failing the build.
        _ => "other",
    }
}

/// Versioned persistence for the control plane's alarm mirror.
///
/// Implementations are control-plane state only and never sit on the engine's
/// data plane, so however they synchronise internally they cannot back-pressure
/// the engine (invariant #10).
pub trait AlarmRepository: Send + Sync + 'static {
    /// List the stored alarms matching `filter`, in a stable id-sorted order.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn list(&self, filter: &AlarmFilter) -> ControlResult<Vec<VersionedAlarm>>;

    /// Fetch one alarm by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no alarm has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn get(&self, id: &AlarmId) -> ControlResult<VersionedAlarm>;

    /// Insert or update the stored copy of an engine-published alarm record.
    ///
    /// If the id is new it is stored at [`Version::INITIAL`]. If it exists and
    /// the record differs, the stored record is replaced and its version bumped;
    /// an identical record is a no-op (the version is unchanged) so a repeated
    /// transition does not churn the `ETag`. Returns the stored alarm after the
    /// upsert.
    ///
    /// This is the **engine-driven** write path: it is lossy and never carries an
    /// `If-Match`, because the engine is authoritative for alarm state.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn upsert(&self, record: AlarmRecord) -> ControlResult<VersionedAlarm>;

    /// Acknowledge the alarm `id` as operator `who` at media time `when`.
    ///
    /// Sets the record's [`AckState`] to acknowledged and bumps the version. The
    /// caller is responsible for having already enforced any `If-Match`
    /// precondition against [`AlarmRepository::get`]'s version. Returns the
    /// stored alarm after the acknowledge.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no alarm has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn acknowledge(
        &self,
        id: &AlarmId,
        who: &str,
        when: MediaTime,
    ) -> ControlResult<VersionedAlarm>;
}

/// An in-memory [`AlarmRepository`] backed by a `Mutex<HashMap>`.
///
/// The default, fully-tested store. Its lock guards only control-plane state —
/// it is never held by the engine — so it cannot back-pressure the engine.
#[derive(Debug, Default)]
pub struct InMemoryAlarmStore {
    alarms: Mutex<HashMap<String, VersionedAlarm>>,
}

impl InMemoryAlarmStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VersionedAlarm>> {
        match self.alarms.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl AlarmRepository for InMemoryAlarmStore {
    fn list(&self, filter: &AlarmFilter) -> ControlResult<Vec<VersionedAlarm>> {
        let guard = self.lock();
        let mut out: Vec<VersionedAlarm> = guard
            .values()
            .filter(|v| filter.matches(&v.record))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.record.id.as_str().cmp(b.record.id.as_str()));
        Ok(out)
    }

    fn get(&self, id: &AlarmId) -> ControlResult<VersionedAlarm> {
        self.lock()
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| ControlError::NotFound {
                kind: ALARM_KIND,
                id: id.as_str().to_owned(),
            })
    }

    fn upsert(&self, record: AlarmRecord) -> ControlResult<VersionedAlarm> {
        let mut guard = self.lock();
        let key = record.id.as_str().to_owned();
        let next = match guard.get(&key) {
            Some(existing) if existing.record == record => {
                // No change: do not churn the version/ETag.
                return Ok(existing.clone());
            }
            Some(existing) => VersionedAlarm {
                version: existing.version.next(),
                record,
            },
            None => VersionedAlarm {
                version: Version::INITIAL,
                record,
            },
        };
        guard.insert(key, next.clone());
        Ok(next)
    }

    fn acknowledge(
        &self,
        id: &AlarmId,
        who: &str,
        when: MediaTime,
    ) -> ControlResult<VersionedAlarm> {
        let mut guard = self.lock();
        let existing = guard
            .get(id.as_str())
            .ok_or_else(|| ControlError::NotFound {
                kind: ALARM_KIND,
                id: id.as_str().to_owned(),
            })?;
        let mut record = existing.record.clone();
        record.ack = AckState::acked(who, when);
        let next = VersionedAlarm {
            version: existing.version.next(),
            record,
        };
        guard.insert(id.as_str().to_owned(), next.clone());
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
    use multiview_core::time::MediaTime;

    use super::{AlarmFilter, AlarmRepository, InMemoryAlarmStore};
    use crate::concurrency::Version;

    fn record(id: &str, severity: PerceivedSeverity, scope: AlarmScope) -> AlarmRecord {
        AlarmRecord::new(
            AlarmId::new(id),
            AlarmKind::Black,
            severity,
            scope,
            MediaTime::from_nanos(10),
        )
    }

    #[test]
    fn upsert_inserts_at_initial_then_bumps_on_change_and_is_idempotent() {
        let store = InMemoryAlarmStore::new();
        let r = record("a", PerceivedSeverity::Major, AlarmScope::System);

        let v1 = store.upsert(r.clone()).unwrap();
        assert_eq!(v1.version, Version::INITIAL);

        // Re-upserting the SAME record does not churn the version.
        let v2 = store.upsert(r.clone()).unwrap();
        assert_eq!(v2.version, Version::INITIAL, "identical upsert is a no-op");

        // A changed record bumps the version.
        let mut r2 = r;
        r2.severity = PerceivedSeverity::Critical;
        let v3 = store.upsert(r2).unwrap();
        assert_eq!(v3.version, Version::INITIAL.next());
        assert_eq!(v3.record.severity, PerceivedSeverity::Critical);
    }

    #[test]
    fn acknowledge_sets_ack_and_bumps_version() {
        let store = InMemoryAlarmStore::new();
        let id = AlarmId::new("a");
        store
            .upsert(record("a", PerceivedSeverity::Major, AlarmScope::System))
            .unwrap();

        let acked = store
            .acknowledge(&id, "alice", MediaTime::from_nanos(99))
            .unwrap();
        assert_eq!(acked.version, Version::INITIAL.next());
        assert!(acked.record.ack.is_acked());
        match acked.record.ack {
            multiview_core::alarm::AckState::Acked { who, when } => {
                assert_eq!(who, "alice");
                assert_eq!(when, MediaTime::from_nanos(99));
            }
            other => panic!("expected acked, got {other:?}"),
        }
    }

    #[test]
    fn acknowledge_unknown_alarm_is_not_found() {
        let store = InMemoryAlarmStore::new();
        let err = store
            .acknowledge(&AlarmId::new("missing"), "bob", MediaTime::ZERO)
            .unwrap_err();
        assert!(matches!(err, crate::error::ControlError::NotFound { .. }));
    }

    #[test]
    fn list_filters_by_severity_active_and_scope() {
        let store = InMemoryAlarmStore::new();
        store
            .upsert(record(
                "tile-major",
                PerceivedSeverity::Major,
                AlarmScope::Tile { index: 1 },
            ))
            .unwrap();
        store
            .upsert(record(
                "sys-warning",
                PerceivedSeverity::Warning,
                AlarmScope::System,
            ))
            .unwrap();
        store
            .upsert(record(
                "tile-cleared",
                PerceivedSeverity::Cleared,
                AlarmScope::Tile { index: 2 },
            ))
            .unwrap();

        // min_severity >= Major: only the tile-major alarm.
        let by_sev = store
            .list(&AlarmFilter {
                min_severity: Some(PerceivedSeverity::Major),
                ..AlarmFilter::default()
            })
            .unwrap();
        assert_eq!(by_sev.len(), 1);
        assert_eq!(by_sev[0].record.id.as_str(), "tile-major");

        // active only: excludes the cleared alarm.
        let active = store
            .list(&AlarmFilter {
                active: Some(true),
                ..AlarmFilter::default()
            })
            .unwrap();
        let active_ids: Vec<&str> = active.iter().map(|v| v.record.id.as_str()).collect();
        assert_eq!(active_ids, vec!["sys-warning", "tile-major"]);

        // scope_kind == tile: the two tile alarms, id-sorted.
        let tiles = store
            .list(&AlarmFilter {
                scope_kind: Some("tile".to_owned()),
                ..AlarmFilter::default()
            })
            .unwrap();
        let tile_ids: Vec<&str> = tiles.iter().map(|v| v.record.id.as_str()).collect();
        assert_eq!(tile_ids, vec!["tile-cleared", "tile-major"]);
    }
}
