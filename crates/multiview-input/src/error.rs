//! Error taxonomy for the input subsystem.
//!
//! `multiview-input` owns ingest, timestamp normalization, jitter buffering,
//! wall-clock pacing, and supervised reconnect. Fallible operations here return
//! [`Result`]; the [`enum@Error`] variants convert into the workspace-wide
//! [`multiview_core::Error::Input`] arm at the crate boundary.
use thiserror::Error;

/// Convenient result alias for the input subsystem.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors raised by the input timing/resilience logic.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A timebase or cadence rational was not usable (zero/degenerate
    /// denominator). Timestamp math cannot proceed.
    #[error("invalid timebase: {0}")]
    InvalidTimebase(&'static str),

    /// The normalizer has not yet seen its anchoring (first) frame, so the
    /// requested operation has no reference point.
    #[error("normalizer not yet anchored")]
    NotAnchored,

    /// The pacer has not yet seen its anchoring (first) frame, so no release
    /// deadline can be computed.
    #[error("pacer not yet anchored")]
    PacerNotAnchored,

    /// A configuration value was rejected during construction or validation.
    #[error("invalid input configuration: {0}")]
    InvalidConfig(&'static str),

    /// A real-ingest demux/decode operation faulted. The supervisor treats this
    /// as a connection fault and applies the reconnect backoff rather than
    /// crashing the engine. Only constructed when the `ffmpeg` feature is on; the
    /// detail string carries the underlying `multiview-ffmpeg` error so the pure-Rust
    /// error surface does not name a feature-gated type.
    #[error("ingest fault: {0}")]
    Ingest(String),

    /// A TSL UMD wire packet could not be decoded (framing, checksum, length, or
    /// out-of-range tally code). Carries the underlying [`crate::tsl::TslError`].
    #[error("tsl decode: {0}")]
    Tsl(#[from] crate::tsl::TslError),

    /// An SMPTE ST 2110 RTP packet could not be depacketized (RTP header, -20,
    /// -30, or -40). Carries the underlying [`crate::st2110::St2110Error`].
    #[error("st2110 depacketize: {0}")]
    St2110(#[from] crate::st2110::St2110Error),

    /// An SMPTE ST 2022-6 HBRMT payload could not be parsed. Carries the
    /// underlying [`crate::st2022_6::St2022_6Error`].
    #[error("st2022-6 parse: {0}")]
    St2022_6(#[from] crate::st2022_6::St2022_6Error),

    /// An MPEG-2 Transport Stream PSI/SI section could not be parsed. Carries the
    /// underlying [`crate::mpegts::MpegTsError`].
    #[error("mpeg-ts psi/si: {0}")]
    MpegTs(#[from] crate::mpegts::MpegTsError),

    /// A SCTE-35 / SCTE-104 cue message could not be parsed. Carries the
    /// underlying [`crate::scte::ScteError`].
    #[error("scte cue: {0}")]
    Scte(#[from] crate::scte::ScteError),

    /// An MPEG-DASH MPD manifest could not be parsed or a segment could not be
    /// selected. Carries the underlying [`crate::dash::DashError`].
    #[error("dash: {0}")]
    Dash(#[from] crate::dash::DashError),

    /// An SRT connection-parameter / stream-id value was rejected. Carries the
    /// underlying [`crate::srt::SrtError`].
    #[error("srt: {0}")]
    Srt(#[from] crate::srt::SrtError),

    /// An NDI received frame could not be converted to NV12 (geometry, stride, or
    /// an unsupported `FourCC`). Carries the underlying
    /// [`crate::ndi::NdiConvertError`]. Only constructed under the `ndi` feature.
    #[cfg(feature = "ndi")]
    #[error("ndi convert: {0}")]
    NdiConvert(#[from] crate::ndi::NdiConvertError),

    /// An NDI receive faulted (connect failed / source disconnected). Carries the
    /// underlying [`crate::ndi::NdiRecvError`]. The supervisor treats this as a
    /// connection fault and reconnects. Only constructed under the `ndi` feature.
    #[cfg(feature = "ndi")]
    #[error("ndi receive: {0}")]
    NdiRecv(#[from] crate::ndi::NdiRecvError),
}

#[cfg(feature = "ffmpeg")]
impl From<multiview_ffmpeg::FfmpegError> for Error {
    fn from(value: multiview_ffmpeg::FfmpegError) -> Self {
        // Flatten to a string so the (always-compiled) `Error` enum never carries
        // a feature-gated libav type in its public shape.
        Error::Ingest(value.to_string())
    }
}

impl From<Error> for multiview_core::Error {
    fn from(value: Error) -> Self {
        multiview_core::Error::Input(value.to_string())
    }
}
