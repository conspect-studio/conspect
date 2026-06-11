//! Error taxonomy for the compositor crate.
//!
//! The compositor exposes a single [`enum@Error`] enum (per-crate `thiserror`); it
//! converts into the workspace-wide [`multiview_core::Error`] at the crate
//! boundary via [`From`], so callers can propagate compositor failures with
//! `?` into the shared taxonomy.

use thiserror::Error;

/// Result alias for fallible compositor operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors produced by the CPU reference compositor and its color math.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A color axis was left [`multiview_core::color::ColorPrimaries::Unspecified`]
    /// (or the transfer/matrix/range equivalent) when a resolved tuple was
    /// required. The detection step ([`multiview_core::color::ColorInfo::resolve_defaults`])
    /// must run first so the kernel never sees an unspecified axis.
    #[error("unresolved color axis: {0}")]
    UnresolvedColor(&'static str),

    /// A transfer characteristic that has no closed-form linearization in the
    /// CPU reference path (e.g. a `#[non_exhaustive]` variant added upstream).
    #[error("unsupported transfer characteristic: {0:?}")]
    UnsupportedTransfer(multiview_core::color::TransferCharacteristic),

    /// A matrix-coefficients value that the CPU reference path cannot realize
    /// as a YUV<->RGB matrix.
    #[error("unsupported matrix coefficients: {0:?}")]
    UnsupportedMatrix(multiview_core::color::MatrixCoefficients),

    /// A primaries value with no supported gamut-conversion matrix.
    #[error("unsupported primaries: {0:?}")]
    UnsupportedPrimaries(multiview_core::color::ColorPrimaries),

    /// A supplied buffer did not match the geometry/format it was declared with
    /// (e.g. an NV12 plane shorter than `width * height`).
    #[error("buffer geometry mismatch: {0}")]
    Geometry(String),

    /// No usable GPU adapter could be acquired (headless/GPU-free environment).
    ///
    /// This is the **graceful-degradation** signal: the wgpu backend returns it
    /// instead of panicking when there is no `/dev/dri`/Vulkan/Metal device, so
    /// callers (and tests) can fall back to the CPU reference or skip.
    #[cfg(feature = "wgpu")]
    #[error("no usable GPU adapter available: {0}")]
    NoAdapter(String),

    /// A wgpu device/queue could not be requested from an otherwise-valid
    /// adapter.
    #[cfg(feature = "wgpu")]
    #[error("failed to request GPU device: {0}")]
    DeviceRequest(String),

    /// A WGSL shader source failed to parse with `naga`.
    #[cfg(feature = "wgpu")]
    #[error("shader parse error: {0}")]
    ShaderParse(String),

    /// A WGSL shader parsed but failed `naga` validation.
    #[cfg(feature = "wgpu")]
    #[error("shader validation error: {0}")]
    ShaderValidation(String),

    /// A GPU operation (buffer map, submission, readback) failed at runtime.
    #[cfg(feature = "wgpu")]
    #[error("GPU runtime error: {0}")]
    GpuRuntime(String),

    /// The composite request exceeded a fixed GPU resource limit (e.g. more
    /// tiles than the bound tile-array / storage buffer was sized for).
    #[cfg(feature = "wgpu")]
    #[error("GPU limit exceeded: {0}")]
    GpuLimit(String),

    /// A bundled OFL font failed to load into the overlay text engine's font
    /// database (corrupt embedded asset or an unparseable face).
    #[cfg(feature = "overlay")]
    #[error("overlay font load failed: {0}")]
    FontLoad(String),

    /// A single glyph's rasterized coverage box was larger than the overlay
    /// atlas could ever hold (even when empty) — i.e. it exceeds the byte cap on
    /// its own, so no eviction can make room. The caller should hold last-good
    /// or skip the layer rather than crash (hot-path safety rule #3).
    #[cfg(feature = "overlay")]
    #[error("overlay glyph too large for atlas: {0}")]
    AtlasGlyphTooLarge(String),
}

impl From<Error> for multiview_core::Error {
    fn from(value: Error) -> Self {
        multiview_core::Error::Compositor(value.to_string())
    }
}
