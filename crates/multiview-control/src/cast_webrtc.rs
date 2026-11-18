//! The `WebRTC` rung of a cast stream (ADR-0114 slice 5): the control-plane
//! seam the engine implements.
//!
//! The control plane admits the token (the shared check), builds the
//! attachment with its clearance and hands the receiver's SDP offer to the
//! engine. The engine attaches a viewer session to the ONE registry
//! subscription and the ONE Opus encoder of that cast output (created with
//! the first viewer, closed with the last) and judges every window against
//! the attachment's clearance before any packet of it is written to the
//! session. Every method is synchronous and bounded: it never waits on the
//! data plane (invariant #10).

use std::sync::Arc;

use crate::cast_streams::identity::CastAttachment;

/// The answer to a cast WHEP offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastWhepAnswer {
    /// The minted session id (the `DELETE` path's last segment).
    pub session_id: String,
    /// The answer SDP (UDP 443 host candidates and passive ICE-TCP ones).
    pub sdp: String,
}

/// Why the engine refused a cast WHEP offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastWhepReject {
    /// The offer is malformed or has no compatible codec.
    Offer,
    /// No viewer or registry subscription is free, or no transport runs.
    Unavailable,
}

/// The engine's cast `WebRTC` rung.
pub trait CastWebRtc: Send + Sync {
    /// Whether the rung can serve output `config_id` now (a transport runs,
    /// the output is in the run). The ladder offers `webrtc` only then.
    fn available(&self, config_id: &str) -> bool;

    /// Negotiate one viewer of cast output `config_id` from `offer`, gated by
    /// `attachment` (whose closer the engine sets: it ends the session).
    ///
    /// # Errors
    /// [`CastWhepReject`].
    fn negotiate(
        &self,
        config_id: &str,
        offer: &str,
        attachment: Arc<CastAttachment>,
    ) -> Result<CastWhepAnswer, CastWhepReject>;

    /// End viewer `session_id` of output `config_id` (`false`: unknown).
    fn release(&self, config_id: &str, session_id: &str) -> bool;
}

/// No `WebRTC` transport in this build or run: never available.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCastWebRtc;

impl CastWebRtc for NoCastWebRtc {
    fn available(&self, _: &str) -> bool {
        false
    }
    fn negotiate(
        &self,
        _: &str,
        _: &str,
        _: Arc<CastAttachment>,
    ) -> Result<CastWhepAnswer, CastWhepReject> {
        Err(CastWhepReject::Unavailable)
    }
    fn release(&self, _: &str, _: &str) -> bool {
        false
    }
}

/// A shared [`CastWebRtc`].
pub type SharedCastWebRtc = Arc<dyn CastWebRtc>;
