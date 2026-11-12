//! The **dedicated output-clock thread** (issue #311, ADR-0109, invariant #1).
//!
//! Each program's tick loop runs on its own named OS thread instead of a
//! `tokio::spawn`ed task on the runtime that also runs the control plane. On
//! that thread the loop is driven by [`block_on`], a minimal single-future
//! executor with no reactor and no timer wheel, and the production
//! [`RealtimePacer`](crate::RealtimePacer) waits for each tick with
//! [`sleep_until`]: a blocking **absolute-deadline** sleep against the
//! monotonic clock.
//!
//! * **Linux**: `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` through
//!   rustix's safe wrapper (the engine stays `forbid(unsafe_code)`; rustix owns
//!   the syscall), with the thread's timer slack lowered to 1 ns
//!   (`PR_SET_TIMERSLACK`) so the kernel does not coalesce the wakeup by its
//!   default 50 µs.
//! * **macOS / Windows** (and any other target): a portable fallback —
//!   `std::thread::sleep` until [`SPIN_MARGIN`] before the deadline, then a
//!   short yielding spin to the deadline. Correct everywhere; precise to the
//!   platform's sleep granularity plus the spin.
//!
//! Every wait is sized from the tick's absolute deadline (`seed + pts_at(n)`,
//! from the integer tick counter), re-read after every wakeup, so a sleep that
//! overshoots or returns early never accumulates (no relative-sleep drift). The
//! wait is split into steps of at most [`STOP_POLL`] so a raised
//! [`StopSignal`](crate::StopSignal) is observed promptly.
//!
//! The thread optionally lowers its niceness ([`CLOCK_NICE`]) where the process
//! is permitted to (`CAP_SYS_NICE`); it never requires it and logs the outcome.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use crate::clock::TimeSource;
use crate::runtime::StopSignal;

/// The longest single sleep step before the stop signal is re-checked. Matches
/// the async pacer's poll bound, so shutdown latency is unchanged (~10 ms).
pub const STOP_POLL: Duration = Duration::from_millis(10);

/// Portable fallback only: how long before the deadline the fallback stops
/// sleeping and spins (yielding) instead. Covers the macOS/Windows sleep
/// overshoot so the tick is not emitted a whole sleep quantum late.
pub const SPIN_MARGIN: Duration = Duration::from_micros(500);

/// The niceness the clock thread asks for (Linux, best effort). Only applied if
/// the process may lower niceness; otherwise the thread runs at the default
/// priority and says so in the log. Never `SCHED_FIFO`: a real-time policy can
/// starve the host if a bug ever made the loop spin.
pub const CLOCK_NICE: i32 = -10;

thread_local! {
    static ON_CLOCK_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the calling thread is a dedicated output-clock thread started by
/// [`spawn`]. The production pacer uses the blocking precise sleep only here;
/// anywhere else (a tokio worker) it keeps the async sleep, so it can never
/// block an executor thread.
#[must_use]
pub fn on_clock_thread() -> bool {
    ON_CLOCK_THREAD.with(std::cell::Cell::get)
}

/// Block the calling (clock) thread until `source` reaches `deadline_nanos` or
/// `stop` is raised, whichever comes first.
///
/// Must only be called on a dedicated clock thread ([`on_clock_thread`]): it
/// blocks the OS thread. Each step is sized from the absolute deadline and
/// capped at [`STOP_POLL`]; the source is re-read after every wakeup, so an
/// early or late wakeup never accumulates into drift.
pub fn sleep_until(deadline_nanos: i64, source: &dyn TimeSource, stop: &StopSignal) {
    loop {
        if stop.is_stopped() {
            return;
        }
        let remaining = deadline_nanos.saturating_sub(source.now_nanos());
        if remaining <= 0 {
            return;
        }
        let poll = i64::try_from(STOP_POLL.as_nanos()).unwrap_or(i64::MAX);
        if remaining > poll {
            // Far from the deadline: a plain bounded step, then re-check stop.
            sleep_step(poll, false);
        } else {
            sleep_step(remaining, true);
        }
    }
}

/// Sleep for `nanos` from now, against the monotonic clock. `last` marks the
/// final step to the deadline (the portable fallback spins only there).
#[cfg(target_os = "linux")]
fn sleep_step(nanos: i64, _last: bool) {
    use rustix::thread::{clock_nanosleep_absolute, ClockId};
    use rustix::time::{clock_gettime, Timespec};
    // Convert "deadline in `nanos`" to an ABSOLUTE CLOCK_MONOTONIC instant and
    // sleep with TIMER_ABSTIME: an interrupted sleep (EINTR) resumes against the
    // same instant instead of restarting a relative interval. The two clock
    // reads are back to back (tens of ns); the caller re-reads its source after.
    // No `unsafe` here: rustix's `clock_nanosleep_absolute` is the safe wrapper
    // (it owns the raw syscall), so the engine stays `forbid(unsafe_code)`.
    let now = clock_gettime(ClockId::Monotonic);
    let total = i128::from(now.tv_sec)
        .saturating_mul(1_000_000_000)
        .saturating_add(i128::from(now.tv_nsec))
        .saturating_add(i128::from(nanos.max(0)));
    let target = Timespec {
        tv_sec: i64::try_from(total / 1_000_000_000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(total % 1_000_000_000).unwrap_or(0),
    };
    loop {
        match clock_nanosleep_absolute(ClockId::Monotonic, &target) {
            Err(rustix::io::Errno::INTR) => {}
            // `Ok`, or an error the absolute sleep cannot recover from: return
            // and let the caller re-read the time source (never spin forever).
            _ => return,
        }
    }
}

/// Portable fallback (macOS, Windows, other targets): sleep to [`SPIN_MARGIN`]
/// before the step end, then yield-spin the rest on the final step.
#[cfg(not(target_os = "linux"))]
fn sleep_step(nanos: i64, last: bool) {
    let nanos = u64::try_from(nanos.max(0)).unwrap_or(0);
    let step = Duration::from_nanos(nanos);
    if !last {
        std::thread::sleep(step);
        return;
    }
    let start = std::time::Instant::now();
    if let Some(sleep) = step.checked_sub(SPIN_MARGIN) {
        std::thread::sleep(sleep);
    }
    while start.elapsed() < step {
        std::thread::yield_now();
    }
}

/// Wakes the clock thread by unparking it.
struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive `future` to completion on the calling thread.
///
/// A minimal executor: poll; on `Pending` park until woken. On a clock thread
/// the production pacer blocks inside `poll` (its wait has no `.await`), so the
/// loop normally completes each tick in a single poll and never parks; the park
/// path only serves futures that genuinely wait (for example a cooperative test
/// pacer, whose wakeups are immediate outside a tokio runtime).
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park();
    }
}

/// A clock thread panicked; the payload's message when it was a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockThreadPanic(pub String);

impl std::fmt::Display for ClockThreadPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "output clock thread panicked: {}", self.0)
    }
}

/// The handle to a running clock thread: await [`ClockThread::join`] from async
/// code (it never blocks an executor thread) or check [`ClockThread::is_finished`].
#[derive(Debug)]
pub struct ClockThread<T> {
    result: tokio::sync::oneshot::Receiver<Result<T, ClockThreadPanic>>,
    finished: Arc<AtomicBool>,
    name: String,
}

impl<T> ClockThread<T> {
    /// Await the thread's result without blocking the caller's executor. A
    /// panic on the thread is returned as [`ClockThreadPanic`], never resumed
    /// into the caller.
    ///
    /// # Errors
    ///
    /// [`ClockThreadPanic`] if the body panicked (or the thread vanished without
    /// reporting, which is reported the same way).
    pub async fn join(self) -> Result<T, ClockThreadPanic> {
        match self.result.await {
            Ok(result) => result,
            Err(_closed) => Err(ClockThreadPanic(format!(
                "{} ended without reporting",
                self.name
            ))),
        }
    }

    /// Whether the body has returned (or panicked). Wait-free.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// The OS thread name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Start a dedicated, named output-clock thread running `body`.
///
/// The thread marks itself as a clock thread (so the production pacer uses the
/// precise blocking sleep), applies the best-effort scheduling tweaks, enters
/// the caller's tokio runtime context if there is one (so a stray
/// `tokio::spawn` from a hook lands on that runtime instead of panicking — the
/// clock itself never awaits it), runs `body` under `catch_unwind`, and sends
/// the result to the [`ClockThread`] handle. The thread is detached: a body that
/// cannot observe its stop is shed by the caller's bounded join, never joined
/// forever.
///
/// # Errors
///
/// The OS refused to create the thread.
pub fn spawn<T, F>(name: &str, body: F) -> std::io::Result<ClockThread<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    let runtime = tokio::runtime::Handle::try_current().ok();
    // Linux limits thread names to 15 bytes; keep the readable prefix.
    let thread_name: String = name.chars().take(15).collect();
    let label = name.to_owned();
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            ON_CLOCK_THREAD.with(|on| on.set(true));
            let scheduling = tune_current_thread();
            tracing::info!(clock_thread = %label, %scheduling, "output clock thread started");
            let _runtime = runtime.as_ref().map(tokio::runtime::Handle::enter);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
                .map_err(|payload| ClockThreadPanic(panic_message(payload.as_ref())));
            if let Err(panic) = &outcome {
                tracing::error!(clock_thread = %label, %panic, "output clock thread panicked");
            }
            done.store(true, Ordering::Release);
            let _ = tx.send(outcome);
        })?;
    Ok(ClockThread {
        result: rx,
        finished,
        name: name.to_owned(),
    })
}

/// Run a tick-loop future that **borrows** from the caller on a dedicated,
/// named clock thread and await its output (#311).
///
/// For loops that cannot be `'static` (they borrow the caller's publisher,
/// pipeline or engine), the loop runs on a scoped thread: inside a
/// multi-thread tokio runtime the caller's worker is handed off with
/// `block_in_place` (the runtime keeps scheduling every other task on its
/// remaining workers) and the scoped clock thread drives `future` with
/// [`block_on`], with the clock-thread marker set and the runtime context
/// entered. Outside a multi-thread runtime (a `current_thread` test runtime,
/// where `block_in_place` is not allowed) the future is awaited inline as
/// before — still correct, just not on a dedicated thread.
///
/// A panic on the clock thread is resumed on the caller after the thread has
/// ended, exactly as awaiting the future inline would have.
///
/// # Errors
///
/// The OS refused to create the thread (the loop never ran).
pub async fn run_on_clock_thread<F>(name: &str, future: F) -> std::io::Result<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            handle
        }
        _ => return Ok(future.await),
    };
    let thread_name: String = name.chars().take(15).collect();
    let label = name.to_owned();
    let outcome = tokio::task::block_in_place(|| {
        std::thread::scope(|scope| {
            let spawned = std::thread::Builder::new()
                .name(thread_name)
                .spawn_scoped(scope, || {
                    ON_CLOCK_THREAD.with(|on| on.set(true));
                    let scheduling = tune_current_thread();
                    tracing::info!(clock_thread = %label, %scheduling, "output clock thread started");
                    let _runtime = runtime.enter();
                    block_on(future)
                });
            spawned.map(std::thread::ScopedJoinHandle::join)
        })
    });
    match outcome {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(payload)) => std::panic::resume_unwind(payload),
        Err(error) => {
            tracing::error!(clock_thread = %name, %error, "could not start the output clock thread");
            Err(error)
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Best-effort scheduling for the calling clock thread; returns a one-line
/// description for the log. Never fails the thread.
#[cfg(target_os = "linux")]
fn tune_current_thread() -> String {
    let slack = match rustix::thread::set_current_timer_slack(std::num::NonZeroU64::new(1)) {
        Ok(()) => "timer slack 1 ns".to_owned(),
        Err(error) => format!("timer slack unchanged ({error})"),
    };
    // Linux niceness is per thread: PRIO_PROCESS with the thread id.
    let tid = rustix::thread::gettid();
    let nice = match rustix::process::setpriority_process(Some(tid), CLOCK_NICE) {
        Ok(()) => format!("nice {CLOCK_NICE}"),
        Err(error) => format!("default priority (nice {CLOCK_NICE} not permitted: {error})"),
    };
    format!("clock_nanosleep(TIMER_ABSTIME), {slack}, {nice}")
}

#[cfg(not(target_os = "linux"))]
fn tune_current_thread() -> String {
    format!(
        "portable sleep + {} µs spin, default priority",
        SPIN_MARGIN.as_micros()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{ManualTimeSource, MonotonicTimeSource};

    #[test]
    fn sleep_until_reaches_the_deadline_and_never_returns_early() {
        let source = MonotonicTimeSource::new();
        let stop = StopSignal::new();
        for step in [1_000_000_i64, 3_000_000, 15_000_000] {
            let deadline = source.now_nanos() + step;
            sleep_until(deadline, &source, &stop);
            assert!(source.now_nanos() >= deadline, "woke before the deadline");
        }
    }

    #[test]
    fn sleep_until_observes_stop_on_an_unreachable_deadline() {
        let source = ManualTimeSource::new();
        let stop = StopSignal::new();
        let raiser = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            raiser.stop();
        });
        let started = std::time::Instant::now();
        sleep_until(i64::MAX, &source, &stop);
        assert!(started.elapsed() < Duration::from_secs(5));
        t.join().ok();
    }

    #[test]
    fn spawn_marks_the_thread_and_reports_a_panic_as_an_error() {
        let rt = tokio::runtime::Builder::new_current_thread().build().ok();
        let Some(rt) = rt else { return };
        assert!(!on_clock_thread());
        let marked = spawn("clock-test", on_clock_thread).ok();
        let marked = marked.map(|h| rt.block_on(h.join()));
        assert_eq!(marked, Some(Ok(true)));
        let panicked = spawn("clock-panic", || -> u8 { panic!("boom") }).ok();
        let panicked = panicked.map(|h| rt.block_on(h.join()));
        assert_eq!(panicked, Some(Err(ClockThreadPanic("boom".to_owned()))));
    }

    #[test]
    fn block_on_drives_a_future_that_yields() {
        let out = block_on(async {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            7
        });
        assert_eq!(out, 7);
    }
}
