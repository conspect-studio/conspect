//! The runtime store of **ephemeral** cast sessions (DEV-D2, ADR-M011).
//!
//! An ad-hoc cast session is runtime-only state: it lives here (and as a
//! running session actor in the
//! [`DevicePollerRegistry`](crate::devices::DevicePollerRegistry)) and **never**
//! enters the devices resource store, so a config export can never emit it.
//! "Save as device" promotes a session into a normal `Device{driver: cast}`
//! registry entry — at which point the *device* is durable desired state and
//! this record is dropped.
//!
//! Control-plane only, `Mutex`-guarded, bounded by the number of live
//! sessions (invariant #10 — the engine never touches this).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// One ephemeral cast session's descriptive record (the live lifecycle state
/// rides the latest-wins status registry under the same id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastSessionRecord {
    /// The runtime session id (`cast-session-…`, UUID-fresh per start).
    pub id: String,
    /// The operator-facing name, if given.
    pub name: Option<String>,
    /// The device authority dialled (`host[:port]`, IPv6 bracketed).
    pub address: String,
    /// The output id whose rendition the session casts.
    pub output: String,
    /// The resolved device-reachable media URL the session LOADs.
    pub media_url: String,
    /// When the receiver **accepted** the session's `LOAD` (the first
    /// `MEDIA_STATUS` attributing an active media session to our actor — the
    /// moment the cast verifiably began showing), as Unix nanoseconds from
    /// the control plane's injectable clock (the same `AckClock` the audit
    /// log stamps with). `None` until then: a session whose LOAD was refused,
    /// or is still establishing, has not started (DEV-D3.1).
    pub started_unix_ns: Option<i64>,
}

/// The `Mutex`-guarded map of live ephemeral sessions, id-sorted.
#[derive(Debug, Default)]
pub struct CastSessionStore {
    inner: Mutex<BTreeMap<String, CastSessionRecord>>,
}

impl CastSessionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the map, recovering from a poisoned lock (a panic in one request
    /// must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, CastSessionRecord>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Insert (or replace) a session record.
    pub fn insert(&self, record: CastSessionRecord) {
        self.lock().insert(record.id.clone(), record);
    }

    /// The record for `id`, if the session is live.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<CastSessionRecord> {
        self.lock().get(id).cloned()
    }

    /// Remove (and return) the record for `id`.
    pub fn remove(&self, id: &str) -> Option<CastSessionRecord> {
        self.lock().remove(id)
    }

    /// Stamp `id`'s started-at (Unix nanoseconds). **First-write-wins**: the
    /// session started when its LOAD was first accepted; a supervised re-LOAD
    /// after an IDLE never moves the start. A no-op for an id this store does
    /// not track — a *saved* cast device's actor runs the same driver but has
    /// no ephemeral record to stamp.
    pub fn mark_started(&self, id: &str, started_unix_ns: i64) {
        let mut guard = self.lock();
        if let Some(record) = guard.get_mut(id) {
            if record.started_unix_ns.is_none() {
                record.started_unix_ns = Some(started_unix_ns);
            }
        }
    }

    /// All live session records, id-sorted.
    #[must_use]
    pub fn list(&self) -> Vec<CastSessionRecord> {
        self.lock().values().cloned().collect()
    }
}
