//! Shareable-packet tests (the `ffmpeg` feature).
//!
//! Layer 1 of encode-once-mux-many (ADR-0026 / invariant #7): the cli encodes
//! the canvas **once** on one thread and fans packet copies to N mux-only sinks,
//! each on its own thread. That requires:
//!
//! 1. a way to carry an encoded packet across threads and hand each muxer its
//!    **own owned** packet (so `Muxer::write_packet`'s mutate-in-place rescale is
//!    sound), exposing `pts`/`dts`/`is_keyframe`; and
//! 2. a `Send` representation of the encoder's codec parameters so each sink
//!    thread can build its muxer stream **without** the encoder instance.
//!
//! These tests prove an [`EncodedPacket`] round-trips its data, PTS and keyframe
//! flag through an owned copy, that it is `Send`, that the codec-params handle is
//! `Send`, and that a muxer stream built from the params alone accepts a header.
//!
//! Licensing: encodes use `mpeg2video`, an LGPL software codec already in
//! `FFmpeg` — never x264/x265 (which would be GPL).
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
use multiview_ffmpeg::{
    EncodedPacket, Muxer, StreamCodecParameters, VideoEncodeTarget, VideoEncoder,
};

const W: u32 = 64;
const H: u32 = 48;

fn target(codec: &str) -> VideoEncodeTarget {
    VideoEncodeTarget {
        codec_name: codec.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, 25),
        bit_rate: 500_000,
        gop: 12,
        cuda_device: None,
    }
}

/// A flat-gray YUV420P frame stamped at `pts` (the tick counter).
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

/// Encode a few frames and collect the [`EncodedPacket`]s.
fn encode_some(encoder: &mut VideoEncoder) -> Vec<EncodedPacket> {
    let mut out = Vec::new();
    for tick in 0..10_i64 {
        let frame = gray_yuv420p(tick);
        encoder.send_frame(&frame).expect("send frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv packet") {
            out.push(EncodedPacket::from_packet(pkt));
        }
    }
    encoder.send_eof().expect("flush");
    while let Some(pkt) = encoder.receive_packet().expect("drain") {
        out.push(EncodedPacket::from_packet(pkt));
    }
    out
}

#[test]
fn encoded_packet_and_codec_params_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<EncodedPacket>();
    assert_send::<StreamCodecParameters>();
}

#[test]
fn owned_copy_preserves_data_pts_and_keyframe_flag() {
    let mut encoder = VideoEncoder::new(&target("mpeg2video")).expect("open encoder");
    let packets = encode_some(&mut encoder);
    assert!(!packets.is_empty(), "encoder produced packets");
    assert!(
        packets.iter().any(EncodedPacket::is_keyframe),
        "at least one packet is a keyframe (the first GOP)"
    );

    for pkt in &packets {
        // Two independent owned copies must carry identical bytes, PTS and the
        // keyframe flag — they are what each mux-sink thread writes.
        let a = pkt.to_owned_packet();
        let b = pkt.to_owned_packet();
        assert_eq!(a.data(), b.data(), "owned copies share the same bytes");
        assert_eq!(
            a.data().map(<[u8]>::len),
            Some(pkt.len()),
            "EncodedPacket::len matches the owned payload length"
        );
        assert_eq!(a.pts(), pkt.pts(), "owned copy preserves PTS");
        assert_eq!(a.dts(), pkt.dts(), "owned copy preserves DTS");
        assert_eq!(
            a.is_key(),
            pkt.is_keyframe(),
            "owned copy preserves the keyframe flag"
        );
    }
}

#[test]
fn muxer_stream_built_from_send_params_accepts_a_header() {
    let encoder = VideoEncoder::new(&target("mpeg2video")).expect("open encoder");
    let time_base = encoder.time_base();
    // The Send codec-params snapshot built WITHOUT keeping the encoder instance.
    let params = StreamCodecParameters::from_encoder(&encoder);
    drop(encoder);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("from_params.mp4");
    let mut muxer = Muxer::create(&path).expect("create muxer");
    let index = muxer
        .add_stream_from_parameters(&params, time_base)
        .expect("add stream from params");
    assert_eq!(index, 0, "the first stream is index 0");
    muxer
        .write_header()
        .expect("write header from params-only stream");
    muxer.finish().expect("finish trailer");
    assert!(path.exists(), "container was written");
    assert!(
        path.metadata().expect("stat").len() > 0,
        "container is non-empty"
    );
}

#[test]
fn one_encode_two_muxers_get_the_same_packets() {
    // The encode-once-mux-many property at the packet level: one encoder's
    // packets, copied per muxer, produce two byte-identical streams. (Each muxer
    // gets its OWN owned packet so write_packet's in-place rescale is sound.)
    let mut encoder = VideoEncoder::new(&target("mpeg2video")).expect("open encoder");
    let time_base = encoder.time_base();
    let params = StreamCodecParameters::from_encoder(&encoder);
    let packets = encode_some(&mut encoder);
    assert!(packets.len() >= 2, "need several packets to be meaningful");

    let dir = tempfile::tempdir().expect("tempdir");
    let a_path = dir.path().join("a.ts");
    let b_path = dir.path().join("b.ts");

    for path in [&a_path, &b_path] {
        let mut muxer = Muxer::create_as(path, "mpegts").expect("create mpegts muxer");
        let index = muxer
            .add_stream_from_parameters(&params, time_base)
            .expect("add stream");
        muxer.write_header().expect("write header");
        for pkt in &packets {
            muxer
                .write_packet(index, pkt.to_owned_packet())
                .expect("write owned packet");
        }
        muxer.finish().expect("finish");
    }

    let a = std::fs::read(&a_path).expect("read a");
    let b = std::fs::read(&b_path).expect("read b");
    assert_eq!(
        a, b,
        "the same packets fanned to two muxers yield identical containers"
    );
    assert!(!a.is_empty(), "containers are non-empty");
}
