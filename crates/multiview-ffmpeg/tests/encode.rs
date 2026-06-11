//! Encoder wrapper tests: configuration, the LGPL-codec contract, and the
//! send-frame/receive-packet loop on synthetic frames (no media file needed).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{Pixel, Sample};
use ffmpeg::util::frame::Video;
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{AudioEncodeTarget, AudioEncoder, VideoEncodeTarget, VideoEncoder};

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
        // Default: no device pin — behaviour is unchanged from before the NVENC
        // affinity seam existed (P1a). Tests that exercise the pin set it below.
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

#[test]
fn opens_lgpl_mpeg2video_encoder_and_reports_time_base() {
    let enc = VideoEncoder::new(&target("mpeg2video")).expect("open mpeg2video");
    // The exact time-base must survive configuration (invariant #3).
    assert_eq!(enc.time_base(), Rational::new(1, 25));
}

#[test]
fn unknown_codec_is_a_typed_error() {
    // A codec that does not exist must surface CodecNotFound — never a panic.
    match VideoEncoder::new(&target("this_codec_does_not_exist")) {
        Ok(_) => panic!("a nonexistent codec must not open"),
        Err(err) => assert!(
            err.to_string().contains("not found"),
            "typed CodecNotFound error, got: {err}"
        ),
    }
}

#[test]
fn encodes_frames_into_packets_with_carried_timestamps() {
    let mut enc = VideoEncoder::new(&target("mpeg2video")).expect("open encoder");

    let mut packet_count = 0_u32;
    let mut saw_pts = false;
    for tick in 0..10_i64 {
        // Re-stamp from the tick counter (the output-clock contract).
        let frame = gray_yuv420p(tick);
        enc.send_frame(&frame).expect("send frame");
        while let Some(pkt) = enc.receive_packet().expect("recv packet") {
            packet_count += 1;
            if pkt.pts().is_some() {
                saw_pts = true;
            }
        }
    }
    enc.send_eof().expect("flush");
    while let Some(pkt) = enc.receive_packet().expect("drain") {
        packet_count += 1;
        if pkt.pts().is_some() {
            saw_pts = true;
        }
    }

    assert!(packet_count > 0, "encoder produced packets");
    assert!(saw_pts, "encoded packets carry presentation timestamps");
}

#[test]
fn video_encoder_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<VideoEncoder>();
}

// --- Audio encoder (AUD-4) ------------------------------------------------

fn aac_target() -> AudioEncodeTarget {
    AudioEncodeTarget {
        codec_name: "aac".to_owned(),
        format: Sample::F32(SampleType::Planar),
        channel_layout: ChannelLayout::STEREO,
        sample_rate: 48_000,
        bit_rate: 128_000,
    }
}

#[test]
fn audio_encoder_encodes_planar_f32_blocks_into_packets() {
    // AUD-4: the program bus hands the encoder planar f32 samples; the encoder
    // builds the libav Audio frame (frame construction lives in this crate),
    // stamps it from a sample counter, and emits real AAC packets.
    let mut enc = AudioEncoder::new(&aac_target()).expect("open aac");
    let fs_u32 = if enc.frame_size() == 0 {
        1024
    } else {
        enc.frame_size()
    };
    let fs = usize::try_from(fs_u32).unwrap_or(1024);
    let left = vec![0.01_f32; fs];
    let right = vec![-0.01_f32; fs];

    let mut packets = 0_u32;
    let mut pts = 0_i64;
    for _ in 0..8 {
        enc.send_planar_f32(&[&left, &right], fs, pts)
            .expect("send planar f32");
        while enc.receive_packet().expect("recv").is_some() {
            packets += 1;
        }
        pts = pts.saturating_add(i64::from(fs_u32));
    }
    enc.send_eof().expect("eof");
    while enc.receive_packet().expect("drain").is_some() {
        packets += 1;
    }
    assert!(packets > 0, "aac produced packets from planar f32 input");
}

#[test]
fn software_codec_ignores_cuda_device_and_opens_unchanged() {
    // P1a: the encoder hw-device bind is gated by the `_nvenc` codec-name
    // suffix. A software codec (`mpeg2video`) given a `cuda_device` must NOT
    // attempt any hardware bind — it opens exactly as it would with `None`,
    // produces packets, and reports the same time-base. This proves the suffix
    // guard: `cuda_device.is_some()` alone never reaches the bind path.
    let mut tgt = target("mpeg2video");
    tgt.cuda_device = Some("0".to_owned());
    let mut enc = VideoEncoder::new(&tgt).expect("software codec opens despite cuda_device");
    assert_eq!(enc.time_base(), Rational::new(1, 25));

    let mut packets = 0_u32;
    for tick in 0..6_i64 {
        enc.send_frame(&gray_yuv420p(tick)).expect("send frame");
        while enc.receive_packet().expect("recv").is_some() {
            packets += 1;
        }
    }
    enc.send_eof().expect("flush");
    while enc.receive_packet().expect("drain").is_some() {
        packets += 1;
    }
    assert!(
        packets > 0,
        "software codec with cuda_device still encodes normally"
    );
}

#[test]
fn nvenc_target_with_cuda_device_does_not_panic_on_a_gpu_free_box() {
    // P1a graceful fallback: a `*_nvenc` target naming a CUDA ordinal must never
    // panic when no GPU/driver is present. `VideoEncoder::new` either:
    //   * succeeds on the default device because the bind degraded gracefully
    //     (a real GPU runner without that ordinal), OR
    //   * fails with a typed error if the `_nvenc` codec is absent from the
    //     build / the encoder open is rejected.
    // The load-bearing assertion is "typed Result, never a panic" — exactly the
    // decode-stream HW->default degradation contract mirrored on the encode side.
    let mut tgt = target("h264_nvenc");
    tgt.cuda_device = Some("1".to_owned());
    match VideoEncoder::new(&tgt) {
        Ok(enc) => {
            // If it opened, the time-base survived configuration.
            assert_eq!(enc.time_base(), Rational::new(1, 25));
        }
        Err(err) => {
            // Typed error rendered without panicking.
            assert!(!err.to_string().is_empty(), "error renders to a message");
        }
    }
}

#[test]
fn audio_encoder_rejects_wrong_plane_count() {
    // A stereo encoder fed a single plane is a wiring bug — a typed error, never
    // a panic from a mismatched plane index.
    let mut enc = AudioEncoder::new(&aac_target()).expect("open aac");
    let one = vec![0.0_f32; 1024];
    assert!(
        enc.send_planar_f32(&[&one], 1024, 0).is_err(),
        "mono plane into a stereo encoder must be a typed error"
    );
}
