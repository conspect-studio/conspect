//! The crate error taxonomy.
//!
//! `multiview-config` owns parsing (TOML/JSON), grid solving, and semantic
//! validation; each failure mode is a distinct [`ConfigError`] variant so
//! callers (and the CLI) can attribute a problem precisely. Conversions into
//! the workspace-wide [`multiview_core::Error`] are provided at the boundary.

use thiserror::Error;

/// A configuration parse, solve, or validation failure.
///
/// Marked `#[non_exhaustive]`: new variants may be added without a breaking
/// change, so downstream `match` statements must carry a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The TOML text could not be parsed into the schema.
    #[error("TOML parse error: {0}")]
    TomlParse(String),

    /// The document could not be serialized back to TOML.
    #[error("TOML serialize error: {0}")]
    TomlSerialize(String),

    /// The JSON text could not be parsed into the schema.
    #[error("JSON parse error: {0}")]
    JsonParse(String),

    /// The document could not be serialized to JSON.
    #[error("JSON serialize error: {0}")]
    JsonSerialize(String),

    /// A frame-rate string was not an exact `num/den` rational (invariant #3:
    /// never a float fps).
    #[error("invalid fps {value:?}: {reason}")]
    InvalidFps {
        /// The offending string as written in the document.
        value: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A CSS-grid track (`columns`/`rows` entry) could not be parsed.
    #[error("invalid grid track {value:?}: expected `<n>fr`, `<n>px`, or `<n>%`")]
    InvalidTrack {
        /// The offending track string.
        value: String,
    },

    /// The grid could not be solved (ragged area map, non-rectangular area,
    /// area referencing tracks that do not exist, etc.).
    #[error("grid solve error: {0}")]
    Grid(String),

    /// A semantic validation invariant was violated (dangling binding, unknown
    /// area, duplicate id, out-of-range geometry, unusable cadence, …).
    #[error("validation error: {0}")]
    Validation(String),

    /// An output's audio selection exceeds what its transport can actually
    /// deliver (ADR-R005 §4.2 capability matrix): e.g. selectable discrete
    /// tracks on NDI (channel-map only), or more discrete tracks than a legacy
    /// RTMP endpoint carries. Distinct from a generic [`ConfigError::Validation`]
    /// so the API/UI can attribute a *capability* refusal precisely and offer the
    /// honest degradation (fall back to the mixed program bus).
    #[error("audio capability error on output {output:?}: {reason}")]
    AudioCapability {
        /// The offending output's stable label (its kind + addressed endpoint).
        output: String,
        /// Why the transport cannot deliver the requested audio selection.
        reason: String,
    },
}

impl From<ConfigError> for multiview_core::Error {
    fn from(err: ConfigError) -> Self {
        multiview_core::Error::Config(err.to_string())
    }
}
