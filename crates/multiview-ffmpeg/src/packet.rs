//! Thread-movable encoded packet + codec parameters (the `ffmpeg` feature).
//!
//! Encode-once-mux-many (invariant #7, ADR-0026) encodes the canvas **once** and
//! fans the *same* coded packets to N transports. Each transport runs its own
//! muxer on its own thread, and [`Muxer::write_packet`](crate::mux::Muxer)
//! mutates the packet in place (`set_stream` + `rescale_ts`). So one encoded
//! packet cannot be shared by reference across muxers — each muxer needs its
//! **own owned** packet.
//!
//! This module provides the two values the fan-out moves between threads:
//!
//! * [`EncodedPacket`] — a `Send` wrapper around one coded packet that exposes
//!   its `pts`/`dts`/keyframe flag and yields an owned
//!   [`ffmpeg::codec::packet::Packet`] per consumer (a ref-counted
//!   `av_packet_ref` copy that is independently writable), so each muxer's
//!   in-place rescale is sound.
//! * [`StreamCodecParameters`] — a `Send` snapshot of an encoder's codec
//!   parameters, so a sink thread can register its muxer stream **without**
//!   holding the encoder instance.
//!
//! ## Timestamps (invariants #1/#3)
//! Packets arrive already stamped from the output tick counter; this wrapper
//! only carries them. Raw input PTS never reaches a muxer.
//!
//! ## Thread-safety
//! `ffmpeg_next`'s `Packet` and `Parameters` are both declared `Send` (the
//! `AVPacket`/`AVCodecParameters` they own carry no thread affinity). A
//! [`StreamCodecParameters`] is always built by **copying** into a freshly
//! allocated `AVCodecParameters` (no borrowed `Rc` owner), so moving it across
//! threads is genuinely sound, not merely permitted by the marker.

use ffmpeg::codec;
use ffmpeg_next as ffmpeg;

/// Which elementary stream an [`EncodedPacket`] belongs to.
///
/// Encode-once-mux-many fans the *same* coded packets to N muxers; with audio
/// (AUD-4) a muxer registers two streams (video + audio), so each packet must
/// say which one it writes to. The kind selects the muxer `stream_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamKind {
    /// The composited program video.
    Video,
    /// The mixed program audio.
    Audio,
}

/// One encoded packet, movable across threads and copyable per consumer.
///
/// Built from the encoder's `receive_packet` output. The fan-out hands one of
/// these to the consumer thread; each mux-only sink then takes its **own** owned
/// packet via [`EncodedPacket::to_owned_packet`] (so the muxer's in-place
/// timestamp rescale never aliases another sink's packet).
pub struct EncodedPacket {
    packet: codec::packet::Packet,
    kind: StreamKind,
}

impl EncodedPacket {
    /// Wrap a **video** encoded packet pulled from a
    /// [`VideoEncoder::receive_packet`](crate::encode::VideoEncoder::receive_packet).
    #[must_use]
    pub fn from_packet(packet: codec::packet::Packet) -> Self {
        Self {
            packet,
            kind: StreamKind::Video,
        }
    }

    /// Wrap an **audio** encoded packet pulled from an
    /// [`AudioEncoder::receive_packet`](crate::encode::AudioEncoder::receive_packet)
    /// (AUD-4): tags it [`StreamKind::Audio`] so the fan-out routes it to the
    /// muxer's audio stream.
    #[must_use]
    pub fn from_audio_packet(packet: codec::packet::Packet) -> Self {
        Self {
            packet,
            kind: StreamKind::Audio,
        }
    }

    /// Which elementary stream this packet belongs to (video or audio).
    #[must_use]
    pub const fn kind(&self) -> StreamKind {
        self.kind
    }

    /// The packet presentation timestamp in encoder time-base, or `None` if the
    /// codec did not stamp one. The muxer rescales this into stream time-base.
    #[must_use]
    pub fn pts(&self) -> Option<i64> {
        self.packet.pts()
    }

    /// The packet decode timestamp in encoder time-base, or `None`.
    #[must_use]
    pub fn dts(&self) -> Option<i64> {
        self.packet.dts()
    }

    /// Whether this packet is a keyframe (carries `AV_PKT_FLAG_KEY`). The
    /// segmented mux sink starts a new segment on a keyframe-flagged packet —
    /// the GOP boundary — rather than counting encoder frames.
    #[must_use]
    pub fn is_keyframe(&self) -> bool {
        self.packet.is_key()
    }

    /// The coded payload bytes, or `None` for an empty packet.
    ///
    /// The encode-once-mux-many file/HLS/push sinks mux the whole [`EncodedPacket`]
    /// (rescaling its timestamps in place via [`Self::to_owned_packet`]); the
    /// **WebRTC** outputs instead packetize the *bytes* (one H.264 access unit /
    /// one Opus frame) into SRTP per viewer, so they read the payload here. Reading
    /// the bytes never copies the underlying buffer and never re-encodes — it is
    /// the same coded data the muxers write (invariant #7).
    #[must_use]
    pub fn payload(&self) -> Option<&[u8]> {
        self.packet.data()
    }

    /// The coded payload length in bytes (`0` for an empty packet).
    #[must_use]
    pub fn len(&self) -> usize {
        self.packet.size()
    }

    /// Whether the coded payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packet.size() == 0
    }

    /// Yield an **owned** packet copy for one muxer to write.
    ///
    /// `ffmpeg_next`'s `Packet::clone` performs `av_packet_ref` followed by
    /// `av_packet_make_writable`, so the returned packet is a ref-counted,
    /// independently-writable copy: handing each muxer its own copy keeps
    /// [`Muxer::write_packet`](crate::mux::Muxer::write_packet)'s in-place
    /// `set_stream` + `rescale_ts` sound even when the same `EncodedPacket` fans
    /// to many muxers (invariant #7).
    #[must_use]
    pub fn to_owned_packet(&self) -> codec::packet::Packet {
        self.packet.clone()
    }

    /// Consume the wrapper, yielding its single owned packet (for the last/only
    /// muxer, avoiding the extra ref-count copy of [`Self::to_owned_packet`]).
    #[must_use]
    pub fn into_owned_packet(self) -> codec::packet::Packet {
        self.packet
    }
}

impl Clone for EncodedPacket {
    /// Clone the underlying packet (ref-counted copy), so the wrapper itself can
    /// be cheaply duplicated before the per-muxer owned copies are taken.
    fn clone(&self) -> Self {
        Self {
            packet: self.packet.clone(),
            kind: self.kind,
        }
    }
}

impl std::fmt::Debug for EncodedPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedPacket")
            .field("kind", &self.kind())
            .field("pts", &self.pts())
            .field("dts", &self.dts())
            .field("is_keyframe", &self.is_keyframe())
            .field("len", &self.len())
            .finish()
    }
}

/// A `Send` snapshot of an encoder's codec parameters.
///
/// Carries everything a muxer needs to register a matching output stream
/// (codec id, extradata, geometry, …) **without** the encoder instance, so a
/// sink thread can build its stream from this alone. Register it with
/// [`Muxer::add_stream_from_parameters`](crate::mux::Muxer::add_stream_from_parameters).
///
/// The snapshot is always an independent copy (`avcodec_parameters_copy` into a
/// freshly-allocated `AVCodecParameters`), never a view borrowing the encoder,
/// so it is safe to move across threads and outlives the encoder.
pub struct StreamCodecParameters {
    parameters: codec::Parameters,
}

impl StreamCodecParameters {
    /// Snapshot the codec parameters of an opened video encoder.
    ///
    /// The encoder runs `avcodec_parameters_from_context` into a
    /// freshly-allocated owner-less `AVCodecParameters`, so the result is a
    /// standalone, `Send` copy that outlives the encoder.
    #[must_use]
    pub fn from_encoder(encoder: &crate::encode::VideoEncoder) -> Self {
        Self {
            parameters: encoder.codec_parameters(),
        }
    }

    /// Snapshot the codec parameters of an opened audio encoder.
    #[must_use]
    pub fn from_audio_encoder(encoder: &crate::encode::AudioEncoder) -> Self {
        Self {
            parameters: encoder.codec_parameters(),
        }
    }

    /// Borrow the underlying `ffmpeg_next` parameters (for the muxer wrapper).
    pub(crate) fn as_parameters(&self) -> &codec::Parameters {
        &self.parameters
    }
}

impl Clone for StreamCodecParameters {
    /// Independent copy (`avcodec_parameters_copy` into a fresh allocation), so
    /// each clone is itself owner-less and `Send`.
    fn clone(&self) -> Self {
        Self {
            parameters: self.parameters.clone(),
        }
    }
}

impl std::fmt::Debug for StreamCodecParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCodecParameters")
            .field("medium", &self.parameters.medium())
            .field("codec_id", &self.parameters.id())
            .finish()
    }
}

#[cfg(test)]
mod stream_kind_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{EncodedPacket, StreamKind};
    use ffmpeg_next as ffmpeg;

    #[test]
    fn from_packet_defaults_to_video_and_from_audio_tags_audio() {
        // AUD-4: the fan-out must route audio packets to the audio mux stream, so
        // every EncodedPacket carries its stream kind. The existing video path
        // (`from_packet`) must stay Video so video-only sinks are unchanged.
        let v = EncodedPacket::from_packet(ffmpeg::codec::packet::Packet::empty());
        assert_eq!(v.kind(), StreamKind::Video);

        let a = EncodedPacket::from_audio_packet(ffmpeg::codec::packet::Packet::empty());
        assert_eq!(a.kind(), StreamKind::Audio);

        // The kind survives the per-muxer ref-counted clone (the fan-out clones
        // before handing each sink its owned copy).
        assert_eq!(a.clone().kind(), StreamKind::Audio);
    }

    #[test]
    fn payload_exposes_the_coded_bytes_for_webrtc_packetization() {
        // The WebRTC outputs (ADR-0049) packetize the coded AU bytes into SRTP per
        // viewer — the SAME bytes the file/HLS/push muxers write (encode-once,
        // invariant #7). An empty packet has no payload; a packet built from bytes
        // exposes exactly those bytes.
        let empty = EncodedPacket::from_packet(ffmpeg::codec::packet::Packet::empty());
        assert!(empty.payload().is_none(), "empty packet has no payload");

        let au = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAB, 0xCD];
        let coded = EncodedPacket::from_packet(ffmpeg::codec::packet::Packet::copy(&au));
        assert_eq!(
            coded.payload(),
            Some(au.as_slice()),
            "payload() returns the coded access-unit bytes verbatim"
        );
        assert_eq!(coded.len(), au.len());
    }
}
