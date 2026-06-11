//! Pipeline-stage traits and the shared enums every stage crate depends on.
//!
//! These contracts are pure-Rust and **object-safe** where it makes sense
//! (`Backend`, `Source`, `Sink`), so a heterogeneous registry can hold
//! `Box<dyn Trait>` values without naming any native/backend type. Native
//! surfaces (CUDA pointers, `IOSurface`, wgpu textures) never appear here — the
//! feature-gated crates carry those.

/// The concrete kind of a backend implementing a pipeline stage.
///
/// Shared across crates so the HAL/planner and telemetry can describe an
/// assignment without depending on any feature-gated backend crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BackendKind {
    /// Pure-CPU / SIMD software path (always available; the universal fallback).
    #[default]
    Software,
    /// NVIDIA CUDA (NVDEC/NVENC + custom CUDA compositor).
    Cuda,
    /// Apple `VideoToolbox` (+ Metal).
    VideoToolbox,
    /// Linux VA-API (Intel/AMD).
    Vaapi,
    /// Intel Quick Sync via oneVPL.
    Qsv,
    /// Portable wgpu compositor backend.
    Wgpu,
    /// Apple Metal compositor backend.
    Metal,
}

/// The lifecycle state of an input source / tile.
///
/// Tiles ride this state machine (invariant #2):
/// `Live -> Stale -> Reconnecting -> NoSignal`. A freshly declared, not-yet
/// connected source starts at [`SourceState::NoSignal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SourceState {
    /// Delivering fresh frames within the staleness window.
    Live,
    /// No fresh frame recently; compositor holds the last-good frame.
    Stale,
    /// The supervisor is re-establishing the connection.
    Reconnecting,
    /// No usable signal; the compositor renders a placeholder card.
    #[default]
    NoSignal,
}

impl SourceState {
    /// Whether the source is currently [`SourceState::Live`].
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// A hardware/software backend implementing a pipeline stage.
///
/// Object-safe: backends are typically held as `Box<dyn Backend>` in a
/// registry.
pub trait Backend {
    /// Human-readable backend name (e.g. `"nvenc"`, `"videotoolbox"`,
    /// `"software"`).
    fn name(&self) -> &str;

    /// The concrete [`BackendKind`] this backend belongs to.
    fn kind(&self) -> BackendKind;
}

/// A live input source.
///
/// Object-safe so a supervisor can manage `Box<dyn Source>` values uniformly.
pub trait Source {
    /// Stable source identifier (used to bind a source to a layout cell).
    fn id(&self) -> &str;

    /// The source's current lifecycle [`SourceState`].
    fn state(&self) -> SourceState;
}

/// An output sink/publisher (RTSP server, HLS packager, NDI out, push, …).
///
/// Object-safe.
pub trait Sink {
    /// Stable sink identifier.
    fn id(&self) -> &str;
}

/// A decode stage: turns coded packets into frames on the internal timeline.
///
/// Backend-specific frame storage is owned by the implementing crate; only the
/// pure-Rust [`crate::frame::FrameMeta`] crosses this boundary.
pub trait Decoder {
    /// The backend kind providing this decoder.
    fn kind(&self) -> BackendKind;
}

/// An encode stage: turns composited canvas frames into coded packets.
pub trait Encoder {
    /// The backend kind providing this encoder.
    fn kind(&self) -> BackendKind;
}

/// Composites tiles into the output canvas.
pub trait Compositor {
    /// The backend kind providing this compositor.
    fn kind(&self) -> BackendKind;
}
