//! Demuxer subtitle-stream surfacing, proven against a self-contained,
//! libav-generated DVB-sub MPEG-TS fixture (the CLI cannot transcode text →
//! `dvbsub`; the fixture is built through libav by
//! [`multiview_ffmpeg::test_fixtures`], the `test-fixtures` feature).
//!
//! Gated behind the `test-fixtures` feature (⊇ `ffmpeg`); LGPL-clean
//! (`mpeg2video` + in-tree `dvbsub`, no x264/x265).
#![cfg(feature = "test-fixtures")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::Demuxer;
use tempfile::TempDir;

#[test]
fn surfaces_dvbsub_subtitle_stream_with_language_and_yields_packets() {
    let dir = TempDir::new().unwrap();
    let clip = dir.path().join("dvbsub.ts");
    multiview_ffmpeg::test_fixtures::generate_dvbsub_ts(&clip).expect("generate dvbsub fixture");

    let mut demux = Demuxer::open(&clip).expect("open dvbsub ts");

    // The container exposes a Subtitle stream, codec `dvbsub`, language `eng`.
    let streams = demux.streams();
    let sub = streams
        .iter()
        .find(|s| s.kind == MediaKind::Subtitle)
        .expect("a subtitle stream is surfaced");
    assert_eq!(sub.codec_name, "dvbsub", "dvbsub codec name");
    assert_eq!(
        sub.language.as_deref(),
        Some("eng"),
        "language metadata is carried"
    );
    assert!(sub.time_base.is_valid(), "subtitle time-base is valid");

    // best_stream resolves the subtitle stream and its index matches.
    let best = demux
        .best_stream(MediaKind::Subtitle)
        .expect("best subtitle stream resolves");
    assert_eq!(best, sub.index, "best subtitle stream is the dvbsub one");

    // stream_parameters() hands back the Parameters CaptionDecoder consumes.
    assert!(
        demux.stream_parameters(best).is_some(),
        "subtitle stream parameters are exposed"
    );

    // The subtitle stream yields at least one coded packet (the cue).
    let pkt = demux
        .read_packet_for(best)
        .expect("read without error")
        .expect("at least one subtitle packet");
    assert_eq!(pkt.stream_index, best);
    assert!(pkt.size() > 0, "subtitle packet carries payload");
}
