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
        lease: multiview_webengine::artifact::Lease,
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
        _: multiview_webengine::artifact::Lease,
    ) -> ControlResult<LoadOperation> {
        Err(ControlError::EngineBusy)
    }
    fn operation(&self, _: &str, _: &Principal, _: u64) -> ControlResult<LoadOperation> {
        Err(ControlError::EngineBusy)
    }
}
