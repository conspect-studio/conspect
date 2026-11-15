//! The real capture provider (unix: the browser host needs process groups and
//! private `openat`-pinned files).
use super::{
    windy_account::{AccountError, WindyAccount},
    Admission, CancelResult, CaptureBody, JobView, WindyAccountView, WindyVerifyResult,
    ARTIFACT_STREAMS, WINDY_TERMS_VERSION,
};
use crate::web_capture_store::{digest_hex, Ledger, Reserve, TermsAcceptance, TermsRepository};
use crate::{
    auth::{authorize_object, Action, Principal},
    error::{ControlError, ControlResult},
};
use multiview_webengine::{
    capture::{Background, Capture, WindyView},
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
    windy: Option<Arc<WindyAccount>>,
}
impl Default for Provider {
    fn default() -> Self {
        Self {
            service: None,
            terms: None,
            ledger: Arc::new(Ledger::memory()),
            streams: Arc::new(Semaphore::new(ARTIFACT_STREAMS)),
            pin_requests: Arc::new(Semaphore::new(ARTIFACT_STREAMS)),
            windy: None,
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
            windy: None,
        }
    }
    /// Attach the encrypted Windy account store (Premium capture).
    #[must_use]
    pub fn with_windy_account(mut self, account: Arc<WindyAccount>) -> Self {
        self.windy = Some(account);
        self
    }
    fn windy(&self) -> ControlResult<&WindyAccount> {
        self.windy.as_deref().ok_or_else(|| {
            ControlError::Conflict(
                "the Windy account store is not configured on this server".into(),
            )
        })
    }
    /// Admin-only, whole-system: the Windy account is a deployment integration.
    fn require_admin(p: &Principal) -> ControlResult<()> {
        p.role.require(Action::Administer)?;
        crate::routes::require_unscoped_for_whole_system(p)
    }
    /// Secret-free Windy account status (admin only).
    /// # Errors
    /// Non-admin or scoped principal; store not configured.
    pub fn windy_account(&self, p: &Principal) -> ControlResult<WindyAccountView> {
        Self::require_admin(p)?;
        Ok(self.windy()?.view())
    }
    /// Store (replace) the Windy credential; resets Premium. Blocking.
    /// # Errors
    /// Non-admin/scoped, invalid input, store not configured or storage failure.
    pub fn set_windy_account(
        &self,
        p: &Principal,
        email: &str,
        password: &str,
    ) -> ControlResult<WindyAccountView> {
        Self::require_admin(p)?;
        self.windy()?.set(email, password).map_err(account_error)
    }
    /// Remove the Windy credential. Blocking.
    /// # Errors
    /// Non-admin/scoped, store not configured or storage failure.
    pub fn clear_windy_account(&self, p: &Principal) -> ControlResult<WindyAccountView> {
        Self::require_admin(p)?;
        self.windy()?.clear().map_err(account_error)
    }
    /// Bounded login check of the stored credential. Blocking (up to ~90 s).
    /// # Errors
    /// Non-admin/scoped, nothing stored, a check already running, storage.
    pub fn verify_windy_account(&self, p: &Principal) -> ControlResult<WindyVerifyResult> {
        Self::require_admin(p)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let (result, account) = self.windy()?.verify(now).map_err(account_error)?;
        Ok(WindyVerifyResult { result, account })
    }
    fn premium_available(&self) -> bool {
        self.service.is_some() && self.windy.as_ref().is_some_and(|w| w.premium_available())
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
        serde_json::json!({"capture_available":self.service.is_some(),"premium_available":self.premium_available(),"url_fetch_available":false,"url_fetch_reason":"enforced download boundary not provisioned","accounting":accounting,"kernel_limits":false})
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
        // Premium fails closed: only a credential that passed its latest verify
        // is resolved, and it travels only inside the job request (host stdin).
        let credentials = if body.view.public {
            None
        } else {
            Some(
                self.windy
                    .as_ref()
                    .and_then(|w| w.resolve_for_capture())
                    .ok_or_else(|| {
                        ControlError::Conflict(
                            "Windy Premium is not available: set and verify the Windy account \
                             (Settings → Integrations → Windy)"
                                .into(),
                        )
                    })?,
            )
        };
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
                require_premium: credentials.is_some(),
                credentials,
            }),
            capture: body.view.capture_options(),
        };
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
fn account_error(e: AccountError) -> ControlError {
    match e {
        AccountError::Invalid => {
            ControlError::Validation("a Windy email and a non-empty password are required".into())
        }
        AccountError::NotConfigured => ControlError::Conflict("no Windy account is set".into()),
        AccountError::Busy => {
            ControlError::Conflict("a Windy login check is already running".into())
        }
        AccountError::Storage => ControlError::Repository("Windy account store unavailable".into()),
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
        SubmitError::Incompatible => ControlError::Conflict(
            "this fill already has an active capture with different settings (for example public vs Premium); wait for it to finish or cancel it".into(),
        ),
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
