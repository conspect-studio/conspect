//! The **untrusted** discovered-peer inventory (ADR-0051 §3, brief §9.1).
//!
//! Discovered peers populate a **bounded, untrusted** table keyed by their
//! salted fingerprint digest. A peer is **never** auto-trusted (the classic mDNS
//! footgun): `relaying_for_us` is set only by an explicit operator action
//! (confirm-adopt — the ADR-0041 doctrine), never by observation. `last_seen`
//! advances on each observation; a peer un-refreshed past [`PEER_STALE_AFTER`]
//! ages out; the table is capped at [`PEER_TABLE_CAP`] (drop-oldest — a flood can
//! never grow memory unbounded, invariant #10).
//!
//! All times are a monotonic [`Duration`] handed in by the caller (the live mDNS
//! task samples a monotonic clock; tests inject fixed instants) — this module
//! reads no system clock itself, so peer aging is deterministic and testable
//! offline.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ClaimState;

/// How long a peer may go un-refreshed before [`PeerTable::age_out`] removes it.
///
/// mDNS announcements repeat on the order of seconds-to-a-minute; a generous
/// multiple of that absorbs a missed announcement or two without dropping a live
/// neighbour, while still pruning a peer that has genuinely left the segment.
pub const PEER_STALE_AFTER: Duration = Duration::from_secs(300);

/// The maximum number of peers held in the untrusted inventory. A hostile or
/// noisy segment that announces many distinct digests can never grow the table
/// past this; the least-recently-seen peer is evicted (drop-oldest, invariant
/// #10). A LAN mesh is small; this ceiling is comfortably above any real fleet.
pub const PEER_TABLE_CAP: usize = 256;

/// The number of bytes in a salted fingerprint digest (a 256-bit hash), matching
/// [`multiview_licence::fingerprint::DIGEST_LEN`].
pub const PEER_DIGEST_LEN: usize = 32;

/// A peer's stable key: its **salted** fingerprint digest (opaque 32 bytes).
///
/// The digest is handed in already salted + hashed (data minimisation, brief §8)
/// — this crate never reverses it to an identifier. Two deployments with
/// different salts cannot correlate a peer across them. The key renders as
/// lowercase hex for the API surface ([`PeerKey::as_hex`]) — still never a raw
/// identifier.
///
/// **Wire shape:** serialises as the lowercase-hex string (64 chars), not a raw
/// byte array — the id every API surface (`GET /api/v1/mesh/peers`, the `via`
/// in `GET /api/v1/mesh/status`) renders and adoption addresses. The `Serialize`/
/// `Deserialize` impls go through [`PeerKey::as_hex`]/[`PeerKey::from_hex`] so the
/// JSON shape is the stable, human-meaningful hex id, never `[171, 171, …]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerKey {
    digest: [u8; PEER_DIGEST_LEN],
}

impl Serialize for PeerKey {
    /// Serialise as the lowercase-hex id (the stable API surface; never a raw
    /// byte array).
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_hex())
    }
}

impl<'de> Deserialize<'de> for PeerKey {
    /// Parse from the 64-char lowercase-hex id, the inverse of the `Serialize`
    /// impl. A malformed id is a typed deserialisation error (never a panic).
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        Self::from_hex(&hex).ok_or_else(|| {
            serde::de::Error::custom("a peer key must be exactly 64 lowercase-hex characters")
        })
    }
}

impl PeerKey {
    /// Wrap a salted digest as a peer key.
    #[must_use]
    pub const fn from_digest(digest: [u8; PEER_DIGEST_LEN]) -> Self {
        Self { digest }
    }

    /// The salted digest bytes (opaque).
    #[must_use]
    pub const fn digest(&self) -> &[u8; PEER_DIGEST_LEN] {
        &self.digest
    }

    /// The lowercase-hex rendering of the digest — the stable id the API surfaces
    /// (`GET /api/v1/mesh/peers`) and adoption addresses. Pure hex, never a raw
    /// identifier.
    #[must_use]
    pub fn as_hex(&self) -> String {
        let mut out = String::with_capacity(PEER_DIGEST_LEN * 2);
        for byte in &self.digest {
            out.push(nibble_hex(byte >> 4));
            out.push(nibble_hex(byte & 0x0f));
        }
        out
    }

    /// Parse a peer key from its 64-char lowercase-hex id, the inverse of
    /// [`PeerKey::as_hex`]. `None` if the string is not exactly 64 hex chars.
    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != PEER_DIGEST_LEN * 2 {
            return None;
        }
        let mut digest = [0_u8; PEER_DIGEST_LEN];
        let bytes = hex.as_bytes();
        for (i, slot) in digest.iter_mut().enumerate() {
            let hi = hex_nibble(*bytes.get(i * 2)?)?;
            let lo = hex_nibble(*bytes.get(i * 2 + 1)?)?;
            *slot = (hi << 4) | lo;
        }
        Some(Self { digest })
    }
}

/// Map a low nibble (`0..=15`) to its lowercase-hex char.
const fn nibble_hex(nibble: u8) -> char {
    match nibble {
        0 => '0',
        1 => '1',
        2 => '2',
        3 => '3',
        4 => '4',
        5 => '5',
        6 => '6',
        7 => '7',
        8 => '8',
        9 => '9',
        10 => 'a',
        11 => 'b',
        12 => 'c',
        13 => 'd',
        14 => 'e',
        // Only the low nibble is ever passed (callers mask with `& 0x0f` / `>> 4`
        // on a u8), so 15 is the only remaining case.
        _ => 'f',
    }
}

/// Map an ASCII hex digit byte to its nibble value, or `None` if not a hex digit.
const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A single observation of a peer from a (verified) mDNS announcement: who it is
/// (salted digest), its advertised claim state, and the monotonic instant it was
/// observed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerObservation {
    /// The observed peer's salted-digest key.
    pub key: PeerKey,
    /// The claim state the peer advertised (coarse, never an identifier).
    pub claim_state: ClaimState,
    /// The monotonic instant the announcement was observed.
    pub observed_at: Duration,
}

/// Serialise/deserialise a [`Duration`] as whole seconds (the API surfaces a
/// `last_seen_secs`-style number; the in-memory type keeps a `Duration`).
mod secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &Duration, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_u64(value.as_secs())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(de)?;
        Ok(Duration::from_secs(secs))
    }
}

/// A discovered peer in the untrusted inventory (brief §9.1).
///
/// Carries the salted digest, an optional name (populated only once the operator
/// has confirm-adopted + named the peer — never from the announce, brief §9.1),
/// the `claimed` flag (observed), `last_seen` (a monotonic [`Duration`],
/// serialised as whole seconds), and `relaying_for_us` (an operator-set adoption
/// flag, never auto). Serialisable for the `GET /api/v1/mesh/peers` surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Peer {
    /// The peer's salted-digest key (its stable id).
    pub key: PeerKey,
    /// An operator-assigned name, present only once the peer is adopted + named.
    /// `None` for a freshly-discovered (untrusted) peer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// Whether the peer advertised itself as claimed (observed from the announce).
    pub claimed: bool,
    /// The monotonic instant the peer was last seen (serialised as whole seconds).
    #[serde(with = "secs")]
    pub last_seen: Duration,
    /// Whether THIS machine relays for the peer — set only by explicit operator
    /// confirm-adopt, never by observation (untrusted inventory).
    pub relaying_for_us: bool,
}

/// The bounded, untrusted discovered-peer inventory.
///
/// Keyed by salted digest. Read-mostly control-plane state; it holds no engine
/// handle and can never back-pressure the engine (invariant #10).
#[derive(Debug, Default)]
pub struct PeerTable {
    peers: HashMap<PeerKey, Peer>,
}

impl PeerTable {
    /// A new, empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// The number of peers currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// The peer with `key`, if present.
    #[must_use]
    pub fn get(&self, key: &PeerKey) -> Option<&Peer> {
        self.peers.get(key)
    }

    /// Record an observation: insert a new peer or advance an existing one's
    /// `last_seen` + claim state (never duplicating). Enforces the cap with
    /// drop-oldest **before** inserting a genuinely new peer.
    ///
    /// An observation never sets `relaying_for_us` or a name — discovery is
    /// untrusted (brief §9.1); those are explicit operator actions.
    pub fn observe(&mut self, obs: PeerObservation) {
        let claimed = obs.claim_state.is_claimed();
        if let Some(existing) = self.peers.get_mut(&obs.key) {
            existing.claimed = claimed;
            existing.last_seen = obs.observed_at;
            return;
        }
        // A genuinely new peer: enforce the cap (drop-oldest) before inserting.
        if self.peers.len() >= PEER_TABLE_CAP {
            self.evict_oldest();
        }
        self.peers.insert(
            obs.key.clone(),
            Peer {
                key: obs.key,
                name: None,
                claimed,
                last_seen: obs.observed_at,
                relaying_for_us: false,
            },
        );
    }

    /// Remove every peer not seen within [`PEER_STALE_AFTER`] of `now`. A peer
    /// exactly at the window is **not** yet stale (strictly-greater ages out).
    /// Returns how many were removed.
    pub fn age_out(&mut self, now: Duration) -> usize {
        let before = self.peers.len();
        self.peers.retain(|_, peer| {
            // Saturating: a `now` before a peer's last_seen (a clock anomaly)
            // keeps the peer rather than dropping it spuriously.
            now.saturating_sub(peer.last_seen) <= PEER_STALE_AFTER
        });
        before - self.peers.len()
    }

    /// Set whether THIS machine relays for the peer with `key` (the operator
    /// confirm-adopt / decline action, brief §9.1 / §9.2). Returns `false` (no
    /// change) when the peer is unknown — discovery never auto-inserts a trusted
    /// relayer.
    pub fn set_relaying_for_us(&mut self, key: &PeerKey, relaying: bool) -> bool {
        match self.peers.get_mut(key) {
            Some(peer) => {
                peer.relaying_for_us = relaying;
                true
            }
            None => false,
        }
    }

    /// Assign (or clear) an operator-chosen name for the peer with `key`. Returns
    /// `false` when the peer is unknown.
    pub fn set_name(&mut self, key: &PeerKey, name: Option<String>) -> bool {
        match self.peers.get_mut(key) {
            Some(peer) => {
                peer.name = name;
                true
            }
            None => false,
        }
    }

    /// A snapshot of every peer, id-sorted by hex for a stable API ordering.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self.peers.values().cloned().collect();
        peers.sort_by_key(|peer| peer.key.as_hex());
        peers
    }

    /// Evict the least-recently-seen peer (drop-oldest). Used only when at the
    /// cap and inserting a new peer.
    fn evict_oldest(&mut self) {
        let oldest = self
            .peers
            .iter()
            .min_by_key(|(_, peer)| peer.last_seen)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.peers.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{PeerKey, PEER_DIGEST_LEN};

    #[test]
    fn hex_round_trips() {
        let key = PeerKey::from_digest([0xDE; PEER_DIGEST_LEN]);
        let hex = key.as_hex();
        assert_eq!(PeerKey::from_hex(&hex).unwrap(), key);
    }

    #[test]
    fn bad_hex_is_none() {
        assert!(PeerKey::from_hex("not-hex").is_none());
        assert!(PeerKey::from_hex("zz").is_none());
    }
}
