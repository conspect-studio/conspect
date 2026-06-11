//! SMPTE **ST 2110** uncompressed-over-IP ingest: pure RTP depacketizers plus
//! the (feature-gated) UDP receive transport.
//!
//! ST 2110 sends each essence — video (-20), PCM audio (-30/-31), ancillary
//! data (-40) — as its own RTP stream, timed to a PTP (ST 2059-2) reference. The
//! **depacketizers** in this module are **pure** byte-slice → typed value
//! parsers, exhaustively golden-vector and property tested in the default
//! pure-Rust build:
//!
//! * [`rtp`] — the common RFC 3550 fixed-header parser every essence rides on.
//! * [`v20`] — ST 2110-20 uncompressed video (extended sequence + SRD segments).
//! * [`v30`] — ST 2110-30 / AES67 PCM audio (interleaved L16/L24 sample groups).
//! * [`v40`] — ST 2110-40 ancillary data (RFC 8331 ANC packets; 10-bit symbols).
//!
//! ## Feature gating (no NIC here)
//!
//! The actual UDP/RTP receive sockets and the PTP-disciplined receive timing
//! live behind the off-by-default **`st2110`** feature in the `transport`
//! submodule (compiled only with that feature enabled). This
//! devcontainer has no ST 2110 network, no PTP NIC, and no real router, so that
//! path is **compile-verified only**; the algorithms above are what carry the
//! correctness load and are fully unit/property tested. Keeping the socket layer
//! thin and gated preserves the LGPL-clean, native-dep-free default build.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Depacketized essence feeds the last-good-frame stores like every other
//! ingest path — it is *sampled*, never *pacing*. Nothing in this module blocks
//! or `.await`s the output clock.

pub mod assembler;
pub mod packetize;
pub mod rtp;
pub mod sdp;
pub mod v20;
pub mod v30;
pub mod v40;

#[cfg(feature = "st2110")]
pub mod transport;

pub use packetize::{Aes67Error, Aes67Packetizer};
pub use rtp::{RtpError, RtpHeader, RtpPacket};
pub use sdp::{AudioSdpSession, SdpError, TsRefclk};
pub use v20::{SrdSegment, V20Error, V20Payload};
pub use v30::{Aes3Format, SampleDepth, V30Error, V30Payload};
pub use v40::{AncPacket, V40Error, V40Payload};

/// Unified error type for the ST 2110 depacketizers.
///
/// Each essence parser has its own fine-grained error; this `#[non_exhaustive]`
/// enum aggregates them so an ingest pipeline can return one type, and it
/// converts into [`crate::Error`] at the crate boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum St2110Error {
    /// An RTP fixed-header parse failure.
    #[error("rtp: {0}")]
    Rtp(#[from] RtpError),

    /// An ST 2110-20 video depacketization failure.
    #[error("st2110-20: {0}")]
    V20(#[from] V20Error),

    /// An ST 2110-30 audio depacketization failure.
    #[error("st2110-30: {0}")]
    V30(#[from] V30Error),

    /// An ST 2110-40 ANC depacketization failure.
    #[error("st2110-40: {0}")]
    V40(#[from] V40Error),

    /// An AES67 / ST 2110-30 audio SDP parse failure.
    #[error("sdp: {0}")]
    Sdp(#[from] SdpError),

    /// An AES67 / ST 2110-30 audio packetization (egress) failure.
    #[error("aes67 packetize: {0}")]
    Aes67(#[from] Aes67Error),
}
