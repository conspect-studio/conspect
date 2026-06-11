//! The actor **supervisor** (invariant #2 resilience; isolates faults from the
//! output clock).
//!
//! Input and output work runs in supervised actors — independent `tokio` tasks
//! that the supervisor spawns, monitors, and **restarts with bounded backoff**
//! when they fail. The supervisor is on the control plane; the output clock and
//! drive loop run independently. A crashing actor (a dead RTSP source, an output
//! transport that errored) is contained: it is restarted on its own schedule and
//! **never takes the output clock down with it** — the compositor simply samples
//! that tile's held/`NoSignal` frame in the meantime.
//!
//! The restart policy is a pure, deterministic [`RestartPolicy`] (capped
//! exponential backoff with a max-restarts budget over a window), so it is
//! unit-testable with no real time. The async [`Supervisor`] drives a
//! restartable [`Actor`] using that policy.
use std::time::Duration;

use tracing::{info, warn};

/// Why an actor's run ended, deciding whether the supervisor restarts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorExit {
    /// The actor completed its work and should **not** be restarted.
    Completed,
    /// The actor failed and should be restarted per the [`RestartPolicy`].
    Failed,
}

/// What the supervisor decided to do after an actor run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart the actor after waiting `backoff`.
    Restart {
        /// The backoff to wait before the next attempt.
        backoff: Duration,
    },
    /// Stop supervising: the actor completed, or the restart budget is spent.
    Stop {
        /// Why supervision stopped.
        reason: StopReason,
    },
}

/// Why the supervisor stopped restarting an actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The actor signalled clean completion.
    Completed,
    /// The actor exhausted its restart budget within the window.
    BudgetExhausted,
}

/// A deterministic, capped-exponential-backoff restart policy with a budget.
///
/// On each consecutive failure the backoff doubles from `base` up to `max`
/// (capped). The supervisor permits at most `max_restarts` restarts; once that
/// budget is spent it stops (the fault is escalated rather than hot-looping
/// forever). Because the policy is a pure function of the consecutive-failure
/// count it is fully unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    base: Duration,
    max: Duration,
    max_restarts: u32,
}

impl RestartPolicy {
    /// Construct a policy. A `base` of zero is promoted to 1 ms so the backoff
    /// still grows; `max` is clamped to be at least `base`.
    #[must_use]
    pub fn new(base: Duration, max: Duration, max_restarts: u32) -> Self {
        let base = if base.is_zero() {
            Duration::from_millis(1)
        } else {
            base
        };
        let max = if max < base { base } else { max };
        Self {
            base,
            max,
            max_restarts,
        }
    }

    /// A sensible default: 100 ms base, 30 s cap, up to 1000 restarts.
    #[must_use]
    pub fn default_policy() -> Self {
        Self::new(Duration::from_millis(100), Duration::from_secs(30), 1_000)
    }

    /// The maximum number of restarts permitted.
    #[must_use]
    pub const fn max_restarts(&self) -> u32 {
        self.max_restarts
    }

    /// The backoff to wait before the restart following `consecutive_failures`
    /// prior failures (so the first failure -> `consecutive_failures == 1`).
    ///
    /// Capped exponential: `min(base * 2^(n-1), max)`. Saturating throughout —
    /// a huge failure count clamps to `max`, never overflows or panics.
    #[must_use]
    pub fn backoff(&self, consecutive_failures: u32) -> Duration {
        if consecutive_failures == 0 {
            return Duration::ZERO;
        }
        let shift = consecutive_failures.saturating_sub(1).min(63);
        // `Duration::as_nanos()` already yields `u128`.
        let base_nanos = self.base.as_nanos();
        // `base_nanos << shift`, saturating into the `max` cap.
        let scaled = base_nanos
            .checked_shl(shift)
            .unwrap_or(u128::MAX)
            .min(self.max.as_nanos());
        let nanos = u64::try_from(scaled).unwrap_or(u64::MAX);
        Duration::from_nanos(nanos).min(self.max)
    }

    /// Decide what to do after a run ended with `exit`, given how many restarts
    /// have already been spent.
    ///
    /// * [`ActorExit::Completed`] -> stop ([`StopReason::Completed`]).
    /// * [`ActorExit::Failed`] with budget remaining -> restart after
    ///   [`RestartPolicy::backoff`].
    /// * [`ActorExit::Failed`] with budget spent -> stop
    ///   ([`StopReason::BudgetExhausted`]).
    #[must_use]
    pub fn decide(&self, exit: ActorExit, restarts_used: u32) -> RestartDecision {
        match exit {
            ActorExit::Completed => RestartDecision::Stop {
                reason: StopReason::Completed,
            },
            ActorExit::Failed => {
                if restarts_used >= self.max_restarts {
                    RestartDecision::Stop {
                        reason: StopReason::BudgetExhausted,
                    }
                } else {
                    // The upcoming attempt is restart number `restarts_used + 1`.
                    RestartDecision::Restart {
                        backoff: self.backoff(restarts_used.saturating_add(1)),
                    }
                }
            }
        }
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A restartable unit of supervised work (an input or output actor).
///
/// Each [`Actor::run`] is one attempt: it returns when the work finishes or
/// fails. The supervisor calls `run` again (after a backoff) on failure. An
/// implementation must be cancel-safe and own no resource that, if dropped on
/// restart, would corrupt shared state.
#[allow(async_fn_in_trait)]
// reason: this trait is consumed only inside the engine (the supervisor drives
// it directly); we do not need `Send`-bound futures via the `trait-variant`
// crate here, and adding it would pull an external dep for no behavioural gain.
pub trait Actor {
    /// A stable name for diagnostics/tracing.
    fn name(&self) -> &str;

    /// Run one attempt to completion or failure.
    async fn run(&mut self) -> ActorExit;
}

/// Drives a single [`Actor`] under a [`RestartPolicy`], restarting it with
/// bounded backoff on failure and stopping when it completes or exhausts its
/// budget.
///
/// Runs as its own `tokio` task, fully decoupled from the output clock: nothing
/// in this loop can block, pace, or crash the data plane. The total restarts
/// performed are returned so a caller/test can assert the policy was honoured.
#[derive(Debug, Clone)]
pub struct Supervisor {
    policy: RestartPolicy,
}

impl Supervisor {
    /// Construct a supervisor with the given restart policy.
    #[must_use]
    pub fn new(policy: RestartPolicy) -> Self {
        Self { policy }
    }

    /// The restart policy in force.
    #[must_use]
    pub const fn policy(&self) -> RestartPolicy {
        self.policy
    }

    /// Supervise `actor` until it completes cleanly or exhausts its restart
    /// budget. Returns the number of restarts performed and why supervision
    /// ended.
    ///
    /// Backoffs are awaited with [`tokio::time::sleep`] (paused-time aware, so
    /// tests can drive it deterministically). This never touches the output
    /// clock; a misbehaving actor is contained entirely within this loop.
    pub async fn supervise<A: Actor>(&self, mut actor: A) -> SupervisionOutcome {
        let mut restarts_used: u32 = 0;
        loop {
            let exit = actor.run().await;
            match self.policy.decide(exit, restarts_used) {
                RestartDecision::Stop { reason } => {
                    info!(
                        actor = actor.name(),
                        ?reason,
                        restarts_used,
                        "supervision ended"
                    );
                    return SupervisionOutcome {
                        restarts: restarts_used,
                        reason,
                    };
                }
                RestartDecision::Restart { backoff } => {
                    restarts_used = restarts_used.saturating_add(1);
                    warn!(
                        actor = actor.name(),
                        restart = restarts_used,
                        backoff_ms = backoff.as_millis(),
                        "actor failed; restarting after backoff"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
}

/// The result of supervising an actor to its terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionOutcome {
    /// How many times the actor was restarted before stopping.
    pub restarts: u32,
    /// Why supervision ended.
    pub reason: StopReason,
}
