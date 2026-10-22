//! # multiview-mesh — the Conspect **local-mesh discovery + relay** plane
//! (ADR-0051, brief §9).
//!
//! Machines on a LAN form a local mesh so an **offline** machine (no internet
//! path to the licence server) can still keep its entitlement lease live: an
//! **online** neighbour **relays** its heartbeat to the server and carries the
//! signed response back. This crate models that plane:
//!
//! * [`announce`] — the always-on mDNS announce **payload**: a salted fingerprint
//!   digest set + a signed entitlement summary (level + lease bounds) + claim
//!   state + the protocol version. **Never** a raw identifier (serial/MAC/URL/
//!   hostname/media/config) — data-minimisation is enforced *structurally* (the
//!   types have no field that could hold one) and *by test* (the announce-payload
//!   key set is pinned exhaustively).
//! * [`peer`] — the **untrusted** discovered-peer inventory: each peer is a salted
//!   digest, an optional name (only once claimed), a `claimed` flag, `last_seen`,
//!   and `relaying_for_us`. Bounded (drop-oldest), aged-out by a staleness window,
//!   never auto-trusted (ADR-0041 doctrine — confirm-adopt is an explicit action).
//! * [`role`] — the machine's mesh **role**: `direct` (own internet path),
//!   `relay` (online + opted-in to relay), or `leaf` (offline, via an adopted
//!   relaying neighbour). A pure function of connectivity + opt-in + adopted relay.
//! * [`relay`] — the relay **carrier**: a bounded drop-oldest queue of
//!   end-to-end-signed [`LeaseBinding`](multiview_licence::store::LeaseBinding)s.
//!   The relayer is a **dumb carrier** — it lacks the originator/server keys, so
//!   it can neither read past the signed envelope nor forge/alter the assertion;
//!   a tampered relayed payload fails the server signature check at the
//!   destination and is rejected.
//!
//! ## Isolation — physically incapable of touching the engine (invariant #10)
//!
//! The mesh is a **control-plane actor over channels**, the proven managed-devices
//! isolation shape (ADR-RT004/ADR-P001). It holds **no** `multiview-engine`
//! dependency, no engine handle, and never sends on a path the engine awaits.
//! Mesh traffic can lose *neighbour relay attempts* under pressure (the relay
//! queue is bounded drop-oldest), but it can never stall the engine — or even the
//! local heartbeat. The pure logic (digest payload, peer-table aging, role
//! determination, the relay carrier) is unit-tested **without sockets**; the live
//! mDNS announce/browse lives behind the off-by-default [`mdns`](crate#features)
//! feature with the socket isolated behind the [`transport`] trait so the logic is
//! testable offline.
//!
//! ## Data minimisation (brief §8)
//!
//! Salted digests are **handed in** already salted + hashed (this crate never
//! gathers raw serials/MACs — same contract as
//! [`multiview_licence::fingerprint`]). Ed25519 here is **verification-only** in
//! non-test code: the announce summary is signed by the originating machine's own
//! key so a peer can detect a spoof; the crate verifies signatures handed to it
//! and never mints a server key.
//!
//! ## Features
//!
//! * `mdns` (off by default) — the live mDNS socket announce/browse task over
//!   IPv6-first link-local multicast (`ff02::fb`; IPv4 `224.0.0.251` legacy
//!   interop, ADR-0042). The default build is a pure shell so `cargo check
//!   --workspace` stays native-dep-free and deny-clean.
//!
//! The library target is `multiview_mesh`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod announce;
pub mod driver;
pub mod error;
pub mod peer;
pub mod relay;
pub mod role;
pub mod state;
pub mod transport;

#[cfg(feature = "mdns")]
pub mod service;

use serde::{Deserialize, Serialize};

#[doc(inline)]
pub use announce::{AnnouncePayload, EntitlementSummary, SaltedDigest, ANNOUNCE_PROTOCOL_VERSION};
#[doc(inline)]
pub use error::MeshError;
#[doc(inline)]
pub use peer::{Peer, PeerKey, PeerObservation, PeerTable};
#[doc(inline)]
pub use relay::{RelayConfig, RelayQueue, RelayedBinding};
#[doc(inline)]
pub use role::{determine_role, Connectivity, MeshRole, RoleInputs};
#[doc(inline)]
pub use state::{DiscoveryMode, MeshState, MeshStatus};
#[doc(inline)]
pub use transport::MeshTransport;

/// The claim state a machine advertises in its mesh announcement (brief §9.1, the
/// claim state machine §12). Serialised `kebab-case` so the wire tag is stable.
///
/// This is the **only** identity-adjacent fact in the announce — and it is a
/// coarse state, never an identifier: a peer learns *that* a neighbour is claimed,
/// never *by whom* (data minimisation, brief §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ClaimState {
    /// The machine has not been claimed by any account.
    Unclaimed,
    /// A claim is in progress (a code has been issued, brief §12).
    Claiming,
    /// The machine is claimed (carries an owner + an entitlement lease).
    Claimed,
}

impl ClaimState {
    /// Whether the machine is fully claimed.
    #[must_use]
    pub const fn is_claimed(self) -> bool {
        matches!(self, ClaimState::Claimed)
    }
}
