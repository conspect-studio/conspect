//! Actual background browser capture provider. No browser is started by default.
//! Only service-selected tools/templates enter this boundary; HTTP cannot submit HTML.
use crate::{
    auth::{authorize_object, Action, Principal},
    error::{ControlError, ControlResult},
};
use multiview_webengine::{
    capture::{Background, Capture, WindyView},
    service::{JobId, Request, Service, State, SubmitError, Submitted},
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

/// Windy terms version attached to this built-in template.
pub const WINDY_TERMS_VERSION: &str = "windy-2023-09-01";
/// One authorized, bounded public Windy view. Premium needs a configured secret resolver.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureBody {
    /// Fill authorization object.
    pub fill_id: String,
    /// Existing target-player authorization object, when supplied.
    pub target_player: Option<String>,
    /// Site parameters, never a URL or executable path.
    pub view: View,
    /// User acceptance checked against the current template version.
    pub terms: Terms,
}
/// Site-specific bounded view.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    /// Latitude.
    pub lat: f64,
    /// Longitude.
    pub lon: f64,
    /// Map zoom.
    pub zoom: u8,
    /// Radar or wind.
    pub overlay: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Capture cadence.
    pub fps: u32,
    /// Capture frame count.
    pub frames: u32,
    /// Explicitly use the public unauthenticated page. Default requests Premium.
    #[serde(default)]
    pub public: bool,
}
/// Explicit versioned site-terms acceptance.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Terms {
    /// Must be true.
    pub accepted: bool,
    /// Must match the built-in template version.
    pub version: String,
}
/// Public job status projection with no URL, credentials, document or disk path.
#[derive(Serialize)]
pub struct JobView {
    /// Stable opaque job number.
    pub job_id: u64,
    /// Authorized fill identity.
    pub fill_id: String,
    /// Queued, running, succeeded, failed, cancelled or uncertain.
    pub state: &'static str,
    /// Present only after the immutable artifact is committed.
    pub artifact_id: Option<String>,
    /// Estimated producer resources; GPU remains unknown.
    pub resources: Option<serde_json::Value>,
}
/// Shared provider; its service worker has no reference to the output clock.
#[derive(Clone, Default)]
pub struct Provider {
    service: Option<Arc<Service>>,
}
impl Provider {
    /// Inject a configured real service created at CLI startup.
    #[must_use]
    pub fn configured(service: Arc<Service>) -> Self {
        Self {
            service: Some(service),
        }
    }
    /// Honest runtime capabilities, including the disabled URL download boundary.
    #[must_use]
    pub fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({"capture_available":self.service.is_some(),"premium_available":false,"url_fetch_available":false,"url_fetch_reason":"enforced download boundary not provisioned","accounting":"process_tree_estimated","kernel_limits":false})
    }
    /// Authorize all named objects before allocating a background job.
    /// # Errors
    /// Rejects unauthorized objects, stale terms, unsupported Premium, or full service.
    pub fn submit(&self, p: &Principal, body: CaptureBody) -> ControlResult<u64> {
        p.role.require(Action::Write)?;
        authorize_object(p, &body.fill_id)?;
        if let Some(player) = &body.target_player {
            authorize_object(p, player)?;
        }
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
                overlay: body.view.overlay,
                terms_accepted: true,
                require_premium: false,
                credentials: None,
            }),
        };
        capture
            .validate()
            .map_err(|_| ControlError::Validation("invalid capture view".into()))?;
        let service = self.service.as_ref().ok_or(ControlError::EngineBusy)?;
        let request = Request {
            owner: p.key_id.clone(),
            fill: body.fill_id,
            capture,
            maximum_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(120),
        };
        match service.submit(request).map_err(submit_error)? {
            Submitted::Accepted(id) | Submitted::Coalesced(id) => Ok(id.raw()),
        }
    }
    /// Fetch an actor-owned job after object authorization.
    /// # Errors
    /// Another actor's job is indistinguishable from an unknown job.
    pub fn status(&self, p: &Principal, id: u64) -> ControlResult<JobView> {
        p.role.require(Action::Read)?;
        let service = self.service.as_ref().ok_or(ControlError::EngineBusy)?;
        let s = service
            .status(&p.key_id, JobId::from_raw(id))
            .ok_or_else(|| missing(id))?;
        authorize_object(p, &s.fill)?;
        let (state, artifact) = match s.state {
            State::Queued => ("queued", None),
            State::Running => ("running", None),
            State::Succeeded { artifact } => ("succeeded", Some(artifact.as_str().to_owned())),
            State::Failed { .. } => ("failed", None),
            State::Cancelled => ("cancelled", None),
            State::Uncertain => ("uncertain", None),
        };
        Ok(JobView {
            job_id: id,
            fill_id: s.fill,
            state,
            artifact_id: artifact,
            resources: s.resource.and_then(|r| serde_json::to_value(r).ok()),
        })
    }
    /// Obtain an immutable artifact lease after authenticating the owning job.
    /// Blocking hash verification belongs on a management blocking worker.
    /// # Errors
    /// Rejects unknown, cross-owner, unready or unavailable artifacts.
    pub fn pin(
        &self,
        p: &Principal,
        id: u64,
    ) -> ControlResult<multiview_webengine::artifact::Lease> {
        let job = self.status(p, id)?;
        let artifact = job
            .artifact_id
            .ok_or_else(|| ControlError::Conflict("capture has no completed artifact".into()))?;
        let artifact = multiview_webengine::artifact::ArtifactId::parse(&artifact)
            .map_err(|_| ControlError::Repository("invalid capture artifact identity".into()))?;
        self.service
            .as_ref()
            .ok_or(ControlError::EngineBusy)?
            .pin(&artifact, &p.key_id)
            .map_err(|_| ControlError::EngineBusy)
    }

    /// Cancel an actor-owned, authorized job.
    /// # Errors
    /// Requires write permission on the job's fill.
    pub fn cancel(&self, p: &Principal, id: u64) -> ControlResult<()> {
        p.role.require(Action::Write)?;
        let _ = self.status(p, id)?;
        self.service
            .as_ref()
            .ok_or(ControlError::EngineBusy)?
            .cancel(&p.key_id, JobId::from_raw(id));
        Ok(())
    }
}
fn missing(id: u64) -> ControlError {
    ControlError::NotFound {
        kind: "capture-job",
        id: id.to_string(),
    }
}
fn submit_error(error: SubmitError) -> ControlError {
    match error {
        SubmitError::Invalid => ControlError::Validation("invalid capture request".into()),
        SubmitError::Full | SubmitError::ShuttingDown => ControlError::EngineBusy,
        SubmitError::Persistence => {
            ControlError::Repository("capture intent could not be persisted".into())
        }
    }
}
