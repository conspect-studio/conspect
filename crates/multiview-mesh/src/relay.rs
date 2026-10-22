//! The relay **carrier** model (ADR-0051 §4, brief §9.2).
//!
//! An online machine relays an **adopted** offline neighbour's heartbeat to the
//! licence server and carries the server-signed response back. The relayer is a
//! **dumb carrier**: the payload is end-to-end signed by the **originating
//! machine** (request) and the **licence server** (response —
//! [`multiview_licence::store::LeaseBinding`]). The relayer lacks both keys, so it
//! can neither read past the signed envelope nor forge/alter the assertion. Relay
//! integrity therefore does **not** depend on trusting the relayer: a tampered or
//! spoofed payload fails the server signature check **at the destination** (the
//! licence store's `install_binding`, which verifies against the **pinned server
//! key** — never the relayer's) and is rejected.
//!
//! This crate models the **carrier** — the queue, the opt-in, and the
//! origin-tagged binding. The actual licence-server forwarding (the wire protocol,
//! O1) is gated on operator-confirm like everything server-side; the carrier here
//! transports the signed **file-exchange artefacts** ([`LeaseBinding`]) between
//! machines locally. The relay queue is **bounded drop-oldest** so a flood of
//! neighbour requests can never grow memory or stall anything (invariant #10).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use multiview_licence::store::LeaseBinding;

use crate::peer::PeerKey;

/// The maximum number of in-flight neighbour relay requests a relayer holds. A
/// flood of neighbour requests beyond this drops the **oldest** (drop-oldest,
/// invariant #10) — mesh relay is best-effort and can lose attempts under
/// pressure, but it can never grow memory or stall the engine/heartbeat.
pub const RELAY_QUEUE_CAP: usize = 64;

/// A machine's relay opt-in configuration (brief §9.2). A machine is a willing
/// relayer or declines; the default is **decline** (opt-out) — no neighbour
/// traffic is carried unless explicitly enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RelayConfig {
    /// Whether this machine relays for neighbours. Defaults to `false` (decline).
    pub enabled: bool,
}

impl RelayConfig {
    /// A relay config with the given opt-in state.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

/// An end-to-end-signed lease binding the carrier transports, tagged with the
/// **origin** peer it is being relayed for (so the operator's Mesh screen can
/// show *who* a relayer is carrying for — for audit, brief §9.2). The carrier
/// never interprets the binding; it is opaque, server-signed bytes it forwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RelayedBinding {
    /// The salted-digest key of the originating (offline) machine this binding is
    /// being relayed for. Audit only — never a raw identifier (brief §8).
    origin: PeerKey,
    /// The end-to-end-signed lease binding (server-signed; verified at the
    /// destination against the **pinned server key**, not the relayer's).
    binding: LeaseBinding,
}

impl RelayedBinding {
    /// Bundle a server-signed binding with the origin peer it is relayed for.
    #[must_use]
    pub fn new(origin: PeerKey, binding: LeaseBinding) -> Self {
        Self { origin, binding }
    }

    /// The originating (offline) machine's salted-digest key (audit surface).
    #[must_use]
    pub const fn origin(&self) -> &PeerKey {
        &self.origin
    }

    /// The end-to-end-signed binding the carrier forwards. The destination
    /// verifies this against its **pinned server key** —
    /// [`multiview_licence::store::LeaseStore::install_binding`] — so a tampered
    /// or relayer-forged binding is rejected; the carrier has no authority.
    #[must_use]
    pub const fn binding(&self) -> &LeaseBinding {
        &self.binding
    }
}

/// A **bounded drop-oldest** queue of neighbour relay requests (invariant #10).
///
/// A relayer enqueues at most [`RELAY_QUEUE_CAP`] in-flight requests; pushing
/// beyond the cap drops the **oldest** so the queue never grows. Best-effort: a
/// dropped attempt simply means that neighbour retries on its next announcement;
/// the relayer never blocks on a neighbour and never holds a lock the engine
/// holds.
#[derive(Debug, Default)]
pub struct RelayQueue {
    queue: VecDeque<RelayedBinding>,
}

impl RelayQueue {
    /// A new, empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// The number of in-flight relay requests currently queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Enqueue a relay request, dropping the **oldest** if at the cap
    /// (drop-oldest, invariant #10). Returns `true` when a drop occurred (the
    /// caller may WARN), `false` otherwise.
    pub fn push(&mut self, carried: RelayedBinding) -> bool {
        let dropped = if self.queue.len() >= RELAY_QUEUE_CAP {
            let evicted = self.queue.pop_front().is_some();
            if evicted {
                tracing::warn!(
                    cap = RELAY_QUEUE_CAP,
                    "relay queue full — dropping the oldest neighbour request (best-effort, never off air)"
                );
            }
            evicted
        } else {
            false
        };
        self.queue.push_back(carried);
        dropped
    }

    /// Pop the oldest queued relay request (FIFO), if any. The carrier services
    /// requests oldest-first.
    pub fn pop(&mut self) -> Option<RelayedBinding> {
        self.queue.pop_front()
    }

    /// Drain every queued request, oldest first.
    pub fn drain(&mut self) -> impl Iterator<Item = RelayedBinding> + '_ {
        self.queue.drain(..)
    }
}
