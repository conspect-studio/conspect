//! The shared **mesh state** the control endpoints render and the announce/browse
//! loop maintains (ADR-0051 §3/§4/§5, brief §9).
//!
//! [`MeshState`] bundles the always-on discovery facts: the untrusted
//! [`PeerTable`], the [`RelayConfig`] opt-in, this machine's sampled
//! [`Connectivity`], and the adopted relaying neighbour (`via`) it leafs through
//! when offline. From these it computes the machine's [`MeshRole`] and the
//! `GET /api/v1/mesh/status` view. It is **control-plane-only** state behind an
//! `RwLock`: it holds **no** engine handle and can never back-pressure the engine
//! (invariant #10). Discovery is **always-on** — there is no field, method, or
//! endpoint that disables it; only relay opt-in toggles.

use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::peer::{Peer, PeerKey, PeerObservation, PeerTable};
use crate::relay::RelayConfig;
use crate::role::{determine_role, Connectivity, MeshRole, RoleInputs};

/// The discovery mode the status reports. There is exactly one value —
/// **always-on** — and no way to set it to anything else (ADR-0051 §2: discovery
/// runs whenever the account plane runs; it is the spec's *locked* row). Modelled
/// as a unit-like enum so the wire tag is stable and the type can never carry an
/// "off" state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiscoveryMode {
    /// Discovery is always on (the only value).
    AlwaysOn,
}

/// The `GET /api/v1/mesh/status` view (brief §11 endpoint 24-adjacent): the
/// always-on discovery flag, the relay opt-in, the computed role, the optional
/// `via` peer (present only when leafed), and the peer count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MeshStatus {
    /// Discovery is always-on (no off switch exists).
    pub discovery: DiscoveryMode,
    /// Whether this machine relays for neighbours (the toggle).
    pub relay_enabled: bool,
    /// The computed mesh role (`direct`/`relay`/`leaf`).
    pub role: MeshRole,
    /// The peer this machine leafs through, present only when the role is `leaf`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<PeerKey>,
    /// How many peers are in the untrusted inventory.
    pub peers_count: usize,
}

/// The shared, control-plane-only mesh state.
///
/// Behind a single `RwLock`. The announce/browse loop takes the write lock briefly
/// to fold observations + age peers; the control endpoints take the read lock to
/// render. Neither holds an engine handle (invariant #10).
#[derive(Debug)]
pub struct MeshState {
    inner: RwLock<Inner>,
}

/// The guarded interior of [`MeshState`].
#[derive(Debug)]
struct Inner {
    peers: PeerTable,
    relay: RelayConfig,
    connectivity: Connectivity,
    /// The operator-adopted relaying neighbour this machine leafs through when
    /// offline (set by confirm-adopt; `None` until adopted).
    via: Option<PeerKey>,
}

impl Default for MeshState {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshState {
    /// A fresh mesh state: empty inventory, relay declined (opt-out default),
    /// assumed online until the heartbeat client reports otherwise, no adopted
    /// relay.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                peers: PeerTable::new(),
                relay: RelayConfig::default(),
                connectivity: Connectivity::Online,
                via: None,
            }),
        }
    }

    /// Record an observed (already-verified) peer announcement. Best-effort: a
    /// poisoned lock is recovered (the inventory is a cache; there is no invariant
    /// to lose).
    pub fn observe(&self, obs: PeerObservation) {
        self.with_write(|inner| inner.peers.observe(obs));
    }

    /// Age out peers not seen within the staleness window at `now`. Returns how
    /// many were removed.
    pub fn age_out(&self, now: Duration) -> usize {
        self.with_write(|inner| inner.peers.age_out(now))
    }

    /// Toggle whether this machine relays for neighbours (the `PUT
    /// /api/v1/mesh/relay` action). Returns the new effective relay-enabled state.
    pub fn set_relay_enabled(&self, enabled: bool) -> bool {
        self.with_write(|inner| {
            inner.relay = RelayConfig::new(enabled);
            inner.relay.enabled
        })
    }

    /// Whether relay is currently enabled.
    #[must_use]
    pub fn relay_enabled(&self) -> bool {
        self.with_read(|inner| inner.relay.enabled)
    }

    /// Update this machine's sampled connectivity (the cli's heartbeat client
    /// reports online/offline). Drives the computed role.
    pub fn set_connectivity(&self, connectivity: Connectivity) {
        self.with_write(|inner| inner.connectivity = connectivity);
    }

    /// Adopt (or clear) the relaying neighbour this machine leafs through when
    /// offline (operator confirm-adopt). Also marks the peer `relaying_for_us` in
    /// the inventory when known. Returns `true` when the peer is known (adoption
    /// requires a discovered peer — untrusted inventory + confirm-adopt).
    pub fn adopt_relay(&self, via: Option<PeerKey>) -> bool {
        self.with_write(|inner| {
            if let Some(key) = &via {
                // Adoption requires the peer to be in the untrusted inventory.
                if !inner.peers.set_relaying_for_us(key, true) {
                    return false;
                }
            }
            inner.via = via;
            true
        })
    }

    /// The computed mesh role from the current sampled inputs.
    #[must_use]
    pub fn role(&self) -> MeshRole {
        self.with_read(|inner| {
            determine_role(&RoleInputs {
                connectivity: inner.connectivity,
                relay_enabled: inner.relay.enabled,
                via: inner.via.clone(),
            })
        })
    }

    /// The current `GET /api/v1/mesh/status` view.
    #[must_use]
    pub fn status(&self) -> MeshStatus {
        self.with_read(|inner| {
            let role = determine_role(&RoleInputs {
                connectivity: inner.connectivity,
                relay_enabled: inner.relay.enabled,
                via: inner.via.clone(),
            });
            let via = role.via().cloned();
            MeshStatus {
                discovery: DiscoveryMode::AlwaysOn,
                relay_enabled: inner.relay.enabled,
                role,
                via,
                peers_count: inner.peers.len(),
            }
        })
    }

    /// A stable, id-sorted snapshot of the untrusted peer inventory for
    /// `GET /api/v1/mesh/peers`.
    #[must_use]
    pub fn peers(&self) -> Vec<Peer> {
        self.with_read(|inner| inner.peers.snapshot())
    }

    /// Run `f` under the write lock, recovering a poisoned lock (best-effort:
    /// the mesh inventory is a cache, no invariant is lost on poison).
    fn with_write<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        match self.inner.write() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    /// Run `f` under the read lock, recovering a poisoned lock.
    fn with_read<T>(&self, f: impl FnOnce(&Inner) -> T) -> T {
        match self.inner.read() {
            Ok(guard) => f(&guard),
            Err(poisoned) => f(&poisoned.into_inner()),
        }
    }
}
