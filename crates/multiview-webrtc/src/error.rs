//! The crate error taxonomy.
//!
//! Every fallible operation in `multiview-webrtc` returns [`WebRtcError`] (or a
//! `Result` aliased to it). It is a single `thiserror` enum so callers — the cli
//! adapters that mount the signalling handlers and drive the endpoint — match one
//! type. Variants are `#[non_exhaustive]`-friendly groupings (config, signalling,
//! TURN, transport) so a new failure mode never silently maps onto an unrelated
//! one.

use std::net::SocketAddr;

/// The result type used throughout the crate.
pub type Result<T> = std::result::Result<T, WebRtcError>;

/// Everything that can go wrong in the WebRTC transport crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WebRtcError {
    /// A configuration value was invalid (e.g. an unparseable ICE-server URL, a
    /// TURN server configured without credentials).
    #[error("invalid webrtc configuration: {0}")]
    Config(String),

    /// An SDP document could not be parsed or was missing a required attribute.
    #[error("malformed SDP: {0}")]
    MalformedSdp(&'static str),

    /// The offer shared no codec the endpoint can answer for (H.264 video / Opus
    /// audio). Maps to the signalling `406 Not Acceptable`.
    #[error("no compatible codec in the offer")]
    NoCompatibleCodec,

    /// A session id was not known to the endpoint (and not a live tombstone).
    /// Maps to the signalling `404 Not Found`.
    #[error("unknown session: {0}")]
    UnknownSession(String),

    /// A second publisher tried to claim a WHIP source that already has a live
    /// session. Maps to the signalling `409 Conflict`.
    #[error("resource already has a live publisher: {0}")]
    PublisherConflict(String),

    /// The endpoint cannot admit another session (the viewer pool is full). Maps
    /// to the signalling `503 Service Unavailable`.
    #[error("endpoint at capacity")]
    AtCapacity,

    /// A TURN server rejected a request, or the TURN exchange failed.
    #[error("turn: {0}")]
    Turn(#[from] TurnError),

    /// A native transport (str0m / socket) fault. Only constructed behind the
    /// `native` feature; preview is best-effort, so a transport fault never
    /// reaches the engine.
    #[error("transport: {0}")]
    Transport(String),

    /// A UDP socket operation failed.
    #[error("socket {addr}: {source}")]
    Socket {
        /// The local address the failing socket was bound to (or attempted).
        addr: SocketAddr,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Failures specific to the TURN client (RFC 5766 / RFC 8656).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnError {
    /// A received datagram was not a parseable STUN/TURN message.
    #[error("not a STUN/TURN message")]
    NotStun,

    /// A STUN/TURN message was structurally invalid (bad length, truncated
    /// attribute, …).
    #[error("malformed STUN/TURN message: {0}")]
    Malformed(&'static str),

    /// The server returned an ERROR-CODE response.
    #[error("server error {code}: {reason}")]
    ServerError {
        /// The STUN error code (e.g. 401 Unauthorized, 438 Stale Nonce).
        code: u16,
        /// The human-readable reason phrase.
        reason: String,
    },

    /// A success response was missing a mandatory attribute (e.g. an Allocate
    /// success without XOR-RELAYED-ADDRESS).
    #[error("response missing required attribute: {0}")]
    MissingAttribute(&'static str),

    /// The server demanded long-term credentials but none were configured.
    #[error("server requires authentication but no credentials are configured")]
    AuthRequired,

    /// The allocation expired or was never established when an operation needed
    /// it.
    #[error("no live TURN allocation")]
    NoAllocation,

    /// A transaction id did not match any in-flight request.
    #[error("unexpected transaction id")]
    UnknownTransaction,
}
