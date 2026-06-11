//! Error taxonomy for the overlay subsystem.
//!
//! Pure-Rust, allocation-light, and convertible into the workspace-wide
//! [`multiview_core::Error`] at this crate's boundary so callers can fold overlay
//! failures into the engine's [`multiview_core::Error::Config`] /
//! [`multiview_core::Error::Compositor`] arms.

use thiserror::Error;

/// Result alias used throughout `multiview-overlay`.
pub type Result<T> = core::result::Result<T, Error>;

/// An overlay-model or layout-resolution error.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A normalized rectangle (placement region or tile target) is malformed:
    /// out of the `0.0..=1.0` range, non-positive extent, or non-finite.
    #[error("invalid rect: {0}")]
    InvalidRect(String),

    /// Two layers in the same stack share an `id` (ids must be unique so the
    /// compositor and the management API can address a layer unambiguously).
    #[error("duplicate layer id: {0}")]
    DuplicateLayerId(String),

    /// A canvas dimension was zero (cannot resolve normalized rects to pixels).
    #[error("invalid canvas: {0}")]
    InvalidCanvas(String),

    /// A confidence scope was fed malformed sample data: a sample-plane length
    /// that does not match the declared dimensions, mismatched parallel planes,
    /// or interleaved data of the wrong stride.
    #[error("invalid scope: {0}")]
    InvalidScope(String),

    /// A UMD field was addressed by an out-of-range index.
    #[error("umd field index out of range: {0}")]
    FieldIndex(usize),

    /// A round-robin / timer parameter was non-positive where a positive value
    /// is required (e.g. zero pages, or a zero dwell/period that would divide by
    /// zero).
    #[error("invalid timer parameter: {0}")]
    InvalidTimer(String),

    /// A subtitle/caption document (SRT/VTT) was malformed: a bad timing line,
    /// an out-of-order cue window, or an unparseable timestamp.
    #[error("invalid subtitle: {0}")]
    InvalidSubtitle(String),
}

impl From<Error> for multiview_core::Error {
    /// Fold an overlay error into the workspace taxonomy. Every overlay error
    /// is a configuration/validation fault, so it maps to
    /// [`multiview_core::Error::Config`].
    fn from(value: Error) -> Self {
        multiview_core::Error::Config(value.to_string())
    }
}
