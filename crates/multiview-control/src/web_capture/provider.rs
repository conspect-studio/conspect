//! The real capture provider (unix: the browser host needs process groups and
//! private `openat`-pinned files).
use super::{Admission, CancelResult, CaptureBody, JobView, ARTIFACT_STREAMS, WINDY_TERMS_VERSION};
use crate::web_capture_store::{digest_hex, Ledger, Reserve, TermsAcceptance, TermsRepository};
use crate::{
    auth::{authorize_object, Action, Principal},
    error::{ControlError, ControlResult},
};
use multiview_webengine::{
    capture::{Background, Capture, OutputCadence, WindyView},
    service::{CancelOutcome, JobId, Request, Service, State, SubmitError, Submitted},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Shared provider; its service worker has no reference to the output clock.
#[derive(Clone)]
pub struct Provider {
    service: Option<Arc<Service>>,
    terms: Option<Arc<dyn TermsRepository>>,
    ledger: Arc<Ledger>,
    streams: Arc<Semaphore>,
    pin_requests: Arc<Semaphore>,
    /// The running canvas cadence every artifact is conformed to, so a media
    /// player (which accepts only the exact canvas rate) can load it.
    output_cadence: Option<OutputCadence>,
}
impl Default for Provider {
    fn default() -> Self {
        Self {
            service: None,
            terms: None,
            ledger: Arc::new(Ledger::memory()),
            streams: Arc::new(Semaphore::new(ARTIFACT_STREAMS)),
            pin_requests: Arc::new(Semaphore::new(ARTIFACT_STREAMS)),
            output_cadence: None,
        }
    }
}
impl Provider {
    /// Inject a configured real service plus its private terms and idempotency stores.
    #[must_use]
    pub fn configured(
        service: Arc<Service>,
        terms: Arc<dyn TermsRepository>,
        ledger: Arc<Ledger>,
    ) -> Self {
        Self {
            service: Some(service),
            terms: Some(terms),
            ledger,
            streams: Arc::new(Semaphore::new(ARTIFACT_STREAMS)),
            pin_requests: Arc::new(Semaphore::new(ARTIFACT_STREAMS)),
            output_cadence: None,
        }
    }
    /// Conform every capture to the engine's canvas cadence `num/den` (exact
    /// rational). An out-of-range rate leaves captures unconformed, which a
    /// `target_player` submit then refuses rather than producing an artifact
    /// the player cannot load.
    #[must_use]
    pub fn with_canvas_cadence(mut self, num: i64, den: i64) -> Self {
        self.output_cadence = canvas_output_cadence(num, den);
        self
    }
    /// The conform cadence captures are encoded at, if known.
    #[must_use]
    pub const fn output_cadence(&self) -> Option<OutputCadence> {
        self.output_cadence
    }
    /// Honest runtime capabilities, including the disabled URL download boundary.
    #[must_use]
    pub fn capabilities(&self) -> serde_json::Value {
        let accounting = match multiview_webengine::service::Accounting::current() {
            multiview_webengine::service::Accounting::Unavailable => "unavailable",
            multiview_webengine::service::Accounting::ProcessTreeEstimated => {
                "process_tree_estimated"
            }
        };
        serde_json::json!({"capture_available":self.service.is_some(),"premium_available":false,"url_fetch_available":false,"url_fetch_reason":"enforced download boundary not provisioned","accounting":accounting,"kernel_limits":false})
    }
    /// Authorize all named objects before allocating a background job. Blocking.
    /// # Errors
    /// Rejects unauthorized objects, stale terms, unsupported Premium, divergent
    /// idempotent replays, or full service.
    pub fn submit(&self, p: &Principal, key: &str, body: &CaptureBody) -> ControlResult<Admission> {
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
                overlay: body.view.overlay.clone(),
                terms_accepted: true,
                require_premium: false,
                credentials: None,
            }),
            capture: body.view.capture_options(),
            output_cadence: self.output_cadence,
        };
        if body.target_player.is_some() && self.output_cadence.is_none() {
            // A player accepts only the exact canvas rate: an unconformed
            // artifact would be admitted, captured and then refused at load.
            return Err(ControlError::Conflict(
                "player-targeted capture needs the canvas cadence, which this run does not know"
                    .into(),
            ));
        }
        capture
            .validate()
            .map_err(|_| ControlError::Validation("invalid capture view".into()))?;
        let service = self.service.as_ref().ok_or(ControlError::EngineBusy)?;
        let terms = self.terms.as_ref().ok_or(ControlError::EngineBusy)?;
        let digest = digest_hex(
            &serde_json::to_vec(&body)
                .map_err(|_| ControlError::Validation("capture body not serializable".into()))?,
        );
        let token = correlation_token(&p.key_id, "submit", key, &digest);
        match self.ledger.reserve(&p.key_id, "submit", key, &digest) {
            Ok(Reserve::Replay(id)) => {
                return Ok(Admission {
                    job_id: id,
                    fresh: false,
                })
            }
            Ok(Reserve::Fresh) => {}
            Err(true) => {
                return Err(ControlError::Conflict(
                    "Idempotency-Key reused with a different request".into(),
                ))
            }
            Err(false) => {
                // A pending receipt may hide an exact job: the journaled
                // correlation (token hash only) binds it. Completion records
                // the id; the capture is never re-run. Unknown is NOT proof
                // that nothing started, so it stays pending (fail closed).
                if let Some(id) = correlated_job(service, &p.key_id, &body.fill_id, &token) {
                    self.ledger
                        .complete(&p.key_id, "submit", key, id)
                        .map_err(|_| {
                            ControlError::Repository(
                                "idempotency record could not be persisted".into(),
                            )
                        })?;
                    return Ok(Admission {
                        job_id: id,
                        fresh: false,
                    });
                }
                if self.ledger.outcome_uncertain(&p.key_id, "submit", key) {
                    return Err(ControlError::CaptureOutcomeUncertain);
                }
                return Err(ControlError::Conflict(
                    "Idempotency-Key request already in progress".into(),
                ));
            }
        }
        // Keep the durable pending receipt on any indeterminate outcome. The
        // service may already have accepted a job even if the final receipt
        // fsync failed. A retry must not allocate a second browser job. Only a
        // definite pre-acceptance failure durably abandons the reservation.
        match self.admit(p, key, service, terms.as_ref(), (body, &token), capture) {
            Ok(admission) => Ok(admission),
            Err(AdmitError::Uncertain(e)) => Err(e),
            Err(AdmitError::Definite(e)) => {
                self.ledger.abandon(&p.key_id, "submit", key).map_err(|_| {
                    ControlError::Repository("idempotency reservation could not be released".into())
                })?;
                Err(e)
            }
        }
    }
    fn admit(
        &self,
        p: &Principal,
        key: &str,
        service: &Service,
        terms: &dyn TermsRepository,
        (body, token): (&CaptureBody, &str),
        capture: Capture,
    ) -> Result<Admission, AdmitError> {
        let accepted_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        terms
            .record(&TermsAcceptance {
                key_id: p.key_id.clone(),
                fill: body.fill_id.clone(),
                version: body.terms.version.clone(),
                accepted_at_unix,
            })
            .map_err(|_| {
                AdmitError::Definite(ControlError::Repository(
                    "terms acceptance could not be persisted".into(),
                ))
            })?;
        let request = Request {
            owner: p.key_id.clone(),
            fill: body.fill_id.clone(),
            capture,
            maximum_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(120),
        };
        let id = match service.submit_correlated(request, token) {
            Ok(Submitted::Accepted(id) | Submitted::Coalesced(id)) => id.raw(),
            Err(e @ SubmitError::Persistence) => {
                // The journal may already hold the correlated intent: bind it.
                match correlated_job(service, &p.key_id, &body.fill_id, token) {
                    Some(id) => id,
                    None => return Err(AdmitError::Uncertain(submit_error(e))),
                }
            }
            Err(e) => return Err(AdmitError::Definite(submit_error(e))),
        };
        self.ledger
            .complete(&p.key_id, "submit", key, id)
            .map_err(|_| {
                AdmitError::Uncertain(ControlError::Repository(
                    "idempotency record could not be persisted".into(),
                ))
            })?;
        Ok(Admission {
            job_id: id,
            fresh: true,
        })
    }
    fn release(&self, actor: &str, op: &str, key: &str) -> ControlResult<()> {
        self.ledger.abandon(actor, op, key).map_err(|_| {
            ControlError::Repository("idempotency reservation could not be released".into())
        })
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
    /// The returned permit bounds concurrent streams and must live as long as the reader.
    /// Blocking hash verification belongs on a management blocking worker.
    /// # Errors
    /// Rejects unknown, cross-owner, unready or unavailable artifacts, or no stream capacity.
    pub fn pin(
        &self,
        p: &Principal,
        id: u64,
    ) -> ControlResult<(multiview_webengine::artifact::Lease, OwnedSemaphorePermit)> {
        let job = self.status(p, id)?;
        let artifact = job
            .artifact_id
            .ok_or_else(|| ControlError::Conflict("capture has no completed artifact".into()))?;
        let artifact = multiview_webengine::artifact::ArtifactId::parse(&artifact)
            .map_err(|_| ControlError::Repository("invalid capture artifact identity".into()))?;
        let lease = self.with_pin_request(|| {
            self.service
                .as_ref()
                .ok_or(ControlError::EngineBusy)?
                .pin(&artifact, &p.key_id)
                .map_err(|_| ControlError::EngineBusy)
        })?;
        let permit = self
            .streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| ControlError::EngineBusy)?;
        Ok((lease, permit))
    }

    /// Resolve an exact completed capture for native playback through this service.
    /// No download-stream permit survives this call. Call off the output clock.
    /// # Errors
    /// Rejects foreign jobs/fills, mismatched identity and unavailable pin capacity.
    pub fn pin_for_player(
        &self,
        p: &Principal,
        job_id: u64,
        artifact_id: &str,
    ) -> ControlResult<multiview_webengine::artifact::Lease> {
        p.role.require(Action::Write)?;
        let job = self.status(p, job_id)?;
        if job.state != "succeeded" || job.artifact_id.as_deref() != Some(artifact_id) {
            return Err(ControlError::Conflict(
                "capture artifact does not match completed job".into(),
            ));
        }
        let artifact = multiview_webengine::artifact::ArtifactId::parse(artifact_id)
            .map_err(|_| ControlError::Conflict("invalid capture artifact identity".into()))?;
        self.with_pin_request(|| {
            self.service
                .as_ref()
                .ok_or(ControlError::EngineBusy)?
                .pin(&artifact, &p.key_id)
                .map_err(|_| ControlError::EngineBusy)
        })
    }

    fn with_pin_request<T>(&self, pin: impl FnOnce() -> ControlResult<T>) -> ControlResult<T> {
        let _request = self
            .pin_requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| ControlError::EngineBusy)?;
        pin()
    }

    /// Cancel an actor-owned, authorized job under an idempotency key. Blocking.
    /// # Errors
    /// Unknown 404; finished 409; divergent key reuse 409.
    pub fn cancel(&self, p: &Principal, key: &str, id: u64) -> ControlResult<CancelResult> {
        p.role.require(Action::Write)?;
        let _ = self.status(p, id)?;
        let op = "cancel";
        let digest = digest_hex(id.to_string().as_bytes());
        match self.ledger.reserve(&p.key_id, op, key, &digest) {
            Ok(Reserve::Replay(_)) => return Ok(CancelResult { fresh: false }),
            Ok(Reserve::Fresh) => {}
            Err(_) => {
                return Err(ControlError::Conflict(
                    "Idempotency-Key reused with a different or in-flight request".into(),
                ))
            }
        }
        let outcome = self
            .service
            .as_ref()
            .ok_or(ControlError::EngineBusy)?
            .cancel(&p.key_id, JobId::from_raw(id));
        match outcome {
            CancelOutcome::Cancelled | CancelOutcome::Cancelling => {
                self.ledger.complete(&p.key_id, op, key, id).map_err(|_| {
                    ControlError::Repository("idempotency record could not be persisted".into())
                })?;
                Ok(CancelResult { fresh: true })
            }
            CancelOutcome::Finished => {
                self.release(&p.key_id, op, key)?;
                Err(ControlError::Conflict(
                    "capture job already finished".into(),
                ))
            }
            CancelOutcome::Unknown => {
                self.release(&p.key_id, op, key)?;
                Err(missing(id))
            }
        }
    }
}
/// Stage outcome: `Definite` failed before acceptance (safe to release the
/// reservation); `Uncertain` may have started a capture (keep it pending).
enum AdmitError {
    Definite(ControlError),
    Uncertain(ControlError),
}
/// Unambiguous correlation: SHA-256 over a JSON array of actor, operation,
/// key hash and body digest (no delimiter ambiguity; no key or payload bytes).
fn correlation_token(actor: &str, op: &str, key: &str, body_digest: &str) -> String {
    let doc = serde_json::json!([actor, op, digest_hex(key.as_bytes()), body_digest]);
    digest_hex(&serde_json::to_vec(&doc).unwrap_or_default())
}
/// Existing job for a trusted, already-authorized owner+fill and token.
fn correlated_job(service: &Service, owner: &str, fill: &str, token: &str) -> Option<u64> {
    service
        .lookup_correlated(owner, fill, token)
        .map(|(id, _)| id.raw())
}
fn missing(id: u64) -> ControlError {
    ControlError::NotFound {
        kind: "capture-job",
        id: id.to_string(),
    }
}
/// The canvas `num/den` as a capture conform cadence, when representable.
fn canvas_output_cadence(num: i64, den: i64) -> Option<OutputCadence> {
    let cadence = OutputCadence {
        num: u32::try_from(num).ok()?,
        den: u32::try_from(den).ok()?,
    };
    cadence.validate().ok().map(|()| cadence)
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

#[cfg(test)]
mod pin_admission_tests {
    #[test]
    fn two_failed_pin_waits_release_admission_and_do_not_consume_stream_slots() {
        use std::sync::{Arc, Barrier};
        let provider = super::Provider::default();
        let entered = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let provider = provider.clone();
            let entered = entered.clone();
            let release = release.clone();
            joins.push(std::thread::spawn(move || {
                provider.with_pin_request::<()>(|| {
                    entered.wait();
                    release.wait();
                    Err(crate::error::ControlError::EngineBusy)
                })
            }));
        }
        entered.wait();
        assert_eq!(provider.pin_requests.available_permits(), 0);
        assert_eq!(provider.streams.available_permits(), 2);
        assert!(provider.with_pin_request(|| Ok(())).is_err());
        release.wait();
        for join in joins {
            assert!(join.join().is_ok_and(|result| result.is_err()));
        }
        assert_eq!(provider.pin_requests.available_permits(), 2);
        assert_eq!(provider.streams.available_permits(), 2);
        assert!(provider.with_pin_request(|| Ok(())).is_ok());
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::{canvas_output_cadence, Provider};
    use crate::auth::{Principal, Role};
    use crate::error::ControlError;
    use multiview_webengine::capture::OutputCadence;

    fn operator() -> Principal {
        Principal {
            key_id: "op".into(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        }
    }
    fn body(target: Option<&str>) -> super::CaptureBody {
        serde_json::from_value(serde_json::json!({
            "fill_id": "radar", "target_player": target,
            "view": {"lat": -33.9, "lon": 150.3, "zoom": 10, "overlay": "radar",
                     "width": 1280, "height": 720, "fps": 25, "frames": 25, "public": true,
                     "capture": "realtime"},
            "terms": {"accepted": true, "version": super::WINDY_TERMS_VERSION}
        }))
        .unwrap()
    }

    #[test]
    fn canvas_cadence_maps_exactly_and_rejects_unrepresentable_rates() {
        assert_eq!(
            canvas_output_cadence(30000, 1001),
            Some(OutputCadence {
                num: 30000,
                den: 1001
            })
        );
        assert_eq!(
            Provider::default()
                .with_canvas_cadence(25, 1)
                .output_cadence(),
            Some(OutputCadence { num: 25, den: 1 })
        );
        for (num, den) in [(0, 1), (-25, 1), (25, 0), (25, -1), (1, 2), (i64::MAX, 1)] {
            assert_eq!(canvas_output_cadence(num, den), None, "{num}/{den}");
        }
    }

    #[test]
    fn player_targeted_capture_requires_a_known_canvas_cadence() {
        let unknown = Provider::default();
        assert!(matches!(
            unknown.submit(&operator(), "k", &body(Some("vt-1"))),
            Err(ControlError::Conflict(_))
        ));
        // Untargeted captures still pass validation (then: no service).
        assert!(matches!(
            unknown.submit(&operator(), "k", &body(None)),
            Err(ControlError::EngineBusy)
        ));
        // With the canvas cadence known, the conformed request validates.
        let known = Provider::default().with_canvas_cadence(30000, 1001);
        assert!(matches!(
            known.submit(&operator(), "k", &body(Some("vt-1"))),
            Err(ControlError::EngineBusy)
        ));
    }
}
