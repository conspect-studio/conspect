//! Web capture on platforms without the browser host (Windows). The capture
//! runtime (`multiview-webengine`) needs unix process groups and private
//! `openat`-pinned files, so it is not built here. The HTTP surface stays the
//! same and reports capture as unavailable: capabilities say so, and every job
//! operation answers as an unconfigured provider does on unix.
use super::{Admission, CancelResult, CaptureBody, JobView, ARTIFACT_STREAMS, WINDY_TERMS_VERSION};
use crate::{
    auth::{authorize_object, Action, Principal},
    error::{ControlError, ControlResult},
};
use multiview_webengine::capture::{Background, Capture, WindyView};
use tokio::sync::OwnedSemaphorePermit;

/// The capture provider: never configured on this platform.
#[derive(Clone, Default)]
pub struct Provider;

/// Stand-in for the artifact lease type; no value is ever constructed.
pub enum ArtifactLease {}

/// Artifact metadata the route reads from a lease.
pub struct ArtifactInfo {
    /// Artifact size in bytes.
    pub bytes: u64,
}

impl ArtifactLease {
    /// Artifact metadata (unreachable: no lease exists on this platform).
    #[must_use]
    pub fn artifact(&self) -> &ArtifactInfo {
        match *self {}
    }
}

impl std::io::Read for ArtifactLease {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        match *self {}
    }
}

impl Provider {
    /// Honest runtime capabilities: no capture on this platform.
    #[must_use]
    pub fn capabilities(&self) -> serde_json::Value {
        let _ = ARTIFACT_STREAMS;
        serde_json::json!({"capture_available":false,"capture_unavailable_reason":"browser capture is not supported on this platform","premium_available":false,"url_fetch_available":false,"url_fetch_reason":"enforced download boundary not provisioned","accounting":"unavailable","kernel_limits":false})
    }

    /// Authorize, then refuse: no capture service on this platform.
    /// # Errors
    /// Unauthorized objects, or [`ControlError::EngineBusy`].
    pub fn submit(
        &self,
        p: &Principal,
        _key: &str,
        body: &CaptureBody,
    ) -> ControlResult<Admission> {
        p.role.require(Action::Write)?;
        authorize_object(p, &body.fill_id)?;
        if let Some(player) = &body.target_player {
            authorize_object(p, player)?;
        }
        // Same validation, in the same order, as the unix provider, so a bad
        // request gets the same answer on every platform.
        if !body.terms.accepted || body.terms.version != WINDY_TERMS_VERSION {
            return Err(ControlError::Validation(
                "current Windy terms acceptance required".into(),
            ));
        }
        if !body.view.public {
            return Err(ControlError::Conflict("authenticated Windy capture is unavailable until the account resolver is configured".into()));
        }
        let capture = Capture {
            width: body.view.width,
            height: body.view.height,
            fps: body.view.fps,
            frames: body.view.frames,
            document: String::new(),
            background: Background::Opaque,
            windy: Some(WindyView {
                lat: body.view.lat,
                lon: body.view.lon,
                zoom: body.view.zoom,
                overlay: body.view.overlay.clone(),
                terms_accepted: true,
                require_premium: false,
                credentials: None,
            }),
        };
        capture
            .validate()
            .map_err(|_| ControlError::Validation("invalid capture view".into()))?;
        Err(ControlError::EngineBusy)
    }

    /// No jobs exist.
    /// # Errors
    /// Missing role, or [`ControlError::EngineBusy`] (no service).
    pub fn status(&self, p: &Principal, _id: u64) -> ControlResult<JobView> {
        p.role.require(Action::Read)?;
        Err(ControlError::EngineBusy)
    }

    /// No artifacts exist.
    /// # Errors
    /// Always: as [`status`](Self::status).
    pub fn pin(
        &self,
        p: &Principal,
        id: u64,
    ) -> ControlResult<(ArtifactLease, OwnedSemaphorePermit)> {
        let _ = self.status(p, id)?;
        Err(ControlError::EngineBusy)
    }

    /// No artifacts exist, so nothing can be adopted by a native player.
    /// # Errors
    /// Missing role, or [`ControlError::EngineBusy`].
    pub fn pin_for_player(
        &self,
        p: &Principal,
        job_id: u64,
        _artifact_id: &str,
    ) -> ControlResult<ArtifactLease> {
        p.role.require(Action::Write)?;
        let _ = self.status(p, job_id)?;
        Err(ControlError::EngineBusy)
    }

    /// No jobs exist.
    /// # Errors
    /// Always: as [`status`](Self::status).
    pub fn cancel(&self, p: &Principal, _key: &str, id: u64) -> ControlResult<CancelResult> {
        p.role.require(Action::Write)?;
        let _ = self.status(p, id)?;
        Err(ControlError::EngineBusy)
    }
}
