//! ADR-W018 §7 / ADR-0018 — a placement-REJECTED live decode must actually
//! open the **software** decoder (the `ffmpeg` feature).
//!
//! The live placement consult can reject a runtime-added source
//! (over-headroom island, island vanished). The reject must be **explicit**
//! in the plan ([`DecodePlacement::SoftwareOnly`]) and must force the decoder
//! open to software: encoding it as "no ordinal" is NOT enough, because
//! `new_preferring_hw(.., want_hw=true, None)` opens NVDEC on libav's
//! **default** CUDA device — on a single-GPU host that IS the over-headroom
//! island (overcommit), and on a multi-GPU host it may be a *different* GPU
//! (silent island fragmentation, forbidden by ADR-0018's never-fragment rule).
//!
//! These tests pin the decode-open **behaviour** (the decoder kind through the
//! exact `new_preferring_hw` call `open_and_stream` makes, with the exact
//! arguments the placement gate computes), not just a field on the plan.
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg_next as ffmpeg;
use multiview_cli::pipeline::{decoder_open_args, DecodePlacement};
use multiview_ffmpeg::StreamVideoDecoder;

/// Generate a 1-second `testsrc` MPEG-2 clip (LGPL; has a registered
/// `mpeg2_cuvid` wrapper, so a `cuda` build would genuinely attempt NVDEC when
/// hardware is wanted — the discriminating codec for this pin).
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
            "testsrc=size=320x240:rate=25",
            "-t",
            "1",
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

/// The best video stream's owned `Parameters` + time-base.
fn video_params(clip: &Path) -> (ffmpeg::codec::Parameters, multiview_core::time::Rational) {
    let input = ffmpeg::format::input(&clip).expect("open clip");
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .expect("best video");
    (
        stream.parameters(),
        multiview_ffmpeg::from_ff_rational(stream.time_base()),
    )
}

#[test]
fn software_only_placement_opens_the_software_decoder() {
    // The placement gate: a SoftwareOnly plan must yield want_hw = false and
    // no ordinal — REGARDLESS of the NVDEC env opt-out being absent (None =
    // hardware otherwise wanted).
    let (want_hw, ordinal) = decoder_open_args(&DecodePlacement::SoftwareOnly, None);
    assert!(
        !want_hw,
        "a placement-rejected source must not WANT hardware decode \
         (want_hw=true with no ordinal opens NVDEC on the DEFAULT device — \
         overcommitting or fragmenting the island)"
    );
    assert!(ordinal.is_none(), "SoftwareOnly pins no CUDA ordinal");

    // And the decoder open — the exact call `open_and_stream` makes with the
    // gate's arguments — must come back as the SOFTWARE decoder.
    multiview_ffmpeg::ensure_initialized().expect("init libav");
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = generate_mpeg2_clip(dir.path());
    let (params, time_base) = video_params(&clip);
    let (decoder, used_hw) =
        StreamVideoDecoder::new_preferring_hw(params, time_base, want_hw, ordinal)
            .expect("the software decoder must always open");
    assert!(
        !used_hw,
        "a placement-rejected source must open the SOFTWARE decoder"
    );
    assert!(
        !decoder.is_hardware(),
        "the opened decoder must report software"
    );
    assert_eq!(
        decoder.hw_decoder_name(),
        None,
        "no cuvid decoder may be named for a placement-rejected source"
    );
}

#[test]
fn pinned_and_default_placements_keep_the_hardware_preference() {
    // Pinned: hardware stays wanted (subject only to the operator env
    // opt-out, the same canonical reading the run uses) and the island's
    // ordinal threads through to the open.
    let pinned = DecodePlacement::Pinned("1".to_owned());
    let (want_hw, ordinal) = decoder_open_args(&pinned, None);
    assert_eq!(
        want_hw,
        multiview_ffmpeg::want_hw_decode(None),
        "a pinned placement must not suppress the hardware preference"
    );
    assert_eq!(ordinal, Some("1"), "the island ordinal threads to the open");

    // Default (no placement decision): hardware preference unchanged, no pin
    // — exactly today's GPU-free / no-admission behaviour.
    let (want_hw, ordinal) = decoder_open_args(&DecodePlacement::Default, None);
    assert_eq!(want_hw, multiview_ffmpeg::want_hw_decode(None));
    assert!(ordinal.is_none());

    // The operator opt-out still wins over a pin (env says software).
    let (want_hw, _) = decoder_open_args(&pinned, Some("1"));
    assert!(
        !want_hw,
        "MULTIVIEW_DISABLE_NVDEC must still force software over a pin"
    );
}
