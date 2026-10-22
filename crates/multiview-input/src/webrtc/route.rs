//! Payload-type routing: decrypted RTP -> **typed compressed media events** —
//! feature `webrtc`.
//!
//! A negotiated WebRTC session carries every stream over one RTP flow,
//! distinguished by the **payload type** the answer chose per media section.
//! The [`RtpRouter`] binds those negotiated payload types to the matching pure
//! depacketizer — H.264 video ([RFC 6184], the keyframe-gated
//! [`H264Depacketizer`]) and Opus audio ([RFC 7587], the
//! [`OpusDepacketizer`](crate::webrtc::opus::OpusDepacketizer)) — and yields a
//! typed [`MediaEvent`] per completed unit (ADR-T014 §4/§5).
//!
//! The events are **compressed**: codec bitstream bytes plus the verbatim RTP
//! timestamp, keyframe, and discontinuity flags. Decode happens at the
//! application layer (the `multiview-ffmpeg` packet decoders), where geometry
//! comes from the SPS and color from the VUI — never declared here.
//!
//! Packets on a payload type the session did not negotiate (or negotiated on
//! a section we do not receive) are **counted and dropped, never errors** —
//! an unexpected stream from a publisher is sampled away, not a fault
//! (invariants #1 / #2).
//!
//! [RFC 6184]: https://www.rfc-editor.org/rfc/rfc6184
//! [RFC 7587]: https://www.rfc-editor.org/rfc/rfc7587

use multiview_core::time::Rational;

use crate::normalize::WrapBits;
use crate::webrtc::opus::OpusDepacketizer;
use crate::webrtc::transport::{H264Depacketizer, RtpFrame};
use crate::webrtc::{Codec, MediaKind, NegotiatedSession, SdpDirection};

/// One typed compressed media unit: codec bitstream bytes plus the metadata
/// the ingest pipeline needs to decode and rebase it.
///
/// `data` is **compressed** codec payload — an H.264 access unit's NAL bytes
/// or one Opus packet — never pixels; geometry/color come from the decoder
/// (SPS/VUI) at the application layer (ADR-T014). `raw_pts` is the unit's
/// 32-bit RTP timestamp, verbatim, in the clock declared by `codec`
/// (`codec.clock_rate`: 90 kHz video, 48 kHz audio).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaUnit {
    /// The negotiated codec this unit's bytes are encoded with (carries the
    /// RTP clock rate the `raw_pts` ticks in).
    pub codec: Codec,
    /// The compressed bitstream bytes (an H.264 access unit / one Opus packet).
    pub data: Vec<u8>,
    /// The 32-bit RTP timestamp surfaced verbatim as a producer-timebase raw
    /// PTS ([`MediaUnit::WRAP_BITS`]). Always `Some` (every RTP packet is
    /// timestamped); typed `Option` to match the raw-PTS plumbing downstream.
    pub raw_pts: Option<i64>,
    /// Whether this unit is a safe decoder entry point: an H.264 IDR access
    /// unit for video; **always `true` for Opus audio** (the codec has no
    /// delta-frame gating — every packet is fed to the decoder).
    pub keyframe: bool,
    /// Whether a decode gap precedes this unit (a lost or locally dropped
    /// packet); downstream normalizers re-anchor on it (invariant #3).
    pub discontinuity: bool,
}

impl MediaUnit {
    /// The timestamp wrap width of every RTP clock this seam surfaces: 32-bit
    /// ([`WrapBits::Rtp32`]) for **both** the 90 kHz video clock and the
    /// 48 kHz audio clock. Hand this to the
    /// [`PtsNormalizer`](crate::normalize::PtsNormalizer) for either stream.
    pub const WRAP_BITS: WrapBits = WrapBits::Rtp32;

    /// The unit's raw-PTS timebase: one tick of its codec's RTP clock
    /// (`1/90000` for H.264 video, `1/48000` for Opus audio).
    #[must_use]
    pub fn timebase(&self) -> Rational {
        Rational::new(1, i64::from(self.codec.clock_rate))
    }
}

/// One typed media event yielded by the router/producer seam — a video access
/// unit or an audio frame, per ADR-T014.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaEvent {
    /// One reassembled, keyframe-gated H.264 video access unit (90 kHz clock).
    VideoAccessUnit(MediaUnit),
    /// One Opus audio frame (48 kHz clock).
    AudioFrame(MediaUnit),
}

/// The bound video route: the negotiated payload type plus its depacketizer.
#[derive(Debug)]
struct VideoRoute {
    payload_type: u8,
    codec: Codec,
    depacketizer: H264Depacketizer,
}

/// The bound audio route: the negotiated payload type plus its depacketizer.
#[derive(Debug)]
struct AudioRoute {
    payload_type: u8,
    codec: Codec,
    depacketizer: OpusDepacketizer,
}

/// Routes decrypted [`RtpFrame`]s by payload type to the matching pure
/// depacketizer, yielding typed [`MediaEvent`]s.
///
/// Built from a [`NegotiatedSession`]: it binds the H.264 video section and
/// the Opus audio section **that the answer actually receives** (a section
/// answered `inactive`/`sendonly` is not bound — e.g. an `audio = false`
/// ingest, ADR-T014 §5). Each bound stream keeps its **own** sequence space:
/// a video gap never flags the audio stream and vice versa. Packets on any
/// other payload type are counted ([`RtpRouter::unknown_dropped`]) and
/// dropped — never an error and never a panic (bad inputs are sampled away,
/// invariants #1 / #2).
#[derive(Debug)]
pub struct RtpRouter {
    video: Option<VideoRoute>,
    audio: Option<AudioRoute>,
    unknown_dropped: u64,
}

impl RtpRouter {
    /// Bind the routable sections of a negotiated session.
    ///
    /// Only sections this end *receives* (`recvonly` / `sendrecv` in the
    /// answer) are bound, and only for the codecs this seam depacketizes:
    /// H.264 video and Opus audio (the only codecs Multiview answers,
    /// ADR-T014 §2). The first matching section of each kind wins.
    #[must_use]
    pub fn new(negotiated: &NegotiatedSession) -> Self {
        let mut video: Option<VideoRoute> = None;
        let mut audio: Option<AudioRoute> = None;
        for section in &negotiated.sections {
            // Our negotiated direction must admit *receiving* media.
            if !matches!(
                section.direction,
                SdpDirection::RecvOnly | SdpDirection::SendRecv
            ) {
                continue;
            }
            match section.kind {
                MediaKind::Video if section.codec == Codec::H264 && video.is_none() => {
                    video = Some(VideoRoute {
                        payload_type: section.payload_type,
                        codec: section.codec,
                        depacketizer: H264Depacketizer::new(),
                    });
                }
                MediaKind::Audio if section.codec == Codec::OPUS && audio.is_none() => {
                    audio = Some(AudioRoute {
                        payload_type: section.payload_type,
                        codec: section.codec,
                        depacketizer: OpusDepacketizer::new(),
                    });
                }
                _ => {}
            }
        }
        Self {
            video,
            audio,
            unknown_dropped: 0,
        }
    }

    /// Dispatch one decrypted RTP packet to its stream's depacketizer.
    ///
    /// Returns `Some(event)` when the packet completes a unit (a reassembled,
    /// gate-admitted video access unit, or a valid audio frame), `None` when
    /// it only advances a reassembly, is gated/dropped by its depacketizer, or
    /// rides an unknown payload type (counted, never an error).
    pub fn route(&mut self, packet: &RtpFrame) -> Option<MediaEvent> {
        if let Some(video) = self.video.as_mut() {
            if video.payload_type == packet.payload_type {
                let frame = video.depacketizer.push(packet)?;
                return Some(MediaEvent::VideoAccessUnit(MediaUnit {
                    codec: video.codec,
                    data: frame.data,
                    raw_pts: frame.raw_pts,
                    keyframe: frame.keyframe,
                    discontinuity: frame.discontinuity,
                }));
            }
        }
        if let Some(audio) = self.audio.as_mut() {
            if audio.payload_type == packet.payload_type {
                let frame = audio.depacketizer.push(packet)?;
                return Some(MediaEvent::AudioFrame(MediaUnit {
                    codec: audio.codec,
                    data: frame.data,
                    raw_pts: frame.raw_pts,
                    // Opus has no delta-frame gating: every frame is a safe
                    // decoder entry point.
                    keyframe: true,
                    discontinuity: frame.discontinuity,
                }));
            }
        }
        // An unrecognized payload type: count and drop — never an error
        // (a misbehaving publisher is sampled away, invariants #1 / #2).
        self.unknown_dropped = self.unknown_dropped.saturating_add(1);
        None
    }

    /// How many packets arrived on a payload type the session did not bind
    /// (unknown PT, or a section we do not receive). Counted and dropped.
    #[must_use]
    pub const fn unknown_dropped(&self) -> u64 {
        self.unknown_dropped
    }

    /// Whether the video keyframe gate has opened (a first IDR was seen).
    /// `false` when the session has no bound video section.
    #[must_use]
    pub fn video_gate_open(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(|v| v.depacketizer.gate_open())
    }
}
