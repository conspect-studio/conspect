//! The SAP **discovered-session table** — a bounded, fixed-capacity, drop-oldest
//! inventory of **untrusted** sessions learned from the network (RFC 2974
//! lifecycle; ADR-0041 §3/§4/§8, brief §3/§9).
//!
//! A listener parses each inbound SAP datagram into a [`SapPacket`] and calls
//! [`SapSessionTable::observe`]. The table:
//!
//! * keys a session on `(msg-id hash, originating source)` ([`SessionKey`]) and
//!   **refreshes** it on re-announcement, learning the `observed_period`;
//! * **ignores inbound `T=1` deletions** against a tracked session — a spoofed
//!   deletion must never withdraw one (ADR-0041 §8); sessions expire only by
//!   timeout ([`SapSessionTable::purge`], `max(10 × period, 1 h)`);
//! * bounds **one origin's** share ([`SapSessionTable::with_limits`]'s
//!   `per_origin_cap`) and the **whole table** (`capacity`), always
//!   **drop-oldest, never grow** (bounded memory, inv #10);
//! * publishes a newest-wins snapshot via the wait-free `ArcSwap` read-copy-update
//!   pattern ([`SapSessionTable::inventory`]) — the reader never blocks the
//!   writer and nothing here can pace or back-pressure the engine (inv #1/#10).
//!
//! Discovered sessions are **untrusted hints**: the table only records them.
//! Binding one as a Source is an explicit operator **confirm-to-bind** action
//! elsewhere (ADR-0041 §4) — this table never ingests or acts on a session.

use std::net::IpAddr;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arc_swap::ArcSwap;

use super::packet::{SapMessageType, SapPacket};

/// Default maximum number of discovered sessions retained across all sources.
pub const DEFAULT_SESSION_CAPACITY: usize = 256;

/// Default maximum number of sessions retained from any single origin, so one
/// source cannot monopolise the table by flooding distinct message-id hashes.
pub const DEFAULT_PER_ORIGIN_CAP: usize = 32;

/// The purge floor: a session unseen for at least this long is dropped even when
/// its announcement period is short or unknown (RFC 2974 receivers purge after
/// `max(10 × period, 1 h)`; ADR-0041 §3).
pub const PURGE_FLOOR: Duration = Duration::from_secs(3600);

/// The multiple of the observed announcement period after which a session is
/// purged (when that exceeds [`PURGE_FLOOR`]).
const PURGE_PERIOD_MULTIPLIER: u32 = 10;

/// The identity of a discovered SAP session: its 16-bit message-id hash and its
/// originating source address (RFC 2974 — an announcement with the same pair
/// refreshes the same session; a changed hash from the same origin is a modified
/// session).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// The 16-bit message-id hash (never 0 — enforced by [`NonZeroU16`]).
    pub msg_id_hash: NonZeroU16,
    /// The announcement's originating source address.
    pub origin: IpAddr,
}

/// A single discovered (untrusted) SAP session and the metadata the table tracks
/// to refresh and age it.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    /// The session identity `(msg-id hash, origin)`.
    pub key: SessionKey,
    /// The announcement's payload-type, if it carried an explicit one (`None`
    /// when the SDP body began `v=0`, the common case).
    pub payload_type: Option<String>,
    /// The opaque SDP body most recently announced for this session (never parsed
    /// here — an untrusted hint for the operator to inspect and confirm-to-bind).
    pub sdp: Vec<u8>,
    /// The monotonic time the session was first observed.
    pub first_seen: Duration,
    /// The monotonic time the session was most recently (re-)announced.
    pub last_seen: Duration,
    /// The most recent inter-arrival gap between announcements, or `None` until a
    /// second announcement is seen. Drives the [`SapSessionTable::purge`]
    /// threshold `max(10 × period, 1 h)`.
    pub observed_period: Option<Duration>,
    /// How many announcements have been observed for this session.
    pub announcements: u64,
}

/// The outcome of a single [`SapSessionTable::observe`] call, for the caller's
/// observability (e.g. logging drops/refreshes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObserveOutcome {
    /// A new session was recorded (possibly evicting the oldest to stay bounded).
    Inserted,
    /// An announcement for an already-tracked session refreshed it.
    Refreshed,
    /// An inbound `T=1` deletion was ignored (hijack guard, ADR-0041 §8) — it
    /// neither withdrew a tracked session nor recorded anything.
    DeletionIgnored,
}

/// A bounded, drop-oldest, wait-free-published table of discovered SAP sessions.
///
/// Cheap clone-on-write (RCU): [`observe`](Self::observe)/[`purge`](Self::purge)
/// clone the current snapshot, mutate the copy, and atomically publish it, so a
/// reader calling [`inventory`](Self::inventory) never blocks a writer and vice
/// versa. Nothing here touches the output clock (inv #1) or can back-pressure
/// the engine (inv #10).
#[derive(Debug)]
pub struct SapSessionTable {
    sessions: ArcSwap<Vec<DiscoveredSession>>,
    /// Serializes **writers** ([`observe`](Self::observe) / [`purge`](Self::purge))
    /// so their load → clone → mutate → store is atomic against each other.
    /// `ArcSwap` publishes atomically but does not make the whole read-modify-write
    /// atomic, so without this two concurrent writers (a dual-stack v4 + v6
    /// listener sharing one table via `with_table`; the purge tick) could each
    /// clone the same snapshot and the second store would clobber the first,
    /// losing a session (P2-F2). **Readers never take this lock** — they stay
    /// wait-free on `sessions`, and every writer is control/discovery-plane (never
    /// the engine data plane), so serializing them cannot pace or back-pressure
    /// the engine (inv #1/#10).
    write: Mutex<()>,
    capacity: usize,
    per_origin_cap: usize,
    /// How many [`purge`](Self::purge) reap scans have run since construction
    /// (observability). A well-behaved reaper runs on its steady cadence; a burst
    /// of scans back-to-back means the purge timer amplified missed ticks (I1) —
    /// the [`purge_count`](Self::purge_count) a test asserts against.
    purges: AtomicU64,
}

impl SapSessionTable {
    /// Create an empty table with the default global and per-origin caps.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_SESSION_CAPACITY, DEFAULT_PER_ORIGIN_CAP)
    }

    /// Create an empty table retaining at most `capacity` sessions overall and at
    /// most `per_origin_cap` from any single origin (each clamped to ≥ 1).
    #[must_use]
    pub fn with_limits(capacity: usize, per_origin_cap: usize) -> Self {
        Self {
            sessions: ArcSwap::from_pointee(Vec::new()),
            write: Mutex::new(()),
            capacity: capacity.max(1),
            per_origin_cap: per_origin_cap.max(1),
            purges: AtomicU64::new(0),
        }
    }

    /// Fold one parsed inbound SAP packet into the table.
    ///
    /// An **announcement** (`T=0`) inserts a new session or refreshes the tracked
    /// one with the same `(hash, origin)`. A **deletion** (`T=1`) is **ignored**
    /// (ADR-0041 §8): it never withdraws a tracked session and never records one —
    /// sessions expire only via [`purge`](Self::purge). New sessions are bounded
    /// per-origin then globally, always dropping the oldest (by `last_seen`),
    /// never growing past the configured caps.
    pub fn observe(&self, packet: &SapPacket, now: Duration) -> ObserveOutcome {
        if packet.message_type == SapMessageType::Deletion {
            return ObserveOutcome::DeletionIgnored;
        }
        let key = SessionKey {
            msg_id_hash: packet.msg_id_hash,
            origin: packet.origin,
        };
        // Serialize this read-modify-write against concurrent writers (P2-F2): the
        // `load` → `clone` → `store` below is only atomic as a whole under this
        // lock. A poisoned lock (a writer panicked mid-mutate) still yields the
        // guard — the published snapshot is always consistent, so recovering is
        // correct and keeps the table live.
        let _write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.sessions.load();
        let mut next: Vec<DiscoveredSession> = current.as_ref().clone();

        if let Some(existing) = next.iter_mut().find(|s| s.key == key) {
            // Learn the most recent inter-arrival gap; keep the old period if the
            // clock somehow went backwards (checked_sub → None).
            existing.observed_period = now
                .checked_sub(existing.last_seen)
                .or(existing.observed_period);
            existing.last_seen = now;
            existing.payload_type.clone_from(&packet.payload_type);
            existing.sdp.clone_from(&packet.payload);
            existing.announcements = existing.announcements.saturating_add(1);
            self.sessions.store(Arc::new(next));
            return ObserveOutcome::Refreshed;
        }

        // A genuinely new session. Bound this origin's share (drop-oldest within
        // the origin), insert, then bound the whole table (drop-oldest globally).
        if next.iter().filter(|s| s.key.origin == key.origin).count() >= self.per_origin_cap {
            if let Some(pos) = oldest_position(&next, |s| s.key.origin == key.origin) {
                next.remove(pos);
            }
        }
        next.push(DiscoveredSession {
            key,
            payload_type: packet.payload_type.clone(),
            sdp: packet.payload.clone(),
            first_seen: now,
            last_seen: now,
            observed_period: None,
            announcements: 1,
        });
        while next.len() > self.capacity {
            match oldest_position(&next, |_| true) {
                Some(pos) => {
                    next.remove(pos);
                }
                None => break,
            }
        }
        self.sessions.store(Arc::new(next));
        ObserveOutcome::Inserted
    }

    /// Drop sessions unseen for longer than `max(10 × observed_period, 1 h)` (the
    /// [`PURGE_FLOOR`] applies when the period is short or unknown). A no-op with
    /// no allocation when nothing has expired.
    pub fn purge(&self, now: Duration) {
        // Count every reap scan (observability): a steady cadence increments this
        // once per purge period; a back-to-back burst means the purge timer
        // amplified missed ticks (I1).
        self.purges.fetch_add(1, Ordering::Relaxed);
        // Serialize against `observe` on the same write lock (P2-F2) so a purge
        // and an insert cannot each clone the same snapshot and clobber the other.
        let _write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.sessions.load();
        if !current.iter().any(|s| is_expired(s, now)) {
            return;
        }
        let next: Vec<DiscoveredSession> = current
            .iter()
            .filter(|s| !is_expired(s, now))
            .cloned()
            .collect();
        self.sessions.store(Arc::new(next));
    }

    /// A wait-free snapshot of the current inventory (a cheap atomic-load of the
    /// published `Arc`; the reader never blocks a writer).
    #[must_use]
    pub fn inventory(&self) -> Arc<Vec<DiscoveredSession>> {
        self.sessions.load_full()
    }

    /// The number of sessions currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.load().len()
    }

    /// Whether the table currently tracks no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.load().is_empty()
    }

    /// How many [`purge`](Self::purge) reap scans have run since construction.
    ///
    /// Observability for the reaper cadence: on a steady cadence this advances one
    /// per purge period, so a jump of several periods must advance it by **one**
    /// (a `MissedTickBehavior::Skip` reaper) rather than once per missed tick (a
    /// burst — I1).
    #[must_use]
    pub fn purge_count(&self) -> u64 {
        self.purges.load(Ordering::Relaxed)
    }
}

impl Default for SapSessionTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The index of the oldest (smallest `last_seen`) entry matching `pred`, or
/// `None` if none match. Ties resolve to the earliest such entry, so eviction is
/// deterministic drop-oldest.
fn oldest_position(
    entries: &[DiscoveredSession],
    mut pred: impl FnMut(&DiscoveredSession) -> bool,
) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, s)| pred(s))
        .min_by_key(|(_, s)| s.last_seen)
        .map(|(i, _)| i)
}

/// Whether `session` is stale at `now`: unseen for longer than
/// `max(10 × observed_period, PURGE_FLOOR)`.
fn is_expired(session: &DiscoveredSession, now: Duration) -> bool {
    let threshold = session.observed_period.map_or(PURGE_FLOOR, |p| {
        p.saturating_mul(PURGE_PERIOD_MULTIPLIER).max(PURGE_FLOOR)
    });
    now.checked_sub(session.last_seen)
        .is_some_and(|age| age > threshold)
}
