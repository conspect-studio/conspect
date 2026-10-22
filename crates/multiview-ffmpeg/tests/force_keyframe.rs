//! Force-keyframe seam tests (ADR-0049) on the always-present LGPL software
//! encoder, so the seam is proven in every `ffmpeg` build (the VP8/libvpx
//! variant lives in `vp8_preview.rs` behind the availability probe).
//!
//! Viewer-join and PLI/FIR drive a rate-limited force-IDR into the program
//! encoder; the crate seam is `VideoEncoder::force_next_keyframe`, which makes
//! the NEXT sent frame encode as an intra/IDR picture (`AV_PICTURE_TYPE_I`).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{VideoEncodeTarget, VideoEncoder};

const W: u32 = 128;
const H: u32 = 96;

fn target() -> VideoEncodeTarget {
    VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, 25),
        bit_rate: 800_000,
        // A GOP far longer than the test run, so the ONLY keyframes are frame 0
        // (the unavoidable first intra) and whatever the seam forces.
        gop: 240,
        cuda_device: None,
    }
}

fn gray_yuv420p(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, W, H);
    for p in 0..frame.planes() {
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode 20 frames, forcing before the frame at `force_at` (if any), and
/// return the set of packet PTS values flagged as keyframes.
fn keyframe_pts(force_at: Option<i64>) -> Vec<i64> {
    let mut enc = VideoEncoder::new(&target()).expect("open mpeg2video");
    let mut keys = Vec::new();
    let collect = |pkt: &ffmpeg::codec::packet::Packet, keys: &mut Vec<i64>| {
        if pkt.is_key() {
            keys.push(pkt.pts().expect("packet pts"));
        }
    };
    for tick in 0..20_i64 {
        if force_at == Some(tick) {
            enc.force_next_keyframe();
        }
        enc.send_frame(&gray_yuv420p(tick)).expect("send");
        while let Some(pkt) = enc.receive_packet().expect("recv") {
            collect(&pkt, &mut keys);
        }
    }
    enc.send_eof().expect("eof");
    while let Some(pkt) = enc.receive_packet().expect("drain") {
        collect(&pkt, &mut keys);
    }
    keys.sort_unstable();
    keys
}

#[test]
fn without_forcing_only_frame_zero_is_a_keyframe() {
    // Baseline: with a 240-frame GOP the 20-frame run carries exactly one
    // keyframe (frame 0). This is what makes the forced-IDR assertion below
    // load-bearing rather than coincidental.
    assert_eq!(keyframe_pts(None), vec![0]);
}

#[test]
fn force_next_keyframe_makes_exactly_the_forced_frame_a_keyframe() {
    // Force before frame 7: the output packet for pts 7 must be a keyframe,
    // and no other mid-GOP packet may be.
    assert_eq!(keyframe_pts(Some(7)), vec![0, 7]);
}

#[test]
fn force_flag_is_one_shot() {
    // The seam arms exactly ONE frame: after the forced frame the encoder
    // returns to its normal cadence (proven by the exact key set above), and
    // re-arming works for a later frame too.
    assert_eq!(keyframe_pts(Some(13)), vec![0, 13]);
}
