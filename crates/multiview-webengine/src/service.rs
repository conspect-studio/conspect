//! Queued capture service: one management worker thread owns the
//! [`ArtifactStore`] and runs real [`run_capture`] jobs one at a time.
//!
//! Authorization is the caller's job: `owner` and `fill` are trusted values the
//! caller has already authorized. The shared state mutex is held only for short
//! in-memory updates, never across store I/O or a capture. Cancelling a running
//! job raises the runner's cancellation flag; shutdown cancels the active job,
//! cancels queued jobs and joins the worker, so no thread outlives the service.
//! Artifacts are exposed only after the runner committed them. A failed or
//! cancelled refresh never touches the latest good artifact of its fill.
//! Pins are served by the worker between jobs; while a capture runs, `pin`
//! returns `WouldBlock` rather than waiting up to the job deadline. The pending
//! pin queue is bounded by `queue_capacity` (excess gets `WouldBlock`); the
//! check-and-push and the worker's take-pins/mark-running steps share one mutex,
//! so a pin is either served before the next job starts or refused, and no hash
//! I/O ever happens under that mutex.
//!
//! Commit linearization: once the runner has committed an artifact the job is
//! `Succeeded` and the bytes are kept even if a cancel or shutdown was raised
//! meanwhile; `Cancelled` is reported only when the runner returned an error
//! after cancellation, i.e. nothing was committed.
//!
//! Restart: job ids and generations never come from the wall clock. A fresh
//! [`Service::start`] is for a NEW, EMPTY store only (ids and generations start
//! at 1; a non-empty store rejects stale generations, failing closed). Recovery
//! uses [`Service::start_with_checkpoint`] with a [`Checkpoint`] the caller
//! persisted (see [`Service::checkpoint`]); nothing there is durable by itself.
//!
//! Durable mode ([`Service::start_durable`]) owns a journal (see
//! [`crate::service_journal`]) and needs no caller bookkeeping. Order, all
//! through atomic write + fsync + rename + directory fsync, never under the
//! state mutex:
//! 1. submit: persist `job_high = id + 1` and the queued intent, THEN expose the
//!    id (`Accepted`); a persistence failure refuses the submit.
//! 2. launch: persist `generation_high = generation + 1` and the running intent
//!    BEFORE the runner (and so before the store) sees the generation; a failure
//!    fails the job without launching.
//! 2. (continued) the store's reserved artifact id is journaled on the job by a
//!    runner hook IMMEDIATELY after `begin`, before any process or write.
//! 3. outcome: persist `Succeeded` plus the latest-good record BEFORE the job
//!    reports `Succeeded` or latest-good changes. If that write fails the job is
//!    `Uncertain` (never success), the committed artifact is NOT deleted and
//!    latest-good is unchanged. Failed/cancelled outcomes are persisted before
//!    reporting too; if that write fails the job still reports failed/cancelled
//!    (not a success claim) and the stale intent is resolved at next boot.
//!
//! Boot recovery fails closed on a corrupt, unreadable, inconsistent or stale
//! journal, on a non-empty store without a journal, and on any store artifact
//! whose job id or generation is not below the journal high-waters. Queued and
//! unfinished jobs become `Cancelled`/`Failed(Interrupted)` and are never
//! silently successful; a running job whose artifact the store did commit
//! (found by its trusted provenance, owner-scoped and re-pinned) is recovered
//! as `Succeeded`, otherwise an unverifiable one is `Uncertain`. Counters only
//! ever grow, so they exceed every historic id and generation even when its
//! artifact was removed or its job failed. No artifact is ever deleted here.
use crate::service_journal::{
    kind_from, kind_name, load, valid_correlation, Doc, GoodRec, JobRec, Journal, JournalHook, Rec,
};
use crate::{
    artifact::{Artifact, ArtifactId, ArtifactStore, Lease, Provenance},
    capture::{Capture, Tools},
    fetch::{run_fetch_observed, Fetch, FetchMeta, FetchProgress, FetchTools},
    resource::ResourceSample,
    runner::{is_commit_uncertain, run_capture_observed, Control},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex, MutexGuard, PoisonError,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Longest per-job deadline.
const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// Service-assigned job identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);
impl JobId {
    /// Rebuild an id from its wire value; unknown ids simply have no status.
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
    /// Wire value.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Queue and history bounds.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Maximum queued (not yet running) jobs; at least 1.
    pub queue_capacity: usize,
    /// Maximum finished jobs retained for status; at least 1.
    pub history: usize,
    /// Maximum fills with a retained latest-good artifact; at least 1.
    pub max_fills: usize,
}

/// One trusted capture request.
pub struct Request {
    /// Already-authorized owner object.
    pub owner: String,
    /// Already-authorized fill object.
    pub fill: String,
    /// Bounded capture; never exposed through status.
    pub capture: Capture,
    /// Artifact byte cap for this job.
    pub maximum_bytes: u64,
    /// Deadline from job start, 1 ns through 120 s.
    pub timeout: Duration,
}

/// One trusted URL-fetch request (see [`Service::submit_fetch`]). The URL lives
/// only in the queued payload: it is never journaled and never in [`Status`].
pub struct FetchRequest {
    /// Already-authorized owner object.
    pub owner: String,
    /// Already-authorized fill object.
    pub fill: String,
    /// Bounded fetch, validated at submit.
    pub fetch: Fetch,
    /// Artifact byte cap for this job.
    pub maximum_bytes: u64,
    /// Deadline from job start, 1 ns through 120 s.
    pub timeout: Duration,
}

/// Queued work: a capture or a fetch, same worker, queue, cancel and recovery.
enum Payload {
    Capture(Request),
    Fetch(FetchRequest),
}
impl Payload {
    fn owner(&self) -> &str {
        match self {
            Self::Capture(r) => &r.owner,
            Self::Fetch(r) => &r.owner,
        }
    }
    fn fill(&self) -> &str {
        match self {
            Self::Capture(r) => &r.fill,
            Self::Fetch(r) => &r.fill,
        }
    }
    fn maximum_bytes(&self) -> u64 {
        match self {
            Self::Capture(r) => r.maximum_bytes,
            Self::Fetch(r) => r.maximum_bytes,
        }
    }
    fn timeout(&self) -> Duration {
        match self {
            Self::Capture(r) => r.timeout,
            Self::Fetch(r) => r.timeout,
        }
    }
}

/// Outcome of a submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// A new job was queued.
    Accepted(JobId),
    /// The fill already has a queued or running job; that job answers the trigger.
    Coalesced(JobId),
}

/// Why a submit was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// Queue (or the latest-good fill table) is at capacity.
    Full,
    /// Identity, capture or limit validation failed.
    Invalid,
    /// The service is shutting down.
    ShuttingDown,
    /// The durable journal could not record the reservation.
    Persistence,
}
impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "capture queue full",
            Self::Invalid => "invalid capture request",
            Self::ShuttingDown => "capture service shutting down",
            Self::Persistence => "capture journal unavailable",
        })
    }
}
impl std::error::Error for SubmitError {}

/// Job lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for the worker.
    Queued,
    /// The runner is executing it.
    Running,
    /// Committed by the runner.
    Succeeded {
        /// Durable artifact.
        artifact: ArtifactId,
    },
    /// Runner failure; no artifact was published.
    Failed {
        /// Error class only, never a message that could carry content.
        kind: io::ErrorKind,
    },
    /// Cancelled before or during the run.
    Cancelled,
    /// The outcome could not be proven durable (or was unfinished at a crash and
    /// could not be verified). Never a success; any artifact is kept.
    Uncertain,
}

/// Job status without any document or credential content.
#[derive(Clone, Debug)]
pub struct Status {
    /// Job identity.
    pub id: JobId,
    /// Owner object.
    pub owner: String,
    /// Fill object.
    pub fill: String,
    /// Lifecycle state.
    pub state: State,
    /// Time since submission.
    pub submitted_age: Duration,
    /// Time since the job finished, once terminal.
    pub finished_age: Option<Duration>,
    /// Latest byte-count progress of a running fetch (numbers only; no URL,
    /// title or uploader). Always `None` for captures.
    pub progress: Option<FetchProgress>,
    /// Latest 1 Hz process-tree resource sample of the running producer
    /// (descendants included); the final sample is kept after the job ends.
    pub resource: Option<ResourceSample>,
    /// CPU and memory values are process-tree estimates in this implementation.
    pub accounting: Accounting,
}

/// Honest accounting capability; never label a process-tree sum as kernel exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accounting {
    /// Process-tree CPU and proportional-memory estimate, GPU unknown.
    ProcessTreeEstimated,
}

/// Runtime limitations that a control application must surface as health warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// This service currently uses the process-tree fallback, even if the host
    /// could delegate a subtree. Kernel hard limits are not applied by it.
    pub process_tree_fallback: bool,
    /// A writable delegated cgroup could not be detected for this process.
    pub delegation_unavailable: bool,
    /// True once an operator-provisioned fetch boundary was configured; it is
    /// still checked by the host before every actual download.
    pub fetch_configured: bool,
}

/// Latest successful output of a fill.
#[derive(Clone, Debug)]
pub struct LatestGood {
    /// Durable artifact.
    pub artifact: ArtifactId,
    /// Artifact size.
    pub bytes: u64,
    /// Content digest.
    pub sha256: String,
    /// Producing job.
    pub job: JobId,
    /// Time since it was committed.
    pub age: Duration,
}

/// Result of a cancel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Was queued; now cancelled and will never run.
    Cancelled,
    /// Is running; the runner was told to stop and the state turns final shortly.
    Cancelling,
    /// Already terminal.
    Finished,
    /// No such retained job.
    Unknown,
}

/// One retained latest-good artifact, as supplied to or taken from a checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoodRecord {
    /// Owner object.
    pub owner: String,
    /// Fill object.
    pub fill: String,
    /// Durable artifact.
    pub artifact: ArtifactId,
    /// Artifact size.
    pub bytes: u64,
    /// Content digest.
    pub sha256: String,
    /// Producing job.
    pub job: JobId,
}

/// Restart state the caller must persist (conservatively: values taken from
/// [`Service::checkpoint`] are upper bounds and never reused).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// First job id to assign; must exceed every prior job id.
    pub next_job: u64,
    /// First generation to assign; must exceed every prior generation.
    pub next_generation: u64,
    /// Latest-good table, re-verified against the store at start.
    pub latest_good: Vec<GoodRecord>,
}

struct Entry {
    owner: String,
    fill: String,
    state: State,
    submitted: Instant,
    finished: Option<Instant>,
    cancelling: bool,
    payload: Option<Payload>,
    progress: Option<FetchProgress>,
    resource: Option<ResourceSample>,
    meta: Option<FetchMeta>,
    correlation: Option<String>,
}
struct Good {
    artifact: ArtifactId,
    bytes: u64,
    sha256: String,
    job: JobId,
    at: Instant,
}
struct PinCommand {
    id: ArtifactId,
    owner: String,
    reply: mpsc::Sender<io::Result<Lease>>,
}
#[derive(Default)]
struct Shared {
    next: u64,
    next_generation: u64,
    queue: VecDeque<JobId>,
    jobs: BTreeMap<JobId, Entry>,
    finished: VecDeque<JobId>,
    running: Option<(JobId, Arc<AtomicBool>)>,
    good: BTreeMap<(String, String), Good>,
    pins: Vec<PinCommand>,
    shutdown: bool,
}
struct Inner {
    /// Serialises submitters so check-then-reserve is atomic without holding
    /// the state mutex across journal I/O. Lock order: gate, journal, state.
    gate: Mutex<()>,
    journal: Option<Mutex<Journal>>,
    state: Mutex<Shared>,
    wake: Condvar,
    config: Config,
    fetch_enabled: bool,
}
fn lock(inner: &Inner) -> MutexGuard<'_, Shared> {
    inner.state.lock().unwrap_or_else(PoisonError::into_inner)
}
/// Run a journal update; `Ok` immediately when the service is not durable.
fn persist(inner: &Inner, change: impl FnOnce(&mut Doc)) -> io::Result<()> {
    inner.journal.as_ref().map_or(Ok(()), |j| {
        j.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .update(change)
    })
}
fn state_of(rec: &Rec) -> State {
    match rec {
        Rec::Queued => State::Queued,
        Rec::Running => State::Running,
        Rec::Succeeded { artifact } => State::Succeeded {
            artifact: artifact.clone(),
        },
        Rec::Failed { kind } => State::Failed {
            kind: kind_from(kind),
        },
        Rec::Cancelled => State::Cancelled,
        Rec::Uncertain => State::Uncertain,
    }
}
fn set_job(doc: &mut Doc, id: u64, state: Rec) {
    if let Some(j) = doc.jobs.iter_mut().find(|j| j.id == id) {
        j.state = state;
    }
}
impl Shared {
    fn finish(&mut self, id: JobId, state: State, history: usize) {
        if let Some(entry) = self.jobs.get_mut(&id) {
            entry.state = state;
            entry.finished = Some(Instant::now());
            entry.payload = None;
            entry.progress = None;
            self.finished.push_back(id);
        }
        while self.finished.len() > history {
            if let Some(old) = self.finished.pop_front() {
                self.jobs.remove(&old);
            }
        }
    }
}

/// Handle to the queued capture service.
pub struct Service {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn bad_name(s: &str) -> bool {
    s.is_empty() || s.len() > 128 || s.contains("://")
}

impl Service {
    /// Validate tools and bounds, then start the single management worker, which
    /// takes ownership of `store`.
    /// # Errors
    /// Rejects untrusted tools, zero bounds, or a thread-spawn failure.
    pub fn start(tools: Tools, store: ArtifactStore, config: Config) -> io::Result<Self> {
        Self::start_with_checkpoint(
            tools,
            store,
            config,
            Checkpoint {
                next_job: 1,
                next_generation: 1,
                latest_good: Vec::new(),
            },
        )
    }

    /// Like [`Service::start`] for a recovered store: ids, generations and the
    /// latest-good table continue from `checkpoint`, which the caller must have
    /// persisted and which must exceed every prior provenance value. Each
    /// latest-good record is re-verified against the store (owner-scoped pin,
    /// size, digest, producing job and fill) before any job is accepted.
    /// # Errors
    /// Rejects untrusted tools, zero bounds, zero counters, an inconsistent or
    /// oversized table, an unverifiable artifact, or a thread-spawn failure.
    pub fn start_with_checkpoint(
        tools: Tools,
        store: ArtifactStore,
        config: Config,
        checkpoint: Checkpoint,
    ) -> io::Result<Self> {
        tools.validate()?;
        if config.queue_capacity == 0 || config.history == 0 || config.max_fills == 0 {
            return Err(io::Error::other("invalid service bounds"));
        }
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, "invalid checkpoint");
        if checkpoint.next_job == 0
            || checkpoint.next_generation == 0
            || checkpoint.latest_good.len() > config.max_fills
        {
            return Err(bad());
        }
        let mut good = BTreeMap::new();
        for r in checkpoint.latest_good {
            if bad_name(&r.owner) || bad_name(&r.fill) || r.job.raw() >= checkpoint.next_job {
                return Err(bad());
            }
            let lease = store.pin(&r.artifact, &r.owner, |_| true)?;
            let a = lease.artifact();
            if a.provenance.owner != r.owner
                || a.provenance.fill != r.fill
                || a.provenance.job != format!("j{}", r.job.raw())
                || a.provenance.generation >= checkpoint.next_generation
                || a.bytes != r.bytes
                || a.sha256 != r.sha256
            {
                return Err(bad());
            }
            drop(lease);
            let entry = Good {
                artifact: r.artifact,
                bytes: r.bytes,
                sha256: r.sha256,
                job: r.job,
                at: Instant::now(),
            };
            if good.insert((r.owner, r.fill), entry).is_some() {
                return Err(bad());
            }
        }
        Self::launch(
            tools,
            store,
            config,
            Shared {
                next: checkpoint.next_job,
                next_generation: checkpoint.next_generation,
                good,
                ..Shared::default()
            },
            None,
            None,
        )
    }

    /// [`Service::start`] that also accepts [`Service::submit_fetch`] jobs run
    /// with the controller-chosen `fetch_tools` (never user supplied).
    /// # Errors
    /// As [`Service::start`], plus untrusted fetch tools / egress wrapper.
    pub fn start_with_fetch(
        tools: Tools,
        fetch_tools: FetchTools,
        store: ArtifactStore,
        config: Config,
    ) -> io::Result<Self> {
        fetch_tools.validate()?;
        let svc = Self::start_checked(tools, store, config, Some(fetch_tools))?;
        Ok(svc)
    }

    fn start_checked(
        tools: Tools,
        store: ArtifactStore,
        config: Config,
        fetch: Option<FetchTools>,
    ) -> io::Result<Self> {
        tools.validate()?;
        if config.queue_capacity == 0 || config.history == 0 || config.max_fills == 0 {
            return Err(io::Error::other("invalid service bounds"));
        }
        Self::launch(
            tools,
            store,
            config,
            Shared {
                next: 1,
                next_generation: 1,
                ..Shared::default()
            },
            None,
            fetch,
        )
    }

    fn launch(
        tools: Tools,
        store: ArtifactStore,
        config: Config,
        shared: Shared,
        journal: Option<Journal>,
        fetch: Option<FetchTools>,
    ) -> io::Result<Self> {
        let inner = Arc::new(Inner {
            gate: Mutex::new(()),
            journal: journal.map(Mutex::new),
            state: Mutex::new(shared),
            wake: Condvar::new(),
            config,
            fetch_enabled: fetch.is_some(),
        });
        let worker_inner = inner.clone();
        let handle = std::thread::Builder::new()
            .name("capture-service".into())
            .spawn(move || worker(&worker_inner, store, &tools, fetch.as_ref()))?;
        Ok(Self {
            inner,
            worker: Mutex::new(Some(handle)),
        })
    }

    /// Start with a durable journal at `journal` (a private file dedicated to
    /// this store; its directory must exist). A missing journal is accepted only
    /// for an EMPTY store. See the module docs for ordering and recovery.
    /// # Errors
    /// Rejects untrusted tools, zero bounds, a corrupt/stale/inconsistent journal,
    /// artifacts the journal does not account for, an unverifiable latest-good
    /// artifact, a failure to persist the recovered state, or a spawn failure.
    pub fn start_durable(
        tools: Tools,
        store: ArtifactStore,
        config: Config,
        journal: &Path,
    ) -> io::Result<Self> {
        Self::start_durable_with_hook(tools, store, config, journal, |_| Ok(()))
    }

    /// [`Service::start_durable`] with a journal fault-injection hook (tests).
    /// # Errors
    /// As [`Service::start_durable`].
    #[doc(hidden)]
    pub fn start_durable_with_hook(
        tools: Tools,
        store: ArtifactStore,
        config: Config,
        journal: &Path,
        hook: JournalHook,
    ) -> io::Result<Self> {
        Self::durable(tools, None, store, config, journal, hook)
    }

    /// [`Service::start_durable`] that also accepts [`Service::submit_fetch`]
    /// jobs with controller-chosen `fetch_tools`.
    /// # Errors
    /// As [`Service::start_durable`], plus untrusted fetch tools.
    pub fn start_durable_with_fetch(
        tools: Tools,
        fetch_tools: FetchTools,
        store: ArtifactStore,
        config: Config,
        journal: &Path,
    ) -> io::Result<Self> {
        fetch_tools.validate()?;
        Self::durable(tools, Some(fetch_tools), store, config, journal, |_| Ok(()))
    }

    /// [`Service::start_durable_with_fetch`] with a journal fault hook (tests).
    /// # Errors
    /// As [`Service::start_durable_with_fetch`].
    #[doc(hidden)]
    pub fn start_durable_with_fetch_hook(
        tools: Tools,
        fetch_tools: FetchTools,
        store: ArtifactStore,
        config: Config,
        journal: &Path,
        hook: JournalHook,
    ) -> io::Result<Self> {
        fetch_tools.validate()?;
        Self::durable(tools, Some(fetch_tools), store, config, journal, hook)
    }

    fn durable(
        tools: Tools,
        fetch: Option<FetchTools>,
        store: ArtifactStore,
        config: Config,
        journal: &Path,
        hook: JournalHook,
    ) -> io::Result<Self> {
        tools.validate()?;
        if config.queue_capacity == 0 || config.history == 0 || config.max_fills == 0 {
            return Err(io::Error::other("invalid service bounds"));
        }
        let (doc, shared) = recover(&store, load(journal)?, &config)?;
        let mut j = Journal::new(journal, doc.clone(), hook, config.history);
        // Persist the resolved state before accepting any work (also creates
        // the journal for a fresh store).
        j.update(|d| *d = doc)?;
        Self::launch(tools, store, config, shared, Some(j), fetch)
    }

    /// Queue a capture, or coalesce into the fill's active job.
    /// # Errors
    /// `Invalid`, `Full` (queue at capacity) or `ShuttingDown`.
    pub fn submit(&self, request: Request) -> Result<Submitted, SubmitError> {
        if request.capture.validate().is_err() {
            return Err(SubmitError::Invalid);
        }
        self.enqueue(Payload::Capture(request), None)
    }

    /// Queue a URL fetch (same queue, fill coalescing, quota, cancellation and
    /// recovery as captures), or coalesce into the fill's active job.
    /// # Errors
    /// `Invalid` (also when the service was not started with fetch tools, or the
    /// fetch fails validation), `Full`, `ShuttingDown` or `Persistence`.
    pub fn submit_fetch(&self, request: FetchRequest) -> Result<Submitted, SubmitError> {
        if !self.inner.fetch_enabled || request.fetch.validate().is_err() {
            return Err(SubmitError::Invalid);
        }
        self.enqueue(Payload::Fetch(request), None)
    }

    /// Source metadata (sanitised title/uploader/duration) of a retained fetch
    /// job of `owner`. Opt-in only: [`Service::status`] never carries it, and it
    /// is neither journaled nor retained across restart.
    #[must_use]
    pub fn fetch_meta(&self, owner: &str, id: JobId) -> Option<FetchMeta> {
        let shared = lock(&self.inner);
        shared
            .jobs
            .get(&id)
            .filter(|e| e.owner == owner)
            .and_then(|e| e.meta.clone())
    }

    /// Like [`Service::submit`], but journals the opaque `correlation` (64
    /// lowercase hex, e.g. a SHA-256 of caller identity + request digest) with
    /// the queued intent BEFORE the id or producer is exposed. A repeat with the
    /// same `(owner, fill, correlation)` returns the existing job (even when
    /// finished, while retained) as `Coalesced`, never a second job. Only the
    /// token hash is stored; the request payload is never journaled.
    /// # Errors
    /// As [`Service::submit`]; also `Invalid` for a malformed token.
    pub fn submit_correlated(
        &self,
        request: Request,
        correlation: &str,
    ) -> Result<Submitted, SubmitError> {
        if !valid_correlation(correlation) || request.capture.validate().is_err() {
            return Err(SubmitError::Invalid);
        }
        self.enqueue(Payload::Capture(request), Some(correlation.to_owned()))
    }

    /// Read-only lookup of a retained job by correlation. `owner` and `fill`
    /// must already be authorized by the caller; another owner/fill, an unknown
    /// or pruned token yields `None` (which does NOT prove no job was started).
    /// Briefly takes the journal mutex (no I/O under it).
    #[must_use]
    pub fn lookup_correlated(
        &self,
        owner: &str,
        fill: &str,
        correlation: &str,
    ) -> Option<(JobId, State)> {
        if !valid_correlation(correlation) {
            return None;
        }
        let from_journal = self.inner.journal.as_ref().and_then(|j| {
            let g = j.lock().unwrap_or_else(PoisonError::into_inner);
            g.doc
                .jobs
                .iter()
                .find(|r| {
                    r.owner == owner
                        && r.fill == fill
                        && r.correlation.as_deref() == Some(correlation)
                })
                .map(|r| (JobId(r.id), r.state.clone()))
        });
        let shared = lock(&self.inner);
        if let Some((id, rec)) = from_journal {
            let state = shared
                .jobs
                .get(&id)
                .map_or_else(|| state_of(&rec), |e| e.state.clone());
            return Some((id, state));
        }
        shared
            .jobs
            .iter()
            .find(|(_, e)| {
                e.owner == owner && e.fill == fill && e.correlation.as_deref() == Some(correlation)
            })
            .map(|(id, e)| (*id, e.state.clone()))
    }

    #[allow(clippy::too_many_lines)] // One admission transaction preserves intent/exposure ordering.
    fn enqueue(
        &self,
        request: Payload,
        correlation: Option<String>,
    ) -> Result<Submitted, SubmitError> {
        if bad_name(request.owner())
            || bad_name(request.fill())
            || request.maximum_bytes() == 0
            || request.timeout().is_zero()
            || request.timeout() > MAX_TIMEOUT
        {
            return Err(SubmitError::Invalid);
        }
        let (req_owner, req_fill) = (request.owner().to_owned(), request.fill().to_owned());
        let _gate = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let shared = lock(&self.inner);
        if shared.shutdown {
            return Err(SubmitError::ShuttingDown);
        }
        if let Some(token) = correlation.as_deref() {
            let durable = self.inner.journal.as_ref().and_then(|j| {
                j.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .doc
                    .jobs
                    .iter()
                    .find(|r| {
                        r.owner == req_owner
                            && r.fill == req_fill
                            && r.correlation.as_deref() == Some(token)
                    })
                    .map(|r| JobId(r.id))
            });
            let live = shared
                .jobs
                .iter()
                .find(|(_, e)| {
                    e.owner == req_owner
                        && e.fill == req_fill
                        && e.correlation.as_deref() == Some(token)
                })
                .map(|(id, _)| *id);
            if let Some(id) = durable.or(live) {
                return Ok(Submitted::Coalesced(id));
            }
        }
        if let Some((id, _)) = shared.jobs.iter().find(|(_, e)| {
            e.owner == req_owner
                && e.fill == req_fill
                && !e.cancelling
                && matches!(e.state, State::Queued | State::Running)
        }) {
            return Ok(Submitted::Coalesced(*id));
        }
        let key = (req_owner.clone(), req_fill.clone());
        if shared.queue.len() >= self.inner.config.queue_capacity {
            return Err(SubmitError::Full);
        }
        // Fill reservations: distinct fills with a latest-good plus distinct
        // fills with an active (queued/running) job and no latest-good yet.
        // Failed/cancelled first fills drop out, releasing their reservation.
        let mut reserved: std::collections::BTreeSet<(&str, &str)> = shared
            .good
            .keys()
            .map(|(o, f)| (o.as_str(), f.as_str()))
            .collect();
        reserved.extend(
            shared
                .jobs
                .values()
                .filter(|e| matches!(e.state, State::Queued | State::Running))
                .map(|e| (e.owner.as_str(), e.fill.as_str())),
        );
        if !reserved.contains(&(key.0.as_str(), key.1.as_str()))
            && reserved.len() >= self.inner.config.max_fills
        {
            return Err(SubmitError::Full);
        }
        let id = JobId(shared.next);
        let after = shared.next.checked_add(1).ok_or(SubmitError::Full)?;
        // Reserve the id durably BEFORE it is exposed. Submitters are serialised
        // by the gate and only submit advances `next`, so the id stays valid
        // while the state mutex is released for the journal I/O.
        drop(shared);
        let (o, f) = (req_owner.clone(), req_fill.clone());
        let corr = correlation.clone();
        persist(&self.inner, |d| {
            d.job_high = d.job_high.max(after);
            d.jobs.push(JobRec {
                id: id.raw(),
                owner: o,
                fill: f,
                generation: 0,
                reserved: None,
                correlation: corr,
                state: Rec::Queued,
            });
        })
        .map_err(|_| SubmitError::Persistence)?;
        let mut shared = lock(&self.inner);
        if shared.shutdown {
            drop(shared);
            let _ = persist(&self.inner, |d| set_job(d, id.raw(), Rec::Cancelled));
            // A durable intent already exists. Even when cancellation appears
            // durable, a caller must retain its idempotency reservation until
            // recovery proves the outcome; this is not a pre-admission refusal.
            return Err(SubmitError::Persistence);
        }
        shared.next = after;
        shared.jobs.insert(
            id,
            Entry {
                owner: req_owner,
                fill: req_fill,
                state: State::Queued,
                submitted: Instant::now(),
                finished: None,
                cancelling: false,
                payload: Some(request),
                progress: None,
                resource: None,
                meta: None,
                correlation,
            },
        );
        shared.queue.push_back(id);
        drop(shared);
        self.inner.wake.notify_all();
        Ok(Submitted::Accepted(id))
    }

    /// Current runtime limits for API capability and health-warning reporting.
    /// The controller must warn that fallback accounting lacks kernel limits.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            process_tree_fallback: true,
            delegation_unavailable: crate::resource::delegation_unavailable(),
            fetch_configured: self.inner.fetch_enabled,
        }
    }

    /// Current status of a retained job of `owner` (history is bounded). A job
    /// of another owner is indistinguishable from an unknown one.
    #[must_use]
    pub fn status(&self, owner: &str, id: JobId) -> Option<Status> {
        let shared = lock(&self.inner);
        let e = shared.jobs.get(&id).filter(|e| e.owner == owner)?;
        let now = Instant::now();
        Some(Status {
            id,
            owner: e.owner.clone(),
            fill: e.fill.clone(),
            state: e.state.clone(),
            submitted_age: now.saturating_duration_since(e.submitted),
            finished_age: e.finished.map(|f| now.saturating_duration_since(f)),
            progress: e.progress,
            resource: e.resource.clone(),
            accounting: Accounting::ProcessTreeEstimated,
        })
    }

    /// Latest successful artifact of a fill, which survives later failures.
    #[must_use]
    pub fn latest_good(&self, owner: &str, fill: &str) -> Option<LatestGood> {
        let shared = lock(&self.inner);
        let g = shared.good.get(&(owner.to_owned(), fill.to_owned()))?;
        Some(LatestGood {
            artifact: g.artifact.clone(),
            bytes: g.bytes,
            sha256: g.sha256.clone(),
            job: g.job,
            age: Instant::now().saturating_duration_since(g.at),
        })
    }

    /// Snapshot of the restart state to persist. Counters are upper bounds.
    #[must_use]
    pub fn checkpoint(&self) -> Checkpoint {
        let shared = lock(&self.inner);
        Checkpoint {
            next_job: shared.next,
            next_generation: shared.next_generation,
            latest_good: shared
                .good
                .iter()
                .map(|((owner, fill), g)| GoodRecord {
                    owner: owner.clone(),
                    fill: fill.clone(),
                    artifact: g.artifact.clone(),
                    bytes: g.bytes,
                    sha256: g.sha256.clone(),
                    job: g.job,
                })
                .collect(),
        }
    }

    /// Cancel a queued or running job of `owner`; other owners' ids are `Unknown`.
    #[must_use]
    pub fn cancel(&self, owner: &str, id: JobId) -> CancelOutcome {
        let mut shared = lock(&self.inner);
        let history = self.inner.config.history;
        let Some(entry) = shared.jobs.get_mut(&id).filter(|e| e.owner == owner) else {
            return CancelOutcome::Unknown;
        };
        match entry.state {
            State::Queued => {
                shared.queue.retain(|q| *q != id);
                shared.finish(id, State::Cancelled, history);
                drop(shared);
                // Best effort: an unrecorded queued job is cancelled at boot.
                let _ = persist(&self.inner, |d| set_job(d, id.raw(), Rec::Cancelled));
                CancelOutcome::Cancelled
            }
            State::Running => {
                entry.cancelling = true;
                if let Some((running, flag)) = &shared.running {
                    if *running == id {
                        flag.store(true, Ordering::Release);
                    }
                }
                CancelOutcome::Cancelling
            }
            _ => CancelOutcome::Finished,
        }
    }

    /// Pin a committed artifact for reading. Served by the worker between jobs.
    /// # Errors
    /// `WouldBlock` while a capture runs; otherwise store errors, or an error
    /// after shutdown.
    pub fn pin(&self, id: &ArtifactId, owner: &str) -> io::Result<Lease> {
        let (reply, receive) = mpsc::channel();
        {
            let mut shared = lock(&self.inner);
            if shared.shutdown {
                return Err(io::Error::other("capture service stopped"));
            }
            if shared.running.is_some() || shared.pins.len() >= self.inner.config.queue_capacity {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            shared.pins.push(PinCommand {
                id: id.clone(),
                owner: owner.to_owned(),
                reply,
            });
        }
        self.inner.wake.notify_all();
        receive
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "artifact pin deadline")
                }
                mpsc::RecvTimeoutError::Disconnected => io::Error::other("capture service stopped"),
            })?
    }

    /// Cancel the active and queued jobs, stop the worker and join it. Idempotent.
    pub fn shutdown(&self) {
        {
            let mut shared = lock(&self.inner);
            shared.shutdown = true;
            if let Some((_, flag)) = &shared.running {
                flag.store(true, Ordering::Release);
            }
            let history = self.inner.config.history;
            let mut cancelled = Vec::new();
            while let Some(id) = shared.queue.pop_front() {
                shared.finish(id, State::Cancelled, history);
                cancelled.push(id.raw());
            }
            drop(shared);
            if !cancelled.is_empty() {
                let _ = persist(&self.inner, |d| {
                    for id in cancelled {
                        set_job(d, id, Rec::Cancelled);
                    }
                });
            }
        }
        self.inner.wake.notify_all();
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum Work {
    Pins(Vec<PinCommand>),
    Job(JobId, Box<Payload>, Arc<AtomicBool>, u64),
    Stop,
}
fn next_work(inner: &Inner) -> Work {
    let mut shared = lock(inner);
    loop {
        if shared.shutdown {
            return Work::Stop;
        }
        if !shared.pins.is_empty() {
            return Work::Pins(std::mem::take(&mut shared.pins));
        }
        if let Some(id) = shared.queue.pop_front() {
            let Some(generation) = shared.next_generation.checked_add(1) else {
                let history = inner.config.history;
                shared.finish(
                    id,
                    State::Failed {
                        kind: io::ErrorKind::Other,
                    },
                    history,
                );
                continue;
            };
            let request = shared.jobs.get_mut(&id).and_then(|e| {
                e.state = State::Running;
                e.payload.take()
            });
            if let Some(request) = request {
                let assigned = std::mem::replace(&mut shared.next_generation, generation);
                let flag = Arc::new(AtomicBool::new(false));
                shared.running = Some((id, flag.clone()));
                return Work::Job(id, Box::new(request), flag, assigned);
            }
            continue;
        }
        shared = inner
            .wake
            .wait(shared)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

#[allow(clippy::too_many_lines)]
fn worker(inner: &Inner, mut store: ArtifactStore, tools: &Tools, fetch: Option<&FetchTools>) {
    loop {
        match next_work(inner) {
            Work::Stop => break,
            Work::Pins(commands) => {
                for c in commands {
                    let _ = c.reply.send(store.pin(&c.id, &c.owner, |_| true));
                }
            }
            Work::Job(id, request, flag, generation) => {
                // Reserve the generation and the running intent durably BEFORE
                // the runner (and so the store) can see them.
                let next = generation.saturating_add(1);
                let (o, f) = (request.owner().to_owned(), request.fill().to_owned());
                let launched = persist(inner, |d| {
                    d.generation_high = d.generation_high.max(next);
                    if let Some(j) = d.jobs.iter_mut().find(|j| j.id == id.raw()) {
                        j.generation = generation;
                        j.reserved = None;
                        j.state = Rec::Running;
                    } else {
                        d.jobs.push(JobRec {
                            id: id.raw(),
                            owner: o,
                            fill: f,
                            generation,
                            reserved: None,
                            correlation: None,
                            state: Rec::Running,
                        });
                    }
                });
                let provenance = Provenance {
                    owner: request.owner().to_owned(),
                    fill: request.fill().to_owned(),
                    job: format!("j{}", id.raw()),
                    generation,
                };
                let control = Control {
                    cancelled: &flag,
                    deadline: Instant::now() + request.timeout(),
                };
                // Journal the reserved id before any process or write.
                let reserved = |reserved: &ArtifactId| {
                    persist(inner, |d| {
                        if let Some(j) = d.jobs.iter_mut().find(|j| j.id == id.raw()) {
                            j.reserved = Some(reserved.clone());
                        }
                    })
                };
                let mut meta = None;
                let mut observe = |sample: ResourceSample| {
                    if let Some(e) = lock(inner).jobs.get_mut(&id) {
                        e.resource = Some(sample);
                    }
                };
                let result = match (launched, &*request) {
                    (Err(e), _) => Err(e),
                    (Ok(()), Payload::Capture(r)) => run_capture_observed(
                        &mut store,
                        provenance.clone(),
                        r.maximum_bytes,
                        tools,
                        &r.capture,
                        control,
                        |_| true,
                        reserved,
                        &mut observe,
                    ),
                    (Ok(()), Payload::Fetch(r)) => match fetch {
                        None => Err(io::Error::other("fetch tools not configured")),
                        Some(ft) => run_fetch_observed(
                            &mut store,
                            provenance.clone(),
                            r.maximum_bytes,
                            ft,
                            &r.fetch,
                            control,
                            |_| true,
                            reserved,
                            |p| {
                                if let Some(e) = lock(inner).jobs.get_mut(&id) {
                                    e.progress = Some(p);
                                }
                            },
                            &mut observe,
                        )
                        .map(|(a, report)| {
                            meta = report.meta;
                            a
                        }),
                    },
                };
                if meta.is_some() {
                    if let Some(e) = lock(inner).jobs.get_mut(&id) {
                        e.meta = meta;
                    }
                }
                drop(request);
                let result = match result {
                    Ok(a) => Done::Committed(a),
                    Err(e) => reconcile(&store, &provenance, e),
                };
                // Only the library's explicit durable-reference release may
                // remove an old generation. A refresh never deletes it.
                settle(inner, id, &flag, result);
            }
        }
    }
}

/// Settled runner outcome.
enum Done {
    /// Committed and verified in the store.
    Committed(Artifact),
    /// The commit may or may not be durable, or is unverifiable.
    Uncertain,
    /// Nothing was committed.
    Failed(io::Error),
}

/// Inspect the store for the launched generation's artifact (owner-scoped,
/// by trusted provenance) before any non-success is recorded: a verified commit
/// is never classified failed or cancelled.
fn reconcile(store: &ArtifactStore, provenance: &Provenance, error: io::Error) -> Done {
    let found = store
        .snapshot(|o| o == provenance.owner)
        .into_iter()
        .find(|a| {
            a.provenance.job == provenance.job
                && a.provenance.fill == provenance.fill
                && a.provenance.generation == provenance.generation
        });
    match found {
        Some(a) => match store.pin(&a.id, &provenance.owner, |_| true) {
            Ok(lease) => {
                drop(lease);
                Done::Committed(a)
            }
            Err(_) => Done::Uncertain,
        },
        None if is_commit_uncertain(&error) => Done::Uncertain,
        None => Done::Failed(error),
    }
}

/// Persist then record the runner outcome without deleting any artifact.
fn settle(inner: &Inner, id: JobId, flag: &AtomicBool, result: Done) {
    // A committed artifact wins over a racing cancel (see module docs).
    let history = inner.config.history;
    match result {
        Done::Uncertain => uncertain(inner, id, history),
        Done::Committed(artifact) => {
            let rec = GoodRec {
                owner: artifact.provenance.owner.clone(),
                fill: artifact.provenance.fill.clone(),
                artifact: artifact.id.clone(),
                generation: artifact.provenance.generation,
                bytes: artifact.bytes,
                sha256: artifact.sha256.clone(),
                job: id.raw(),
            };
            let key = (rec.owner.clone(), rec.fill.clone());
            let durable = persist(inner, |d| {
                set_job(
                    d,
                    id.raw(),
                    Rec::Succeeded {
                        artifact: rec.artifact.clone(),
                    },
                );
                d.good
                    .retain(|g| (&g.owner, &g.fill) != (&rec.owner, &rec.fill));
                d.good.push(rec.clone());
            });
            if durable.is_err() {
                // Not proven durable: never success, never latest-good; the
                // committed artifact is kept for explicit reconciliation.
                uncertain(inner, id, history);
                return;
            }
            let mut shared = lock(inner);
            shared.running = None;
            shared.good.insert(
                key,
                Good {
                    artifact: artifact.id.clone(),
                    bytes: artifact.bytes,
                    sha256: artifact.sha256,
                    job: id,
                    at: Instant::now(),
                },
            );
            shared.finish(
                id,
                State::Succeeded {
                    artifact: artifact.id,
                },
                history,
            );
        }
        Done::Failed(e) => {
            let cancelled = flag.load(Ordering::Acquire);
            let rec = if cancelled {
                Rec::Cancelled
            } else {
                Rec::Failed {
                    kind: kind_name(e.kind()).to_owned(),
                }
            };
            // Best effort: an unrecorded outcome stays `Running` in the journal
            // and is resolved (never as success without an artifact) at boot.
            let _ = persist(inner, |d| set_job(d, id.raw(), rec));
            let mut shared = lock(inner);
            shared.running = None;
            let state = if cancelled {
                State::Cancelled
            } else {
                State::Failed {
                    kind: kind_from(kind_name(e.kind())),
                }
            };
            shared.finish(id, state, history);
        }
    }
}

/// Report `Uncertain`. The journal is only moved to `Uncertain` when it does not
/// already hold this job as `Succeeded` (a write that landed before its
/// directory fsync failed): a durable success is never downgraded or erased.
fn uncertain(inner: &Inner, id: JobId, history: usize) {
    let _ = persist(inner, |d| {
        if let Some(j) = d.jobs.iter_mut().find(|j| j.id == id.raw()) {
            if !matches!(j.state, Rec::Succeeded { .. }) {
                j.state = Rec::Uncertain;
            }
        }
    });
    let mut shared = lock(inner);
    shared.running = None;
    shared.finish(id, State::Uncertain, history);
}

fn bad_journal() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "capture journal inconsistent")
}
fn parse_job(job: &str) -> Option<u64> {
    job.strip_prefix('j')?.parse().ok()
}

/// Verify the journal against the store and resolve unfinished jobs.
#[allow(clippy::too_many_lines)]
fn recover(
    store: &ArtifactStore,
    loaded: Option<Doc>,
    config: &Config,
) -> io::Result<(Doc, Shared)> {
    let arts = store.snapshot(|_| true);
    let Some(mut doc) = loaded else {
        if !arts.is_empty() {
            return Err(bad_journal());
        }
        let doc = Doc::new();
        let shared = Shared {
            next: doc.job_high,
            next_generation: doc.generation_high,
            ..Shared::default()
        };
        return Ok((doc, shared));
    };
    if doc.job_high == 0 || doc.generation_high == 0 || doc.good.len() > config.max_fills {
        return Err(bad_journal());
    }
    // Every historic artifact must lie below the high-waters (stale journal
    // otherwise), whether or not its job is still retained.
    for a in &arts {
        let job = parse_job(&a.provenance.job).ok_or_else(bad_journal)?;
        if job >= doc.job_high || a.provenance.generation >= doc.generation_high {
            return Err(bad_journal());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut tokens = std::collections::BTreeSet::new();
    for j in &doc.jobs {
        if j.id == 0
            || j.id >= doc.job_high
            || j.generation >= doc.generation_high
            || !seen.insert(j.id)
            || bad_name(&j.owner)
            || bad_name(&j.fill)
            || j.correlation.as_deref().is_some_and(|t| {
                !valid_correlation(t)
                    || !tokens.insert((j.owner.clone(), j.fill.clone(), t.to_owned()))
            })
        {
            return Err(bad_journal());
        }
    }
    let mut good = BTreeMap::new();
    for g in &doc.good {
        if bad_name(&g.owner) || bad_name(&g.fill) || g.job >= doc.job_high {
            return Err(bad_journal());
        }
        verify_good(store, g, &doc)?;
        if good
            .insert((g.owner.clone(), g.fill.clone()), g.clone())
            .is_some()
        {
            return Err(bad_journal());
        }
    }
    let hw = Doc {
        jobs: Vec::new(),
        good: Vec::new(),
        ..doc.clone()
    };
    for j in &mut doc.jobs {
        let unresolved = match j.state {
            Rec::Queued => {
                // The request payload is deliberately not journaled. An abrupt
                // restart cannot distinguish an acknowledged queued intent from
                // a crash during admission; do not report an operator cancel.
                j.state = Rec::Uncertain;
                continue;
            }
            Rec::Succeeded { .. } => continue,
            Rec::Running => true,
            _ => false,
        };
        if j.generation == 0 {
            if unresolved {
                j.state = Rec::Failed {
                    kind: kind_name(io::ErrorKind::Interrupted).to_owned(),
                };
            }
            continue;
        }
        // Reconcile EVERY launched generation against the store, whatever the
        // journal last recorded: a verified commit is Succeeded even if the
        // outcome write failed or was recorded as failed/uncertain.
        let found = arts.iter().find(|a| {
            parse_job(&a.provenance.job) == Some(j.id)
                && a.provenance.owner == j.owner
                && a.provenance.fill == j.fill
                && a.provenance.generation == j.generation
        });
        match found {
            None if unresolved => {
                // A reserved id with no manifest may be a half-published commit
                // (content renamed, manifest missing): never a clean failure.
                j.state = if j.reserved.is_some() {
                    Rec::Uncertain
                } else {
                    Rec::Failed {
                        kind: kind_name(io::ErrorKind::Interrupted).to_owned(),
                    }
                };
            }
            None => {}
            Some(a) => {
                let g = GoodRec {
                    owner: j.owner.clone(),
                    fill: j.fill.clone(),
                    artifact: a.id.clone(),
                    generation: j.generation,
                    bytes: a.bytes,
                    sha256: a.sha256.clone(),
                    job: j.id,
                };
                if verify_good(store, &g, &hw).is_ok() {
                    let key = (g.owner.clone(), g.fill.clone());
                    let newer = good
                        .get(&key)
                        .is_none_or(|old| old.generation < g.generation);
                    // A shrunken cap never downgrades a verified success: it
                    // only skips the latest-good insert for lack of room.
                    let room = good.contains_key(&key) || good.len() < config.max_fills;
                    if newer && room {
                        good.insert(key, g);
                    }
                    j.state = Rec::Succeeded {
                        artifact: a.id.clone(),
                    };
                } else {
                    j.state = Rec::Uncertain;
                }
            }
        }
    }
    doc.good = good.values().cloned().collect();
    doc.prune(config.history);
    let now = Instant::now();
    let mut shared = Shared {
        next: doc.job_high,
        next_generation: doc.generation_high,
        good: good
            .into_iter()
            .map(|(k, g)| {
                (
                    k,
                    Good {
                        artifact: g.artifact,
                        bytes: g.bytes,
                        sha256: g.sha256,
                        job: JobId(g.job),
                        at: now,
                    },
                )
            })
            .collect(),
        ..Shared::default()
    };
    for j in &doc.jobs {
        let state = match &j.state {
            Rec::Succeeded { artifact } => State::Succeeded {
                artifact: artifact.clone(),
            },
            Rec::Failed { kind } => State::Failed {
                kind: kind_from(kind),
            },
            Rec::Uncertain => State::Uncertain,
            _ => State::Cancelled,
        };
        shared.jobs.insert(
            JobId(j.id),
            Entry {
                owner: j.owner.clone(),
                fill: j.fill.clone(),
                state,
                submitted: now,
                finished: Some(now),
                cancelling: false,
                payload: None,
                progress: None,
                resource: None,
                meta: None,
                correlation: j.correlation.clone(),
            },
        );
        shared.finished.push_back(JobId(j.id));
    }
    Ok((doc, shared))
}

/// Re-verify a latest-good record: owner-scoped pin plus provenance.
fn verify_good(store: &ArtifactStore, g: &GoodRec, doc: &Doc) -> io::Result<()> {
    let lease = store.pin(&g.artifact, &g.owner, |_| true)?;
    let a = lease.artifact();
    if a.provenance.owner != g.owner
        || a.provenance.fill != g.fill
        || a.provenance.job != format!("j{}", g.job)
        || a.provenance.generation != g.generation
        || a.provenance.generation >= doc.generation_high
        || a.bytes != g.bytes
        || a.sha256 != g.sha256
    {
        return Err(bad_journal());
    }
    Ok(())
}
