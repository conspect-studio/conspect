//! Mesh **role** determination (ADR-0051 §4, brief §9.2).
//!
//! A machine's role is a **pure function** of its own connectivity, its relay
//! opt-in, and whether it has an adopted relaying neighbour:
//!
//! * [`MeshRole::Direct`] — the machine has its own internet path to the licence
//!   server and is not relaying for neighbours.
//! * [`MeshRole::Relay`] — the machine is online **and** opted-in to relay an
//!   offline neighbour's heartbeat (brief §9.2). It earns no entitlement from
//!   carrying traffic.
//! * [`MeshRole::Leaf`] — the machine has **no** internet path and depends on an
//!   adopted relaying neighbour, carrying the `via` peer it leafs through.
//!
//! There are no sockets here — the role is computed from sampled inputs, so it is
//! deterministic and testable offline.

use serde::{Deserialize, Serialize};

use crate::peer::PeerKey;

/// The machine's own connectivity to the licence server (sampled by the caller —
/// the cli's heartbeat client reports whether the last contact succeeded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Connectivity {
    /// The machine can reach the licence server directly.
    Online,
    /// The machine has no internet path to the licence server.
    Offline,
}

/// The inputs to [`determine_role`]: own connectivity, relay opt-in, and the
/// adopted relaying neighbour (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInputs {
    /// Whether the machine can reach the licence server itself.
    pub connectivity: Connectivity,
    /// Whether the machine has opted in to relay for neighbours (brief §9.2).
    pub relay_enabled: bool,
    /// The adopted relaying neighbour this machine leafs through when offline.
    pub via: Option<PeerKey>,
}

/// The machine's role in the mesh. Internally tagged on `kind` (conventions §5 —
/// **never** untagged) so `direct`/`relay`/`leaf` parse unambiguously, with the
/// `leaf` variant carrying its `via` peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MeshRole {
    /// Own internet path; not relaying for neighbours.
    Direct,
    /// Online and opted-in to relay an offline neighbour's heartbeat.
    Relay,
    /// Offline; leafing through an adopted relaying neighbour.
    Leaf {
        /// The peer this machine relays its heartbeat through.
        via: PeerKey,
    },
}

impl MeshRole {
    /// The stable kebab tag for the role (`direct`/`relay`/`leaf`).
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        match self {
            MeshRole::Direct => "direct",
            MeshRole::Relay => "relay",
            MeshRole::Leaf { .. } => "leaf",
        }
    }

    /// The peer this machine leafs through, if it is a [`MeshRole::Leaf`].
    #[must_use]
    pub const fn via(&self) -> Option<&PeerKey> {
        match self {
            MeshRole::Leaf { via } => Some(via),
            MeshRole::Direct | MeshRole::Relay => None,
        }
    }
}

/// Determine the machine's mesh role from sampled inputs (brief §9.2).
///
/// * **Offline + an adopted relay** → [`MeshRole::Leaf`] via that peer (relay
///   opt-in is moot while offline — a machine with no path cannot relay for
///   others). This is checked first so an offline machine always leafs when it
///   can.
/// * **Online + relay opt-in** → [`MeshRole::Relay`].
/// * otherwise → [`MeshRole::Direct`] (an online machine with no opt-in, or an
///   offline machine with no adopted relay — the latter simply cannot reach the
///   server, which is the lease/ladder's concern, not the role's).
#[must_use]
pub fn determine_role(inputs: &RoleInputs) -> MeshRole {
    match (inputs.connectivity, &inputs.via) {
        // Offline with an adopted relay: leaf through it (opt-in is irrelevant
        // — an offline machine cannot itself relay for others).
        (Connectivity::Offline, Some(via)) => MeshRole::Leaf { via: via.clone() },
        // Online and willing to relay neighbours.
        (Connectivity::Online, _) if inputs.relay_enabled => MeshRole::Relay,
        // Online without opt-in, or offline with no adopted relay: direct.
        (Connectivity::Online | Connectivity::Offline, _) => MeshRole::Direct,
    }
}
