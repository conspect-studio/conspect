//! Typed model of a **negotiated** WebRTC media session (the RFC 8829 JSEP
//! result), pure and panic-free.
//!
//! WHIP/WebRTC ingest negotiates its media session — codecs, RTP payload types,
//! and per-section directions — via an SDP offer/answer exchange. Production
//! negotiation is performed by **str0m** in the `multiview-webrtc` crate
//! (ADR-0048); the SDP text never reaches this crate. This module models the
//! *result* of that negotiation as value types the gated depacketizer/router
//! seam consumes: the [`Codec`] (name + clock rate), each section's
//! [`MediaKind`] and [`SdpDirection`], and the [`NegotiatedMedia`] /
//! [`NegotiatedSession`] the payload-type router binds.
//!
//! No allocation beyond the model, no `unsafe`.

/// The media kind of a negotiated `m=` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MediaKind {
    /// `m=audio`.
    Audio,
    /// `m=video`.
    Video,
    /// `m=application` (data channels).
    Application,
    /// Any other media kind (Multiview ignores these).
    Other,
}

/// The media direction (RFC 8866 §6.7) of a negotiated section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum SdpDirection {
    /// `a=sendrecv` (the default when no direction attribute is present).
    #[default]
    SendRecv,
    /// `a=sendonly`.
    SendOnly,
    /// `a=recvonly`.
    RecvOnly,
    /// `a=inactive`.
    Inactive,
}

/// A negotiated codec (a name + clock rate, case-insensitive on the name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Codec {
    /// The encoding name (e.g. `H264`, `opus`).
    pub name: &'static str,
    /// The RTP clock rate in Hz (90000 for video, 48000 for Opus, …).
    pub clock_rate: u32,
}

impl Codec {
    /// H.264 video at the WebRTC standard 90 kHz clock.
    pub const H264: Self = Self {
        name: "H264",
        clock_rate: 90_000,
    };
    /// Opus audio.
    pub const OPUS: Self = Self {
        name: "opus",
        clock_rate: 48_000,
    };
}

/// One negotiated media section of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedMedia {
    /// The media kind.
    pub kind: MediaKind,
    /// The chosen RTP payload type.
    pub payload_type: u8,
    /// The chosen codec.
    pub codec: Codec,
    /// The answerer's direction (reciprocal to the offer's).
    pub direction: SdpDirection,
}

/// The result of negotiating an answer to an offer: the media sections the
/// answerer receives, each bound by payload type at the gated `route` seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedSession {
    /// The negotiated media sections (audio/video only).
    pub sections: Vec<NegotiatedMedia>,
}

impl NegotiatedSession {
    /// The negotiated video section, if any.
    #[must_use]
    pub fn video(&self) -> Option<&NegotiatedMedia> {
        self.sections.iter().find(|s| s.kind == MediaKind::Video)
    }

    /// The negotiated audio section, if any.
    #[must_use]
    pub fn audio(&self) -> Option<&NegotiatedMedia> {
        self.sections.iter().find(|s| s.kind == MediaKind::Audio)
    }
}
