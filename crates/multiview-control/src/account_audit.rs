//! The **account-side append-only audit store** (Conspect, ADR-0053 §4 / the
//! Conspect brief §10, §11 #33).
//!
//! Every account-side action — a remote action requested/executed/cancelled, a
//! claim, a transfer, a lease grant/install, an enforcement-level change, a
//! consent change, a relay opt-in/out, a context-pack export, a data-request
//! approve/deny — is written here as an **immutable, timestamped,
//! actor-attributed** entry. This is the operator's evidence trail and a support
//! precondition.
//!
//! **Append-only is structural, not a convention:** the [`AccountAuditStore`]
//! trait exposes [`AccountAuditStore::record`] and [`AccountAuditStore::page`]
//! and **nothing else** — there is no update or delete method, so an installed
//! store *cannot* mutate or remove an entry. ADR-0053's "alternatives rejected"
//! pins this: a mutable audit log destroys the evidence value of the trail.
//!
//! It is also a **dedicated** store, separate from the change-audit log
//! ([`crate::audit`]): conflating engine/config changes with account/licensing
//! actions muddies access control (ADR-0053, alternatives rejected). Same
//! in-memory-bounded default + (later) sqlite-feature persistence pattern
//! (ADR-I003); separate store + route.
//!
//! Like every store in this crate it holds control-plane state only and is never
//! on the engine's data plane, so however it synchronises internally it cannot
//! back-pressure the engine (invariant #10).
use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use multiview_core::time::MediaTime;

/// The resource collection name used in routes and problem documents.
pub const ACCOUNT_AUDIT_KIND: &str = "account-audit";

/// The default cap on retained account-audit entries (oldest evicted past this).
///
/// Bounded so a long-running deployment cannot grow control-plane memory without
/// bound; the most recent activity is always retained (invariant #10 / brief
/// §16). Rotation is by count, like the existing change-audit log.
pub const DEFAULT_ACCOUNT_AUDIT_CAPACITY: usize = 10_000;

/// The kind of account-side action an [`AccountAuditEntry`] records.
///
/// Each variant names a distinct auditable account action from ADR-0053 §4 +
/// the brief §10. Serialised `kebab-case` so the slug is stable across the
/// machine and the portal. `#[non_exhaustive]` so a future account action adds a
/// variant without breaking the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AccountAuditKind {
    /// A machine-claim action (begin / confirm / reject).
    Claim,
    /// A device-transfer action (begin / confirm / cancel).
    Transfer,
    /// A licensing lease was granted by the server (direct or relayed).
    LeaseGrant,
    /// A signed lease binding was verified and installed on this machine
    /// (`POST /api/v1/licence/lease`).
    LeaseInstall,
    /// The enforcement-ladder level changed (e.g. `active` → `warning`).
    EnforcementChange,
    /// The telemetry consent document changed (last-writer-wins, brief §7.1).
    ConsentChange,
    /// The mesh relay opt-in/out toggled (brief §9.2).
    RelayToggle,
    /// A redacted support context-pack was exported (brief §10).
    ContextPackExport,
    /// A data-request egress was approved locally (brief §10, local-wins).
    DataRequestApprove,
    /// A data-request egress was denied locally (brief §10).
    DataRequestDeny,
    /// A remote/operator action was requested (queued — restart/reboot/salvo).
    ActionRequested,
    /// A queued action was cancelled locally (local-always-wins, §11/2).
    ActionCancelled,
    /// A queued action was executed (routed through the command bus or the
    /// machine lifecycle).
    ActionExecuted,
}

/// One immutable account-audit entry: who did what, and when.
///
/// The four spec fields (brief §11: `{at, actor, kind, detail}`) plus a
/// monotonic `seq` cursor the store assigns on append so pagination is stable
/// and resumable even when two entries share an `at` timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AccountAuditEntry {
    /// The monotonic sequence number the store assigned on append — the stable
    /// pagination cursor. Strictly increasing; never reused.
    pub seq: u64,
    /// The media-timeline timestamp (nanoseconds) the action was recorded — the
    /// **when** (`at`).
    pub at_nanos: i64,
    /// The authenticated principal that performed the action (its key id), or a
    /// well-known system actor for machine-originated events — the **actor**.
    pub actor: String,
    /// The kind of account action — the **kind**.
    pub kind: AccountAuditKind,
    /// An optional structured detail (e.g. the lease serial, the salvo name, the
    /// action id). Never contains secrets or raw identifiers (brief §8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// A page of account-audit entries plus the cursor to resume from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AccountAuditPage {
    /// The entries in this page, oldest-first within the page (ascending `seq`).
    pub entries: Vec<AccountAuditEntry>,
    /// The cursor to pass as `?cursor=` to fetch the next page, or `None` when
    /// this page reached the end of the log. Opaque to clients (the `seq` of the
    /// last returned entry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

/// **Append-only** access to the account audit store.
///
/// The trait is deliberately limited to [`AccountAuditStore::record`] (append)
/// and [`AccountAuditStore::page`] (read): there is **no** update or delete, so
/// the store is append-only **by construction** — an implementer cannot offer a
/// mutation path through this trait.
pub trait AccountAuditStore: Send + Sync + 'static {
    /// Append an entry recording an account action, returning the `seq` the store
    /// assigned. `at` is the media-timeline timestamp the control plane injects
    /// (its own clock, off the engine).
    fn record(
        &self,
        actor: &str,
        kind: AccountAuditKind,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    ) -> u64;

    /// Return a page of entries **oldest-first**, starting strictly after the
    /// `cursor` `seq` (or from the beginning when `cursor` is `None`), filtered
    /// to a single [`AccountAuditKind`] when `filter` is set, and capped at
    /// `limit` entries. The returned [`AccountAuditPage::next_cursor`] is `Some`
    /// iff more entries remain past the page.
    fn page(
        &self,
        cursor: Option<u64>,
        filter: Option<AccountAuditKind>,
        limit: usize,
    ) -> AccountAuditPage;
}

/// Alias matching the crate's `*Repository` naming for trait objects in state.
pub use AccountAuditStore as AccountAuditRepository;

/// An in-memory, bounded, **append-only** [`AccountAuditStore`].
///
/// Backed by a `Mutex<VecDeque>` capped at a fixed capacity (oldest evicted) plus
/// a monotonic `next_seq`. The lock guards control-plane state only — never held
/// by the engine — so it cannot back-pressure the engine (invariant #10).
#[derive(Debug)]
pub struct InMemoryAccountAudit {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Entries in ascending `seq` order (append at the back).
    entries: VecDeque<AccountAuditEntry>,
    /// The next `seq` to assign — monotonic, never reused even across eviction.
    next_seq: u64,
}

impl Default for InMemoryAccountAudit {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_ACCOUNT_AUDIT_CAPACITY)
    }
}

impl InMemoryAccountAudit {
    /// A fresh, empty store at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty store retaining at most `capacity` newest entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Lock the inner state, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl AccountAuditStore for InMemoryAccountAudit {
    fn record(
        &self,
        actor: &str,
        kind: AccountAuditKind,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    ) -> u64 {
        let mut guard = self.lock();
        let seq = guard.next_seq;
        guard.next_seq = guard.next_seq.saturating_add(1);
        let entry = AccountAuditEntry {
            seq,
            at_nanos: at.as_nanos(),
            actor: actor.to_owned(),
            kind,
            detail,
        };
        guard.entries.push_back(entry);
        while guard.entries.len() > self.capacity {
            guard.entries.pop_front();
        }
        seq
    }

    fn page(
        &self,
        cursor: Option<u64>,
        filter: Option<AccountAuditKind>,
        limit: usize,
    ) -> AccountAuditPage {
        let limit = limit.max(1);
        let guard = self.lock();
        // Strictly after the cursor (resume semantics): the cursor is the last
        // returned `seq`, so the next page begins at `seq > cursor`.
        let mut matching = guard
            .entries
            .iter()
            .filter(|e| cursor.is_none_or(|c| e.seq > c))
            .filter(|e| filter.is_none_or(|k| e.kind == k));
        let mut entries = Vec::with_capacity(limit.min(guard.entries.len()));
        for entry in matching.by_ref().take(limit) {
            entries.push(entry.clone());
        }
        // There is a next page iff at least one more matching entry remains.
        let next_cursor = if matching.next().is_some() {
            entries.last().map(|e| e.seq)
        } else {
            None
        };
        AccountAuditPage {
            entries,
            next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        AccountAuditKind, AccountAuditStore, InMemoryAccountAudit, DEFAULT_ACCOUNT_AUDIT_CAPACITY,
    };
    use multiview_core::time::MediaTime;

    fn rec(store: &InMemoryAccountAudit, kind: AccountAuditKind, n: i64) -> u64 {
        store.record("actor", kind, MediaTime::from_nanos(n), None)
    }

    #[test]
    fn record_assigns_monotonic_strictly_increasing_seq() {
        let store = InMemoryAccountAudit::new();
        let a = rec(&store, AccountAuditKind::LeaseInstall, 1);
        let b = rec(&store, AccountAuditKind::ActionRequested, 2);
        let c = rec(&store, AccountAuditKind::ActionCancelled, 3);
        assert_eq!((a, b, c), (0, 1, 2), "seq starts at 0 and increments by 1");
    }

    #[test]
    fn page_is_oldest_first_and_resumes_from_cursor_without_gaps_or_dupes() {
        let store = InMemoryAccountAudit::new();
        for i in 0..5 {
            rec(&store, AccountAuditKind::ActionRequested, i);
        }
        let first = store.page(None, None, 2);
        let seqs: Vec<u64> = first.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1], "oldest-first page of 2");
        assert_eq!(first.next_cursor, Some(1), "resume after the last seq");

        let second = store.page(first.next_cursor, None, 2);
        let seqs: Vec<u64> = second.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3], "no gap, no dupe across the page boundary");
        assert_eq!(second.next_cursor, Some(3));

        let third = store.page(second.next_cursor, None, 2);
        let seqs: Vec<u64> = third.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![4], "the final partial page");
        assert_eq!(third.next_cursor, None, "no next page at the end");
    }

    #[test]
    fn page_filters_by_kind() {
        let store = InMemoryAccountAudit::new();
        rec(&store, AccountAuditKind::LeaseInstall, 1);
        rec(&store, AccountAuditKind::ActionRequested, 2);
        rec(&store, AccountAuditKind::LeaseInstall, 3);
        let page = store.page(None, Some(AccountAuditKind::LeaseInstall), 10);
        assert_eq!(page.entries.len(), 2);
        assert!(page
            .entries
            .iter()
            .all(|e| e.kind == AccountAuditKind::LeaseInstall));
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn bounded_eviction_keeps_newest_and_seq_never_rewinds() {
        let store = InMemoryAccountAudit::with_capacity(2);
        for i in 0..5 {
            rec(&store, AccountAuditKind::ActionRequested, i);
        }
        // Capacity is 2 so only seq 3 and 4 survive; seq is never reused.
        let page = store.page(None, None, 100);
        let seqs: Vec<u64> = page.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 4], "oldest evicted, newest retained");
    }

    #[test]
    fn default_capacity_is_the_documented_constant() {
        // The capacity constant is load-bearing (brief §16 bounded memory); pin it
        // so a "tidy" cannot silently change the retention floor.
        assert_eq!(DEFAULT_ACCOUNT_AUDIT_CAPACITY, 10_000);
    }
}
