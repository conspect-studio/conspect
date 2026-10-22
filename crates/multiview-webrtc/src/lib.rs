//! `multiview-webrtc` — the single native WebRTC transport endpoint for Multiview
//! (ICE / DTLS / SRTP), shared by every WebRTC role: WHIP ingest publishers
//! ([ADR-T014]), WHEP preview viewers ([ADR-P006]), WHEP output viewers and the
//! outbound `whip_push` client ([ADR-0049]). It is the [ADR-0048] transport crate
//! — exactly one str0m owner in the workspace, one dual-stack UDP socket, one
//! DTLS certificate, one driver task, one session GC — plus the operator-required
//! **TURN client** (RFC 5766 / RFC 8656) for NAT traversal.
//!
//! ## Feature ladder
//!
//! * `default = []` — the pure shell. **The TURN client, the STUN/TURN message
//!   codec, the session map + GC, the SDP helpers, the candidate ordering, and
//!   the signalling types are all pure** and live in the default build: they
//!   carry no native dependency, so `cargo check --workspace` stays GPU/native-
//!   free and cargo-deny (which scans the default build) sees no str0m. This is
//!   also why the bulk of the crate is unit-tested in ordinary CI.
//! * `native = ["dep:str0m", "tokio/net"]` — the real ICE/DTLS/SRTP stack: the
//!   single dual-stack UDP socket, the str0m-backed session engine, and the
//!   driver task (the `native`-gated `transport` module). str0m only ever rides
//!   behind this gate.
//!
//! ## Isolation (invariant #10)
//!
//! Every WebRTC role is **best-effort and physically incapable of
//! back-pressuring the engine**. Per-session media crosses only bounded
//! drop-oldest rings (the [`multiview_preview`] `SampleFeed` egress, an
//! [`multiview_input`] `RtpFrame` ring ingress); the driver task never `.await`s a
//! peer (UDP send is non-blocking; a full ring drops oldest). A wedged or
//! saturated endpoint loses *preview/ingest media*, never output ticks.
//!
//! [ADR-0048]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0048.md
//! [ADR-0049]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0049.md
//! [ADR-T014]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-T014.md
//! [ADR-P006]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-P006.md
#![warn(missing_docs)]

pub mod config;
pub mod egress;
pub mod error;
pub mod sdp;
pub mod session;
pub mod signalling;
pub mod turn;

#[cfg(feature = "native")]
pub mod transport;

#[cfg(feature = "native")]
pub mod whep_egress;

pub use config::{EndpointConfig, IceServer, IceServerKind, TurnCredentials};
pub use error::{Result, TurnError, WebRtcError};
