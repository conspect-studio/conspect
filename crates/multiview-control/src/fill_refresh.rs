//! Scheduled refresh of `media_fill` VT clips (issue #472).
//!
//! A control-plane scheduler re-captures each `refresh = { mode = "interval" }`
//! fill through the **same** capture [`Provider`](crate::web_capture::Provider)
//! path an operator uses, then loads the new artifact into the fill's player
//! through the same [`PlayerLoadProvider`](crate::player_load::PlayerLoadProvider).
//! It never runs on, or is awaited by, the engine data plane (invariants #1/#10):
//! it is a tokio task whose every provider call runs on the blocking pool.
//!
//! Contract (recorded in `docs/operations/web-capture-startup.md`):
//! * **Single-flight per fill**: a fill in `capturing`/`loading` is never started
//!   again until that run ends (success, failure or deadline).
//! * **Bounded concurrency**: at most [`MAX_IN_FLIGHT`] scheduled runs overall,
//!   at most [`MAX_FILLS`] scheduled fills; the capture service queue is never
//!   flooded by the scheduler.
//! * **`during_refresh = play_last`**: the player is only touched after a
//!   capture succeeded; the native load keeps the old clip on air until its ACK.
//!   A failed run leaves the old clip playing, records `last_error` and backs
//!   off `min(interval, 30 s * 2^(failures-1))`.
//! * **Restart idempotency**: due times sit on a stable per-fill slot grid
//!   (`offset = sha256(fill) mod interval`), and the capture `Idempotency-Key`
//!   is derived from `(fill, slot, body digest)`; the load key from
//!   `(fill, capture job)`. A restart inside a slot replays the same capture
//!   job through the durable idempotency ledger instead of running a second one.
//! * **Authorization**: config-declared fills run as a synthetic least-privilege
//!   principal `media-fill:<id>` (operator role, object scope exactly
//!   `[fill, player]`, no output/discovery scope); API-registered fills run as
//!   the registering API key, re-resolved from the key store before every run
//!   and every load, so revocation or re-scoping stops the schedule. Nothing is
//!   ever escalated beyond what that principal could do over HTTP.
use crate::{
    auth::{authorize_object, Action, Principal, Role},
    error::{ControlError, ControlResult},
    player_load::{LoadThen, PlayerLoadProvider},
    web_capture::{CaptureBody, CaptureMode, StepTiming, Terms, View, WINDY_TERMS_VERSION},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::Duration,
};

/// Most fills the scheduler tracks (the capture service's own `max_fills`).
pub const MAX_FILLS: usize = 32;
/// Most scheduled runs (capture or load phase) in flight at once, overall.
pub const MAX_IN_FLIGHT: usize = 1;
/// A capture job not terminal this long after admission fails the run.
pub const CAPTURE_DEADLINE_SECS: u64 = 300;
/// A native load not terminal this long after admission fails the run.
pub const LOAD_DEADLINE_SECS: u64 = 60;
/// First failure back-off; doubles per consecutive failure, capped at the interval.
pub const BACKOFF_BASE_SECS: u64 = 30;
/// Scheduler tick while nothing is in flight.
pub const TICK_IDLE: Duration = Duration::from_secs(30);
/// Scheduler tick while a run is in flight (job/load polling cadence).
pub const TICK_BUSY: Duration = Duration::from_secs(5);
/// Longest refresh interval, minutes (matches the config schema).
pub const MAX_MINUTES: u32 = 1440;
/// Longest stored error text.
const MAX_ERROR_CHARS: usize = 256;
/// Default capture cadence for a config fill (the config view has no fps).
const CONFIG_FPS: u32 = 10;
/// Default realtime frame count for a config fill (ignored in steps mode).
const CONFIG_FRAMES: u32 = 60;

/// Where a scheduled fill came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum FillSource {
    /// Declared in `[[media_fill]]`; edited through config, not this API.
    Config,
    /// Registered through `PUT /api/v1/media/fills/{id}/refresh` (runtime only).
    Api,
}

/// Which principal a run executes as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FillOwner {
    /// Synthetic least-privilege principal scoped to exactly `[fill, player]`.
    Declared,
    /// The registering API key, re-resolved before every run.
    Key(String),
}

/// One scheduled fill's immutable intent.
#[derive(Clone)]
pub struct FillSpec {
    /// Fill authorization object id.
    pub fill_id: String,
    /// Target media player.
    pub player: String,
    /// Refresh period in minutes (`1..=1440`).
    pub minutes: u32,
    /// Landing intent of each scheduled load.
    pub then: LoadThen,
    /// Capture request submitted every run (its `fill_id`/`target_player` match).
    pub body: CaptureBody,
    /// Run principal.
    pub owner: FillOwner,
    /// Origin of this schedule.
    pub source: FillSource,
}

/// When a newly installed fill is first due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstDue {
    /// Start of the slot containing `now` (due immediately; a replay inside
    /// that slot reuses the same capture key).
    CurrentSlot,
    /// Start of the next slot (the caller just captured by hand).
    NextSlot,
}

/// A capture job's state as the scheduler needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobPoll {
    /// Queued or running.
    Pending,
    /// Committed artifact.
    Succeeded {
        /// Artifact id.
        artifact: String,
    },
    /// Terminal without an artifact (failed, cancelled, uncertain).
    Failed(String),
}

/// A native load's state as the scheduler needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadPoll {
    /// Not yet adopted.
    Pending,
    /// Natively adopted.
    Completed,
    /// Failed or superseded.
    Failed(String),
}

/// The capture + load boundary. Every method is blocking and runs on the
/// management blocking pool, never on an engine thread.
pub trait RefreshBackend: Send + Sync + 'static {
    /// Submit a capture under `key`; returns the job id.
    /// # Errors
    /// Any admission refusal.
    fn submit(&self, p: &Principal, key: &str, body: &CaptureBody) -> ControlResult<u64>;
    /// Poll an actor-owned capture job.
    /// # Errors
    /// Unknown/foreign job or unavailable service.
    fn job(&self, p: &Principal, job: u64) -> ControlResult<JobPoll>;
    /// Pin the artifact and admit a native load; returns the operation id and state.
    /// # Errors
    /// Pin or load refusal.
    fn load(
        &self,
        p: &Principal,
        player: &str,
        key: &str,
        job: u64,
        artifact: &str,
        then: LoadThen,
    ) -> ControlResult<(u64, LoadPoll)>;
    /// Poll a native load operation.
    /// # Errors
    /// Unknown operation or unavailable provider.
    fn load_state(&self, p: &Principal, player: &str, op: u64) -> ControlResult<LoadPoll>;
}

/// The production backend over the capture provider and native load provider.
pub struct LiveBackend {
    capture: crate::web_capture::Provider,
    loader: Arc<dyn PlayerLoadProvider>,
}
impl LiveBackend {
    /// Wrap the running providers.
    #[must_use]
    pub fn new(capture: crate::web_capture::Provider, loader: Arc<dyn PlayerLoadProvider>) -> Self {
        Self { capture, loader }
    }
}
fn load_poll(state: &str) -> LoadPoll {
    match state {
        "completed" => LoadPoll::Completed,
        "pending" => LoadPoll::Pending,
        other => LoadPoll::Failed(format!("native load {other}")),
    }
}
impl RefreshBackend for LiveBackend {
    fn submit(&self, p: &Principal, key: &str, body: &CaptureBody) -> ControlResult<u64> {
        self.capture.submit(p, key, body).map(|a| a.job_id)
    }
    fn job(&self, p: &Principal, job: u64) -> ControlResult<JobPoll> {
        let view = self.capture.status(p, job)?;
        Ok(match (view.state, view.artifact_id) {
            ("succeeded", Some(artifact)) => JobPoll::Succeeded { artifact },
            ("queued" | "running", _) => JobPoll::Pending,
            (state, _) => JobPoll::Failed(format!("capture {state}")),
        })
    }
    fn load(
        &self,
        p: &Principal,
        player: &str,
        key: &str,
        job: u64,
        artifact: &str,
        then: LoadThen,
    ) -> ControlResult<(u64, LoadPoll)> {
        let lease = self.capture.pin_for_player(p, job, artifact)?;
        let reference = crate::player_load::ArtifactRef {
            capture_job_id: job,
            artifact_id: artifact.to_owned(),
        };
        let op = self.loader.load(player, p, key, reference, then, lease)?;
        Ok((op.operation, load_poll(op.state)))
    }
    fn load_state(&self, p: &Principal, player: &str, op: u64) -> ControlResult<LoadPoll> {
        Ok(load_poll(self.loader.operation(player, p, op)?.state))
    }
}

/// Public per-fill refresh status (no URL, credential or path).
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FillRefreshStatus {
    /// Fill id.
    pub fill_id: String,
    /// Target media player.
    pub player: String,
    /// Origin of the schedule.
    pub source: FillSource,
    /// Refresh period in minutes.
    pub minutes: u32,
    /// What the slot shows during a refresh (always `play_last` today).
    pub during_refresh: &'static str,
    /// Landing intent of scheduled loads.
    pub then: LoadThen,
    /// `idle`, `capturing` or `loading`.
    pub phase: &'static str,
    /// Whether a run is in flight.
    pub in_flight: bool,
    /// Next due time (RFC 3339 UTC); meaningful while idle.
    pub next_due: Option<String>,
    /// Last successful native load (RFC 3339 UTC).
    pub last_success_at: Option<String>,
    /// Artifact id of the last successful load.
    pub last_artifact_id: Option<String>,
    /// Capture job id of the current or last run (owner-scoped).
    pub last_capture_job_id: Option<u64>,
    /// Last failure, cleared on success.
    pub last_error: Option<String>,
    /// Consecutive failed runs (drives the back-off).
    pub consecutive_failures: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Capturing {
        job: u64,
        deadline: u64,
    },
    Loading {
        job: u64,
        artifact: String,
        op: u64,
        deadline: u64,
    },
}

struct Entry {
    spec: FillSpec,
    next_due: u64,
    phase: Phase,
    last_success_at: Option<u64>,
    last_artifact_id: Option<String>,
    last_job: Option<u64>,
    last_error: Option<String>,
    failures: u32,
}

/// Resolves an API key id to its current principal (`None` = revoked).
pub type PrincipalResolver = Arc<dyn Fn(&str) -> Option<Principal> + Send + Sync>;

/// The pure, clock-injected scheduler state machine. `now` is unix seconds.
pub struct Scheduler {
    entries: BTreeMap<String, Entry>,
    max_in_flight: usize,
}
impl Default for Scheduler {
    fn default() -> Self {
        Self::new(MAX_IN_FLIGHT)
    }
}

fn interval_secs(minutes: u32) -> u64 {
    u64::from(minutes.clamp(1, MAX_MINUTES)) * 60
}
/// Stable per-fill stagger inside the interval.
fn offset_secs(fill: &str, interval: u64) -> u64 {
    let digest = Sha256::digest(fill.as_bytes());
    let mut head = [0_u8; 8];
    for (dst, src) in head.iter_mut().zip(digest.iter()) {
        *dst = *src;
    }
    u64::from_be_bytes(head) % interval.max(1)
}
/// Slot index containing `now` and that slot's start.
fn slot_at(fill: &str, minutes: u32, now: u64) -> (u64, u64) {
    let interval = interval_secs(minutes);
    let offset = offset_secs(fill, interval);
    // Slot 0 starts at `offset - interval` so every `now >= 0` has a slot.
    let shifted = now + interval - offset;
    let slot = shifted / interval;
    (slot, (slot * interval + offset).saturating_sub(interval))
}
fn short_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(8)
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
/// Deterministic capture key for `(fill, slot, body)`.
#[must_use]
pub fn capture_key(spec: &FillSpec, slot: u64) -> String {
    let body = serde_json::to_vec(&spec.body).unwrap_or_default();
    format!(
        "fill-refresh:{}:{slot}:{}",
        short_digest(spec.fill_id.as_bytes()),
        short_digest(&body)
    )
}
/// Deterministic load key for `(fill, job)`.
#[must_use]
pub fn load_key(spec: &FillSpec, job: u64) -> String {
    format!(
        "fill-refresh-load:{}:{job}",
        short_digest(spec.fill_id.as_bytes())
    )
}
fn bounded(text: &str) -> String {
    text.chars().take(MAX_ERROR_CHARS).collect()
}
fn rfc3339(secs: u64) -> Option<String> {
    let secs = i64::try_from(secs).ok()?;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// The synthetic least-privilege principal of a config-declared fill.
#[must_use]
pub fn declared_principal(fill: &str, player: &str) -> Principal {
    Principal {
        key_id: format!("media-fill:{fill}"),
        role: Role::Operator,
        scoped_object_ids: Some(vec![fill.to_owned(), player.to_owned()]),
        scoped_output_ids: Some(Vec::new()),
        scoped_discovery_domains: Some(Vec::new()),
    }
}

impl Scheduler {
    /// An empty scheduler with an explicit global in-flight cap.
    #[must_use]
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_in_flight: max_in_flight.max(1),
        }
    }

    /// Install or replace a fill. A replaced fill keeps its in-flight run and history.
    /// # Errors
    /// Out-of-range interval, mismatched body identity, or [`MAX_FILLS`] reached.
    pub fn upsert(&mut self, spec: FillSpec, now: u64, first: FirstDue) -> ControlResult<()> {
        if !(1..=MAX_MINUTES).contains(&spec.minutes) {
            return Err(ControlError::Validation(format!(
                "refresh minutes must be 1..={MAX_MINUTES}"
            )));
        }
        if spec.body.fill_id != spec.fill_id
            || spec.body.target_player.as_deref() != Some(spec.player.as_str())
        {
            return Err(ControlError::Validation(
                "capture body must name this fill and its player".into(),
            ));
        }
        let (_, start) = slot_at(&spec.fill_id, spec.minutes, now);
        let due = match first {
            FirstDue::CurrentSlot => start,
            FirstDue::NextSlot => start + interval_secs(spec.minutes),
        };
        if let Some(entry) = self.entries.get_mut(&spec.fill_id) {
            entry.spec = spec;
            entry.next_due = due;
            return Ok(());
        }
        if self.entries.len() >= MAX_FILLS {
            return Err(ControlError::Conflict(format!(
                "at most {MAX_FILLS} scheduled fills"
            )));
        }
        self.entries.insert(
            spec.fill_id.clone(),
            Entry {
                spec,
                next_due: due,
                phase: Phase::Idle,
                last_success_at: None,
                last_artifact_id: None,
                last_job: None,
                last_error: None,
                failures: 0,
            },
        );
        Ok(())
    }

    /// Remove a fill; an in-flight capture is left to finish but never loaded.
    pub fn remove(&mut self, fill: &str) -> bool {
        self.entries.remove(fill).is_some()
    }

    /// Source of a scheduled fill, if any.
    #[must_use]
    pub fn source(&self, fill: &str) -> Option<FillSource> {
        self.entries.get(fill).map(|e| e.spec.source)
    }

    /// Runs in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.phase != Phase::Idle)
            .count()
    }

    /// Advance every in-flight run, then start due fills up to the global cap,
    /// earliest-due first. Never waits: each backend call is one bounded,
    /// non-waiting admission or status read.
    pub fn tick(&mut self, now: u64, backend: &dyn RefreshBackend, resolve: &PrincipalResolver) {
        for entry in self.entries.values_mut() {
            if entry.phase != Phase::Idle {
                advance(entry, now, backend, resolve);
            }
        }
        let mut due: Vec<(u64, String)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.phase == Phase::Idle && e.next_due <= now)
            .map(|(id, e)| (e.next_due, id.clone()))
            .collect();
        due.sort();
        let mut running = self.in_flight();
        for (_, id) in due {
            if running >= self.max_in_flight {
                break;
            }
            if let Some(entry) = self.entries.get_mut(&id) {
                start(entry, now, backend, resolve);
                if entry.phase != Phase::Idle {
                    running += 1;
                }
            }
        }
    }

    /// Status projection of every fill.
    #[must_use]
    pub fn status(&self) -> Vec<FillRefreshStatus> {
        self.entries
            .values()
            .map(|e| FillRefreshStatus {
                fill_id: e.spec.fill_id.clone(),
                player: e.spec.player.clone(),
                source: e.spec.source,
                minutes: e.spec.minutes,
                during_refresh: "play_last",
                then: e.spec.then,
                phase: match e.phase {
                    Phase::Idle => "idle",
                    Phase::Capturing { .. } => "capturing",
                    Phase::Loading { .. } => "loading",
                },
                in_flight: e.phase != Phase::Idle,
                next_due: rfc3339(e.next_due),
                last_success_at: e.last_success_at.and_then(rfc3339),
                last_artifact_id: e.last_artifact_id.clone(),
                last_capture_job_id: e.last_job,
                last_error: e.last_error.clone(),
                consecutive_failures: e.failures,
            })
            .collect()
    }
}

/// The run principal, re-resolved and re-authorized for `[fill, player]`.
fn run_principal(spec: &FillSpec, resolve: &PrincipalResolver) -> ControlResult<Principal> {
    let principal = match &spec.owner {
        FillOwner::Declared => declared_principal(&spec.fill_id, &spec.player),
        FillOwner::Key(key) => resolve(key)
            .ok_or_else(|| ControlError::Forbidden("schedule owner key is revoked".into()))?,
    };
    principal.role.require(Action::Write)?;
    authorize_object(&principal, &spec.fill_id)?;
    authorize_object(&principal, &spec.player)?;
    Ok(principal)
}

fn fail(entry: &mut Entry, now: u64, error: &str) {
    entry.phase = Phase::Idle;
    entry.failures = entry.failures.saturating_add(1);
    entry.last_error = Some(bounded(error));
    let shift = entry.failures.saturating_sub(1).min(16);
    let backoff = BACKOFF_BASE_SECS.saturating_mul(1_u64 << shift);
    entry.next_due = now + backoff.min(interval_secs(entry.spec.minutes));
}

fn succeed(entry: &mut Entry, now: u64, artifact: String) {
    entry.phase = Phase::Idle;
    entry.failures = 0;
    entry.last_error = None;
    entry.last_success_at = Some(now);
    entry.last_artifact_id = Some(artifact);
    let (_, start) = slot_at(&entry.spec.fill_id, entry.spec.minutes, now);
    entry.next_due = start + interval_secs(entry.spec.minutes);
}

fn start(entry: &mut Entry, now: u64, backend: &dyn RefreshBackend, resolve: &PrincipalResolver) {
    let principal = match run_principal(&entry.spec, resolve) {
        Ok(p) => p,
        Err(e) => return fail(entry, now, &e.to_string()),
    };
    let (slot, _) = slot_at(&entry.spec.fill_id, entry.spec.minutes, now);
    let key = capture_key(&entry.spec, slot);
    match backend.submit(&principal, &key, &entry.spec.body) {
        Ok(job) => {
            entry.last_job = Some(job);
            entry.phase = Phase::Capturing {
                job,
                deadline: now + CAPTURE_DEADLINE_SECS,
            };
        }
        Err(e) => fail(entry, now, &e.to_string()),
    }
}

fn advance(entry: &mut Entry, now: u64, backend: &dyn RefreshBackend, resolve: &PrincipalResolver) {
    let principal = match run_principal(&entry.spec, resolve) {
        Ok(p) => p,
        Err(e) => return fail(entry, now, &e.to_string()),
    };
    match entry.phase.clone() {
        Phase::Idle => {}
        Phase::Capturing { job, deadline } => match backend.job(&principal, job) {
            Ok(JobPoll::Pending) if now >= deadline => {
                fail(entry, now, "capture deadline exceeded");
            }
            Ok(JobPoll::Pending) => {}
            Ok(JobPoll::Succeeded { artifact }) => {
                let key = load_key(&entry.spec, job);
                let player = entry.spec.player.clone();
                match backend.load(&principal, &player, &key, job, &artifact, entry.spec.then) {
                    Ok((_, LoadPoll::Completed)) => succeed(entry, now, artifact),
                    Ok((op, LoadPoll::Pending)) => {
                        entry.phase = Phase::Loading {
                            job,
                            artifact,
                            op,
                            deadline: now + LOAD_DEADLINE_SECS,
                        };
                    }
                    Ok((_, LoadPoll::Failed(reason))) => fail(entry, now, &reason),
                    Err(e) => fail(entry, now, &e.to_string()),
                }
            }
            Ok(JobPoll::Failed(reason)) => fail(entry, now, &reason),
            Err(e) => fail(entry, now, &e.to_string()),
        },
        Phase::Loading {
            job: _,
            artifact,
            op,
            deadline,
        } => match backend.load_state(&principal, &entry.spec.player, op) {
            Ok(LoadPoll::Completed) => succeed(entry, now, artifact),
            Ok(LoadPoll::Pending) if now >= deadline => {
                fail(entry, now, "native load deadline exceeded");
            }
            Ok(LoadPoll::Pending) => {}
            Ok(LoadPoll::Failed(reason)) => fail(entry, now, &reason),
            Err(e) => fail(entry, now, &e.to_string()),
        },
    }
}

/// Shared handle: the scheduler core plus a published status snapshot that
/// readers clone without waiting on a tick.
pub struct FillRefresh {
    core: Mutex<Scheduler>,
    snapshot: RwLock<Arc<Vec<FillRefreshStatus>>>,
}
impl Default for FillRefresh {
    fn default() -> Self {
        Self::new(Scheduler::default())
    }
}
impl FillRefresh {
    /// Wrap a scheduler core.
    #[must_use]
    pub fn new(core: Scheduler) -> Self {
        let snapshot = RwLock::new(Arc::new(core.status()));
        Self {
            core: Mutex::new(core),
            snapshot,
        }
    }
    /// Latest published status (never waits on a running tick).
    #[must_use]
    pub fn status(&self) -> Arc<Vec<FillRefreshStatus>> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(PoisonError::into_inner))
    }
    /// Mutate the core and publish its status. Blocking (may wait for a tick):
    /// call on the blocking pool.
    pub fn with_core<T>(&self, f: impl FnOnce(&mut Scheduler) -> T) -> T {
        let mut core = self.core.lock().unwrap_or_else(PoisonError::into_inner);
        let out = f(&mut core);
        let status = Arc::new(core.status());
        drop(core);
        *self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner) = status;
        out
    }
}

/// Map a config `[[media_fill]]` to a schedulable spec, or why it is not one.
/// # Errors
/// Not an interval `web_capture` Windy fill, or terms not for the current version.
pub fn spec_from_config(fill: &multiview_config::MediaFill) -> Result<FillSpec, String> {
    use multiview_config::{media_fill as mf, FillRefresh as Refresh, MediaFillKind};
    let Refresh::Interval { minutes } = fill.refresh else {
        return Err("not an interval refresh".into());
    };
    let MediaFillKind::WebCapture { template, view } = &fill.kind else {
        return Err("only web_capture fills are schedulable".into());
    };
    if template != "windy" {
        return Err(format!("template {template:?} is not schedulable"));
    }
    let terms = fill
        .terms
        .as_ref()
        .ok_or_else(|| "web_capture fill has no terms acceptance".to_owned())?;
    let version = format!("{}-{}", terms.template, terms.terms_version);
    if terms.terms_version != WINDY_TERMS_VERSION && version != WINDY_TERMS_VERSION {
        return Err("accepted terms version is not the current Windy terms".into());
    }
    let number = |key: &str| match view.params.get(key) {
        Some(mf::ParamValue::Float(v)) => Some(*v),
        Some(mf::ParamValue::Integer(v)) => i32::try_from(*v).ok().map(f64::from),
        _ => None,
    };
    let lat = number("lat").ok_or_else(|| "view.params.lat missing".to_owned())?;
    let lon = number("lon").ok_or_else(|| "view.params.lon missing".to_owned())?;
    let zoom = match view.params.get("zoom") {
        Some(mf::ParamValue::Integer(z)) => u8::try_from(*z).map_err(|_| "zoom out of range")?,
        _ => return Err("view.params.zoom missing".into()),
    };
    let overlay = match view.params.get("overlay") {
        Some(mf::ParamValue::String(s)) => s.clone(),
        _ => return Err("view.params.overlay missing".into()),
    };
    let capture = match view.capture {
        mf::CaptureMode::Steps => CaptureMode::Steps,
        _ => CaptureMode::Realtime,
    };
    let timing = match view.timing {
        mf::FrameTiming::Equal => StepTiming::Equal,
        _ => StepTiming::Proportional,
    };
    let body = CaptureBody {
        fill_id: fill.id.clone(),
        target_player: Some(fill.player.clone()),
        view: View {
            lat,
            lon,
            zoom,
            overlay,
            width: view.width,
            height: view.height,
            fps: CONFIG_FPS,
            frames: CONFIG_FRAMES,
            public: true,
            capture: Some(capture),
            timing,
            frame_ms: view.frame_ms,
            end_hold_ms: view.end_hold_ms,
            max_steps: multiview_webengine::capture::DEFAULT_MAX_STEPS,
        },
        terms: Terms {
            accepted: true,
            version: WINDY_TERMS_VERSION.to_owned(),
        },
    };
    Ok(FillSpec {
        fill_id: fill.id.clone(),
        player: fill.player.clone(),
        minutes,
        then: LoadThen::Resume,
        body,
        owner: FillOwner::Declared,
        source: FillSource::Config,
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Install the config fills and spawn the control-plane scheduler task on the
/// caller's (management) runtime. Each tick runs on the blocking pool and the
/// next tick waits for it, so ticks never overlap and nothing grows.
#[must_use = "dropping the handle does not stop the task; abort it to stop refreshing"]
pub fn spawn_scheduler(
    refresh: Arc<FillRefresh>,
    fills: &[multiview_config::MediaFill],
    backend: Arc<dyn RefreshBackend>,
    resolve: PrincipalResolver,
) -> tokio::task::JoinHandle<()> {
    let now = unix_now();
    refresh.with_core(|core| {
        for fill in fills {
            match spec_from_config(fill) {
                Ok(spec) => {
                    if let Err(e) = core.upsert(spec, now, FirstDue::CurrentSlot) {
                        tracing::warn!(fill = %fill.id, error = %e, "media_fill refresh not scheduled");
                    }
                }
                Err(reason) if matches!(fill.refresh, multiview_config::FillRefresh::Interval { .. }) => {
                    tracing::warn!(fill = %fill.id, %reason, "media_fill refresh not scheduled");
                }
                Err(_) => {}
            }
        }
    });
    tokio::spawn(async move {
        loop {
            let refresh2 = Arc::clone(&refresh);
            let backend2 = Arc::clone(&backend);
            let resolve2 = Arc::clone(&resolve);
            let busy = tokio::task::spawn_blocking(move || {
                refresh2.with_core(|core| {
                    core.tick(unix_now(), backend2.as_ref(), &resolve2);
                    core.in_flight() > 0
                })
            })
            .await
            .unwrap_or(false);
            tokio::time::sleep(if busy { TICK_BUSY } else { TICK_IDLE }).await;
        }
    })
}

/// Resolve a schedule owner key to its current principal from the live key
/// store (auth-disabled deployments map the local-admin identity back).
#[must_use]
pub fn principal_resolver(state: &crate::state::AppState) -> PrincipalResolver {
    let keys = Arc::clone(&state.api_keys);
    let auth_disabled = state.auth_disabled;
    Arc::new(move |key: &str| {
        let local = Principal::local_admin();
        if auth_disabled && key == local.key_id {
            return Some(local);
        }
        keys.principal_for_key(key)
    })
}

/// [`spawn_scheduler`] over a finished [`AppState`]'s capture and load providers.
#[must_use = "dropping the handle does not stop the task; abort it to stop refreshing"]
pub fn spawn_for_state(
    state: &crate::state::AppState,
    fills: &[multiview_config::MediaFill],
) -> tokio::task::JoinHandle<()> {
    spawn_scheduler(
        Arc::clone(&state.fill_refresh),
        fills,
        Arc::new(LiveBackend::new(
            state.web_capture.clone(),
            Arc::clone(&state.player_load),
        )),
        principal_resolver(state),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests;
