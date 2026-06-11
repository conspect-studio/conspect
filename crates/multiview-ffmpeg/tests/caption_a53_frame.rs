//! End-to-end embedded CEA-608 caption decode (SUR-3b), proven against a
//! self-contained, libav-generated `mpeg2video` MPEG-TS fixture whose frames
//! carry the caption as `AV_FRAME_DATA_A53_CC` side data (the `FFmpeg` CLI cannot
//! inject known A53 captions into a video bitstream; the fixture is built through
//! libav by [`multiview_ffmpeg::test_fixtures`], the `test-fixtures` feature).
//!
//! This exercises the *whole* embedded-CC path the way the runtime will: decode
//! the video, pull the A53 side data off each decoded frame via
//! [`multiview_ffmpeg::extract_a53_cc`], feed it to `cc_dec` via
//! [`CaptionDecoder::decode_video_frame`], and assert the known caption text and
//! ns timing are recovered — and that a frame with no A53 side data yields no
//! cue (the common, non-erroring case).
//!
//! Gated behind the `test-fixtures` feature (⊇ `ffmpeg`); LGPL-clean
//! (`mpeg2video` + linked `cc_dec`, no x264/x265).
#![cfg(feature = "test-fixtures")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg_next as ffmpeg;

use multiview_core::time::{rescale, Rational};
use multiview_ffmpeg::caption_decode::{CaptionDecoder, CaptionSource, CcChannel};
use multiview_ffmpeg::test_fixtures::{generate_a53_cc_ts, A53_CAPTION_TEXT};
use multiview_ffmpeg::{extract_a53_cc, CaptionCue};
use tempfile::TempDir;

/// One decoded video frame's raw stream PTS and the A53 cc-data bytes it carried
/// (or `None` for the common no-caption frame).
type FrameCc = (Option<i64>, Option<Vec<u8>>);

/// Decode every video frame of `clip`, returning each frame's raw stream PTS and
/// the A53 cc-data bytes it carried (or `None`). Uses the raw decoded frame
/// **before** any pixel conversion so the A53 side data is intact.
fn decoded_frames(clip: &std::path::Path) -> (Rational, Vec<FrameCc>) {
    multiview_ffmpeg::ensure_initialized().unwrap();
    let mut input = ffmpeg::format::input(&clip).expect("open fixture");
    let (idx, params, tb) = {
        let s = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("a video stream");
        let r = s.time_base();
        (
            s.index(),
            s.parameters(),
            Rational::new(i64::from(r.numerator()), i64::from(r.denominator())),
        )
    };
    let ctx = ffmpeg::codec::context::Context::from_parameters(params).expect("decoder ctx");
    let mut dec = ctx.decoder().video().expect("video decoder");

    let mut out = Vec::new();
    let mut frame = ffmpeg::frame::Video::empty();
    let packets: Vec<_> = input
        .packets()
        .filter_map(|(s, p)| (s.index() == idx).then_some(p))
        .collect();
    for pkt in &packets {
        dec.send_packet(pkt).expect("send packet");
        while dec.receive_frame(&mut frame).is_ok() {
            out.push((frame.pts(), extract_a53_cc(&frame)));
        }
    }
    dec.send_eof().expect("eof");
    while dec.receive_frame(&mut frame).is_ok() {
        out.push((frame.pts(), extract_a53_cc(&frame)));
    }
    (tb, out)
}

#[test]
fn embedded_608_caption_round_trips_through_a53_side_data() {
    let dir = TempDir::new().unwrap();
    let clip = dir.path().join("a53.ts");
    generate_a53_cc_ts(&clip).expect("generate A53 fixture");

    let (time_base, frames) = decoded_frames(&clip);

    // At least one decoded frame carries A53 side data, and at least one carries
    // none (the trailing flush frames) — both branches of the embedded-CC path.
    let with_cc = frames.iter().filter(|(_, cc)| cc.is_some()).count();
    let without_cc = frames.iter().filter(|(_, cc)| cc.is_none()).count();
    assert!(
        with_cc > 0,
        "the fixture's frames carry A53 closed-caption side data"
    );
    assert!(
        without_cc > 0,
        "trailing frames carry no A53 side data (the no-caption case is exercised)"
    );

    // Feed each frame's A53 bytes to cc_dec exactly as the runtime would, and
    // collect every recovered cue with the PTS of the frame it arrived on.
    let mut dec = CaptionDecoder::for_embedded(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Cc1,
        },
        time_base,
    )
    .expect("open cc_dec");

    let mut cues: Vec<(CaptionCue, Option<i64>)> = Vec::new();
    for (pts, cc) in &frames {
        // The empty-side-data case is fed through the real `decode_video_frame`
        // surface below; here drive `decode_bytes` directly with the extracted
        // bytes so the test owns the per-frame PTS.
        if let Some(bytes) = cc {
            let got = dec
                .decode_bytes(bytes, *pts)
                .expect("decode A53 bytes without error");
            for cue in got {
                cues.push((cue, *pts));
            }
        }
    }

    assert_eq!(
        cues.len(),
        1,
        "exactly one caption is recovered (emitted on the End-Of-Caption frame), got {cues:?}"
    );
    let (cue, emit_pts) = &cues[0];
    match cue {
        CaptionCue::Text { start, end, text } => {
            assert_eq!(
                text.lines,
                vec![A53_CAPTION_TEXT.to_owned()],
                "recovered the known embedded caption text"
            );
            let emit_pts = emit_pts.expect("the EOC frame carries a PTS");
            let expected_ns = rescale(emit_pts, time_base, Rational::new(1, 1_000_000_000));
            assert_eq!(
                start.as_nanos(),
                expected_ns,
                "cue start is the EOC frame PTS rebased through the stream time-base"
            );
            assert!(
                end.as_nanos() > start.as_nanos(),
                "cue has a bounded, positive on-screen window"
            );
        }
        other => panic!("expected a text cue, got {other:?}"),
    }
}

#[test]
fn decode_video_frame_extracts_and_decodes_in_one_call() {
    // The `decode_video_frame` convenience drives the whole embedded-CC path off a
    // decoded video frame (extract A53 → cc_dec). Driving the real fixture frames
    // through it must recover the same single caption, and frames without A53 side
    // data must yield no cue and never error.
    let dir = TempDir::new().unwrap();
    let clip = dir.path().join("a53.ts");
    generate_a53_cc_ts(&clip).expect("generate A53 fixture");

    multiview_ffmpeg::ensure_initialized().unwrap();
    let mut input = ffmpeg::format::input(&clip).expect("open fixture");
    let (idx, params, time_base) = {
        let s = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("video stream");
        let r = s.time_base();
        (
            s.index(),
            s.parameters(),
            Rational::new(i64::from(r.numerator()), i64::from(r.denominator())),
        )
    };
    let ctx = ffmpeg::codec::context::Context::from_parameters(params).expect("decoder ctx");
    let mut vdec = ctx.decoder().video().expect("video decoder");
    let mut cc = CaptionDecoder::for_embedded(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Cc1,
        },
        time_base,
    )
    .expect("open cc_dec");

    let mut total_cues = 0usize;
    let mut frames_without_cc_returned_empty = 0usize;
    let mut frame = ffmpeg::frame::Video::empty();
    let packets: Vec<_> = input
        .packets()
        .filter_map(|(s, p)| (s.index() == idx).then_some(p))
        .collect();
    for pkt in &packets {
        vdec.send_packet(pkt).expect("send");
        while vdec.receive_frame(&mut frame).is_ok() {
            let had_cc = extract_a53_cc(&frame).is_some();
            let cues = cc
                .decode_video_frame(&frame, frame.pts())
                .expect("decode_video_frame never errors on a normal frame");
            if !had_cc {
                assert!(
                    cues.is_empty(),
                    "a frame with no A53 side data yields no cue"
                );
                frames_without_cc_returned_empty += 1;
            }
            total_cues += cues.len();
        }
    }
    vdec.send_eof().expect("eof");
    while vdec.receive_frame(&mut frame).is_ok() {
        let cues = cc
            .decode_video_frame(&frame, frame.pts())
            .expect("decode_video_frame never errors on flush frames");
        total_cues += cues.len();
    }

    assert_eq!(
        total_cues, 1,
        "decode_video_frame recovers exactly the one embedded caption"
    );
    assert!(
        frames_without_cc_returned_empty > 0,
        "the no-A53-side-data branch was exercised"
    );
}
