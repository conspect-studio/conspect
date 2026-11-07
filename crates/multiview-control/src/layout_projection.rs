//! Repair of the saved-layout projection (`layouts.db` or the in-memory
//! repository) after the desired envelope has committed.
//!
//! `desired.json` is the only truth for accepted layouts; the repository is a
//! replaceable copy. A projection write can therefore fail after a commit
//! without changing any API answer, because every layout read comes from the
//! envelope. This module owns the rest of that contract:
//!
//! * [`after_commit`] runs after every commit. It waits for one attempt, so a
//!   healthy projection has converged when the response leaves, but only for
//!   [`FIRST_ATTEMPT_WAIT`]: a stalled `SQLite` store cannot hold a response or
//!   the config-mutation lock any longer than that.
//! * One background task at a time reconciles the repository to the envelope it
//!   re-reads on each attempt, so a stale attempt cannot regress a newer commit.
//! * A failure sets [`LayoutProjectionStatus::degraded`] (served on
//!   `GET /api/v1/config/boot-model`, logged once at `error`) and retries with a
//!   bounded exponential backoff. Each later commit wakes the task with a fresh
//!   budget, and the next boot reconciles again.
//!
//! Isolation (invariants #1/#10): control-plane tenancy only. Repository calls
//! run on the blocking pool; the engine never sees any of this.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ControlError, ControlResult};
use crate::repository::LayoutInput;
use crate::state::AppState;

/// Attempts per commit before the task stops and waits for the next commit.
const DEFAULT_MAX_ATTEMPTS: u32 = 5;
/// The first retry delay; each further retry doubles it.
const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(250);
/// The longest a commit waits for its first projection attempt.
const FIRST_ATTEMPT_WAIT: Duration = Duration::from_secs(1);

/// The projection's health as reported on `GET /api/v1/config/boot-model`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LayoutProjectionStatus {
    /// Whether the projection differs from the accepted envelope because a
    /// write failed. Layout reads stay correct either way: they use the envelope.
    pub degraded: bool,
    /// Whether a repair is running or waiting for its next attempt. `degraded`
    /// with `retrying: false` means the retry budget is spent, and the next
    /// commit or restart repairs the projection.
    pub retrying: bool,
    /// Failed attempts since the projection was last healthy.
    pub consecutive_failures: u32,
    /// The most recent failure, while degraded. Withheld from a scoped principal,
    /// because a repository error can name any layout.
    pub last_error: Option<String>,
    /// Unix milliseconds of the first failure of the current fault.
    pub degraded_since_ms: Option<i64>,
    /// Unix milliseconds of the last repair that cleared a fault.
    pub repaired_at_ms: Option<i64>,
}

#[derive(Debug, Default)]
struct Inner {
    status: LayoutProjectionStatus,
    /// Requests made so far; each commit takes the next number.
    requested: u64,
    /// The newest request an attempt has started on.
    claimed: u64,
}

/// The shared repair state (see the module docs).
#[derive(Debug)]
pub struct LayoutProjection {
    inner: Mutex<Inner>,
    /// The newest request an attempt has finished on, pass or fail.
    finished: tokio::sync::watch::Sender<u64>,
    /// Ends a backoff sleep early when a commit lands.
    wake: tokio::sync::Notify,
    max_attempts: u32,
    base_delay: Duration,
}

impl Default for LayoutProjection {
    fn default() -> Self {
        Self::with_retry_policy(DEFAULT_MAX_ATTEMPTS, DEFAULT_BASE_DELAY)
    }
}

impl LayoutProjection {
    /// A repair state with an explicit retry budget and first delay.
    #[must_use]
    pub fn with_retry_policy(max_attempts: u32, base_delay: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            finished: tokio::sync::watch::Sender::new(0),
            wake: tokio::sync::Notify::new(),
            max_attempts: max_attempts.max(1),
            base_delay,
        }
    }

    /// The current health snapshot.
    #[must_use]
    pub fn status(&self) -> LayoutProjectionStatus {
        self.lock().status.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mark work pending. Returns the request number, and `true` when the
    /// caller must start the task.
    fn mark_dirty(&self) -> (u64, bool) {
        let mut inner = self.lock();
        inner.requested = inner.requested.saturating_add(1);
        let start = !std::mem::replace(&mut inner.status.retrying, true);
        (inner.requested, start)
    }

    /// Claim the pending work for one attempt. Returns the request number it
    /// covers, and `true` when a commit landed since the last claim, which
    /// grants a fresh retry budget.
    fn begin_attempt(&self) -> (u64, bool) {
        let mut inner = self.lock();
        let fresh = inner.requested != inner.claimed;
        inner.claimed = inner.requested;
        (inner.claimed, fresh)
    }

    /// Record a clean attempt. Returns `true` when the task may exit.
    fn record_success(&self, now_ms: i64) -> bool {
        let mut inner = self.lock();
        if inner.status.degraded {
            tracing::info!(
                failures = inner.status.consecutive_failures,
                "layout projection repaired from the desired envelope"
            );
            inner.status.repaired_at_ms = Some(now_ms);
        }
        inner.status.degraded = false;
        inner.status.consecutive_failures = 0;
        inner.status.last_error = None;
        inner.status.degraded_since_ms = None;
        let finished = inner.requested == inner.claimed;
        if finished {
            inner.status.retrying = false;
        }
        finished
    }

    /// Record failed attempt number `attempt`. Returns the delay before the next
    /// attempt, or [`None`] when the budget is spent and the task must exit.
    fn record_failure(&self, attempt: u32, error: &str, now_ms: i64) -> Option<Duration> {
        let mut inner = self.lock();
        if inner.status.degraded {
            tracing::warn!(%error, attempt, "layout projection repair attempt failed");
        } else {
            tracing::error!(
                %error,
                "layout projection degraded; the desired envelope stays authoritative"
            );
            inner.status.degraded = true;
            inner.status.degraded_since_ms = Some(now_ms);
        }
        inner.status.consecutive_failures = inner.status.consecutive_failures.saturating_add(1);
        inner.status.last_error = Some(error.to_owned());
        if attempt >= self.max_attempts && inner.requested == inner.claimed {
            inner.status.retrying = false;
            tracing::error!(
                attempt,
                "layout projection retry budget spent; the next commit or restart repairs it"
            );
            return None;
        }
        let doublings = attempt.saturating_sub(1).min(16);
        Some(self.base_delay.saturating_mul(1_u32 << doublings))
    }
}

/// Start a reconcile of the repository to the accepted envelope and return at
/// once. Boot calls this; a commit uses [`after_commit`].
pub fn request(state: &AppState) {
    if state.boot_model.is_some() {
        start(state);
    }
}

/// Reconcile after a commit, waiting a bounded time for one attempt that saw it.
pub(crate) async fn after_commit(state: &AppState) {
    let mut finished = state.layout_projection.finished.subscribe();
    let request = start(state);
    let settled = finished.wait_for(|done| *done >= request);
    if tokio::time::timeout(FIRST_ATTEMPT_WAIT, settled)
        .await
        .is_err()
    {
        tracing::warn!("layout projection is still writing; the commit stands");
    }
}

fn start(state: &AppState) -> u64 {
    let (request, spawn) = state.layout_projection.mark_dirty();
    if spawn {
        tokio::spawn(repair(state.clone()));
    } else {
        state.layout_projection.wake.notify_one();
    }
    request
}

async fn repair(state: AppState) {
    let projection = Arc::clone(&state.layout_projection);
    let mut attempt = 0_u32;
    loop {
        let (request, fresh) = projection.begin_attempt();
        attempt = if fresh { 1 } else { attempt.saturating_add(1) };
        let worker = state.clone();
        let outcome = tokio::task::spawn_blocking(move || reconcile(&worker))
            .await
            .unwrap_or_else(|error| {
                Err(ControlError::Repository(format!(
                    "layout projection task failed: {error}"
                )))
            });
        let now_ms = state.ack_now().as_nanos().div_euclid(1_000_000);
        let delay = match outcome {
            Ok(()) => projection.record_success(now_ms).then_some(None),
            Err(error) => Some(projection.record_failure(attempt, &error.to_string(), now_ms)),
        };
        projection.finished.send_replace(request);
        match delay {
            // Clean and nothing newer, or the budget is spent: the task ends.
            Some(None) => return,
            // Clean, but a commit landed during the attempt.
            None => {}
            Some(Some(delay)) => {
                let _ = tokio::time::timeout(delay, projection.wake.notified()).await;
            }
        }
    }
}

/// One idempotent pass: make the repository hold exactly the envelope's library.
fn reconcile(state: &AppState) -> ControlResult<()> {
    let Some(model) = &state.boot_model else {
        return Ok(());
    };
    let Some(desired) = model.accepted().map_err(ControlError::Repository)? else {
        return Ok(());
    };
    for entry in state.repository.list_layouts()? {
        if !desired
            .layouts
            .iter()
            .any(|layout| layout.id == entry.layout.id)
        {
            state.repository.delete_layout(&entry.layout.id)?;
        }
    }
    for layout in &desired.layouts {
        let input = LayoutInput {
            name: layout.name.clone(),
            body: layout.body.clone(),
        };
        match state.repository.get_layout(&layout.id) {
            Ok(entry) if entry.layout == *layout => {}
            Ok(_) => {
                state.repository.update_layout(&layout.id, input)?;
            }
            Err(ControlError::NotFound { .. }) => {
                state.repository.create_layout(&layout.id, input)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
