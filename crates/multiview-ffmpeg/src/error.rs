//! Error taxonomy for [`multiview-ffmpeg`](crate).
//!
//! Every fallible operation in the safe libav* wrappers returns
//! [`FfmpegError`]. The enum is `#[non_exhaustive]` so new arms can be added
//! without a breaking change — downstream `match` statements must carry a
//! wildcard arm.

use thiserror::Error;

/// Result alias for the ffmpeg crate.
pub type Result<T> = core::result::Result<T, FfmpegError>;

/// Errors produced by the safe libav* wrappers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FfmpegError {
    /// One-time global libav* initialization failed.
    #[cfg(feature = "ffmpeg")]
    #[error("libav initialization failed: {0}")]
    Init(#[source] ffmpeg_next::Error),

    /// The input container could not be opened or its header could not be
    /// read (bad path, unknown format, truncated header, …).
    #[cfg(feature = "ffmpeg")]
    #[error("failed to open input {path:?}: {source}")]
    OpenInput {
        /// The input path or URL that failed to open.
        path: String,
        /// The underlying libav error.
        #[source]
        source: ffmpeg_next::Error,
    },

    /// A blocked open/read on a live input was aborted by the injected
    /// `AVIOInterruptCB` / `rw_timeout` because the configured deadline elapsed
    /// (GP-0, ADR-0030). This is the recovery-teardown signal: the demuxer is
    /// wedged on a stalled TCP/RTSP/SRT/UDP read and must be torn down and
    /// reconnected, rather than blocking forever.
    #[cfg(feature = "ffmpeg")]
    #[error("input {path:?} timed out after {timeout_ms} ms (read interrupted)")]
    ReadTimeout {
        /// The input path or URL whose read was interrupted.
        path: String,
        /// The configured read/write timeout in milliseconds.
        timeout_ms: u64,
    },

    /// A small text resource (an HLS caption master/rendition playlist) could
    /// not be fetched over libav I/O — a disallowed scheme, an open/read error,
    /// an oversize body, or a non-UTF-8 body.
    #[cfg(feature = "ffmpeg")]
    #[error("failed to fetch {url}: {reason}")]
    Fetch {
        /// The URL that failed to fetch.
        url: String,
        /// Why it failed (libav error string, blocked protocol, oversize, …).
        reason: String,
    },

    /// The container has no stream of the requested media type.
    #[error("no {0} stream found in input")]
    StreamNotFound(&'static str),

    /// A decoder could not be constructed for the selected stream
    /// (unsupported codec, missing codec parameters, …).
    #[cfg(feature = "ffmpeg")]
    #[error("failed to open decoder: {0}")]
    OpenDecoder(#[source] ffmpeg_next::Error),

    /// Sending a packet to, or receiving a frame from, the decoder failed
    /// with an error other than the expected `EAGAIN` / `EOF` drain signals.
    #[cfg(feature = "ffmpeg")]
    #[error("decode failed: {0}")]
    Decode(#[source] ffmpeg_next::Error),

    /// Demuxing reached end-of-stream (or hit an unrecoverable read error)
    /// before any frame of the requested kind could be decoded.
    #[error("end of stream reached before a {0} frame was decoded")]
    EndOfStream(&'static str),

    /// A requested encoder/decoder codec is not available in the linked
    /// `FFmpeg` build (e.g. the LGPL software encoder was not compiled in).
    #[error("codec {0:?} not found in the linked FFmpeg build")]
    CodecNotFound(&'static str),

    /// Building or opening an encoder failed (bad parameters, unsupported
    /// pixel/sample format for the codec, …).
    #[cfg(feature = "ffmpeg")]
    #[error("failed to open encoder: {0}")]
    OpenEncoder(#[source] ffmpeg_next::Error),

    /// Sending a frame to, or receiving a packet from, the encoder failed with
    /// an error other than the expected `EAGAIN` / `EOF` drain signals.
    #[cfg(feature = "ffmpeg")]
    #[error("encode failed: {0}")]
    Encode(#[source] ffmpeg_next::Error),

    /// Allocating, opening, or writing the output container failed.
    #[cfg(feature = "ffmpeg")]
    #[error("muxing operation failed: {0}")]
    Mux(#[source] ffmpeg_next::Error),

    /// A bitstream-filter operation failed (GP-3, ADR-0030 §4 framing
    /// prerequisite): allocating / configuring / initialising an `AVBSFContext`,
    /// or sending/receiving a packet through it. `op` names the libav call that
    /// failed and `code` is its return code (`0` for a non-libav setup failure
    /// such as an interior NUL or a missing `priv_data`).
    #[cfg(feature = "ffmpeg")]
    #[error("bitstream-filter operation {op} failed (code {code})")]
    Bsf {
        /// The libav `av_bsf_*` / setup call that failed.
        op: &'static str,
        /// The libav return code, or `0` for a setup-side failure.
        code: i64,
    },

    /// Building a libswscale or libswresample conversion context failed, or a
    /// conversion call returned an error.
    #[cfg(feature = "ffmpeg")]
    #[error("pixel/sample conversion failed: {0}")]
    Convert(#[source] ffmpeg_next::Error),

    /// A frame was presented for conversion that does not match the geometry or
    /// format the conversion context was built for.
    #[error("frame does not match the conversion context: {0}")]
    FrameMismatch(&'static str),

    /// A [`multiview_core`] rational could not be represented as a
    /// libav `AVRational` (its `i32` numerator/denominator) — only possible for
    /// pathological values; real timebases fit.
    #[error("rational {num}/{den} does not fit an AVRational (i32/i32)")]
    Rational {
        /// The numerator that did not fit.
        num: i64,
        /// The denominator that did not fit.
        den: i64,
    },

    /// Allocating or initializing a hardware device / frames context failed.
    #[cfg(feature = "ffmpeg")]
    #[error("hardware-frame context operation failed: {0}")]
    HwContext(#[source] ffmpeg_next::Error),

    /// A requested hardware device type is not known to the linked `FFmpeg`
    /// build (name did not resolve to an `AVHWDeviceType`).
    #[error("unknown hardware device type {0:?}")]
    UnknownHwDevice(String),

    /// An embedded-CC decoder was requested for a caption channel the linked
    /// caption decoder cannot decode to text. In particular, the linked libav
    /// `cc_dec` decodes EIA-608 (fields CC1–CC4) but discards CEA-708 service
    /// blocks (DTVCC, `cc_type` 2/3) — so a 708 *service* selector would open a
    /// decoder that silently emits no cues forever, masking the unsupported form
    /// as "no captions". We refuse it up front so the caller can fall back to a
    /// 608 field, teletext, or a sidecar (captions.md §3 row 3, ADR-R007).
    #[error("CEA-708 service {service} is not decodable to text by the linked cc_dec (608 fields CC1–CC4 are supported); select a 608 field, teletext, or a sidecar instead")]
    UnsupportedCaptionChannel {
        /// The CEA-708 service number that was requested (1..=63).
        service: u8,
    },
}
