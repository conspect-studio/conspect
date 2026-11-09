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
    /// A discrete (pre-mix) audio track, by its 0-based index in the run's
    /// declared track list (#152). Muxed as an extra audio PID.
    AudioTrack(u16),
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

    /// Wrap an encoded packet with an explicit stream `kind` (e.g. a discrete
    /// [`StreamKind::AudioTrack`]).
    #[must_use]
    pub fn with_kind(packet: codec::packet::Packet, kind: StreamKind) -> Self {
        Self { packet, kind }
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

    /// Re-stamp the decode timestamp to equal the presentation timestamp.
    ///
    /// Correct ONLY for a proven no-reorder stream (decode order equals
    /// presentation order — no B frames), where a muxer legitimately decodes
    /// each packet at its presentation instant. Used to cancel an encoder's
    /// one-frame DTS lag (e.g. mpeg2video's fixed `delay = 1`, which stamps
    /// the first packet DTS one tick below zero) so downstream tick-window
    /// admission sees the packet's true frame tick — the output-clock rule:
    /// every PTS/DTS is re-stamped from the tick counter, never trusted from a
    /// floating encoder artifact. The per-codec no-reorder fact is pinned
    /// empirically (see `multiview-output` sink tests), not by documentation.
    pub fn set_dts_to_pts(&mut self) {
        if let Some(pts) = self.packet.pts() {
            self.packet.set_dts(Some(pts));
        }
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

    /// Make an independently owned `AVPacket` header referencing the same immutable
    /// coded payload. Unlike ffmpeg-next `Packet::clone`, this does not call
    /// `av_packet_make_writable` and does not duplicate coded bytes. Muxers may
    /// change the returned header's stream index and timestamps; never call
    /// `data_mut` on it or write into its payload.
    ///
    /// # Errors
    /// Returns [`crate::error::FfmpegError::Mux`] on `av_packet_ref` allocation failure.
    #[allow(unsafe_code)]
    pub fn to_owned_packet_shared_ref(&self) -> crate::error::Result<codec::packet::Packet> {
        use ffmpeg::codec::packet::{Mut, Ref};
        let mut owned = codec::packet::Packet::empty();
        // SAFETY: Both source and destination AVPacket headers remain owned by
        // their respective Rust RAII Packet values. `av_packet_ref` initializes
        // the empty destination with independently owned metadata and an
        // AVBufferRef to the same immutable coded payload; the destination is
        // unreferenced by Packet::drop. No mutable access to shared data occurs.
        let result =
            unsafe { ffmpeg::ffi::av_packet_ref(owned.as_mut_ptr(), self.packet.as_ptr()) };
        if result < 0 {
            return Err(crate::error::FfmpegError::Mux(ffmpeg::Error::from(result)));
        }
        Ok(owned)
    }

    /// Clone only the `AVPacket` header and ref-counted immutable coded payload.
    /// Each output binding can hold its own header without another encode or
    /// another payload allocation. Metadata mutations are independent.
    ///
    /// # Errors
    /// Returns an allocation error when libav cannot reference the packet.
    pub fn clone_shared_ref(&self) -> crate::error::Result<Self> {
        Ok(Self {
            packet: self.to_owned_packet_shared_ref()?,
            kind: self.kind,
        })
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
    configured_bit_rate: Option<u64>,
    video_dimensions: Option<(u32, u32)>,
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
            configured_bit_rate: encoder.configured_bit_rate(),
            video_dimensions: Some(encoder.dimensions()),
        }
    }

    /// Snapshot the codec parameters of an opened audio encoder.
    #[must_use]
    pub fn from_audio_encoder(encoder: &crate::encode::AudioEncoder) -> Self {
        Self {
            parameters: encoder.codec_parameters(),
            configured_bit_rate: encoder.configured_bit_rate(),
            video_dimensions: None,
        }
    }

    /// Configured encoder bitrate in bits per second, if specified.
    #[must_use]
    pub fn bit_rate(&self) -> Option<u64> {
        self.configured_bit_rate
    }

    /// Configured video dimensions, if this is a video stream.
    #[must_use]
    pub fn video_dimensions(&self) -> Option<(u32, u32)> {
        self.video_dimensions
    }

    /// RFC 6381 codec identifier only when the actual parameters identify it
    /// without a profile guess. Unknown codecs omit the optional CODECS attribute.
    #[must_use]
    pub fn hls_codec(&self) -> Option<&'static str> {
        match self.parameters.id() {
            ffmpeg::codec::Id::MPEG2VIDEO => Some("mp2v"),
            ffmpeg::codec::Id::AAC => Some("mp4a.40.2"),
            _ => None,
        }
    }

    /// The opened codec's concrete identity; never a pending requested token.
    #[must_use]
    pub fn codec_id(&self) -> ffmpeg::codec::Id {
        self.parameters.id()
    }

    /// Copy the opened encoder's exact codec configuration (e.g. avcC/hvcC or
    /// AAC `AudioSpecificConfig`), if it supplied one. This copies only the
    /// small codec record, never a coded frame or an encoder instance.
    #[must_use]
    pub fn codec_config_bytes(&self) -> Option<Vec<u8>> {
        let bytes = self.extradata();
        (!bytes.is_empty()).then(|| bytes.to_vec())
    }

    /// NAL framing for the opened H.264/HEVC encoder, derived from its codec
    /// parameter record. A malformed avcC/hvcC record returns `None` rather than
    /// guessing that a closed random-access point exists.
    #[must_use]
    pub fn nal_framing(&self) -> Option<crate::idr::NalFraming> {
        let id = self.parameters.id();
        let bytes = self.extradata();
        nal_framing_from_config(id, bytes)
    }

    /// Whether this video codec has a supported, testable recovery RAP gate.
    #[must_use]
    pub fn supports_decoder_ready_rap(&self) -> bool {
        match self.parameters.id() {
            ffmpeg::codec::Id::MPEG2VIDEO => true,
            ffmpeg::codec::Id::H264 | ffmpeg::codec::Id::HEVC => self.nal_framing().is_some(),
            _ => false,
        }
    }

    /// A strict decoder-ready access-unit gate for recovery after packet loss.
    /// A keyframe flag alone is not sufficient for H.264/HEVC open GOPs or CRA.
    #[must_use]
    pub fn is_decoder_ready_rap(&self, packet: &EncodedPacket) -> bool {
        if packet.kind() != StreamKind::Video || !packet.is_keyframe() {
            return false;
        }
        match self.parameters.id() {
            ffmpeg::codec::Id::MPEG2VIDEO => true,
            ffmpeg::codec::Id::H264 => self.nal_framing().is_some_and(|framing| {
                packet.payload().is_some_and(|payload| {
                    crate::idr::is_idr(payload, crate::idr::CodecKind::H264, framing)
                })
            }),
            ffmpeg::codec::Id::HEVC => self.nal_framing().is_some_and(|framing| {
                packet.payload().is_some_and(|payload| {
                    crate::idr::is_idr(payload, crate::idr::CodecKind::Hevc, framing)
                })
            }),
            _ => false,
        }
    }

    #[allow(unsafe_code)]
    fn extradata(&self) -> &[u8] {
        // SAFETY: `parameters` owns its AVCodecParameters for this borrow.
        // Pointer and nonnegative size refer to its allocated extradata; no
        // pointer escapes the borrowed slice or is modified here.
        unsafe {
            let raw = self.parameters.as_ptr();
            let len = usize::try_from((*raw).extradata_size).unwrap_or(0);
            if (*raw).extradata.is_null() || len == 0 {
                &[]
            } else {
                std::slice::from_raw_parts((*raw).extradata, len)
            }
        }
    }

    /// Borrow the underlying `ffmpeg_next` parameters (for the muxer wrapper).
    pub(crate) fn as_parameters(&self) -> &codec::Parameters {
        &self.parameters
    }
}

/// Parse codec-configuration-record framing without borrowing libav state.
/// avcC stores `lengthSizeMinusOne` in byte 4; hvcC stores it in byte 21.
/// Unknown or truncated records fail closed instead of guessing Annex-B.
pub(crate) fn nal_framing_from_config(
    id: ffmpeg::codec::Id,
    extradata: &[u8],
) -> Option<crate::idr::NalFraming> {
    use crate::idr::NalFraming;
    let index = match id {
        ffmpeg::codec::Id::H264 => 4,
        ffmpeg::codec::Id::HEVC => 21,
        _ => return None,
    };
    if extradata.starts_with(&[0, 0, 1]) || extradata.starts_with(&[0, 0, 0, 1]) {
        return Some(NalFraming::AnnexB);
    }
    if extradata.is_empty() {
        // Raw encoder output (for example MPEG-TS) can use Annex-B with no
        // extradata. The strict IDR parser still rejects malformed packets.
        return Some(NalFraming::AnnexB);
    }
    if extradata.first() != Some(&1) {
        return None;
    }
    let size_byte = *extradata.get(index)?;
    let nal_length_size = (size_byte & 0x03) + 1;
    if nal_length_size == 3 {
        return None;
    }
    Some(NalFraming::LengthPrefixed { nal_length_size })
}

impl Clone for StreamCodecParameters {
    /// Independent copy (`avcodec_parameters_copy` into a fresh allocation), so
    /// each clone is itself owner-less and `Send`.
    fn clone(&self) -> Self {
        Self {
            parameters: self.parameters.clone(),
            configured_bit_rate: self.configured_bit_rate,
            video_dimensions: self.video_dimensions,
        }
    }
}

impl std::fmt::Debug for StreamCodecParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCodecParameters")
            .field("medium", &self.parameters.medium())
            .field("codec_id", &self.parameters.id())
            .field("configured_bit_rate", &self.configured_bit_rate)
            .field("video_dimensions", &self.video_dimensions)
            .finish()
    }
}

#[cfg(test)]
mod nal_config_tests {
    use super::nal_framing_from_config;
    use crate::idr::{is_idr, CodecKind, NalFraming};
    use ffmpeg_next::codec::Id;

    #[test]
    fn avcc_uses_byte_four_and_rejects_truncation() {
        assert_eq!(
            nal_framing_from_config(Id::H264, &[1, 0, 0, 0, 0xff]),
            Some(NalFraming::LengthPrefixed { nal_length_size: 4 })
        );
        assert_eq!(nal_framing_from_config(Id::H264, &[1, 0, 0, 0]), None);
        assert_eq!(
            nal_framing_from_config(Id::H264, &[0, 0, 0, 1, 0x65]),
            Some(NalFraming::AnnexB)
        );
        assert!(is_idr(
            &[0, 0, 0, 1, 0x65, 0x80],
            CodecKind::H264,
            NalFraming::AnnexB
        ));
    }

    #[test]
    fn hvcc_uses_byte_twenty_one_not_avcc_offset_and_rejects_cra() {
        let mut hvcc = vec![0; 22];
        hvcc[0] = 1;
        hvcc[4] = 0;
        hvcc[21] = 3;
        assert_eq!(
            nal_framing_from_config(Id::HEVC, &hvcc),
            Some(NalFraming::LengthPrefixed { nal_length_size: 4 })
        );
        assert_eq!(nal_framing_from_config(Id::HEVC, &hvcc[..21]), None);
        assert_eq!(
            nal_framing_from_config(Id::HEVC, &[0, 0, 1, 0x26, 1]),
            Some(NalFraming::AnnexB)
        );
        // HEVC CRA is NAL 21 (0x2a) and may depend on earlier pictures.
        assert!(!is_idr(
            &[0, 0, 1, 0x2a, 1, 0x80],
            CodecKind::Hevc,
            NalFraming::AnnexB
        ));
        assert!(is_idr(
            &[0, 0, 1, 0x26, 1, 0x80],
            CodecKind::Hevc,
            NalFraming::AnnexB
        ));
    }
}

#[cfg(test)]
mod shared_packet_ref_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::EncodedPacket;
    use ffmpeg_next::codec::packet::Packet;

    #[test]
    fn three_bindings_share_payload_but_not_mutable_packet_metadata() {
        let mut raw = Packet::copy(&[0, 0, 0, 1, 0x65, 0x88, 0x77]);
        raw.set_pts(Some(3003));
        raw.set_dts(Some(3003));
        let original = EncodedPacket::from_packet(raw);
        let address = original.payload().unwrap().as_ptr();
        let a = original.clone_shared_ref().unwrap();
        let b = original.clone_shared_ref().unwrap();
        let c = original.clone_shared_ref().unwrap();
        for peer in [&a, &b, &c] {
            assert_eq!(peer.payload().unwrap().as_ptr(), address);
        }
        let mut mux_a = a.to_owned_packet_shared_ref().unwrap();
        let mut mux_b = b.to_owned_packet_shared_ref().unwrap();
        let mux_c = c.to_owned_packet_shared_ref().unwrap();
        mux_a.set_stream(3);
        mux_a.set_pts(Some(9009));
        mux_b.set_stream(7);
        mux_b.set_dts(Some(1001));
        assert_eq!(mux_a.stream(), 3);
        assert_eq!(mux_b.stream(), 7);
        assert_eq!(mux_a.pts(), Some(9009));
        assert_eq!(mux_b.pts(), Some(3003));
        assert_eq!(mux_b.dts(), Some(1001));
        assert_eq!(mux_c.dts(), Some(3003));
        assert_eq!(mux_a.data().unwrap().as_ptr(), address);
        assert_eq!(mux_b.data().unwrap().as_ptr(), address);
        assert_eq!(mux_c.data().unwrap().as_ptr(), address);
        drop(original);
        drop(b);
        drop(mux_a);
        drop(a);
        assert_eq!(c.payload().unwrap(), &[0, 0, 0, 1, 0x65, 0x88, 0x77]);
        assert_eq!(mux_c.data().unwrap().as_ptr(), address);
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
