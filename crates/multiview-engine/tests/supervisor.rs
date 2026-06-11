//! Supervisor / actor tests — invariant #2 fault isolation.
//!
//! Prove the restart policy (capped exponential backoff + bounded budget) and
//! that a failed actor never takes down the output clock: the clock keeps
//! ticking, on schedule, while an actor crashes and is restarted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_core::time::Rational;
use multiview_engine::supervisor::{Actor, ActorExit, RestartDecision, RestartPolicy, StopReason};
use multiview_engine::{OutputClock, Supervisor};

#[test]
fn backoff_is_capped_exponential() {
    let policy = RestartPolicy::new(Duration::from_millis(100), Duration::from_secs(10), 100);
    assert_eq!(policy.backoff(0), Duration::ZERO);
    assert_eq!(policy.backoff(1), Duration::from_millis(100));
    assert_eq!(policy.backoff(2), Duration::from_millis(200));
    assert_eq!(policy.backoff(3), Duration::from_millis(400));
    assert_eq!(policy.backoff(4), Duration::from_millis(800));
    // Caps at `max`.
    assert_eq!(policy.backoff(20), Duration::from_secs(10));
    // A huge count saturates rather than overflowing/panicking.
    assert_eq!(policy.backoff(u32::MAX), Duration::from_secs(10));
}

#[test]
fn backoff_is_monotonic_nondecreasing() {
    let policy = RestartPolicy::default_policy();
    let mut prev = Duration::ZERO;
    for n in 1..40_u32 {
        let b = policy.backoff(n);
        assert!(b >= prev, "backoff must be non-decreasing at n={n}");
        prev = b;
    }
}

#[test]
fn decide_completed_stops_without_restart() {
    let policy = RestartPolicy::default_policy();
    assert_eq!(
        policy.decide(ActorExit::Completed, 0),
        RestartDecision::Stop {
            reason: StopReason::Completed
        }
    );
}

#[test]
fn decide_failed_restarts_until_budget_exhausted() {
    let policy = RestartPolicy::new(Duration::from_millis(10), Duration::from_secs(1), 3);
    // 0,1,2 restarts used -> still restart.
    for used in 0..3 {
        match policy.decide(ActorExit::Failed, used) {
            RestartDecision::Restart { backoff } => {
                assert_eq!(backoff, policy.backoff(used + 1));
            }
            RestartDecision::Stop { reason } => {
                panic!("expected Restart at used={used}, got Stop({reason:?})")
            }
        }
    }
    // Budget spent (>= max_restarts) -> stop.
    assert_eq!(
        policy.decide(ActorExit::Failed, 3),
        RestartDecision::Stop {
            reason: StopReason::BudgetExhausted
        }
    );
}

/// An actor that fails a fixed number of times then completes.
struct FlakyActor {
    name: String,
    fail_count: u32,
    runs: Arc<AtomicU64>,
}

impl Actor for FlakyActor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&mut self) -> ActorExit {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        if n < u64::from(self.fail_count) {
            ActorExit::Failed
        } else {
            ActorExit::Completed
        }
    }
}

#[tokio::test(start_paused = true)]
async fn supervisor_restarts_then_completes() {
    // Paused time: the backoff sleeps auto-advance, so the test is instant and
    // deterministic.
    let runs = Arc::new(AtomicU64::new(0));
    let actor = FlakyActor {
        name: "input-rtsp-1".to_owned(),
        fail_count: 3,
        runs: Arc::clone(&runs),
    };
    let sup = Supervisor::new(RestartPolicy::new(
        Duration::from_millis(50),
        Duration::from_secs(5),
        100,
    ));
    let outcome = sup.supervise(actor).await;
    assert_eq!(outcome.reason, StopReason::Completed);
    assert_eq!(
        outcome.restarts, 3,
        "restarted exactly 3 times before success"
    );
    // run() called 4 times: 3 failures + 1 success.
    assert_eq!(runs.load(Ordering::SeqCst), 4);
}

/// An actor that always fails — exhausts the restart budget.
struct AlwaysFails {
    runs: Arc<AtomicU64>,
}

impl Actor for AlwaysFails {
    // reason: the `Actor::name` contract is `-> &str` (tied to `&self`); a fixed
    // test actor legitimately returns a static literal, which trips
    // `unnecessary_literal_bound` for the impl but cannot change the trait sig.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "always-fails"
    }
    async fn run(&mut self) -> ActorExit {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ActorExit::Failed
    }
}

#[tokio::test(start_paused = true)]
async fn supervisor_stops_after_budget_exhausted() {
    let runs = Arc::new(AtomicU64::new(0));
    let sup = Supervisor::new(RestartPolicy::new(
        Duration::from_millis(1),
        Duration::from_millis(10),
        5,
    ));
    let outcome = sup
        .supervise(AlwaysFails {
            runs: Arc::clone(&runs),
        })
        .await;
    assert_eq!(outcome.reason, StopReason::BudgetExhausted);
    assert_eq!(outcome.restarts, 5);
    // 1 initial run + 5 restarts = 6 runs.
    assert_eq!(runs.load(Ordering::SeqCst), 6);
}

#[tokio::test(start_paused = true)]
async fn output_clock_survives_a_crashing_actor() {
    // The crux of invariant #2: an actor that keeps crashing must NOT take down
    // the output clock. We run the clock loop concurrently with a forever-
    // crashing supervised actor and assert the clock produced every tick,
    // strictly monotonic, while the actor thrashed.
    let runs = Arc::new(AtomicU64::new(0));
    let sup = Supervisor::new(RestartPolicy::new(
        Duration::from_millis(1),
        Duration::from_millis(5),
        50,
    ));
    let actor = AlwaysFails {
        runs: Arc::clone(&runs),
    };

    // The clock loop is an independent task; it shares NOTHING with the actor.
    let clock_task = tokio::spawn(async move {
        let mut clock = OutputClock::new(Rational::FPS_60).unwrap();
        let mut last = i64::MIN;
        let mut count = 0_u64;
        for _ in 0..10_000_u64 {
            let tick = clock.tick();
            assert!(tick.pts.as_nanos() > last, "clock must stay monotonic");
            last = tick.pts.as_nanos();
            count += 1;
            // Yield occasionally so the supervisor task interleaves.
            if count % 256 == 0 {
                tokio::task::yield_now().await;
            }
        }
        count
    });

    // Supervise the doomed actor to its budget exhaustion concurrently.
    let outcome = sup.supervise(actor).await;
    assert_eq!(outcome.reason, StopReason::BudgetExhausted);

    // The clock produced all of its ticks regardless of the actor's thrashing.
    let produced = clock_task.await.unwrap();
    assert_eq!(produced, 10_000, "output clock produced every tick");
    assert!(runs.load(Ordering::SeqCst) >= 6, "actor actually thrashed");
}
