//! Immutable, tick-stamped encoded packet shared by output egress paths.
//!
//! This type lives in `multiview-core` because both `multiview-ffmpeg` and
//! `multiview-output` depend on core, while the output crate can optionally
//! depend on `FFmpeg`. The dependency direction therefore permits one carrier.

use std::sync::Arc;

/// Stable identifier for one encoded rendition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenditionId(String);

impl RenditionId {
    /// Create a rendition identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RenditionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Elementary stream carried by an encoded packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamKind {
    /// Program video.
    Video,
    /// Mixed program audio.
    Audio,
    /// Discrete audio track.
    AudioTrack(u16),
}

/// Kind of media carried by an encoded packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketKind {
    /// Video keyframe suitable for a random-access boundary.
    VideoKeyframe,
    /// Non-keyframe video packet.
    VideoDelta,
    /// Mixed program audio packet.
    Audio,
    /// Discrete output audio track.
    AudioTrack(u16),
}

impl PacketKind {
    /// Return whether this packet is a keyframe.
    #[must_use]
    pub const fn is_keyframe(self) -> bool {
        matches!(self, Self::VideoKeyframe)
    }
}

/// Immutable packet metadata and a shared coded payload.
///
/// Timestamps use the encoder time base and must be stamped from the output
/// tick counter before construction. Cloning this value only increments the
/// payload reference count; it never copies coded bytes.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// The output rendition that produced this packet.
    pub rendition: RenditionId,
    /// The media kind and random-access status.
    pub kind: PacketKind,
    /// The elementary stream kind.
    pub stream_kind: StreamKind,
    /// Whether the payload contains codec parameter sets.
    pub parameter_sets: bool,
    /// Tick-derived presentation timestamp in the encoder time base.
    pub pts: i64,
    /// Tick-derived decode timestamp in the encoder time base.
    pub dts: i64,
    /// Exact packet duration in the encoder time base.
    pub duration: i64,
    /// Immutable payload shared by all egress consumers.
    pub data: Arc<[u8]>,
}

impl EncodedPacket {
    /// Build a packet and adopt its payload allocation.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        rendition: RenditionId,
        kind: PacketKind,
        stream_kind: StreamKind,
        parameter_sets: bool,
        pts: i64,
        dts: i64,
        duration: i64,
        data: impl Into<Arc<[u8]>>,
    ) -> Self {
        Self {
            rendition,
            kind,
            stream_kind,
            parameter_sets,
            pts,
            dts,
            duration,
            data: data.into(),
        }
    }

    /// Return whether this packet is a keyframe.
    #[must_use]
    pub const fn is_keyframe(&self) -> bool {
        self.kind.is_keyframe()
    }

    /// Return the stable rendition identifier.
    #[must_use]
    pub fn rendition(&self) -> &RenditionId {
        &self.rendition
    }

    /// Return the elementary stream kind.
    #[must_use]
    pub const fn stream_kind(&self) -> StreamKind {
        self.stream_kind
    }

    /// Return the timestamp derived from the output tick.
    #[must_use]
    pub const fn pts(&self) -> i64 {
        self.pts
    }

    /// Return the decode timestamp derived from the output tick.
    #[must_use]
    pub const fn dts(&self) -> i64 {
        self.dts
    }

    /// Return the packet duration in encoder time-base units.
    #[must_use]
    pub const fn duration(&self) -> i64 {
        self.duration
    }

    /// Return the packet payload length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Return whether the packet payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Return whether this packet carries codec parameter sets.
    #[must_use]
    pub const fn has_parameter_sets(&self) -> bool {
        self.parameter_sets
    }

    /// Borrow payload bytes without copying them.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{EncodedPacket, PacketKind, RenditionId, StreamKind};

    #[test]
    fn clones_share_one_immutable_payload_and_preserve_tick_stamps() {
        let payload: Arc<[u8]> = Arc::from([0_u8, 1, 2, 3]);
        let packet = EncodedPacket::new(
            RenditionId::new("program-h264"),
            PacketKind::VideoKeyframe,
            StreamKind::Video,
            true,
            120,
            120,
            1,
            Arc::clone(&payload),
        );
        let clone = packet.clone();
        assert!(Arc::ptr_eq(&packet.data, &clone.data));
        assert!(Arc::ptr_eq(&packet.data, &payload));
        assert_eq!((clone.pts, clone.dts, clone.duration), (120, 120, 1));
        assert!(clone.is_keyframe());
        assert!(clone.has_parameter_sets());
    }

    #[test]
    fn vec_payload_is_copied_once_then_shared() {
        let packet = EncodedPacket::new(
            RenditionId::new("program"),
            PacketKind::VideoDelta,
            StreamKind::Video,
            false,
            3,
            2,
            1,
            vec![8_u8, 9, 10],
        );
        let clone = packet.clone();
        assert!(Arc::ptr_eq(&packet.data, &clone.data));
        assert_eq!(clone.payload(), &[8, 9, 10]);
    }
}
