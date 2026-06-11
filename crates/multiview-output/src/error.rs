//! Error taxonomy for `multiview-output`.
//!
//! Output-side failures (playlist construction, fan-out routing, and — behind
//! feature flags — transport servers and muxers) surface through this per-crate
//! [`enum@Error`] enum. It converts into the workspace-wide [`multiview_core::Error`] at
//! the crate boundary so callers can treat all pipeline errors uniformly.
use thiserror::Error;

/// Convenient result alias used throughout `multiview-output`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the output stage.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A playlist could not be constructed because the inputs were structurally
    /// invalid (e.g. a Low-Latency tag was requested without a part target).
    #[error("playlist error: {0}")]
    Playlist(String),

    /// A fan-out routing/registration error (e.g. duplicate sink id under one
    /// rendition).
    #[error("fan-out error: {0}")]
    FanOut(String),

    /// A transport/serve failure (RTSP/HLS server, NDI, RTMP/SRT push).
    #[error("output error: {0}")]
    Output(String),

    /// A TSL UMD message could not be encoded to wire bytes (over-long or
    /// non-representable label, invalid display count, or size-ceiling overflow).
    /// Carries the underlying [`crate::tsl::TslError`].
    #[error("tsl encode: {0}")]
    Tsl(#[from] crate::tsl::TslError),

    /// An NDI output failure: the runtime license was not accepted, the NDI
    /// runtime could not be resolved/loaded, or the sender could not be created
    /// (behind the off-by-default `ndi` feature). Carries a human-readable detail.
    #[cfg(feature = "ndi")]
    #[error("ndi: {0}")]
    Ndi(String),
}

impl From<Error> for multiview_core::Error {
    fn from(value: Error) -> Self {
        // Every output-side failure maps onto the workspace-wide `Output` arm,
        // preserving the human-readable detail.
        multiview_core::Error::Output(value.to_string())
    }
}
