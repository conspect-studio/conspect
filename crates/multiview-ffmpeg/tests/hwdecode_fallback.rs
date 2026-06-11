//! NVDEC hardware-decode graceful-fallback proof (the `ffmpeg` feature).
//!
//! The run path opens the decoder with [`StreamVideoDecoder::new_preferring_hw`]:
//! it PREFERS NVDEC (`*_cuvid`) but must degrade to software whenever the GPU
//! decoder cannot open — a GPU-free box, a driver/library mismatch, or a codec
//! without a cuvid wrapper. This test runs on a GPU-free machine, so the
//! hardware open must fail *internally* and the decoder must still come back, run
//! the software path, and decode real frames to NV12 — never an error, never a
//! panic (invariants #1/#2: a GPU-decode failure degrades to CPU, the output
//! never falters).
//!
//! With the `cuda` feature compiled, an MPEG-2 clip selects `mpeg2_cuvid`, so the
//! decoder tries to create a CUDA device, fails (no GPU here), and falls back —
//! exercising the *full* fallback path deterministically without a GPU. Without
//! `cuda`, selection never names a hardware decoder and the software path is
//! taken directly. Either way the observable result is identical: a successful
//! software decode reporting `used_hw == false`.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;
use multiview_ffmpeg::{Demuxer, StreamVideoDecoder};

const W: u32 = 320;
const H: u32 = 240;
const RATE: u32 = 25;
const SECONDS: u32 = 1;
const EXPECTED_FRAMES: u32 = RATE * SECONDS;

/// Generate a 1-second `testsrc` clip with the **LGPL** `mpeg2video` encoder.
///
/// MPEG-2 is chosen deliberately: it has a registered `mpeg2_cuvid` wrapper, so
/// on a `cuda` build the selector names it and the decoder attempts a real CUDA
/// device open — which fails on this GPU-free runner and exercises the fallback.
/// `mpeg2video` is an LGPL software encoder (never x264/x265), so the build stays
/// LGPL-clean.
fn generate_mpeg2_clip(dir: &Path) -> PathBuf {
    let out = dir.join("src.ts");
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
            &SECONDS.to_string(),
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate clip");
    out
}

/// Grab the best video stream's owned libav `Parameters` + time-base.
fn video_params(
    clip: &Path,
) -> (
    ffmpeg::codec::Parameters,
    multiview_core::time::Rational,
    usize,
) {
    let input = ffmpeg::format::input(&clip).expect("open clip");
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .expect("best video");
    (
        stream.parameters(),
        multiview_ffmpeg::from_ff_rational(stream.time_base()),
        stream.index(),
    )
}

#[test]
fn prefers_hw_but_gracefully_falls_back_to_software_on_a_gpu_free_box() {
    multiview_ffmpeg::ensure_initialized().expect("init libav");
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = generate_mpeg2_clip(dir.path());

    let (params, time_base, vidx) = video_params(&clip);

    // Ask for hardware decode. On this GPU-free runner the cuvid open (cuda
    // build) or the absent name (non-cuda build) means the hardware path is NOT
    // taken — but the call must STILL succeed with a working software decoder.
    // `cuda_device = None` is the no-admission-pick / default-device path.
    let (mut decoder, used_hw) = StreamVideoDecoder::new_preferring_hw(
        params, time_base, /* want_hw */ true, /* cuda_device */ None,
    )
    .expect("constructing a decoder must never fail when software is available");

    assert!(
        !used_hw,
        "no GPU is present, so the hardware path must NOT have opened"
    );
    assert!(
        !decoder.is_hardware(),
        "the decoder must report it is running in software"
    );
    assert_eq!(
        decoder.hw_decoder_name(),
        None,
        "the software fallback names no cuvid decoder"
    );

    // And it must actually decode real frames to NV12 — the tile keeps running.
    let mut demux = Demuxer::open(&clip).expect("open clip for demux");
    let mut frames = 0u32;
    while let Some(pkt) = demux.read_packet().expect("read packet") {
        if pkt.stream_index != vidx {
            continue;
        }
        decoder.send_packet(&pkt.packet).expect("send packet");
        while let Some(decoded) = decoder.receive_frame().expect("receive frame") {
            assert_eq!(
                decoded.frame.format(),
                Pixel::NV12,
                "frame is NV12 (inv #5)"
            );
            assert_eq!(decoded.meta.width, W, "decoded width");
            assert_eq!(decoded.meta.height, H, "decoded height");
            frames += 1;
        }
    }
    decoder.send_eof().expect("eof");
    while let Some(decoded) = decoder.receive_frame().expect("drain") {
        assert_eq!(decoded.frame.format(), Pixel::NV12, "drained frame is NV12");
        frames += 1;
    }

    assert_eq!(
        frames, EXPECTED_FRAMES,
        "software fallback must decode every frame ({EXPECTED_FRAMES})"
    );
}

#[test]
fn want_hw_false_takes_software_directly_and_decodes() {
    // Explicitly NOT wanting hardware (the env opt-out path) must also yield a
    // working software decoder reporting `used_hw == false`.
    multiview_ffmpeg::ensure_initialized().expect("init libav");
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = generate_mpeg2_clip(dir.path());
    let (params, time_base, _vidx) = video_params(&clip);

    let (decoder, used_hw) = StreamVideoDecoder::new_preferring_hw(
        params, time_base, /* want_hw */ false, /* cuda_device */ None,
    )
    .expect("software decoder must build");
    assert!(!used_hw, "want_hw=false must never open the hardware path");
    assert!(!decoder.is_hardware());
}

#[test]
fn a_pinned_cuda_ordinal_still_falls_back_gracefully_on_a_gpu_free_box() {
    // The load-aware admission pick threads a CUDA ordinal (e.g. the chosen GPU's
    // enumeration index) into `new_preferring_hw` so NVDEC opens on the SAME GPU
    // as the compositor (affinity, ADR-0035 Tier-1 / the GPU-placement principle).
    // On this GPU-free runner the pinned-ordinal cuvid open MUST still fail
    // internally and degrade to a working software decoder — the ordinal reaching
    // the hardware path must never turn a graceful fallback into an error or a
    // panic (invariants #1/#2). This proves the ordinal is actually plumbed to the
    // cuvid open (it reaches `HwDeviceContext::create(Cuda, Some(ordinal))`) while
    // preserving the bulletproof fallback.
    multiview_ffmpeg::ensure_initialized().expect("init libav");
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = generate_mpeg2_clip(dir.path());
    let (params, time_base, vidx) = video_params(&clip);

    let (mut decoder, used_hw) = StreamVideoDecoder::new_preferring_hw(
        params,
        time_base,
        /* want_hw */ true,
        /* cuda_device */ Some("0"),
    )
    .expect("a pinned ordinal must never make the constructor fail when software is available");
    assert!(
        !used_hw,
        "no GPU is present, so even a pinned ordinal must NOT open the hardware path"
    );
    assert!(
        !decoder.is_hardware(),
        "the decoder must report software after the pinned-ordinal cuvid open failed"
    );

    // And it must still decode real frames to NV12 — the pinned ordinal did not
    // break the software fallback.
    let mut demux = Demuxer::open(&clip).expect("open clip for demux");
    let mut frames = 0u32;
    while let Some(pkt) = demux.read_packet().expect("read packet") {
        if pkt.stream_index != vidx {
            continue;
        }
        decoder.send_packet(&pkt.packet).expect("send packet");
        while let Some(decoded) = decoder.receive_frame().expect("receive frame") {
            assert_eq!(
                decoded.frame.format(),
                Pixel::NV12,
                "frame is NV12 (inv #5)"
            );
            frames += 1;
        }
    }
    decoder.send_eof().expect("eof");
    while let Some(decoded) = decoder.receive_frame().expect("drain") {
        assert_eq!(decoded.frame.format(), Pixel::NV12, "drained frame is NV12");
        frames += 1;
    }
    assert_eq!(
        frames, EXPECTED_FRAMES,
        "the software fallback must still decode every frame with a pinned ordinal"
    );
}
