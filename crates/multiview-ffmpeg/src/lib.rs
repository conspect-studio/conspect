//! # multiview-ffmpeg
//!
//! Safe RAII wrappers over libav\* (demux/decode/encode, hardware-frame
//! lifecycle). All raw libav\* FFI is owned by this crate and lives **behind
//! the off-by-default `ffmpeg` feature**, so the default workspace build is
//! pure-Rust with no native dependencies. The library target is
//! `multiview_ffmpeg`.
//!
//! ## Binding
//!
//! The `ffmpeg` feature links the **system** libav\* through
//! [`ffmpeg_next`](https://docs.rs/ffmpeg-next), which discovers the libraries
//! via `pkg-config` and auto-detects the installed `FFmpeg` version at build
//! time (no per-version Cargo feature required). It is validated against
//! `FFmpeg` 7.1 / libavcodec 61. `ffmpeg_next` provides the `unsafe` boundary
//! for the demux/decode/encode/mux/scale/resample wrappers, which are therefore
//! ordinary safe Rust. The **only** raw FFI in the crate is the hardware-frame
//! lifecycle scaffold in `hwframe`; the crate compiles under
//! `unsafe_code = "deny"` (not `forbid`), and every `unsafe` block there carries
//! a `// SAFETY:` comment stating the invariant it upholds (CLAUDE.md §7).
//!
//! ## Module map
//!
//! The modules below are compiled behind the off-by-default `ffmpeg` feature, so
//! they are named as plain code spans (they are absent from the default,
//! pure-Rust doc build).
//!
//! * `convert` — pure value mappings between libav and [`multiview_core`] types.
//! * `demux` — `Demuxer`: open a container, list streams, read packets, seek.
//! * `decode` — `VideoDecoder`: minimal first-frame spike.
//! * `decode_stream` — `StreamVideoDecoder`/`StreamAudioDecoder`: packet-fed
//!   decoders yielding NV12 host frames + [`multiview_core::frame::FrameMeta`].
//! * `audio_file` — `AudioFileDecoder`: file → interleaved-`f32` blocks
//!   (`AudioSamplesF32`) with no libav type in its public API.
//! * `scale` / `resample` — libswscale / libswresample wrappers.
//! * `encode` — `VideoEncoder`/`AudioEncoder` configured from a target.
//! * `mux` — `Muxer`: write a container, re-stamping packet timestamps.
//! * `hwframe` — `AVHWFramesContext`/`AVHWDeviceContext` RAII scaffold.
//!
//! One further module is **always compiled** (its decision logic is pure):
//!
//! * [`jpegxs`] — ST 2110-22 JPEG XS codec identity + capability detection. The
//!   path-selection algorithm and value types are native-dep-free and unit
//!   tested; only the libav-backed `probe`/`is_available` runtime queries are
//!   behind the `ffmpeg` feature.
//!
//! ## Licensing
//!
//! The default build stays LGPL-clean: only LGPL software codecs are used.
//! GPL software encoders (x264/x265) are reserved for the separate
//! `gpl-codecs` feature and are never pulled in by `ffmpeg` alone. Test and
//! default encodes use in-tree LGPL software codecs (`mpeg2video`, `ffv1`,
//! `mjpeg`, `rawvideo`).
//!
//! See [`docs/research/core-engine.md`](../docs/research/core-engine.md) §7,
//! §8.1, §12 for the FFI / hardware-frame design this crate builds toward.

/// The unified **caption cue** model (`CaptionCue` text/bitmap shapes, regions,
/// rects) plus the pure ASS-event markup stripper. Always compiled and
/// native-dep-free; the feature-gated [`caption_decode`] decoders emit these
/// types. See [`docs/io/captions.md`](../docs/io/captions.md) §1.
pub mod caption;

/// Video-codec identity + encoder selection. The logical [`codec::VideoCodec`]
/// enum and the feature-gated candidate-list logic are pure (always compiled,
/// unit-tested); only the libav-backed `codec::select_encoder` run-time probe
/// lives behind the `ffmpeg` feature.
pub mod codec;

/// In-band parameter-set / Annex-B framing **filter selection** (GP-3,
/// ADR-0030 §4 "Framing prerequisite"). Pure decision logic — the
/// codec→filter-name selection and chain composition are always compiled and
/// unit-tested (libav-free); the feature-gated [`bsf`](crate) module instantiates
/// the chosen filters over the raw `av_bsf_*` FFI.
pub mod bsf_select;

pub mod error;

/// libav → `tracing` log bridge with anti-flood rate limiting. The level
/// mapping, the bounded-LRU repetition suppressor, and the rendered-line
/// sanitiser are pure (always compiled, unit-tested, libav-free); only the
/// `av_log_set_callback` installation and the `extern "C"` trampoline live
/// behind the `ffmpeg` feature. Installed once from
/// [`decode::ensure_initialized`] so a glitchy input never floods stderr.
pub mod log_bridge;

/// Strict-IDR classifier (GP-1, ADR-0030). A cheap header inspection over a
/// coded access unit that reports a true random-access point — distinct from
/// FFmpeg's `AV_PKT_FLAG_KEY`, which also flags HEVC CRA / open-GOP and H.264
/// recovery-point I-frames. The [`idr::is_idr`] byte parser is pure (always
/// compiled, unit-tested, libav-free); the feature-gated
/// `demux::ReadPacket::is_idr` wires it to a demuxed packet.
pub mod idr;

/// Host-side hardware-decode planning. The [`hwdecode::HwDeviceKind`] enum, the
/// per-backend decode-resize strategy, the content-sized surface-pool geometry,
/// and the logical-codec -> `*_cuvid` name mapping are pure (always compiled,
/// unit-tested, GPU-free); only the libav-backed `hwdecode::select_decoder`
/// registry resolve lives behind the `ffmpeg` feature (named as a plain code
/// span: it is absent from the default doc build).
pub mod hwdecode;

/// ST 2110-22 JPEG XS codec identity + capability detection. The selection
/// algorithm and value types are pure (always compiled, unit-tested); the
/// libav-backed `probe`/`is_available` runtime queries live behind the
/// `ffmpeg` feature.
pub mod jpegxs;

/// Typed, libav-free muxer-`AVOption` surface (GP-6 Piece B, ADR-0030 §4). The
/// pure [`mux_options::MuxOptions`] model — the `avoid_negative_ts` /
/// `max_interleave_delta` knobs a guarded passthrough sets before
/// `write_header`, with up-front interior-NUL validation — is always compiled
/// and unit-tested; the feature-gated [`Muxer::create_with_options`] consumes it.
pub mod mux_options;

pub use caption::{
    strip_ass_event, CaptionCue, CueAnchor, CueBitmap, CueError, CueRect, CueRegion, CueText,
};

pub use codec::{
    can_encode, can_encode_audio, candidate_audio_encoders, candidate_encoders, AudioCodec,
    VideoCodec,
};

#[cfg(feature = "ffmpeg")]
pub use codec::{select_audio_encoder, select_encoder};

#[cfg(feature = "ffmpeg")]
pub use avio_fetch::{fetch_url_text, FetchedText};

pub use bsf_select::{
    needs_keyframe_freq_option, plan_bsf_chain, BsfFraming, BsfPlan, InputFraming,
    DUMP_EXTRA_FREQ_KEYFRAMES, FILTER_DUMP_EXTRA, FILTER_EXTRACT_EXTRADATA,
    FILTER_H264_MP4TOANNEXB, FILTER_HEVC_MP4TOANNEXB, MAX_BSF_CHAIN,
};

pub use error::{FfmpegError, Result};

pub use log_bridge::{
    map_av_level, sanitize_line, BridgeLevel, SuppressOutcome, Suppressor, AV_LOG_DEBUG,
    AV_LOG_ERROR, AV_LOG_FATAL, AV_LOG_INFO, AV_LOG_PANIC, AV_LOG_TRACE, AV_LOG_VERBOSE,
    AV_LOG_WARNING, MAX_LINE_LEN,
};

#[cfg(feature = "ffmpeg")]
pub use log_bridge::install as install_log_bridge;

pub use idr::{is_idr, CodecKind, NalFraming};

pub use hwdecode::{
    cuvid_decoder, decode_surface_pool, nvdec_disabled, plan_decode_resize, want_hw_decode,
    DecodeResizeStrategy, DecodeSurfacePool, HwBitDepth, HwDecodePlan, HwDeviceKind, HwInputCodec,
    PoolInputs, ResizeInputs, TileSize, NVDEC_DISABLE_ENV,
};

#[cfg(feature = "ffmpeg")]
pub use hwdecode::{hw_input_codec_for_id, select_decoder, select_decoder_for_id};

/// The linked-libav codec identity enum, re-exported so callers (and tests) can
/// name a stream's codec without depending on `ffmpeg-next` directly. Behind the
/// `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub use ffmpeg_next::codec::Id as CodecId;

pub use jpegxs::{
    resolve_availability, select_codec_name, JpegXsAvailability, JpegXsRole, JPEGXS_CODEC_NAMES,
};

pub use mux_options::{MuxOptionError, MuxOptions};

#[cfg(feature = "ffmpeg")]
pub use jpegxs::{is_available as jpegxs_is_available, probe as jpegxs_probe};

/// In-band parameter-set / Annex-B framing **bitstream-filter stage** (GP-3,
/// ADR-0030 §4 "Framing prerequisite"). Safe RAII wrappers
/// ([`bsf::BitstreamFilter`] / [`bsf::BsfChain`]) over the libav `av_bsf_*` FFI
/// that repeat the active SPS/PPS(/VPS) in-band before every keyframe and
/// normalise framing, so the copied-input side and the slate side reach the
/// muxer identically. Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod bsf;

#[cfg(feature = "ffmpeg")]
pub mod audio_file;

/// In-process fetch of a small text resource (an HLS caption playlist) over
/// libav I/O, replacing a `curl` shell-out. Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod avio_fetch;

/// Safe RAII caption decoders over the linked libav subtitle decoders, emitting
/// the unified [`caption::CaptionCue`] model. Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod caption_decode;

#[cfg(feature = "ffmpeg")]
pub mod convert;

#[cfg(feature = "ffmpeg")]
pub mod decode;

#[cfg(feature = "ffmpeg")]
pub mod decode_stream;

#[cfg(feature = "ffmpeg")]
pub mod demux;

#[cfg(feature = "ffmpeg")]
pub mod encode;

#[cfg(feature = "ffmpeg")]
pub mod hwframe;

#[cfg(feature = "ffmpeg")]
pub mod mux;

/// Thread-movable encoded packet ([`packet::EncodedPacket`]) + `Send`
/// codec-parameter snapshot ([`packet::StreamCodecParameters`]) for the
/// encode-once-mux-many fan-out (invariant #7, ADR-0026). Behind the `ffmpeg`
/// feature.
#[cfg(feature = "ffmpeg")]
pub mod packet;

#[cfg(feature = "ffmpeg")]
pub mod resample;

#[cfg(feature = "ffmpeg")]
pub mod scale;

/// Test-only DVB-sub MPEG-TS fixture generator (`test-fixtures` feature).
/// `#[doc(hidden)]` — **not** part of the public API; it exists so the
/// strictly-`forbid(unsafe)` `multiview-cli` tests (and this crate's own demux
/// test) can build a real `dvbsub` clip the FFmpeg CLI cannot transcode.
#[cfg(feature = "test-fixtures")]
#[doc(hidden)]
pub mod test_fixtures;

#[cfg(feature = "ffmpeg")]
pub use audio_file::{AudioFileDecoder, AudioSamplesF32};

#[cfg(feature = "ffmpeg")]
pub use bsf::{BitstreamFilter, BsfChain, FilteredPacket};

#[cfg(feature = "ffmpeg")]
pub use caption_decode::{extract_a53_cc, CaptionDecoder, CaptionSource, CcChannel};

#[cfg(feature = "ffmpeg")]
pub use convert::{
    color_from_ff, from_ff_rational, pixel_to_ff, pixel_to_multiview,
    stream_kind_from_media_and_codec, to_ff_rational, MediaKind,
};

#[cfg(feature = "ffmpeg")]
pub use decode::{ensure_initialized, DecodedFrameInfo, VideoDecoder};

#[cfg(feature = "ffmpeg")]
pub use decode_stream::{
    DecodedAudioFrame, DecodedVideoFrame, StreamAudioDecoder, StreamVideoDecoder,
};

#[cfg(feature = "ffmpeg")]
pub use demux::{discard_unrouted_subtitles, DemuxOptions, Demuxer, ReadPacket, StreamParams};

#[cfg(feature = "ffmpeg")]
pub use encode::{AudioEncodeTarget, AudioEncoder, VideoEncodeTarget, VideoEncoder};

// `HwDeviceKind` is re-exported unconditionally from `hwdecode` (it is pure);
// `hwframe` provides the FFI device/frames handles behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub use hwframe::{
    is_cuda_frame, transfer_hwframe_to_host, HwDeviceContext, HwFramesContext, HwFramesSpec,
};

#[cfg(feature = "ffmpeg")]
pub use mux::Muxer;

#[cfg(feature = "ffmpeg")]
pub use packet::{EncodedPacket, StreamCodecParameters, StreamKind};

#[cfg(feature = "ffmpeg")]
pub use resample::{ResampleSpec, Resampler};

#[cfg(feature = "ffmpeg")]
pub use scale::{ScaleSpec, Scaler};
