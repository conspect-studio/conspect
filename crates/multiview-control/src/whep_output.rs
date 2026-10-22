//! The WHEP **output-viewer** provider seam (ADR-0049 §5.1) — the codec-free
//! boundary the control plane delegates WHEP-serve negotiation/teardown to.
//!
//! This is the viewing twin of the WHIP **ingest** [`WhipProvider`](crate::whip::WhipProvider):
//! a browser WHEP player `POST`s an SDP offer to `/api/v1/whep/{output_id}` and
//! the control plane hands the offer + output id + presented credentials to this
//! seam, getting a [`WhepOutputAnswer`] or a [`WhepOutputReject`] back. `multiview-control`
//! never links str0m — the binary (`multiview-cli`) implements this over the
//! `multiview-webrtc` WHEP-serve endpoint; the default [`NoWhepOutput`] answers
//! every offer `503`, so the routes stay present and authz-enforced even without
//! a native transport (never a `404`/panic, never a fake success).
//!
//! ## Auth model (ADR-0049 §5.1)
//!
//! Output viewing is **never anonymous**. With a per-output `token`, the bearer
//! alone suffices ([RFC 6750]); with `token = None`, viewing requires a
//! control-plane API key with **View** scope (read-shaped — View suffices, not
//! Write, because viewing the program is read-shaped). The route extracts the
//! presented bearer + whether it verified as a View-scope key, and hands both to
//! the provider as [`WhepOutputAuth`]; the provider authorizes on the per-output
//! token match **or** `view_key`.
//!
//! ## Isolation (invariant #10)
//!
//! A WHEP viewer is a **real output** consumer fed the encode-once program AUs
//! over a bounded drop-oldest ring. Both methods are synchronous and must never
//! block on the data plane: control never `.await`s a WHEP negotiation against
//! the engine; a slow viewer loses only its own media.
//!
//! [RFC 6750]: https://www.rfc-editor.org/rfc/rfc6750

use std::sync::Arc;

/// The credentials presented on a WHEP output request, resolved by the route.
///
/// The route reads the raw `Authorization: Bearer <token>` (the per-output token
/// form), and when that token also verifies as a **View-scope** control-plane API
/// key sets [`WhepOutputAuth::view_key`]. The provider authorizes on either the
/// per-output token match (against `bearer`) or `view_key`.
#[derive(Debug, Clone, Default)]
pub struct WhepOutputAuth {
    /// The raw bearer token presented (the value after `Bearer `), or `None`.
    pub bearer: Option<String>,
    /// Whether the presented bearer verified as a control-plane API key with at
    /// least **View** scope (read-shaped). A token-less output accepts only this.
    pub view_key: bool,
}

/// A negotiated WHEP answer: the endpoint-minted session id + the SDP answer body.
///
/// Distinct from the preview-focus [`WhepOutputAnswer`](crate::preview::WhepAnswer) (a
/// canvas-approximate focus session); this carries the **real** encoded program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhepOutputAnswer {
    /// The session id; the trailing segment of the WHEP resource URL the viewer
    /// `DELETE`s to tear the session down.
    pub session_id: String,
    /// The SDP **answer** body (`application/sdp`).
    pub sdp: String,
}

/// Why a WHEP output negotiation/teardown was refused (ADR-0049 §5.1 mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhepOutputReject {
    /// The offer was not well-formed enough to negotiate (`400`).
    Malformed(String),
    /// No credential was presented (`401` + `WWW-Authenticate: Bearer`).
    Unauthorized,
    /// A valid credential lacking view rights (`403`).
    Forbidden,
    /// No configured `webrtc` output by that id (`404`).
    NotFound,
    /// The offer shared no codec the endpoint serves (H.264 + Opus) (`406`).
    NoCompatibleCodec,
    /// Over `max_viewers` or the endpoint-global viewer pool (`503` +
    /// `Retry-After`).
    Unavailable,
}

/// The WHEP output-viewer transport seam.
///
/// Both methods are synchronous and must never block on the engine (invariant
/// #10). See the module docs for the auth model.
pub trait WhepOutputProvider: Send + Sync {
    /// Negotiate a WHEP viewer session for `output_id` from the player's SDP
    /// `offer`, authorized by `auth`.
    ///
    /// # Errors
    ///
    /// Returns a [`WhepOutputReject`] when the credential is missing/insufficient, the
    /// offer is malformed or codec-incompatible, the output is unknown, or the
    /// endpoint cannot admit the viewer. Every refusal is output-viewer-only; it
    /// never affects the engine.
    fn negotiate(
        &self,
        output_id: &str,
        offer: &str,
        auth: &WhepOutputAuth,
    ) -> Result<WhepOutputAnswer, WhepOutputReject>;

    /// Release the viewer session `session_id` for `output_id`, authorized by
    /// `auth` (the same credential class as the creating `POST`).
    ///
    /// Returns `true` if a matching live session was found and torn down, `false`
    /// if it was unknown or already released (so the route answers `200` within
    /// the tombstone window and `404` for a never-known session).
    fn release(&self, output_id: &str, session_id: &str, auth: &WhepOutputAuth) -> bool;

    /// The number of live WHEP output-viewer sessions (descriptor/isolation tests).
    fn active_sessions(&self) -> usize;
}

/// The default WHEP output provider used when the binary wires no serve transport
/// (the pure / negotiation-only build): every offer is refused `503` and there
/// are never any live sessions — the routes stay present and authz-enforced.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWhepOutput;

impl WhepOutputProvider for NoWhepOutput {
    fn negotiate(
        &self,
        _output_id: &str,
        _offer: &str,
        _auth: &WhepOutputAuth,
    ) -> Result<WhepOutputAnswer, WhepOutputReject> {
        Err(WhepOutputReject::Unavailable)
    }

    fn release(&self, _output_id: &str, _session_id: &str, _auth: &WhepOutputAuth) -> bool {
        false
    }

    fn active_sessions(&self) -> usize {
        0
    }
}

/// A shared [`WhepOutputProvider`] handle, as stored in [`crate::AppState`].
pub type SharedWhepOutput = Arc<dyn WhepOutputProvider>;

/// The default shared WHEP output provider ([`NoWhepOutput`]).
#[must_use]
pub fn no_whep_output() -> SharedWhepOutput {
    Arc::new(NoWhepOutput)
}
