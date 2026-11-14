//! The engine seam of cast outputs (ADR-0111): what the control plane can
//! ask the running engine about its `kind = "cast"` outputs.
//!
//! The engine implements [`CastEngine`] over lock-free state; every call
//! returns at once and never waits on the data plane (invariant #10). This
//! module has no engine dependency: ids are config ids.

use std::path::PathBuf;
use std::sync::Arc;

/// One cast output of the running engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastOutputStatus {
    /// The output's config id.
    pub id: String,
    /// The directory its stream files are in while a cast is active.
    pub dir: PathBuf,
    /// A cast stream is active on this output.
    pub active: bool,
    /// The master playlist and enough segments for a receiver to start are
    /// published.
    pub ready: bool,
    /// The activation's generation (0 when inactive).
    pub generation: u64,
    /// Video segments published for this generation.
    pub video_segments: u64,
    /// Packets the output's sink did not receive since the run started.
    pub dropped_packets: u64,
    /// The last error, if any. An output whose directory is not usable
    /// carries the reason here and cannot be activated.
    pub last_error: Option<String>,
    /// Encoded picture width in pixels.
    pub width: u32,
    /// Encoded picture height in pixels.
    pub height: u32,
    /// Frame rate numerator.
    pub fps_num: u32,
    /// Frame rate denominator.
    pub fps_den: u32,
}

/// Why the engine refused a cast request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CastEngineError {
    /// No engine run is publishing cast outputs.
    #[error("the engine is not running")]
    NotRunning,
    /// The running engine has no cast output with this id.
    #[error("no cast output {0:?} in the running engine")]
    UnknownOutput(String),
    /// The output exists but cannot be used; the text names the fix.
    #[error("{0}")]
    Unavailable(String),
}

/// The running engine's cast outputs. Every method is non-blocking.
pub trait CastEngine: Send + Sync {
    /// The cast outputs of the current run, in configured order; empty when
    /// no run is active.
    fn outputs(&self) -> Vec<CastOutputStatus>;

    /// Activate the output `id` and return the activation's generation.
    /// Idempotent while active (the running generation is returned and
    /// `preroll` is ignored). With `preroll` the stream starts with the
    /// output's buffered last seconds, so a full playlist window is
    /// published at once; without, it starts at the next keyframe.
    ///
    /// # Errors
    /// [`CastEngineError`] when there is no run, no such output, or the
    /// output cannot be used.
    fn activate(&self, id: &str, preroll: bool) -> Result<u64, CastEngineError>;

    /// Deactivate the output `id` (idempotent): the engine stops writing and
    /// removes the stream files.
    ///
    /// # Errors
    /// [`CastEngineError`] when there is no run or no such output.
    fn deactivate(&self, id: &str) -> Result<(), CastEngineError>;
}

/// A shared [`CastEngine`].
pub type SharedCastEngine = Arc<dyn CastEngine>;
