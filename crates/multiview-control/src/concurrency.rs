//! Optimistic concurrency (`ETag`/`If-Match` → `412`) and idempotency-key
//! deduplication.
//!
//! Each mutable resource carries a monotonic [`Version`] that renders as a weak
//! `ETag` header. A mutating request presents the version it expects in
//! `If-Match`; a mismatch yields `412 Precondition Failed`
//! ([`ControlError::VersionConflict`]) so two operators cannot silently clobber
//! each other (ADR-W006).
//!
//! Start/stop/swap requests carry an `Idempotency-Key` so a retried submission
//! is de-duplicated rather than enqueuing the command twice. [`IdempotencyStore`]
//! remembers the [`OperationId`] minted for a key so a replay returns the
//! original id.
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::header;
use axum::http::request::Parts;

use crate::command::OperationId;
use crate::error::ControlError;

/// A monotonic resource version.
///
/// Bumped on every successful mutation. Rendered as a weak `ETag`
/// (`W/"<n>"`) and matched against `If-Match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(u64);

impl Version {
    /// The version of a freshly-created resource.
    pub const INITIAL: Self = Self(1);

    /// Construct a version from a raw counter.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next version (saturating at [`u64::MAX`], which is not reachable in
    /// practice).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Render as a weak `ETag` header value (`W/"<n>"`).
    #[must_use]
    pub fn to_etag(self) -> String {
        format!("W/\"{}\"", self.0)
    }

    /// Parse a version out of an `If-Match` / `ETag` header value.
    ///
    /// Accepts both the weak form `W/"5"` and a bare `"5"` or `5`. Returns
    /// [`None`] if no integer can be extracted.
    #[must_use]
    pub fn parse_etag(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let without_weak = trimmed.strip_prefix("W/").unwrap_or(trimmed);
        let inner = without_weak.trim().trim_matches('"');
        inner.parse::<u64>().ok().map(Self)
    }
}

/// An extracted `If-Match` precondition.
///
/// Use [`IfMatch::require`] on a mutating handler to enforce that the client
/// presented a matching version, returning `412`/`428` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfMatch {
    /// The version the client presented, if any well-formed `If-Match` header
    /// was sent.
    pub expected: Option<Version>,
    /// The raw header value (for diagnostics), if present.
    pub raw: Option<String>,
}

impl IfMatch {
    /// Enforce the precondition against the resource's `current` version.
    ///
    /// # Errors
    ///
    /// - [`ControlError::PreconditionRequired`] if no `If-Match` was sent.
    /// - [`ControlError::VersionConflict`] if the presented version differs from
    ///   `current`.
    pub fn require(
        &self,
        kind: &'static str,
        id: &str,
        current: Version,
    ) -> Result<(), ControlError> {
        let Some(expected) = self.expected else {
            return Err(ControlError::PreconditionRequired { kind });
        };
        if expected == current {
            Ok(())
        } else {
            Err(ControlError::VersionConflict {
                kind,
                id: id.to_owned(),
                expected: expected.get().to_string(),
                actual: current.get().to_string(),
            })
        }
    }
}

impl<S: Sync> FromRequestParts<S> for IfMatch {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let raw = parts
            .headers
            .get(header::IF_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let expected = raw.as_deref().and_then(Version::parse_etag);
        std::future::ready(Ok(Self { expected, raw }))
    }
}

/// An extracted `Idempotency-Key` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyKey(pub Option<String>);

impl<S: Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let key = parts
            .headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        std::future::ready(Ok(Self(key)))
    }
}

/// The result of reserving an idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    /// This key was unseen: `op` is the freshly-minted id to use and record.
    Fresh(OperationId),
    /// This key was already used: `op` is the original id; do **not** re-enqueue
    /// the command — return the original `202` outcome.
    Replay(OperationId),
}

/// Default retention of an idempotency key (24 h): long enough for any realistic
/// client retry window, short enough that routine per-action keys (ADR-W021 §6)
/// cannot accumulate forever.
pub const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default cap on retained idempotency keys. At the cap the oldest key is evicted
/// first, so memory is bounded regardless of submission rate.
pub const DEFAULT_IDEMPOTENCY_CAPACITY: usize = 100_000;

#[derive(Debug)]
struct Entry {
    op: OperationId,
    /// When the key was first recorded; the TTL runs from here.
    at: Instant,
    /// Insertion sequence: the key into the oldest-first order index.
    seq: u64,
}

#[derive(Debug, Default)]
struct Inner {
    seen: HashMap<String, Entry>,
    /// Insertion order (`seq` -> key), oldest first, for O(log n) eviction.
    order: BTreeMap<u64, String>,
    next_seq: u64,
}

impl Inner {
    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.seen.remove(key) {
            self.order.remove(&entry.seq);
        }
    }

    /// Drop expired keys (oldest first) so the map never holds dead entries.
    fn sweep(&mut self, now: Instant, ttl: Duration) {
        while let Some((&seq, key)) = self.order.iter().next() {
            let expired = self
                .seen
                .get(key)
                .is_none_or(|e| now.saturating_duration_since(e.at) >= ttl);
            if !expired {
                break;
            }
            let key = key.clone();
            self.order.remove(&seq);
            self.seen.remove(&key);
        }
    }
}

/// An in-memory store mapping `Idempotency-Key` values to the operation id that
/// was minted for the first request bearing that key.
///
/// A retried start/stop/swap with the same key returns the original
/// [`OperationId`] ([`Reservation::Replay`]) so the engine is never asked to
/// apply the command twice. Retention is **bounded** (ADR-W021 §6): a key lives
/// for a TTL ([`DEFAULT_IDEMPOTENCY_TTL`]) after which it is treated as new, and
/// the store holds at most a capacity of keys ([`DEFAULT_IDEMPOTENCY_CAPACITY`]),
/// evicting the oldest first. Eviction is lazy, on insert/lookup, so no
/// background task is needed. The store is a plain `Mutex` — it is **not** on the
/// engine's data plane (control-only state), so guarding it with a lock cannot
/// back-pressure the engine.
#[derive(Debug)]
pub struct IdempotencyStore {
    inner: Mutex<Inner>,
    ttl: Duration,
    capacity: usize,
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::with_limits(DEFAULT_IDEMPOTENCY_TTL, DEFAULT_IDEMPOTENCY_CAPACITY)
    }
}

impl IdempotencyStore {
    /// A fresh, empty store with the default TTL and capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh store with an explicit `ttl` and `capacity` (min 1).
    #[must_use]
    pub fn with_limits(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// The number of keys currently retained (expired keys not yet swept count).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().seen.len()
    }

    /// Whether no keys are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock cannot corrupt correctness here (control-only state);
        // recover the guard so a prior panic in another request does not wedge
        // the whole control plane.
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Reserve an operation id for `key`.
    ///
    /// If `key` is `None`, every call is [`Reservation::Fresh`] (no dedupe). If
    /// `key` was seen within the TTL, returns [`Reservation::Replay`] with the
    /// original id; otherwise records and returns a [`Reservation::Fresh`] id.
    #[must_use]
    pub fn reserve(&self, key: Option<&str>) -> Reservation {
        self.reserve_at(key, Instant::now())
    }

    fn reserve_at(&self, key: Option<&str>, now: Instant) -> Reservation {
        let Some(key) = key else {
            return Reservation::Fresh(OperationId::new());
        };
        let mut guard = self.lock();
        guard.sweep(now, self.ttl);
        if let Some(existing) = guard.seen.get(key) {
            if now.saturating_duration_since(existing.at) < self.ttl {
                return Reservation::Replay(existing.op.clone());
            }
            guard.remove(key);
        }
        while guard.seen.len() >= self.capacity {
            let Some((_, oldest)) = guard.order.pop_first() else {
                break;
            };
            guard.seen.remove(&oldest);
        }
        let op = OperationId::new();
        let seq = guard.next_seq;
        guard.next_seq = seq.wrapping_add(1);
        guard.order.insert(seq, key.to_owned());
        guard.seen.insert(
            key.to_owned(),
            Entry {
                op: op.clone(),
                at: now,
                seq,
            },
        );
        Reservation::Fresh(op)
    }

    /// Release a previously [`reserve`](Self::reserve)d key, but **only** if it
    /// still maps to `op`.
    ///
    /// This rolls back a [`Reservation::Fresh`] whose command never made it onto
    /// the engine command bus (e.g. the bus shed the submission with `503`): the
    /// key must not stay recorded, or a later retry would observe a
    /// [`Reservation::Replay`] and receive a false `202 Accepted` for a command
    /// that was never enqueued. Releasing lets the retry re-reserve and actually
    /// submit.
    ///
    /// The `op`-match guard makes release idempotent and safe under concurrency:
    /// if another request has since re-reserved the same key (minting a *new*
    /// `op`), this is a no-op so we never erase a live reservation. Releasing a
    /// `None` key or an unknown key is a no-op.
    pub fn release(&self, key: Option<&str>, op: &OperationId) {
        let Some(key) = key else {
            return;
        };
        let mut guard = self.lock();
        if guard.seen.get(key).is_some_and(|e| e.op == *op) {
            guard.remove(key);
        }
    }

    /// Re-point a [`Reservation::Fresh`] at the operation that actually
    /// executed.
    ///
    /// Single-flight surfaces (the discovery scan) use this when a fresh
    /// reservation `from` could not start its own operation because it
    /// **attached** to an already-running one (`to`): the key must replay the
    /// *running* op — replaying `from` would name an operation that never
    /// executed. Like [`release`](Self::release), the `from`-match guard makes
    /// this idempotent and concurrency-safe: if another request re-reserved
    /// the key (minting a new op), this is a no-op so a live reservation is
    /// never clobbered. A `None`/unknown key is a no-op.
    pub fn rebind(&self, key: Option<&str>, from: &OperationId, to: OperationId) {
        let Some(key) = key else {
            return;
        };
        let mut guard = self.lock();
        if let Some(entry) = guard.seen.get_mut(key) {
            if entry.op == *from {
                entry.op = to;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::time::{Duration, Instant};

    use super::{IdempotencyStore, Reservation, Version};

    #[test]
    fn parse_etag_accepts_weak_and_bare_forms() {
        assert_eq!(Version::parse_etag("W/\"5\""), Some(Version::new(5)));
        assert_eq!(Version::parse_etag("\"7\""), Some(Version::new(7)));
        assert_eq!(Version::parse_etag("11"), Some(Version::new(11)));
        assert_eq!(Version::parse_etag("not-a-version"), None);
    }

    #[test]
    fn release_rolls_back_a_fresh_reservation_so_the_key_can_be_re_reserved() {
        let store = IdempotencyStore::new();
        let key = Some("k");

        let Reservation::Fresh(op) = store.reserve(key) else {
            panic!("first reserve of an unseen key must be Fresh");
        };

        // Without release, a second reserve replays the same op.
        assert!(matches!(store.reserve(key), Reservation::Replay(_)));

        // Releasing the recorded op frees the key: the NEXT reserve is Fresh
        // again with a brand-new op (proving the shed reservation is gone).
        store.release(key, &op);
        let Reservation::Fresh(op2) = store.reserve(key) else {
            panic!("after release the key must reserve Fresh again");
        };
        assert_ne!(op, op2, "the re-reservation mints a new operation id");
    }

    #[test]
    fn rebind_points_a_fresh_reservation_at_the_running_op() {
        let store = IdempotencyStore::new();
        let key = Some("k");

        let Reservation::Fresh(fresh) = store.reserve(key) else {
            panic!("first reserve is Fresh");
        };
        let running = super::OperationId::new();

        // The fresh reservation attached to a running operation: rebinding
        // makes a later replay answer with the RUNNING op.
        store.rebind(key, &fresh, running.clone());
        assert!(
            matches!(store.reserve(key), Reservation::Replay(op) if op == running),
            "the replay echoes the running op after rebind"
        );

        // A stale rebind (the key no longer maps to `from`) is a no-op.
        let stale = super::OperationId::new();
        store.rebind(key, &fresh, stale.clone());
        assert!(
            matches!(store.reserve(key), Reservation::Replay(op) if op == running),
            "a stale rebind never clobbers the live mapping"
        );
    }

    #[test]
    fn release_is_a_no_op_when_the_key_was_re_reserved_by_someone_else() {
        // Concurrency guard: if another request re-reserved the key (minting a
        // new op) before our late release lands, release must NOT erase the live
        // reservation — it only removes its OWN op.
        let store = IdempotencyStore::new();
        let key = Some("k");

        let Reservation::Fresh(stale_op) = store.reserve(key) else {
            panic!("first reserve is Fresh");
        };
        // Simulate the key being released and re-reserved by a concurrent path.
        store.release(key, &stale_op);
        let Reservation::Fresh(live_op) = store.reserve(key) else {
            panic!("re-reserve is Fresh");
        };

        // A late release carrying the STALE op must not disturb the live one.
        store.release(key, &stale_op);
        assert!(
            matches!(store.reserve(key), Reservation::Replay(replayed) if replayed == live_op),
            "the live reservation survives a stale release"
        );
    }

    #[test]
    fn capacity_cap_holds_under_ten_times_inserts() {
        let cap = 50;
        let store = IdempotencyStore::with_limits(Duration::from_secs(3600), cap);
        let now = Instant::now();
        for i in 0..cap * 10 {
            let _ = store.reserve_at(Some(&format!("k{i}")), now);
            assert!(store.len() <= cap, "entry count must never exceed the cap");
        }
        assert_eq!(store.len(), cap, "memory stays bounded at the cap");
    }

    #[test]
    fn eviction_is_oldest_first() {
        let now = Instant::now();
        let store = IdempotencyStore::with_limits(Duration::from_secs(3600), 3);
        for k in ["a", "b", "c"] {
            let _ = store.reserve_at(Some(k), now);
        }
        let _ = store.reserve_at(Some("d"), now);
        assert!(matches!(
            store.reserve_at(Some("b"), now),
            Reservation::Replay(_)
        ));
        assert!(matches!(
            store.reserve_at(Some("c"), now),
            Reservation::Replay(_)
        ));
        assert!(matches!(
            store.reserve_at(Some("d"), now),
            Reservation::Replay(_)
        ));
        assert!(
            matches!(store.reserve_at(Some("a"), now), Reservation::Fresh(_)),
            "the oldest key was evicted and is treated as new"
        );
    }

    #[test]
    fn an_expired_key_is_treated_as_new() {
        let ttl = Duration::from_secs(60);
        let store = IdempotencyStore::with_limits(ttl, 100);
        let t0 = Instant::now();
        let Reservation::Fresh(first) = store.reserve_at(Some("k"), t0) else {
            panic!("fresh");
        };
        let Reservation::Fresh(second) =
            store.reserve_at(Some("k"), t0 + ttl + Duration::from_secs(1))
        else {
            panic!("an expired key must reserve Fresh again");
        };
        assert_ne!(first, second, "expiry mints a new operation id");
        assert_eq!(store.len(), 1, "the expired entry was replaced, not leaked");
    }

    #[test]
    fn within_the_ttl_a_replay_returns_the_original_acceptance() {
        let ttl = Duration::from_secs(60);
        let store = IdempotencyStore::with_limits(ttl, 100);
        let t0 = Instant::now();
        let Reservation::Fresh(op) = store.reserve_at(Some("k"), t0) else {
            panic!("fresh");
        };
        for secs in [1, 30, 59] {
            assert!(
                matches!(
                    store.reserve_at(Some("k"), t0 + Duration::from_secs(secs)),
                    Reservation::Replay(ref r) if *r == op
                ),
                "replay at +{secs}s must return the original op, not re-execute"
            );
        }
    }

    #[test]
    fn expired_entries_are_swept_on_insert() {
        let ttl = Duration::from_secs(60);
        let store = IdempotencyStore::with_limits(ttl, 1000);
        let t0 = Instant::now();
        for i in 0..100 {
            let _ = store.reserve_at(Some(&format!("k{i}")), t0);
        }
        assert_eq!(store.len(), 100);
        let _ = store.reserve_at(Some("late"), t0 + ttl + Duration::from_secs(1));
        assert_eq!(store.len(), 1, "expired keys are evicted lazily on insert");
    }

    #[test]
    fn release_and_rebind_keep_the_store_consistent() {
        let store = IdempotencyStore::with_limits(Duration::from_secs(3600), 2);
        let now = Instant::now();
        let Reservation::Fresh(a) = store.reserve_at(Some("a"), now) else {
            panic!("fresh");
        };
        store.release(Some("a"), &a);
        assert_eq!(store.len(), 0);
        for k in ["x", "y", "z"] {
            let _ = store.reserve_at(Some(k), now);
        }
        assert_eq!(store.len(), 2);
    }
}
