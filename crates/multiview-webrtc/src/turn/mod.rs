//! The in-crate TURN client (RFC 5766 / RFC 8656), built on a pure STUN/TURN
//! message codec.
//!
//! ## Why an in-crate TURN client
//!
//! The operator requires TURN relay support for NAT traversal end-to-end. str0m
//! 0.16.2 is a **full-ICE** agent but ships **no TURN client** — it can *consume*
//! a relay candidate ([`str0m::Candidate::relayed`]) but does not allocate one.
//! Because the endpoint owns the UDP socket beneath str0m's sans-IO core
//! (the `native`-gated `transport` module), the TURN client lives in that
//! socket-driving layer: it performs the Allocate / Refresh / `CreatePermission`
//! / `ChannelBind` exchange against the configured TURN servers, learns the
//! relayed transport address, and presents it to str0m as a relay candidate.
//! Relayed media is wrapped/unwrapped transparently beneath str0m (Send/Data
//! indications or `ChannelData`).
//!
//! str0m publicly exposes a STUN codec, but it lacks a REQUESTED-TRANSPORT setter
//! (mandatory for Allocate) and keeps its attribute model private, so this codec
//! is owned in-crate — and the client is therefore **pure and offline-testable**
//! against an in-process fake TURN server, no socket and no str0m required.

pub mod auth;
pub mod client;
pub mod driver;
pub mod message;

pub use auth::TurnCredential;
pub use client::{TurnClient, TurnEvent, TurnOutput, TurnState};
pub use driver::{RelayedDatagram, TurnRelayDriver};
