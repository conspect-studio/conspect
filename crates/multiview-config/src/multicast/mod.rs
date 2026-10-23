//! Typed IP-multicast transport configuration (ADR-0040; backlog
//! `docs/development/multicast-transport-backlog.md`).
//!
//! Multicast was previously reachable from config only as an **untyped** URL
//! (`SourceKind::Ts { url: "udp://@group:port" }`) with no address validation,
//! no interface/buffer/SSM control, and no IPv6 type guarantee. This module is
//! where the typed half lands: structured, validated multicast values that a
//! document can express and the engine can trust.
//!
//! [`address`] is the MC-0 foundation: the pure newtypes ([`MulticastGroup`],
//! [`MulticastIface`], [`PacketCount`]) and the **total** RFC-5771 address
//! classifier every later slice (MC-1 schema, MC-3 ingest, MC-4 egress, MC-5/6/7
//! warnings) validates against at config load. IPv6 is the primary path
//! (ADR-0042); the IPv4 form is the legacy peer.

pub mod address;

pub use address::{
    classify, MulticastClass, MulticastGroup, MulticastIface, PacketCount, RejectReason,
    RejectedGroup, V4Block, V4_BLOCKS,
};
