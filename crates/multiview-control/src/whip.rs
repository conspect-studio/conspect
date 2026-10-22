//! The WHIP **ingest** provider seam (ADR-T014 §3) — the codec-free boundary the
//! control plane delegates WHIP negotiation/teardown to.
//!
//! The mirror of [`WhepProvider`](crate::preview::WhepProvider): the control
//! plane carries the SDP offer + the source id + the presented credentials in,
//! and gets a [`WhipAnswer`] or a [`WhipReject`] back. `multiview-control` never
//! links str0m — the binary (`multiview-cli`) implements this over the
//! `multiview-webrtc` endpoint; the in-memory fake used in tests proves the
//! route wiring. The default [`NoWhip`] (the pure / negotiation-only build)
//! answers every offer `503`, so the routes stay present and authz-enforced
//! even without a native transport — never a `404`/panic, never a fake success.
//!
//! ## Auth model (ADR-T014 §2)
//!
//! A WHIP publish is **never anonymous**. The route extracts the presented
//! `Bearer` and whether it verified as a **Write-scope** control-plane API key,
//! and hands both to the provider as [`WhipAuth`]. The provider authorizes when
//! the bearer matches the per-source `token` **or** `write_key` is set; a
//! token-less source accepts only a Write API key. The provider returns
//! [`WhipReject::Unauthorized`] (no credential — `401` + `WWW-Authenticate`) or
//! [`WhipReject::Forbidden`] (a valid-but-insufficient credential — `403`).
//!
//! ## Isolation (invariant #10)
//!
//! A WHIP session is an **ingest** source. An implementation must terminate it
//! on the endpoint task + the ingest threads, feed the last-good store lossily
//! (drop-oldest), and never hold a handle the engine awaits, never block the
//! engine. Both methods are synchronous and must not block on the data plane:
//! control never `.await`s a WHIP negotiation against the engine.

use std::sync::Arc;

/// The credentials presented on a WHIP request, resolved by the route layer.
///
/// The route reads the raw `Authorization: Bearer <token>` (if any) and, when
/// that token also verifies as a control-plane API key with **Write** scope,
/// sets [`WhipAuth::write_key`]. The provider then authorizes on either the
/// per-source token match (against `bearer`) or `write_key` — keeping the token
/// model in the binary (which has the source config) while the route stays
/// free of the per-source secret.
#[derive(Debug, Clone, Default)]
pub struct WhipAuth {
    /// The raw bearer token presented (the value after `Bearer `), or `None`
    /// when no `Authorization` header was sent.
    pub bearer: Option<String>,
    /// Whether the presented bearer verified as a control-plane API key with
    /// **Write** scope (Operator/Admin). A token-less source accepts only this.
    pub write_key: bool,
}

/// A negotiated WHIP answer: the endpoint-minted session id and the SDP answer
/// body the `201 Created` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhipAnswer {
    /// The session id; the trailing segment of the WHIP resource URL the
    /// publisher `DELETE`s to tear the session down.
    pub session_id: String,
    /// The SDP **answer** body (`application/sdp`).
    pub sdp: String,
}

/// Why a WHIP negotiation/teardown was refused (ADR-T014 §2 status mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhipReject {
    /// The offer was not well-formed enough to negotiate (`400`).
    Malformed(String),
    /// No credential was presented (`401` + `WWW-Authenticate: Bearer`).
    Unauthorized,
    /// A valid credential lacking publish rights (`403`).
    Forbidden,
    /// The offer shared no codec the endpoint answers (H.264 + Opus) (`406`).
    NoCompatibleCodec,
    /// A live publisher already holds this source — first publisher wins (`409`).
    Conflict,
    /// The endpoint cannot admit the session (resource exhaustion) (`503` +
    /// `Retry-After`).
    Unavailable,
}

/// The WHIP ingest transport seam — the codec-free boundary the control plane
/// delegates WHIP negotiation/teardown to.
///
/// Both methods are synchronous and must never block on the engine (invariant
/// #10). See the module docs for the auth model.
pub trait WhipProvider: Send + Sync {
    /// Negotiate a WHIP ingest session for `source_id` from the publisher's SDP
    /// `offer`, authorized by `auth`.
    ///
    /// # Errors
    ///
    /// Returns a [`WhipReject`] when the credential is missing/insufficient, the
    /// offer is malformed or codec-incompatible, a publisher already holds the
    /// source, or the endpoint cannot admit the session. Every refusal is
    /// ingest-only; it never affects the engine.
    fn negotiate(
        &self,
        source_id: &str,
        offer: &str,
        auth: &WhipAuth,
    ) -> Result<WhipAnswer, WhipReject>;

    /// Release the WHIP session `session_id` for `source_id`, authorized by
    /// `auth` (the same credential class as the creating `POST`).
    ///
    /// Returns `true` if a matching live session was found and torn down,
    /// `false` if it was unknown or already released (so the route answers `200`
    /// within the tombstone window and `404` for a never-known session).
    fn release(&self, source_id: &str, session_id: &str, auth: &WhipAuth) -> bool;

    /// The number of live WHIP ingest sessions (descriptor / isolation tests).
    fn active_sessions(&self) -> usize;
}

/// The default WHIP provider used when the binary wires no ingest transport (the
/// pure / negotiation-only build): every offer is refused `503` and there are
/// never any live sessions — the routes stay present and authz-enforced.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWhip;

impl WhipProvider for NoWhip {
    fn negotiate(
        &self,
        _source_id: &str,
        _offer: &str,
        _auth: &WhipAuth,
    ) -> Result<WhipAnswer, WhipReject> {
        // No transport is wired: the publish cannot be served, so refuse honestly
        // rather than pretending success (ADR-T014 §3 / "honest surface").
        Err(WhipReject::Unavailable)
    }

    fn release(&self, _source_id: &str, _session_id: &str, _auth: &WhipAuth) -> bool {
        false
    }

    fn active_sessions(&self) -> usize {
        0
    }
}

/// A shared [`WhipProvider`] handle, as stored in [`crate::AppState`].
pub type SharedWhip = Arc<dyn WhipProvider>;

/// The default shared WHIP provider ([`NoWhip`]).
#[must_use]
pub fn no_whip() -> SharedWhip {
    Arc::new(NoWhip)
}
