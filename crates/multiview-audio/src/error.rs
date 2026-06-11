//! Error taxonomy for [`multiview-audio`](crate).
use thiserror::Error;

/// Result alias for the audio crate.
pub type Result<T> = core::result::Result<T, AudioError>;

/// Errors produced by the audio mix/route model and loudness metering.
///
/// `#[non_exhaustive]` so new arms can be added without a breaking change;
/// downstream `match` statements must carry a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AudioError {
    /// A PCM block's sample count is not a whole number of frames for its
    /// channel layout (interleaved samples must be a multiple of the channel
    /// count).
    #[error("ragged audio block: {samples} samples is not a multiple of {channels} channels")]
    RaggedBlock {
        /// Number of interleaved samples supplied.
        samples: usize,
        /// Channel count implied by the format.
        channels: usize,
    },

    /// A block submitted to the mixer does not match the mixer's working
    /// format.
    #[error(
        "audio format mismatch: expected {expected_rate} Hz / {expected_channels} ch, \
         got {actual_rate} Hz / {actual_channels} ch"
    )]
    FormatMismatch {
        /// Sample rate the mixer expects.
        expected_rate: u32,
        /// Channel count the mixer expects.
        expected_channels: usize,
        /// Sample rate of the offending block.
        actual_rate: u32,
        /// Channel count of the offending block.
        actual_channels: usize,
    },

    /// A route or submission referenced an input id the mixer does not know.
    #[error("unknown mixer input id: {0}")]
    UnknownInput(usize),

    /// A meter was constructed with an unusable audio format (e.g. zero sample
    /// rate or zero channels).
    #[error("invalid audio format: {0}")]
    InvalidFormat(&'static str),

    /// A real audio decode/resample (behind the `ffmpeg` feature) failed. The
    /// underlying libav error is flattened to a string so this error — and the
    /// whole audio crate's public surface — never names a binding type.
    #[cfg(feature = "ffmpeg")]
    #[error("audio decode failed: {0}")]
    Decode(String),
}

impl From<AudioError> for multiview_core::Error {
    /// Fold an audio fault into the workspace taxonomy. The audio stage is
    /// first-class alongside video decode/encode, so it routes to the dedicated
    /// [`multiview_core::Error::Audio`] arm rather than being flattened into
    /// [`multiview_core::Error::Config`]; the human-readable detail is preserved.
    fn from(value: AudioError) -> Self {
        multiview_core::Error::Audio(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::AudioError;

    /// The crate boundary folds every audio fault into the workspace-wide
    /// [`multiview_core::Error::Audio`] arm — the first-class audio stage owns a
    /// dedicated arm rather than being flattened into
    /// [`multiview_core::Error::Config`]. Guards the routing against regression
    /// to `Config` and proves the human-readable detail survives.
    #[test]
    fn audio_error_routes_to_core_audio_arm() {
        let err: multiview_core::Error = AudioError::UnknownInput(7).into();
        match err {
            multiview_core::Error::Audio(msg) => {
                assert!(
                    msg.contains("unknown mixer input id: 7"),
                    "detail must survive the conversion, got: {msg}"
                );
            }
            other => panic!("expected Error::Audio, got {other:?}"),
        }
    }

    /// A second, structurally different audio fault also lands on the `Audio`
    /// arm (not just one hand-picked variant), so the whole `From` impl — not a
    /// special case — is rerouted.
    #[test]
    fn ragged_block_also_routes_to_audio_arm() {
        let err: multiview_core::Error = AudioError::RaggedBlock {
            samples: 3,
            channels: 2,
        }
        .into();
        assert!(
            matches!(err, multiview_core::Error::Audio(_)),
            "expected Error::Audio, got {err:?}"
        );
    }
}
