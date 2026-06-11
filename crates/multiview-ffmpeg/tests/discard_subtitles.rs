//! Unit proof for [`multiview_ffmpeg::discard_unrouted_subtitles`] (ADR-T011):
//! the main demuxer marks every unrouted subtitle-medium stream `AVDISCARD_ALL`
//! so libav stops fetching it, while NEVER touching audio or video and KEEPING a
//! single routed (`keep`) subtitle stream.
//!
//! Driven entirely offline. Two scenarios:
//!
//! 1. The **HLS-shared-context** scenario — the ABC-News-AU mechanism. libav only
//!    folds a `TYPE=SUBTITLES` `WebVTT` rendition into the one shared
//!    `AVFormatContext` when opened with `strict <= experimental` +
//!    `allowed_extensions ALL` (`FFmpeg` `hls.c` `new_rendition`); we open the
//!    offline broken-`WebVTT` fixture exactly that way so a real `WebVTT` subtitle
//!    stream sits alongside the video, then prove the discard removes it and
//!    leaves the video untouched.
//! 2. The **routed-keep** scenario — an in-container DVB-sub stream that the
//!    pipeline *consumes* (MPEG-TS DVB-sub route) must be KEPT when named in
//!    `keep`. Driven by the in-tree LGPL `dvbsub` MPEG-TS fixture.
#![cfg(feature = "test-fixtures")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg::media::Type;
use ffmpeg_next as ffmpeg;
use tempfile::TempDir;

use multiview_ffmpeg::discard_unrouted_subtitles;
use multiview_ffmpeg::test_fixtures::{generate_dvbsub_ts, generate_hls_with_broken_webvtt};

/// Open the offline broken-`WebVTT` HLS master the way that makes libav surface
/// the subtitle rendition into the one shared context (the real failure shape).
/// Returns the opened input plus the [`TempDir`] guard — the caller must keep the
/// guard alive so the on-disk fixture outlives the input.
fn open_hls_with_surfaced_subtitle() -> (ffmpeg::format::context::Input, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    generate_hls_with_broken_webvtt(dir.path()).expect("generate broken-webvtt HLS fixture");
    let master = format!("file://{}/master.m3u8", dir.path().display());

    let mut opts = ffmpeg::Dictionary::new();
    // `strict experimental` is what makes FFmpeg's HLS demuxer ADD the WebVTT
    // subtitle rendition as a stream in the shared context (hls.c new_rendition);
    // `allowed_extensions ALL` lets its `.vtt` segments through. This reproduces
    // the dangerous shared-context shape the main demuxer must defend against.
    opts.set("strict", "experimental");
    opts.set("allowed_extensions", "ALL");
    let input = ffmpeg::format::input_with_dictionary(&master.as_str(), opts)
        .expect("open broken-webvtt HLS master with the subtitle surfaced");
    (input, dir)
}

#[test]
fn discards_the_surfaced_hls_subtitle_and_leaves_video_untouched() {
    let (mut input, _dir) = open_hls_with_surfaced_subtitle();

    // Precondition: libav really did surface a subtitle stream alongside the video
    // (else this test would not be exercising the bug).
    let subtitle_before = input
        .streams()
        .filter(|s| s.parameters().medium() == Type::Subtitle)
        .count();
    assert!(
        subtitle_before >= 1,
        "the HLS master must surface at least one WebVTT subtitle stream into the \
         shared context (got {subtitle_before}); otherwise the discard guard is untested"
    );

    // The HLS case: keep = None ⇒ every subtitle stream is discarded.
    let discarded = discard_unrouted_subtitles(&mut input, None);
    assert_eq!(
        discarded, subtitle_before,
        "every surfaced subtitle stream must be discarded (got {discarded}, expected \
         {subtitle_before})"
    );

    for stream in input.streams() {
        let medium = stream.parameters().medium();
        let discard = stream.discard();
        match medium {
            Type::Subtitle => assert_eq!(
                discard,
                ffmpeg::Discard::All,
                "an unrouted subtitle stream (index {}) must be AVDISCARD_ALL",
                stream.index()
            ),
            // Audio/video MUST be left exactly as libav opened them — the guard is
            // keyed strictly on medium == Subtitle (audio-safety invariant).
            _ => assert!(
                matches!(discard, ffmpeg::Discard::None | ffmpeg::Discard::Default),
                "a non-subtitle stream (index {}, medium {medium:?}) must NOT be discarded \
                 (got {discard:?})",
                stream.index()
            ),
        }
    }
}

/// Open the in-tree LGPL `dvbsub` MPEG-TS fixture (one `mpeg2video` + one
/// `dvb_subtitle` stream). Returns the opened input, its subtitle index, and the
/// [`TempDir`] guard (kept alive by the caller).
fn open_dvbsub() -> (ffmpeg::format::context::Input, usize, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dvbsub.ts");
    generate_dvbsub_ts(&path).expect("generate dvbsub fixture");
    let input = ffmpeg::format::input(&path).expect("open dvbsub fixture");
    let sub = input
        .streams()
        .best(Type::Subtitle)
        .map(|s| s.index())
        .expect("fixture has a subtitle stream");
    (input, sub, dir)
}

#[test]
fn keep_preserves_the_routed_subtitle_stream() {
    let (mut input, sub_index, _dir) = open_dvbsub();

    // With keep=Some(sub_index) (the MPEG-TS DVB-sub route): that stream is KEPT
    // (not discarded). The fixture has exactly one subtitle stream, so with it
    // routed, nothing is discarded.
    let discarded = discard_unrouted_subtitles(&mut input, Some(sub_index));
    assert_eq!(
        discarded, 0,
        "the single routed subtitle stream must be kept, so nothing is discarded"
    );

    let kept = input
        .stream(sub_index)
        .expect("routed subtitle stream present");
    assert!(
        matches!(
            kept.discard(),
            ffmpeg::Discard::None | ffmpeg::Discard::Default
        ),
        "the routed (kept) subtitle stream must NOT be discarded (got {:?})",
        kept.discard()
    );

    // Audio/video remain untouched here too.
    for stream in input.streams() {
        if stream.parameters().medium() != Type::Subtitle {
            assert!(
                matches!(
                    stream.discard(),
                    ffmpeg::Discard::None | ffmpeg::Discard::Default
                ),
                "a non-subtitle stream (index {}) must never be discarded",
                stream.index()
            );
        }
    }
}
