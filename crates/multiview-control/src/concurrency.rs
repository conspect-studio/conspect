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
use std::collections::HashMap;
use std::sync::Mutex;

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

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let raw = parts
            .headers
            .get(header::IF_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let expected = raw.as_deref().and_then(Version::parse_etag);
        Ok(Self { expected, raw })
    }
}

/// An extracted `Idempotency-Key` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyKey(pub Option<String>);

impl<S: Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok(Self(key))
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

/// An in-memory store mapping `Idempotency-Key` values to the operation id that
/// was minted for the first request bearing that key.
///
/// A retried start/stop/swap with the same key returns the original
/// [`OperationId`] ([`Reservation::Replay`]) so the engine is never asked to
/// apply the command twice. The store is a plain `Mutex<HashMap>` — it is **not**
/// on the engine's data plane (control-only state), so guarding it with a lock
/// cannot back-pressure the engine.
#[derive(Debug, Default)]
pub struct IdempotencyStore {
    seen: Mutex<HashMap<String, OperationId>>,
}

impl IdempotencyStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve an operation id for `key`.
    ///
    /// If `key` is `None`, every call is [`Reservation::Fresh`] (no dedupe). If
    /// `key` was seen before, returns [`Reservation::Replay`] with the original
    /// id; otherwise records and returns a [`Reservation::Fresh`] id.
    #[must_use]
    pub fn reserve(&self, key: Option<&str>) -> Reservation {
        let Some(key) = key else {
            return Reservation::Fresh(OperationId::new());
        };
        // A poisoned lock cannot corrupt correctness here (control-only state);
        // recover the guard so a prior panic in another request does not wedge
        // the whole control plane.
        let mut guard = match self.seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(existing) = guard.get(key) {
            return Reservation::Replay(existing.clone());
        }
        let op = OperationId::new();
        guard.insert(key.to_owned(), op.clone());
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
        let mut guard = match self.seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.get(key) == Some(op) {
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
        let mut guard = match self.seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.get(key) == Some(from) {
            guard.insert(key.to_owned(), to);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

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
}
