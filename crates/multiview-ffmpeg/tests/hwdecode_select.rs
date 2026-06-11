//! Hardware-decoder NAME resolution tests (the `cuda`/`ffmpeg` features).
//!
//! These do NOT open a GPU: they only prove the logical-codec -> concrete libav
//! `*_cuvid` decoder-name mapping is correct and that name actually resolves in
//! the linked libav registry (the cuvid *wrapper* decoders are registered even
//! on a GPU-free box; only *opening* one needs a device). Selection on a build
//! WITHOUT the `cuda` feature must yield no hardware candidate, so the LGPL/
//! software default can never silently reach NVDEC.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::hwdecode::{cuvid_decoder, nvdec_disabled, want_hw_decode, HwInputCodec};

#[test]
fn nvdec_opt_out_defaults_to_enabled_and_parses_falsey_tokens() {
    // Unset / empty / explicit falsey tokens leave hardware decode ENABLED so a
    // GPU box prefers NVDEC by default; only an affirmative value opts out.
    assert!(!nvdec_disabled(None), "unset must keep NVDEC enabled");
    assert!(!nvdec_disabled(Some("")), "empty must keep NVDEC enabled");
    assert!(
        !nvdec_disabled(Some("   ")),
        "whitespace-only stays enabled"
    );
    for off in ["0", "false", "FALSE", "No", "off", " off "] {
        assert!(!nvdec_disabled(Some(off)), "{off:?} must NOT disable NVDEC");
    }
    for on in ["1", "true", "YES", "on", "anything"] {
        assert!(nvdec_disabled(Some(on)), "{on:?} must disable NVDEC");
    }
    // `want_hw_decode` is the exact complement.
    assert!(want_hw_decode(None));
    assert!(!want_hw_decode(Some("1")));
    assert!(want_hw_decode(Some("0")));
}

#[test]
fn cuvid_names_follow_the_libav_convention() {
    // The pure name mapping is always available (feature-independent); the names
    // must match libav's registered `*_cuvid` wrappers, verified by name exactly
    // like the encoder candidate list.
    assert_eq!(HwInputCodec::H264.cuvid_name(), Some("h264_cuvid"));
    assert_eq!(HwInputCodec::H265.cuvid_name(), Some("hevc_cuvid"));
    assert_eq!(HwInputCodec::Av1.cuvid_name(), Some("av1_cuvid"));
    assert_eq!(HwInputCodec::Vp9.cuvid_name(), Some("vp9_cuvid"));
    assert_eq!(HwInputCodec::Mpeg2Video.cuvid_name(), Some("mpeg2_cuvid"));
}

#[cfg(feature = "cuda")]
#[test]
fn cuvid_decoder_offers_the_nvdec_name_only_with_cuda() {
    // The feature-gated `cuvid_decoder` selector returns the NVDEC name when the
    // `cuda` feature is compiled — the decode-side analogue of `nvenc_encoder`.
    assert_eq!(cuvid_decoder(HwInputCodec::H264), Some("h264_cuvid"));
    assert_eq!(cuvid_decoder(HwInputCodec::H265), Some("hevc_cuvid"));
}

#[cfg(not(feature = "cuda"))]
#[test]
fn cuvid_decoder_offers_nothing_without_cuda() {
    // Without `cuda`, the selector names no NVDEC decoder: the LGPL/software
    // default can never silently reach NVDEC (matches the encoder-side policy).
    assert_eq!(cuvid_decoder(HwInputCodec::H264), None);
    assert_eq!(cuvid_decoder(HwInputCodec::H265), None);
}

// The candidate-list / availability-probe seam needs the `ffmpeg` feature (it
// queries the linked libav registry). Behind `cuda` the NVDEC wrapper name is a
// candidate; without `cuda` it never is.
#[cfg(all(feature = "ffmpeg", feature = "cuda"))]
#[test]
fn cuvid_wrapper_names_resolve_in_the_linked_libav_registry() {
    use multiview_ffmpeg::hwdecode::select_decoder;
    // The cuvid wrapper decoders are registered even with no GPU present, so the
    // name resolves here; this proves the mapping is not aspirational. (Opening
    // one would need a device — that is the GPU-runner test.)
    assert_eq!(
        select_decoder(HwInputCodec::H264, true),
        Some("h264_cuvid"),
        "h264_cuvid must be registered in the linked FFmpeg 7.1 build"
    );
    assert_eq!(select_decoder(HwInputCodec::H265, true), Some("hevc_cuvid"));
}

#[cfg(all(feature = "ffmpeg", not(feature = "cuda")))]
#[test]
fn without_cuda_no_hardware_decoder_is_selected() {
    use multiview_ffmpeg::hwdecode::select_decoder;
    // Asking for a hardware decoder on a build without `cuda` returns None: the
    // LGPL/software default never reaches NVDEC.
    assert_eq!(select_decoder(HwInputCodec::H264, true), None);
    assert_eq!(select_decoder(HwInputCodec::H265, true), None);
}

// The codec-id -> HwInputCodec mapping is the bridge the run path uses to turn a
// demuxed stream's libav codec id into a hardware-decode request. It needs the
// `ffmpeg` feature for `ffmpeg::codec::Id`.
#[cfg(feature = "ffmpeg")]
#[test]
fn codec_id_maps_to_the_logical_hw_input_codec() {
    use multiview_ffmpeg::hwdecode::hw_input_codec_for_id;
    use multiview_ffmpeg::CodecId;
    assert_eq!(
        hw_input_codec_for_id(CodecId::H264),
        Some(HwInputCodec::H264)
    );
    assert_eq!(
        hw_input_codec_for_id(CodecId::HEVC),
        Some(HwInputCodec::H265)
    );
    assert_eq!(hw_input_codec_for_id(CodecId::AV1), Some(HwInputCodec::Av1));
    assert_eq!(hw_input_codec_for_id(CodecId::VP9), Some(HwInputCodec::Vp9));
    assert_eq!(
        hw_input_codec_for_id(CodecId::MPEG2VIDEO),
        Some(HwInputCodec::Mpeg2Video)
    );
    // A codec with no NVDEC cuvid wrapper (e.g. an audio codec) maps to None, so
    // the caller transparently keeps the software decoder.
    assert_eq!(hw_input_codec_for_id(CodecId::AAC), None);
}

#[cfg(all(feature = "ffmpeg", feature = "cuda"))]
#[test]
fn select_decoder_for_id_resolves_the_cuvid_wrapper_when_wanted() {
    use multiview_ffmpeg::hwdecode::select_decoder_for_id;
    use multiview_ffmpeg::CodecId;
    // H.264 stream + hardware wanted -> the registered cuvid wrapper name.
    assert_eq!(
        select_decoder_for_id(CodecId::H264, true),
        Some("h264_cuvid")
    );
    assert_eq!(
        select_decoder_for_id(CodecId::HEVC, true),
        Some("hevc_cuvid")
    );
    // Hardware not wanted -> software (None), even on a cuda build.
    assert_eq!(select_decoder_for_id(CodecId::H264, false), None);
    // A codec with no cuvid wrapper -> software (None).
    assert_eq!(select_decoder_for_id(CodecId::AAC, true), None);
}

#[cfg(all(feature = "ffmpeg", not(feature = "cuda")))]
#[test]
fn select_decoder_for_id_yields_software_without_cuda() {
    use multiview_ffmpeg::hwdecode::select_decoder_for_id;
    use multiview_ffmpeg::CodecId;
    // No `cuda` feature: even an H.264 stream wanting hardware decodes in
    // software (None) — the default build never reaches NVDEC.
    assert_eq!(select_decoder_for_id(CodecId::H264, true), None);
    assert_eq!(select_decoder_for_id(CodecId::HEVC, true), None);
}
