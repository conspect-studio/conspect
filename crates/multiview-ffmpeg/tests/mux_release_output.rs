//! `Muxer::release_output`: close the current segment file (so it can be
//! renamed on Windows, which refuses to rename an open file), keep the muxer,
//! and continue in a new file with `redirect_output`. The rotation must stay a
//! single continuous MPEG-TS (continuity counters carry over) and each file must
//! be complete when it is released.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{Muxer, VideoEncodeTarget, VideoEncoder};
use tempfile::TempDir;

const W: u32 = 64;
const H: u32 = 48;

fn encoder() -> VideoEncoder {
    VideoEncoder::new(&VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, 25),
        bit_rate: 500_000,
        gop: 5,
        cuda_device: None,
    })
    .expect("open mpeg2video")
}

fn frame(pts: i64) -> ffmpeg::util::frame::Video {
    let mut f = ffmpeg::util::frame::Video::new(Pixel::YUV420P, W, H);
    for p in 0..f.planes() {
        for b in f.data_mut(p).iter_mut() {
            *b = 128;
        }
    }
    f.set_pts(Some(pts));
    f
}

fn encode_into(enc: &mut VideoEncoder, muxer: &mut Muxer, idx: usize, from: i64, to: i64) {
    for pts in from..to {
        enc.send_frame(&frame(pts)).unwrap();
        while let Some(p) = enc.receive_packet().unwrap() {
            muxer.write_packet(idx, p).unwrap();
        }
    }
}

/// The continuity counter (low 4 bits of byte 3) per PID must step by one
/// (mod 16) for every payload-bearing packet across the concatenation.
fn assert_continuous(bytes: &[u8]) {
    assert_eq!(bytes.len() % 188, 0, "whole TS packets");
    let mut last: std::collections::HashMap<u16, u8> = std::collections::HashMap::new();
    for pkt in bytes.chunks(188) {
        assert_eq!(pkt[0], 0x47, "sync byte");
        let pid = (u16::from(pkt[1] & 0x1f) << 8) | u16::from(pkt[2]);
        let has_payload = pkt[3] & 0x10 != 0;
        let cc = pkt[3] & 0x0f;
        if pid == 0x1fff || !has_payload {
            continue;
        }
        if let Some(prev) = last.insert(pid, cc) {
            assert_eq!(cc, (prev + 1) & 0x0f, "CC discontinuity on PID {pid:#x}");
        }
    }
}

#[test]
fn released_segment_can_be_renamed_and_the_muxer_continues() {
    let dir = TempDir::new().unwrap();
    let mut enc = encoder();
    let tb = enc.time_base();
    let tmp0 = dir.path().join("seg0.ts.tmp");
    let mut muxer = Muxer::create_as(&tmp0, "mpegts").unwrap();
    let idx = muxer.add_stream(enc.as_codec_context(), tb).unwrap();
    muxer.write_header().unwrap();

    encode_into(&mut enc, &mut muxer, idx, 0, 10);
    muxer.release_output().unwrap();
    // Released: idempotent, and writing before a redirect is refused.
    muxer.release_output().unwrap();
    // The file is closed and complete: renaming it works on every platform.
    let seg0 = dir.path().join("seg0.ts");
    std::fs::rename(&tmp0, &seg0).unwrap();
    let len0 = std::fs::metadata(&seg0).unwrap().len();
    assert!(len0 > 0);

    let tmp1 = dir.path().join("seg1.ts.tmp");
    muxer.redirect_output(&tmp1).unwrap();
    encode_into(&mut enc, &mut muxer, idx, 10, 20);
    enc.send_eof().unwrap();
    while let Some(p) = enc.receive_packet().unwrap() {
        muxer.write_packet(idx, p).unwrap();
    }
    muxer.release_output().unwrap();
    // finish() after a release completes without a file to write to.
    muxer.finish().unwrap();
    let seg1 = dir.path().join("seg1.ts");
    std::fs::rename(&tmp1, &seg1).unwrap();
    // The first file did not change after its release.
    assert_eq!(std::fs::metadata(&seg0).unwrap().len(), len0);

    let mut all = std::fs::read(&seg0).unwrap();
    all.extend(std::fs::read(&seg1).unwrap());
    assert_continuous(&all);
}

#[test]
fn write_packet_after_release_is_refused_until_redirect() {
    let dir = TempDir::new().unwrap();
    let mut enc = encoder();
    let tb = enc.time_base();
    let mut muxer = Muxer::create_as(&dir.path().join("a.ts"), "mpegts").unwrap();
    let idx = muxer.add_stream(enc.as_codec_context(), tb).unwrap();
    muxer.write_header().unwrap();
    encode_into(&mut enc, &mut muxer, idx, 0, 6);
    muxer.release_output().unwrap();
    enc.send_frame(&frame(6)).unwrap();
    enc.send_eof().unwrap();
    let mut refused = false;
    while let Some(p) = enc.receive_packet().unwrap() {
        refused |= muxer.write_packet(idx, p).is_err();
    }
    assert!(refused, "a packet written while released must be refused");
    muxer.redirect_output(&dir.path().join("b.ts")).unwrap();
}

#[test]
fn release_is_refused_for_containers_that_need_a_trailer() {
    let dir = TempDir::new().unwrap();
    let mut enc = encoder();
    let tb = enc.time_base();
    let path = dir.path().join("a.mp4");
    let mut muxer = Muxer::create_as(&path, "mp4").unwrap();
    let idx = muxer.add_stream(enc.as_codec_context(), tb).unwrap();
    muxer.write_header().unwrap();
    encode_into(&mut enc, &mut muxer, idx, 0, 6);
    assert!(muxer.release_output().is_err(), "mp4 needs its trailer");
    // Still fully usable: the trailer is written normally.
    enc.send_eof().unwrap();
    while let Some(p) = enc.receive_packet().unwrap() {
        muxer.write_packet(idx, p).unwrap();
    }
    muxer.finish().unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
}
