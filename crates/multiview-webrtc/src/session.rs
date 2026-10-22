//! The session map: identifiers, roles, the bounded viewer pool, idle GC, and
//! tombstone eviction (ADR-0048 §8).
//!
//! Every WebRTC session — WHIP ingest publisher, WHEP preview viewer, WHEP output
//! viewer, outbound `whip_push` client — is tracked here. The map is bounded:
//! `max_sessions` caps **preview + output-viewer** sessions only; ingest
//! publishers (bounded by the count of configured `webrtc` sources) and the push
//! client (bounded by configured outputs) are admitted **outside** that pool, so a
//! viewer flood can never starve a publisher or the push reconnect. Sessions die
//! on ICE-disconnect or idle timeout and leave a closed tombstone so the WHIP/WHEP
//! `DELETE` stays idempotent; tombstones are evicted after their TTL (default
//! 60 s), replacing the preview scaffold's retain-forever map.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use rand::Rng;

use crate::error::WebRtcError;

/// A >=128-bit random session id (ADR-0048 §8): never sequential, OS-RNG-seeded,
/// base64url-encoded for use in the WHIP/WHEP resource URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Mint a fresh 128-bit random id from the OS RNG, base64url (no padding).
    #[must_use]
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        Self(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Wrap an existing id string (e.g. parsed from a session resource URL).
    ///
    /// Wrapping a session id is infallible (any string is a valid opaque id, the
    /// endpoint either knows it or returns `404`), so this is an inherent
    /// constructor rather than a fallible [`std::str::FromStr`]; the name matches
    /// the call sites that lift an id straight out of the resource path.
    #[must_use]
    #[allow(
        clippy::should_implement_trait,
        reason = "infallible opaque-id wrap; FromStr's Result is the wrong shape"
    )]
    pub fn from_str(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a session is for. The role decides whether it counts against the bounded
/// viewer pool (ADR-0048 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionRole {
    /// A WHIP ingest publisher (admitted outside the viewer pool).
    IngestPublisher,
    /// A WHEP preview viewer (counts against the viewer pool).
    PreviewViewer,
    /// A WHEP output viewer (counts against the viewer pool).
    OutputViewer,
    /// The outbound `whip_push` client (admitted outside the viewer pool).
    PushClient,
}

impl SessionRole {
    /// Whether this role counts against the `max_sessions` viewer pool.
    #[must_use]
    pub const fn counts_against_pool(self) -> bool {
        matches!(self, Self::PreviewViewer | Self::OutputViewer)
    }
}

/// The lifecycle of a tracked session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// Live: ICE/DTLS in progress or media flowing.
    Live { last_activity: Instant },
    /// Closed tombstone: kept until `evict_at` for idempotent `DELETE`.
    Closed { evict_at: Instant },
}

#[derive(Debug, Clone)]
struct Entry {
    role: SessionRole,
    lifecycle: Lifecycle,
}

/// The bounded session table with idle GC and tombstone eviction.
#[derive(Debug)]
pub struct SessionTable {
    entries: HashMap<SessionId, Entry>,
    max_viewer_sessions: u32,
    idle_timeout: Duration,
    tombstone_ttl: Duration,
}

impl SessionTable {
    /// Build a table capping viewer sessions at `max_viewer_sessions`, with the
    /// given idle GC horizon and tombstone TTL.
    #[must_use]
    pub fn new(max_viewer_sessions: u32, idle_timeout: Duration, tombstone_ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            max_viewer_sessions,
            idle_timeout,
            tombstone_ttl,
        }
    }

    /// Count of live viewer-pool sessions.
    #[must_use]
    pub fn live_viewer_count(&self) -> u32 {
        let n = self
            .entries
            .values()
            .filter(|e| {
                e.role.counts_against_pool() && matches!(e.lifecycle, Lifecycle::Live { .. })
            })
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Admit a new session for `role`, returning its minted id.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::AtCapacity`] when `role` counts against the viewer pool and
    /// the pool is full. Ingest/push roles are admitted outside the pool.
    pub fn admit(&mut self, role: SessionRole, now: Instant) -> Result<SessionId, WebRtcError> {
        if role.counts_against_pool() && self.live_viewer_count() >= self.max_viewer_sessions {
            return Err(WebRtcError::AtCapacity);
        }
        let id = SessionId::random();
        self.entries.insert(
            id.clone(),
            Entry {
                role,
                lifecycle: Lifecycle::Live { last_activity: now },
            },
        );
        Ok(id)
    }

    /// Record activity on a session (media/STUN), deferring idle GC.
    pub fn touch(&mut self, id: &SessionId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(id) {
            if let Lifecycle::Live { last_activity } = &mut entry.lifecycle {
                *last_activity = now;
            }
        }
    }

    /// Close a session, leaving a tombstone evicted after the TTL. Idempotent.
    pub fn close(&mut self, id: &SessionId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(id) {
            if matches!(entry.lifecycle, Lifecycle::Live { .. }) {
                entry.lifecycle = Lifecycle::Closed {
                    evict_at: now + self.tombstone_ttl,
                };
            }
        }
    }

    /// Whether the id is tracked (live or a still-live tombstone) — drives the
    /// idempotent `DELETE`/status path.
    #[must_use]
    pub fn is_known(&self, id: &SessionId) -> bool {
        self.entries.contains_key(id)
    }

    /// Whether the id names a closed tombstone.
    #[must_use]
    pub fn is_closed(&self, id: &SessionId) -> bool {
        matches!(
            self.entries.get(id).map(|e| e.lifecycle),
            Some(Lifecycle::Closed { .. })
        )
    }

    /// Whether the id names a live session.
    #[must_use]
    pub fn is_live(&self, id: &SessionId) -> bool {
        matches!(
            self.entries.get(id).map(|e| e.lifecycle),
            Some(Lifecycle::Live { .. })
        )
    }

    /// The role of a tracked session, if known.
    #[must_use]
    pub fn role(&self, id: &SessionId) -> Option<SessionRole> {
        self.entries.get(id).map(|e| e.role)
    }

    /// Whether a live publisher already exists for the ingest pool — a WHIP source
    /// allows only one (the `409` rule). The caller scopes "for this source" by
    /// keying its own per-source table; here it is a simple any-live-publisher
    /// query for callers that keep a per-source table.
    #[must_use]
    pub fn has_live_publisher(&self) -> bool {
        self.entries.values().any(|e| {
            matches!(e.role, SessionRole::IngestPublisher)
                && matches!(e.lifecycle, Lifecycle::Live { .. })
        })
    }

    /// Run garbage collection at `now`: close idle live sessions, evict expired
    /// tombstones. Bounded memory by construction.
    pub fn gc(&mut self, now: Instant) {
        let idle_timeout = self.idle_timeout;
        let tombstone_ttl = self.tombstone_ttl;
        // First, close idle live sessions into tombstones.
        for entry in self.entries.values_mut() {
            if let Lifecycle::Live { last_activity } = entry.lifecycle {
                if now.saturating_duration_since(last_activity) >= idle_timeout {
                    entry.lifecycle = Lifecycle::Closed {
                        evict_at: now + tombstone_ttl,
                    };
                }
            }
        }
        // Then evict expired tombstones.
        self.entries.retain(|_id, entry| match entry.lifecycle {
            Lifecycle::Closed { evict_at } => now < evict_at,
            Lifecycle::Live { .. } => true,
        });
    }
}
