//! Control-owned native load contract. CLI implements it; control never imports CLI.
use crate::{
    auth::Principal,
    error::{ControlError, ControlResult},
};
use serde::{Deserialize, Serialize};

/// Exact immutable completed capture selector; no path or owner override.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ArtifactRef {
    /// Actor-owned completed capture job.
    pub capture_job_id: u64,
    /// Exact artifact ID returned by that job.
    pub artifact_id: String,
}
/// Commit landing intent.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum LoadThen {
    /// Land cued (paused on the first frame).
    #[default]
    Cue,
    /// Land and resume playback.
    Resume,
}
/// Actual native facts, never acceptance-derived metadata.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NativeLoadAck {
    /// Operation id of the load.
    pub operation: u64,
    /// Load generation the player committed.
    pub generation: u64,
    /// Source id the player adopted.
    pub source_id: String,
    /// Probed duration in whole frames, if known.
    pub duration_frames: Option<u64>,
    /// Actual probed duration converted with the exact rational cadence.
    pub duration_ns: Option<i64>,
    /// Cadence numerator.
    pub cadence_num: i64,
    /// Cadence denominator.
    pub cadence_den: i64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Whether the clip carries adopted audio.
    pub has_audio: bool,
    /// Timeline position in nanoseconds.
    pub timeline_ns: i64,
}
/// Typed reason a load operation ended `failed`. Stable wire codes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum LoadFailureCode {
    /// The clip could not be found.
    NotFound,
    /// The container or its header could not be parsed.
    InvalidData,
    /// The open or prime deadline expired.
    Timeout,
    /// The clip frame rate is not the canvas frame rate.
    WrongCadence,
    /// The clip is variable frame rate.
    Vfr,
    /// The clip declares no frame rate.
    NoCadence,
    /// The clip carries alpha the player cannot decode (an unsupported alpha
    /// pixel format, or HEVC alpha, which decodes opaque).
    Alpha,
    /// The container has no video stream.
    NoVideoStream,
    /// The clip length is unknown and no out-point was declared.
    UnknownLength,
    /// The resolved playout window is invalid or exceeds a bound.
    InvalidWindow,
    /// The video decoder could not be opened.
    DecoderOpen,
    /// Priming produced no decodable frame.
    NoFrames,
    /// The clip carries audio but the player's audio rail is unavailable.
    AudioUnavailable,
    /// The durable binding journal refused the commit.
    JournalFailed,
    /// The load has no resolvable durable identity.
    ReferenceMissing,
    /// Any other failure.
    Other,
}

/// Upper bound on [`LoadOperation::detail`] in bytes.
pub const MAX_LOAD_DETAIL_BYTES: usize = 256;

/// Bounded operation view; completion needs native video and audio adoption.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LoadOperation {
    /// Operation id.
    pub operation: u64,
    /// Lifecycle state name.
    pub state: &'static str,
    /// Native facts once adopted.
    pub native: Option<NativeLoadAck>,
    /// Typed failure reason; present only when `state` is `failed`.
    pub reason: Option<LoadFailureCode>,
    /// Short operator-facing explanation of `reason`. Redacted: never carries
    /// a filesystem path, URL or credential; at most 256 bytes.
    pub detail: Option<String>,
}
/// Native-only admission, off-clock and after authorization.
pub trait PlayerLoadProvider: Send + Sync + 'static {
    /// Admit a load of `reference` onto `player` for `principal`.
    ///
    /// # Errors
    /// Returns a [`ControlError`] when the player is unknown, the caller is
    /// not authorised, or the engine cannot admit the load.
    fn load(
        &self,
        player: &str,
        principal: &Principal,
        key: &str,
        reference: ArtifactRef,
        then: LoadThen,
        lease: crate::web_capture::ArtifactLease,
    ) -> ControlResult<LoadOperation>;
    /// Read the state of a previously admitted load operation.
    ///
    /// # Errors
    /// Returns a [`ControlError`] when the player or operation is unknown or
    /// not visible to `principal`.
    fn operation(
        &self,
        player: &str,
        principal: &Principal,
        operation: u64,
    ) -> ControlResult<LoadOperation>;
}
/// Explicit unavailable result, never an accepted no-op.
pub struct UnavailablePlayerLoad;
impl PlayerLoadProvider for UnavailablePlayerLoad {
    fn load(
        &self,
        _: &str,
        _: &Principal,
        _: &str,
        _: ArtifactRef,
        _: LoadThen,
        _: crate::web_capture::ArtifactLease,
    ) -> ControlResult<LoadOperation> {
        Err(ControlError::EngineBusy)
    }
    fn operation(&self, _: &str, _: &Principal, _: u64) -> ControlResult<LoadOperation> {
        Err(ControlError::EngineBusy)
    }
}
