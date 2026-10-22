//! WebRTC ingest: a pure, testable model of a **negotiated** media session plus a
//! gated pure depacketizer/router seam. No ICE/DTLS/SRTP transport lives in this
//! crate — that is str0m's, in `multiview-webrtc` (ADR-0048).
//!
//! WHIP-style WebRTC contribution negotiates its media session with an SDP
//! offer/answer exchange. Production negotiation is performed by **str0m** in the
//! `multiview-webrtc` crate (ADR-0048); this crate never parses SDP. What lives
//! here is the pure, typed **result** of that negotiation ([`NegotiatedSession`]
//! and friends) plus the gated pure depacketizer/router seam that turns
//! decrypted RTP into typed compressed media events.
//!
//! ## Feature gating (no socket here)
//!
//! No ICE agent, DTLS handshake, SRTP, or socket lives in this crate: the real
//! transport is str0m in `multiview-webrtc`, which decrypts SRTP and feeds this
//! crate plain RTP. The off-by-default **`webrtc`** feature gates only the pure
//! depacketizer/router seam and the `MediaEngine` application-seam trait the
//! transport plugs into — pure-Rust, native-dep-free, and LGPL-clean either way.
//! The gated half is still
//! itself pure and exhaustively tested over *injected* packets: the
//! [`transport`] module owns the application-layer `MediaEngine` seam and the
//! keyframe-gated H.264 depacketizer (RFC 6184); the [`opus`] module owns the
//! Opus depacketizer (RFC 7587); and the [`route`] module dispatches decrypted
//! RTP by negotiated payload type into typed compressed media events (ADR-T014).
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Negotiated media feeds the last-good stores like every other ingest path — it
//! is *sampled*, never *pacing*. Nothing here blocks the output clock.

#[cfg(feature = "webrtc")]
pub mod opus;
#[cfg(feature = "webrtc")]
pub mod route;
#[cfg(feature = "webrtc")]
pub mod transport;

mod sdp;

pub use sdp::{Codec, MediaKind, NegotiatedMedia, NegotiatedSession, SdpDirection};
