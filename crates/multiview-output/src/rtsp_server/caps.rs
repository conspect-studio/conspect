//! Codec → `appsrc` caps + RTP payloader selection for the in-process RTSP
//! server.
//!
//! The server feeds **already-encoded** NAL units into a `GStreamer` `appsrc`, then
//! `h264parse`/`h265parse` performs lightweight stream-format/alignment/SPS-PPS
//! fixups (no decode, no re-encode), then `rtph264pay`/`rtph265pay` packetizes to
//! RTP (core-engine §9.2). The mapping from the encoded codec to the right
//! `appsrc` caps string and payloader/parser element names is pure data — it is
//! always compiled and CI-testable without `GStreamer` (the actual element
//! construction lives behind the `rtsp-server` feature and consumes these
//! strings).

use thiserror::Error;

/// The video codec carried in the encoded NAL stream the RTSP server payloads.
///
/// Only the two RTSP-payloadable canvas codecs are modeled; an unsupported codec
/// is a typed refusal at server-build time, never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspCodec {
    /// H.264 / AVC (`appsrc → h264parse → rtph264pay`).
    H264,
    /// H.265 / HEVC (`appsrc → h265parse → rtph265pay`).
    H265,
}

impl RtspCodec {
    /// Resolve a codec from an `EncodeConfig` short codec name (e.g. `"h264"`,
    /// `"libx264"`, `"h264_nvenc"`, `"hevc"`, `"libx265"`, `"hevc_nvenc"`).
    ///
    /// # Errors
    ///
    /// Returns [`RtspCapsError::UnsupportedCodec`] when the codec name does not
    /// map to an RTSP-payloadable video codec (the RTSP server only payloads
    /// H.264/H.265 byte-streams; raw/intra-only codecs like `mpeg2video`/`mjpeg`
    /// are not RTSP renditions here).
    pub fn from_codec_name(name: &str) -> Result<Self, RtspCapsError> {
        let lower = name.to_ascii_lowercase();
        if lower.contains("h264") || lower.contains("x264") || lower.contains("avc") {
            Ok(Self::H264)
        } else if lower.contains("h265") || lower.contains("hevc") || lower.contains("x265") {
            Ok(Self::H265)
        } else {
            Err(RtspCapsError::UnsupportedCodec {
                codec: name.to_owned(),
            })
        }
    }

    /// The `appsrc` caps string for this codec's encoded byte-stream.
    ///
    /// `stream-format=byte-stream, alignment=au` matches the Annex-B AU-aligned
    /// output of the software/hardware encoders; `h264parse`/`h265parse`
    /// downstream perform any avc↔byte-stream / SPS-PPS fixups for the payloader.
    #[must_use]
    pub const fn appsrc_caps(self) -> &'static str {
        match self {
            Self::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            Self::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
        }
    }

    /// The `GStreamer` parser element name (lightweight fixups, **not** decode).
    #[must_use]
    pub const fn parser_element(self) -> &'static str {
        match self {
            Self::H264 => "h264parse",
            Self::H265 => "h265parse",
        }
    }

    /// The `GStreamer` RTP payloader element name.
    #[must_use]
    pub const fn payloader_element(self) -> &'static str {
        match self {
            Self::H264 => "rtph264pay",
            Self::H265 => "rtph265pay",
        }
    }

    /// The `gst-rtsp-server` launch description for the shared media factory.
    ///
    /// `appsrc name=src is-live=true format=time ! <parse> ! <pay> name=pay0
    /// config-interval=-1`: `is-live=true`/`format=time` mark a live timed source,
    /// `config-interval=-1` repeats SPS/PPS in-band so late-joining clients
    /// decode without waiting for the next IDR, and `pay0` is the payloader name
    /// `gst-rtsp-server` requires (core-engine §9.2). `set_shared(true)` on the
    /// factory (done at construction, not in this string) makes one encode fan to
    /// all clients (invariant #7).
    #[must_use]
    pub fn launch_description(self) -> String {
        format!(
            "( appsrc name=src is-live=true format=time ! {parse} ! {pay} name=pay0 config-interval=-1 )",
            parse = self.parser_element(),
            pay = self.payloader_element(),
        )
    }
}

/// Convert a non-negative `units` count in the `(num, den)`-seconds `timebase`
/// to **nanoseconds** (`ns = units * 1_000_000_000 * num / den`), for stamping a
/// `format=time` `appsrc` buffer (the RTSP server feed).
///
/// Returns `None` for a negative `units` (output timestamps are re-stamped from
/// the monotonic tick counter and are never negative), a zero denominator, or an
/// arithmetic overflow — never wraps and never panics. The intermediate product
/// is computed in `u128` so a 90 kHz timebase across hours of run time does not
/// overflow before the divide.
///
/// This is the pure seam of the server's buffer-timestamp path: it is always
/// compiled and CI-tested, so the `units → ns` contract is verified without the
/// `GStreamer` C stack.
#[must_use]
pub fn units_to_nanos(units: i64, timebase: (u32, u32)) -> Option<u64> {
    let (num, den) = timebase;
    if den == 0 {
        return None;
    }
    let units = u128::try_from(units).ok()?;
    let nanos = units
        .checked_mul(1_000_000_000)?
        .checked_mul(u128::from(num))?
        .checked_div(u128::from(den))?;
    u64::try_from(nanos).ok()
}

/// Why an RTSP codec/caps could not be resolved.
///
/// `#[non_exhaustive]`: downstream `match` must carry a wildcard.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspCapsError {
    /// The encoded codec is not an RTSP-payloadable video codec (only H.264/H.265
    /// are payloaded by the in-process RTSP server).
    #[error("codec `{codec}` is not RTSP-payloadable (only h264/h265)")]
    UnsupportedCodec {
        /// The offending codec name.
        codec: String,
    },
}

impl From<RtspCapsError> for crate::Error {
    fn from(value: RtspCapsError) -> Self {
        crate::Error::Output(value.to_string())
    }
}
