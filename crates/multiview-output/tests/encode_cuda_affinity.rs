//! NVENC encoder device-affinity seam (Tier-2 P1a) — the output-crate edge.
//!
//! `EncodeConfig.cuda_ordinal` lets the encoder be pinned to a chosen CUDA GPU
//! instead of always defaulting to device 0. The ordinal is threaded through
//! `EncodeConfig::target()` into the `multiview-ffmpeg` `VideoEncodeTarget`; the
//! actual hardware bind only fires for a `*_nvenc` codec (the suffix guard in
//! `VideoEncoder::new`). These tests pin the seam from the output side without a
//! GPU:
//!
//! * the default constructor (`EncodeConfig::mpeg2`) leaves the pin `None`, so
//!   every existing video-only run is byte-for-byte unchanged;
//! * a software codec (`mpeg2video`) with `cuda_ordinal: Some(..)` set opens a
//!   `ProgramEncoder` and encodes normally — the suffix guard means the pin is
//!   inert for software codecs (no bind attempt, no panic on a GPU-free host).
//!
//! Licensing: `mpeg2video` is an LGPL software codec already in `FFmpeg` — never
//! x264/x265 (which would be GPL).
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
use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::DecodedVideoFrame;
use multiview_output::sink::ProgramEncoder;
use multiview_output::EncodeConfig;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

fn gray_nv12_frame(width: u32, height: u32) -> DecodedVideoFrame {
    // ProgramEncoder converts NV12 -> the encoder format internally; feed it a
    // flat-gray NV12 canvas (Y=128, chroma=128) so the codec emits real packets.
    let mut frame = Video::new(Pixel::NV12, width, height);
    for p in 0..frame.planes() {
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128;
        }
    }
    DecodedVideoFrame {
        frame,
        meta: FrameMeta {
            pts: MediaTime::ZERO,
            width,
            height,
            format: PixelFormat::Nv12,
            color: ColorInfo::default(),
        },
        raw_pts: None,
        a53_cc: None,
    }
}

#[test]
fn default_encode_config_leaves_cuda_ordinal_unset() {
    // The default LGPL-clean constructor must not pin a device — behaviour is
    // unchanged from before the affinity seam existed (P1a).
    let cfg = EncodeConfig::mpeg2(WIDTH, HEIGHT);
    assert!(
        cfg.cuda_ordinal.is_none(),
        "EncodeConfig::mpeg2 must default cuda_ordinal to None"
    );
}

#[test]
fn software_codec_with_cuda_ordinal_opens_and_encodes_unchanged() {
    // Setting cuda_ordinal on a SOFTWARE-codec config must be inert: the encoder
    // opens and encodes exactly as it would with `None` (the `_nvenc` suffix
    // guard means no hardware bind is attempted). No GPU is required and there
    // must be no panic on a GPU-free host.
    let mut cfg = EncodeConfig::mpeg2(WIDTH, HEIGHT);
    cfg.cuda_ordinal = Some("1".to_owned());

    let mut encoder = ProgramEncoder::new(&cfg).expect("software ProgramEncoder opens");
    assert_eq!(encoder.time_base(), Rational::new(1, 30));

    let mut packets = 0_usize;
    for _ in 0..8 {
        let frame = gray_nv12_frame(WIDTH, HEIGHT);
        packets += encoder.encode_frame(frame).expect("encode frame").len();
    }
    packets += encoder.finish().expect("finish").len();
    assert!(
        packets > 0,
        "software codec with cuda_ordinal still produces packets"
    );
}
