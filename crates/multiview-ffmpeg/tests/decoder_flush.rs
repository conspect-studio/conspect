//! Proof that a video decoder can be **flushed mid-stream** so that, after a
//! container [`seek`](multiview_ffmpeg::Demuxer::seek), the decoder drops every
//! buffered/reordered frame and resumes cleanly from the new position — never
//! leaking a stale reordered frame from before the seek.
//!
//! This is the media-player loop's non-negotiable rule (ADR-0097 §rule-2,
//! media-playout §7.5): a seek without an `avcodec_flush_buffers` leaves
//! reordered **B-frames** buffered in the decoder, which then surface *after*
//! the wrap as out-of-order/stale pictures. The wrapper under test is
//! [`StreamVideoDecoder::flush`] (and the lower-level [`VideoDecoder::flush`]).
//!
//! The clip is generated at test time with the **LGPL** software codec
//! `mpeg2video` **with B-frames** (`-bf 2`) — never x264/x265 — so the decoder
//! genuinely reorders and *holds* buffered frames (the precondition the flush
//! exists to handle). Gated behind the `ffmpeg` feature so the default pure-Rust
//! build never touches native deps, and self-contained (no checked-in media).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::{Demuxer, StreamVideoDecoder, VideoDecoder};
use tempfile::TempDir;

/// Width of the generated test pattern.
const W: u32 = 160;
/// Height of the generated test pattern.
const H: u32 = 120;
/// Frame rate of the generated test pattern (whole-number → exact time-base).
const RATE: u32 = 25;
/// Clip length in seconds (2s ⇒ ~50 frames over several GOPs, plenty of
/// reorder structure for a deterministic buffered-frame-drop observation).
const SECS: u32 = 2;

/// Generate a short `testsrc` clip into `dir` with the LGPL `mpeg2video` codec
/// **and B-frames** (`-bf 2`), returning the path. B-frames make the decoder
/// reorder and hold buffered frames — the exact condition `flush()` must clear.
///
/// Panics (the harness reports it) only if the CLI is genuinely unavailable,
/// which would mean the test environment is misconfigured.
fn generate_clip(dir: &Path) -> PathBuf {
    let out = dir.join("flush_src.ts");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate={RATE}"),
            "-t",
            &SECS.to_string(),
            // LGPL, in-tree software codec — NOT x264/x265 (GPL).
            "-c:v",
            "mpeg2video",
            // Two B-frames between references ⇒ genuine reorder + buffered frames.
            "-bf",
            "2",
            // A short GOP keeps a keyframe near any seek target.
            "-g",
            "12",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("failed to spawn the `ffmpeg` CLI (is FFmpeg installed?)");
    assert!(
        status.success(),
        "ffmpeg CLI exited with failure while generating the test clip"
    );
    out
}

/// Open `clip`, returning a demuxer, the video stream index, and a
/// `StreamVideoDecoder` built from that stream's owned codec parameters.
fn open_decoder(clip: &Path) -> (Demuxer, usize, StreamVideoDecoder) {
    let demux = Demuxer::open(clip).expect("open clip");
    let streams = demux.streams();
    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("video stream present");
    let vidx = video.index;
    let params = demux
        .stream_parameters(vidx)
        .expect("owned codec parameters for the video stream");
    let decoder = StreamVideoDecoder::new(params, video.time_base).expect("build video decoder");
    (demux, vidx, decoder)
}

/// Feed every video packet (without EOF), draining frames after each, and
/// return `(frames_emitted, packets_sent)`. With B-frames `packets_sent`
/// exceeds `frames_emitted`: the surplus is the decoder's still-buffered
/// reorder window.
fn feed_all_packets(
    demux: &mut Demuxer,
    vidx: usize,
    decoder: &mut StreamVideoDecoder,
) -> (usize, usize) {
    let mut emitted = 0usize;
    let mut sent = 0usize;
    while let Some(pkt) = demux.read_packet_for(vidx).expect("read packet") {
        decoder.send_packet(&pkt.packet).expect("send packet");
        sent += 1;
        while decoder.receive_frame().expect("receive frame").is_some() {
            emitted += 1;
        }
    }
    (emitted, sent)
}

/// Drain the decoder to EOF, returning how many frames it still emits.
fn drain_to_eof(decoder: &mut StreamVideoDecoder) -> usize {
    decoder.send_eof().expect("send eof");
    let mut drained = 0usize;
    while decoder.receive_frame().expect("drain frame").is_some() {
        drained += 1;
    }
    drained
}

/// `flush()` **discards the decoder's buffered reorder window**: after feeding
/// every packet (which, with B-frames, leaves several frames buffered awaiting
/// EOF), a flush drops them, so a subsequent EOF-drain yields **zero** frames —
/// whereas a decoder that was *not* flushed would emit its held frames on EOF.
///
/// This is the behaviour ADR-0097 rule #2 requires: the stale reordered frames
/// the decoder is holding must not survive the flush. A no-op `flush()` (one
/// that does not reach `avcodec_flush_buffers`) fails this test, because the
/// buffered frames reappear on the EOF drain.
#[test]
fn flush_discards_the_buffered_reorder_window() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let (mut demux, vidx, mut decoder) = open_decoder(&clip);

    let (emitted, sent) = feed_all_packets(&mut demux, vidx, &mut decoder);
    // Sanity: B-frame reorder means the decoder is holding frames it has not yet
    // emitted (more packets went in than frames came out). If this is not true
    // the fixture lost its B-frames and the rest of the assertion is vacuous.
    assert!(
        sent > emitted,
        "the B-frame clip must leave frames buffered before EOF \
         (sent {sent}, emitted {emitted}); fixture has no reorder window"
    );
    let buffered = sent - emitted;
    assert!(
        buffered >= 1,
        "expected at least one buffered reorder frame, got {buffered}"
    );

    // Flush drops the buffered window.
    decoder.flush().expect("flush mid-stream");

    // The held frames must be GONE: a flushed decoder emits nothing on EOF.
    let after_flush = drain_to_eof(&mut decoder);
    assert_eq!(
        after_flush, 0,
        "flush() must discard the {buffered} buffered reorder frame(s); the EOF \
         drain emitted {after_flush}, so the buffered state was NOT flushed"
    );
}

/// Control case proving the precondition: **without** a flush, the buffered
/// reorder window DOES survive to the EOF drain. This pins that the previous
/// test's `assert_eq!(after_flush, 0)` is meaningful (the frames really would
/// have come out otherwise) rather than vacuously true.
#[test]
fn without_flush_the_buffered_window_survives_to_eof() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let (mut demux, vidx, mut decoder) = open_decoder(&clip);

    let (emitted, sent) = feed_all_packets(&mut demux, vidx, &mut decoder);
    let buffered = sent.saturating_sub(emitted);
    assert!(buffered >= 1, "fixture must buffer reorder frames");

    // No flush: the EOF drain releases exactly the buffered window, and the
    // grand total equals every packet we sent (mpeg2video emits one frame per
    // coded picture). This is the behaviour flush() deliberately suppresses.
    let drained = drain_to_eof(&mut decoder);
    assert_eq!(
        drained, buffered,
        "without flush the EOF drain must release the {buffered} held frame(s)"
    );
    assert_eq!(
        emitted + drained,
        sent,
        "every coded picture must decode to a frame across the full drain"
    );
}

/// After a real container seek, **seek + flush** resumes decoding cleanly and
/// the first frame emitted is the stream's true start frame (minimum PTS) — a
/// decoder still holding pre-seek reordered frames could not yield the start
/// frame first.
#[test]
fn seek_then_flush_resumes_from_the_stream_start() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let (mut demux, vidx, mut decoder) = open_decoder(&clip);

    // Decode the whole clip (without EOF) so the decoder has advanced to the end
    // and is holding a reorder window.
    let (emitted, sent) = feed_all_packets(&mut demux, vidx, &mut decoder);
    assert!(emitted >= 1, "must decode frames on the first pass");
    assert!(sent > emitted, "first pass must leave buffered frames");

    // Seek back to the very start and flush the held window.
    demux.seek(0).expect("seek to start");
    decoder.flush().expect("flush after seek");

    // The first frame decoded after the flush must be the stream start frame.
    // Read packets until a frame appears, and assert its raw PTS is the minimum
    // (earliest) — i.e. the decoder genuinely restarted, it did not hand back a
    // leftover buffered frame from the end of the first pass.
    let mut first_after: Option<i64> = None;
    while first_after.is_none() {
        match demux.read_packet_for(vidx).expect("read packet after seek") {
            Some(pkt) => {
                decoder
                    .send_packet(&pkt.packet)
                    .expect("send packet after seek");
                if let Some(frame) = decoder.receive_frame().expect("receive after seek") {
                    first_after = frame.raw_pts;
                }
            }
            None => break,
        }
    }
    let first_after = first_after.expect("a frame must decode after seek + flush");

    // Independently establish the stream's minimum PTS with a fresh decoder.
    let (mut d2, v2, mut dec2) = open_decoder(&clip);
    let mut min_pts = i64::MAX;
    while let Some(pkt) = d2.read_packet_for(v2).expect("read packet") {
        dec2.send_packet(&pkt.packet).expect("send packet");
        while let Some(frame) = dec2.receive_frame().expect("receive frame") {
            if let Some(p) = frame.raw_pts {
                min_pts = min_pts.min(p);
            }
        }
    }
    dec2.send_eof().expect("eof");
    while let Some(frame) = dec2.receive_frame().expect("drain") {
        if let Some(p) = frame.raw_pts {
            min_pts = min_pts.min(p);
        }
    }
    assert_ne!(min_pts, i64::MAX, "stream must carry at least one PTS");

    assert_eq!(
        first_after, min_pts,
        "after seek-to-start + flush the first frame PTS ({first_after}) must be \
         the stream's earliest PTS ({min_pts}); a different value means the \
         decoder leaked a stale buffered frame instead of being flushed"
    );
}

/// `flush()` is callable on a fresh/idempotently and decoding continues after
/// it — pinning that the wrapper is wired to the real libav decoder context and
/// is safe to call between packets (including when nothing is buffered).
#[test]
fn flush_is_idempotent_and_decoding_survives_it() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let (mut demux, vidx, mut decoder) = open_decoder(&clip);

    // Flush on a fresh decoder (no buffered state) is a harmless no-op.
    decoder.flush().expect("flush on a fresh decoder");
    decoder.flush().expect("flush is idempotent");

    // Decode to EOF after the early flushes; decoding must still produce frames.
    let (emitted, _sent) = feed_all_packets(&mut demux, vidx, &mut decoder);
    let drained = drain_to_eof(&mut decoder);
    assert!(
        emitted + drained >= 1,
        "decoding must still produce frames after early flushes (got {})",
        emitted + drained
    );
}

/// The lower-level [`VideoDecoder`] (the first-frame spike decoder) also exposes
/// a `flush()` that is callable without error, mirroring the streaming
/// decoder's wiring on the same FFI surface.
#[test]
fn inner_video_decoder_flush_is_callable() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());

    let mut decoder = VideoDecoder::open(&clip).expect("open the inner decoder");
    let first = decoder
        .decode_first_frame()
        .expect("decode the first frame");
    assert_eq!(first.width, W, "decoded width");
    assert_eq!(first.height, H, "decoded height");

    // Flushing drops any buffered frames; it must not error and the decoder
    // remains usable for a subsequent flush.
    decoder.flush().expect("flush the inner decoder");
    decoder.flush().expect("inner flush is idempotent");
}
