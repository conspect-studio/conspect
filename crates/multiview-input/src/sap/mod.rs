//! **SAP** — the essence-agnostic Session Announcement Protocol
//! (IETF **RFC 2974**, Experimental) engine: announce Multiview's own multicast
//! outputs and discover those announced by VLC / Dante-AES67 / RAVENNA gear.
//!
//! > **SAP = Session Announcement Protocol, *not* subtitles.** It periodically
//! > multicasts an **SDP** descriptor of a media session to a well-known group
//! > on UDP **9875**; receivers build a browsable list (VLC's "Network Streams
//! > (SAP)"). SAP carries **only** the SDP — the media itself rides RTP or raw
//! > UDP MPEG-TS on the address in the SDP `c=`/`m=` lines.
//!
//! This module is the RFC 2974 layer of [ADR-0041]: it is **SDP-content
//! agnostic** (it never parses the SDP body — that is the `sdp/` model's job).
//! It provides
//!
//! * [`packet`] — the pure byte-slice ⇄ typed [`SapPacket`] codec (parse +
//!   encode), with the adversarially-corrected wire details honoured (3-bit
//!   version, `auth_len` counted in 32-bit **words**, `E=1`/hash-0 rejected,
//!   `C=1` inflated under a hard cap).
//! * [`groups`] — the multicast group set a listener joins and the scope
//!   selector that picks the announce group from the media address.
//! * [`session`] — a bounded, fixed-capacity, drop-oldest table of discovered
//!   (untrusted) sessions with implicit purge and anti-hijack deletion handling.
//! * [`announce`] — the pure announce schedule (±1/3 jitter, ≥30 s floor) and
//!   the announcement / deletion packet builders.
//! * [`transport`] *(feature `st2110`)* — the supervised tokio `UdpSocket`
//!   listener that folds each datagram into the wait-free session table, plus
//!   the independent-timer announcer.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! SAP lives strictly on the control/discovery plane: the listener, the
//! announce timer, and the session table are **off the output-clock data
//! plane** and are physically incapable of back-pressuring the engine. The
//! session table is the bounded, drop-oldest structure (no unbounded queue
//! anywhere); the engine never awaits any of this.
//!
//! ## Security (ADR-0041 §8, brief §9)
//!
//! SAP is unauthenticated and trivially spoofable. Discovered sessions are
//! **untrusted candidates** requiring explicit operator confirm-to-bind — never
//! auto-ingested. The table is hard-capped (a global capacity **and** a
//! per-origin cap so one source cannot monopolise it), encrypted (`E=1`)
//! packets are rejected while compressed (`C=1`) packets are inflated under a
//! hard size cap (a decompression-bomb guard), and inbound deletions (`T=1`)
//! against a tracked entry are ignored (a hijack vector).
//!
//! [ADR-0041]: ../../../docs/decisions/ADR-0041.md

pub mod announce;
pub mod groups;
pub mod packet;
pub mod ratelimit;
pub mod session;
#[cfg(feature = "st2110")]
pub mod transport;

pub use announce::{announcement, deletion, stable_hash, AnnounceSchedule, MIN_ANNOUNCE_INTERVAL};
pub use groups::{announce_group_for, receive_group_set, MediaGroup, SapGroup, SAP_PORT, SAP_TTL};
pub use packet::{SapMessageType, SapPacket};
pub use ratelimit::{SapRateLimiter, DEFAULT_ACCEPT_BURST, DEFAULT_ACCEPT_WINDOW};
pub use session::{DiscoveredSession, ObserveOutcome, SapSessionTable, SessionKey};
#[cfg(feature = "st2110")]
pub use transport::{AnnouncedSession, SapAnnouncer, SapListener};

/// Errors raised by the RFC 2974 SAP **packet codec**.
///
/// `#[non_exhaustive]`: downstream `match` arms must carry a wildcard so new
/// variants stay non-breaking. The feature-gated [`transport`] layer flattens
/// these into [`crate::Error::Ingest`] at the socket boundary (mirroring the
/// ST 2110 transport), so this pure type never needs to name a crate-wide arm.
///
/// [`transport`]: crate::sap::transport
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SapError {
    /// The buffer was shorter than the bytes the header structure (flags,
    /// auth-length, hash, origin, `auth_len`×4 auth words) declared it must
    /// contain.
    #[error("sap packet too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the header structure required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },

    /// The version field (the **top 3 bits** of the flags byte) was not
    /// [`packet::SAP_VERSION`] (1). `SAPv0` (`224.2.127.255`) and any other
    /// value are rejected.
    #[error("sap version {0} unsupported (only version 1)")]
    BadVersion(u8),

    /// The 16-bit message-id hash was 0. RFC 2974 reserves 0 (a stable non-zero
    /// hash keys a session); an announcer must never emit it and a receiver
    /// rejects it.
    #[error("sap message-id hash 0 is reserved")]
    ZeroHash,

    /// The encryption bit (`E=1`) was set. `SAPv2` defines no encryption
    /// algorithm and VLC rejects encrypted announcements; so do we (never set on
    /// send, rejected on receive).
    #[error("sap encrypted (E=1) announcements are unsupported")]
    Encrypted,

    /// A `C=1` (zlib) body could not be inflated: it was not a valid zlib stream
    /// (corrupt or truncated). SAP is unauthenticated, so a malformed compressed
    /// announcement is simply dropped. Our own announcer always emits `C=0`.
    #[error("sap compressed (C=1) body is not a valid zlib stream")]
    DecompressFailed,

    /// Inflating a `C=1` (zlib) body would exceed [`packet::MAX_SDP_PAYLOAD`] — a
    /// decompression-bomb guard. The cap is enforced *during* inflate, so an
    /// adversarial announcement can never allocate past it (brief §9).
    #[error("sap decompressed body exceeds the {max}-byte cap (decompression-bomb guard)")]
    DecompressedTooLarge {
        /// The enforced maximum ([`packet::MAX_SDP_PAYLOAD`]).
        max: usize,
    },

    /// The (opaque) SDP payload exceeded [`packet::MAX_SDP_PAYLOAD`] — a hard
    /// bound so an adversarial announcement can never make the parser allocate
    /// unboundedly (brief §9).
    #[error("sap payload too large: {size} bytes exceeds the {max}-byte cap")]
    PayloadTooLarge {
        /// The payload size the packet carried.
        size: usize,
        /// The enforced maximum ([`packet::MAX_SDP_PAYLOAD`]).
        max: usize,
    },

    /// The body did not begin `v=0` (an omitted payload-type) yet carried no
    /// NUL-terminated MIME payload-type, or that type was not valid UTF-8 — the
    /// packet is malformed.
    #[error("sap payload-type field is malformed (no v=0 body and no NUL-terminated MIME type)")]
    MalformedPayloadType,
}
