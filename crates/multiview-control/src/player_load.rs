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
    #[default]
    Cue,
    Resume,
}
/// Actual native facts, never acceptance-derived metadata.
#[derive(Debug, Clone, Serialize)]
pub struct NativeLoadAck {
    pub operation: u64,
    pub generation: u64,
    pub source_id: String,
    pub duration_frames: Option<u64>,
    /// Actual probed duration converted with the exact rational cadence.
    pub duration_ns: Option<i64>,
    pub cadence_num: i64,
    pub cadence_den: i64,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub timeline_ns: i64,
}
/// Bounded operation view; completion needs native video and audio adoption.
#[derive(Debug, Clone, Serialize)]
pub struct LoadOperation {
    pub operation: u64,
    pub state: &'static str,
    pub native: Option<NativeLoadAck>,
}
/// Native-only admission, off-clock and after authorization.
pub trait PlayerLoadProvider: Send + Sync + 'static {
    fn load(
        &self,
        player: &str,
        principal: &Principal,
        key: &str,
        reference: ArtifactRef,
        then: LoadThen,
        lease: multiview_webengine::artifact::Lease,
    ) -> ControlResult<LoadOperation>;
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
