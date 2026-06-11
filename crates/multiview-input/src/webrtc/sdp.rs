//! A pure, panic-free SDP (RFC 8866) parser and offer/answer negotiator for the
//! WebRTC ingest model.
//!
//! SDP is a line-oriented `type=value` format. This module parses the subset
//! Multiview negotiates — the session version line, each `m=` media description, its
//! direction attribute, and its `a=rtpmap:` payload-type → codec map — into a
//! typed [`SessionDescription`], and answers an offer by intersecting payload
//! types with a supported set. No allocation beyond the parsed model, no `unsafe`,
//! and malformed input surfaces as [`super::WebRtcError`].

use super::WebRtcError;

/// The media kind of an `m=` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MediaKind {
    /// `m=audio`.
    Audio,
    /// `m=video`.
    Video,
    /// `m=application` (data channels).
    Application,
    /// Any other media kind, preserving nothing (Multiview ignores these).
    Other,
}

impl MediaKind {
    /// Parse the first token of an `m=` line.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "audio" => Self::Audio,
            "video" => Self::Video,
            "application" => Self::Application,
            _ => Self::Other,
        }
    }

    /// The SDP token for this kind.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Application => "application",
            Self::Other => "other",
        }
    }
}

/// The media direction attribute (RFC 8866 §6.7).
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

impl SdpDirection {
    /// Parse a direction attribute keyword.
    #[must_use]
    pub fn from_attr(attr: &str) -> Option<Self> {
        match attr {
            "sendrecv" => Some(Self::SendRecv),
            "sendonly" => Some(Self::SendOnly),
            "recvonly" => Some(Self::RecvOnly),
            "inactive" => Some(Self::Inactive),
            _ => None,
        }
    }

    /// The SDP keyword for this direction.
    #[must_use]
    pub const fn as_attr(self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }

    /// The reciprocal direction the answerer offers: a `sendonly` offer is
    /// answered `recvonly`, and vice versa; `sendrecv`/`inactive` are symmetric.
    #[must_use]
    pub const fn reciprocal(self) -> Self {
        match self {
            Self::SendRecv => Self::SendRecv,
            Self::SendOnly => Self::RecvOnly,
            Self::RecvOnly => Self::SendOnly,
            Self::Inactive => Self::Inactive,
        }
    }
}

/// A codec the negotiator can match against (a name + clock rate, case-insensitive
/// on the name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Codec {
    /// The encoding name (e.g. `H264`, `VP8`, `opus`).
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
    /// VP8 video.
    pub const VP8: Self = Self {
        name: "VP8",
        clock_rate: 90_000,
    };
    /// Opus audio.
    pub const OPUS: Self = Self {
        name: "opus",
        clock_rate: 48_000,
    };

    /// Whether `rtpmap` describes this codec (case-insensitive name + matching
    /// clock rate).
    #[must_use]
    pub fn matches(self, rtpmap: &RtpMap) -> bool {
        rtpmap.encoding_name.eq_ignore_ascii_case(self.name) && rtpmap.clock_rate == self.clock_rate
    }
}

/// One `a=rtpmap:PT name/clock[/channels]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMap {
    /// The RTP payload type (`0..=127`).
    pub payload_type: u8,
    /// The encoding name.
    pub encoding_name: String,
    /// The clock rate in Hz.
    pub clock_rate: u32,
    /// The channel count (audio only).
    pub channels: Option<u8>,
}

/// One `m=` media description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    /// The media kind.
    pub kind: MediaKind,
    /// The transport port from the `m=` line.
    pub port: u16,
    /// The payload types listed on the `m=` line, in offered preference order.
    pub payload_types: Vec<u8>,
    /// The `a=rtpmap:` entries for this section.
    pub rtpmaps: Vec<RtpMap>,
    /// The media direction.
    pub direction: SdpDirection,
}

impl MediaDescription {
    /// The `RtpMap` for a payload type, if declared.
    #[must_use]
    pub fn rtpmap(&self, payload_type: u8) -> Option<&RtpMap> {
        self.rtpmaps.iter().find(|m| m.payload_type == payload_type)
    }

    /// Choose the first offered payload type whose rtpmap matches one of
    /// `supported`, preserving the offerer's preference order.
    #[must_use]
    pub fn select_codec(&self, supported: &[Codec]) -> Option<(u8, Codec)> {
        for &pt in &self.payload_types {
            if let Some(map) = self.rtpmap(pt) {
                if let Some(codec) = supported.iter().copied().find(|c| c.matches(map)) {
                    return Some((pt, codec));
                }
            }
        }
        None
    }
}

/// A parsed SDP session description (the subset Multiview negotiates).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionDescription {
    /// The protocol version from `v=` (always `0` in practice).
    pub version: u32,
    /// The media descriptions, in order.
    pub media: Vec<MediaDescription>,
}

impl SessionDescription {
    /// Parse an SDP document.
    ///
    /// Lines before the first `m=` form the session section (only `v=` is read);
    /// `a=` lines after an `m=` apply to that media section.
    ///
    /// # Errors
    ///
    /// * [`WebRtcError::MalformedSdp`] when a line lacks its `type=value` form or
    ///   the `v=`/`m=` structure is wrong.
    /// * [`WebRtcError::BadField`] when a numeric field fails to parse.
    pub fn parse(sdp: &str) -> Result<Self, WebRtcError> {
        let mut version = 0u32;
        let mut media: Vec<MediaDescription> = Vec::new();

        for raw_line in sdp.lines() {
            let line = raw_line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let (ty, value) = line
                .split_once('=')
                .ok_or(WebRtcError::MalformedSdp("line is not type=value"))?;
            match ty {
                "v" => {
                    version = value.trim().parse().map_err(|_e| WebRtcError::BadField {
                        field: "v",
                        value: value.to_owned(),
                    })?;
                }
                "m" => media.push(parse_media_line(value)?),
                "a" => {
                    let Some(current) = media.last_mut() else {
                        // Session-level attribute; not needed for negotiation.
                        continue;
                    };
                    apply_attribute(current, value)?;
                }
                // c=, o=, s=, t=, etc. are not needed for codec negotiation.
                _ => {}
            }
        }

        Ok(Self { version, media })
    }

    /// The first video media section, if any.
    #[must_use]
    pub fn video(&self) -> Option<&MediaDescription> {
        self.media.iter().find(|m| m.kind == MediaKind::Video)
    }

    /// The first audio media section, if any.
    #[must_use]
    pub fn audio(&self) -> Option<&MediaDescription> {
        self.media.iter().find(|m| m.kind == MediaKind::Audio)
    }

    /// Negotiate an answer to this offer, selecting one codec per audio/video
    /// section from `supported_video` / `supported_audio`.
    ///
    /// Returns the answer as a [`NegotiatedSession`] describing the chosen
    /// payload type, codec, and reciprocal direction for each negotiable section.
    ///
    /// # Errors
    ///
    /// * [`WebRtcError::NoMedia`] when the offer has no media sections.
    /// * [`WebRtcError::NoCompatibleCodec`] when an audio/video section shares no
    ///   codec with the supported set.
    pub fn negotiate_answer(
        &self,
        supported_video: &[Codec],
        supported_audio: &[Codec],
    ) -> Result<NegotiatedSession, WebRtcError> {
        if self.media.is_empty() {
            return Err(WebRtcError::NoMedia);
        }
        let mut sections = Vec::new();
        for m in &self.media {
            let supported = match m.kind {
                MediaKind::Video => supported_video,
                MediaKind::Audio => supported_audio,
                // Multiview does not negotiate data/other sections; skip them.
                MediaKind::Application | MediaKind::Other => continue,
            };
            let (payload_type, codec) = m
                .select_codec(supported)
                .ok_or(WebRtcError::NoCompatibleCodec(m.kind.as_token()))?;
            sections.push(NegotiatedMedia {
                kind: m.kind,
                payload_type,
                codec,
                direction: m.direction.reciprocal(),
            });
        }
        if sections.is_empty() {
            return Err(WebRtcError::NoMedia);
        }
        Ok(NegotiatedSession { sections })
    }
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

/// The result of negotiating an answer to an offer.
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

/// Parse an `m=` line value: `<media> <port>[/<count>] <proto> <fmt> ...`.
fn parse_media_line(value: &str) -> Result<MediaDescription, WebRtcError> {
    let mut parts = value.split_whitespace();
    let kind = MediaKind::from_token(
        parts
            .next()
            .ok_or(WebRtcError::MalformedSdp("m= line missing media kind"))?,
    );
    let port_token = parts
        .next()
        .ok_or(WebRtcError::MalformedSdp("m= line missing port"))?;
    // Port may be `port/count`; take the leading number.
    let port_str = port_token.split('/').next().unwrap_or(port_token);
    let port: u16 = port_str.parse().map_err(|_e| WebRtcError::BadField {
        field: "m.port",
        value: port_token.to_owned(),
    })?;
    // Skip the transport proto token (e.g. UDP/TLS/RTP/SAVPF).
    let _proto = parts
        .next()
        .ok_or(WebRtcError::MalformedSdp("m= line missing transport proto"))?;
    let mut payload_types = Vec::new();
    for fmt in parts {
        let pt: u8 = fmt.parse().map_err(|_e| WebRtcError::BadField {
            field: "m.fmt",
            value: fmt.to_owned(),
        })?;
        payload_types.push(pt);
    }
    Ok(MediaDescription {
        kind,
        port,
        payload_types,
        rtpmaps: Vec::new(),
        direction: SdpDirection::default(),
    })
}

/// Apply an `a=` attribute to the current media section.
fn apply_attribute(media: &mut MediaDescription, value: &str) -> Result<(), WebRtcError> {
    if let Some(direction) = SdpDirection::from_attr(value.trim()) {
        media.direction = direction;
        return Ok(());
    }
    if let Some(rtpmap) = value.strip_prefix("rtpmap:") {
        media.rtpmaps.push(parse_rtpmap(rtpmap)?);
    }
    // Other attributes (fmtp, fingerprint, ice-ufrag, …) are transport concerns,
    // handled by the gated transport layer; the model ignores them.
    Ok(())
}

/// Parse an `rtpmap` attribute body: `<PT> <name>/<clock>[/<channels>]`.
fn parse_rtpmap(body: &str) -> Result<RtpMap, WebRtcError> {
    let (pt_str, rest) = body
        .split_once(char::is_whitespace)
        .ok_or(WebRtcError::MalformedSdp("rtpmap missing encoding"))?;
    let payload_type: u8 = pt_str.trim().parse().map_err(|_e| WebRtcError::BadField {
        field: "rtpmap.pt",
        value: pt_str.to_owned(),
    })?;
    let mut fields = rest.trim().split('/');
    let encoding_name = fields
        .next()
        .ok_or(WebRtcError::MalformedSdp("rtpmap missing encoding name"))?
        .to_owned();
    let clock_str = fields
        .next()
        .ok_or(WebRtcError::MalformedSdp("rtpmap missing clock rate"))?;
    let clock_rate: u32 = clock_str.parse().map_err(|_e| WebRtcError::BadField {
        field: "rtpmap.clock",
        value: clock_str.to_owned(),
    })?;
    let channels = match fields.next() {
        Some(c) => Some(c.parse().map_err(|_e| WebRtcError::BadField {
            field: "rtpmap.channels",
            value: c.to_owned(),
        })?),
        None => None,
    };
    Ok(RtpMap {
        payload_type,
        encoding_name,
        clock_rate,
        channels,
    })
}
