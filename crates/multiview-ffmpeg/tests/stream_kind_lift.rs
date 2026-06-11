//! RT-0: the libav `MediaKind` → canonical `StreamKind` lift (ADR-0034 §1).
//!
//! `MediaKind` lives behind the off-by-default `ffmpeg` feature, so this whole
//! test is gated on it. Integration tests do not inherit `clippy.toml`'s test
//! relaxations (CLAUDE.md §A.1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "ffmpeg")]

use multiview_core::stream::{DataKind, StreamKind, TcSourceKind};
use multiview_ffmpeg::{stream_kind_from_media_and_codec, MediaKind};

#[test]
fn from_media_kind_lifts_av_kinds_one_to_one() {
    assert_eq!(StreamKind::from(MediaKind::Video), StreamKind::Video);
    assert_eq!(StreamKind::from(MediaKind::Audio), StreamKind::Audio);
    assert_eq!(StreamKind::from(MediaKind::Subtitle), StreamKind::Subtitle);
    // A bare `Other` (no codec) is a generic data passthrough, never dropped.
    assert!(StreamKind::from(MediaKind::Other).is_data());
}

#[test]
fn from_media_and_codec_refines_other_by_codec_name() {
    // AV kinds ignore the codec name.
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Video, "h264"),
        StreamKind::Video
    );
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Audio, "aac"),
        StreamKind::Audio
    );
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Subtitle, "dvb_subtitle"),
        StreamKind::Subtitle
    );

    // SCTE-35 / KLV / timecode classification from the libav codec name.
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Other, "scte_35"),
        StreamKind::Data(DataKind::Scte35)
    );
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Other, "smpte_klv"),
        StreamKind::Data(DataKind::Klv)
    );
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Other, "klv"),
        StreamKind::Data(DataKind::Klv)
    );
    assert_eq!(
        stream_kind_from_media_and_codec(MediaKind::Other, "timed_id3"),
        StreamKind::Timecode(TcSourceKind::AtcRp188)
    );

    // Unknown data essence stays routable as a generic Data passthrough.
    assert!(stream_kind_from_media_and_codec(MediaKind::Other, "bin_data").is_data());
}
