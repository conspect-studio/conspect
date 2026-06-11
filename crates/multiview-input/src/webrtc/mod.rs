//! WebRTC ingest: a pure, testable **SDP** (RFC 8866 / RFC 8829 JSEP) offer /
//! answer model, with the ICE/DTLS/SRTP transport behind the off-by-default
//! `webrtc` feature.
//!
//! WebRTC contribution (WHIP-style ingest) negotiates the media session with an
//! **SDP offer/answer** exchange: the offerer lists its media descriptions
//! (`m=` lines) with codecs, payload-type maps, and ICE/DTLS parameters; the
//! answerer selects a compatible subset. The negotiation logic — parsing the
//! SDP, choosing a payload type, and producing an answer — is **pure** and
//! exhaustively testable, and lives here in the default build.
//!
//! ## Feature gating (no socket here)
//!
//! The ICE agent, DTLS handshake, and SRTP depacketization need a real network
//! stack and a crypto stack; they live behind the off-by-default **`webrtc`**
//! feature in the `transport` submodule (compiled only with that feature
//! enabled), keeping the default build pure-Rust, native-dep-free, and
//! LGPL-clean. The model in this module carries the correctness load and is fully
//! tested.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Negotiated media feeds the last-good stores like every other ingest path — it
//! is *sampled*, never *pacing*. Nothing here blocks the output clock.

#[cfg(feature = "webrtc")]
pub mod transport;

mod sdp;

pub use sdp::{
    Codec, MediaDescription, MediaKind, NegotiatedMedia, NegotiatedSession, RtpMap, SdpDirection,
    SessionDescription,
};

/// Errors raised while parsing or negotiating a WebRTC session description.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WebRtcError {
    /// The SDP text was malformed (a line lacked its `type=value` form, or a
    /// required field was missing).
    #[error("malformed sdp: {0}")]
    MalformedSdp(&'static str),

    /// A numeric SDP field (port, payload type, clock rate) failed to parse.
    #[error("invalid sdp field {field}: {value:?}")]
    BadField {
        /// The field name.
        field: &'static str,
        /// The raw value that failed.
        value: String,
    },

    /// Offer/answer negotiation found no codec the answerer supports for a media
    /// section.
    #[error("no compatible codec for the {0} media section")]
    NoCompatibleCodec(&'static str),

    /// The offer carried no media sections to negotiate.
    #[error("sdp offer carries no media sections")]
    NoMedia,
}
