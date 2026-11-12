//! Bounded process exit after a fatal output fault or a stop signal (#309,
//! `docs/architecture/bounded-exit.md`).
//!
//! The first [`arm`] starts ONE dedicated OS thread that sleeps until the
//! deadline and then ends the process with the code the run would have
//! returned, if it is still alive. Teardown that finishes in time is never
//! affected: `main` returns and the process exits normally. Teardown that
//! overruns (producers blocked in libav network reads, a runtime drop waiting on
//! blocking-pool work) is cut off at the deadline instead of holding dead
//! output for tens of seconds. The thread never touches the output clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The bound from the exit decision to the process exit.
pub const EXIT_DEADLINE: Duration = Duration::from_secs(2);

/// Why the exit was decided; selects the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCause {
    /// A fatal fault: the run fails (exit code 1).
    Fatal,
    /// A stop signal: a clean stop (exit code 0).
    Stop,
}

impl ExitCause {
    /// The process exit code for this cause.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Fatal => 1,
            Self::Stop => 0,
        }
    }
}

/// What the watchdog does when the deadline passes. Production exits the
/// process; tests observe the call instead.
pub type ExitFn = fn(ExitCause, &'static str);

#[allow(clippy::print_stderr, clippy::exit)]
// reason: this IS the bounded hard exit (#309). It runs only after the deadline,
// when teardown overran; stderr is unbuffered (the tracing pipeline may be the
// part that is stuck), and `process::exit` is the only way to end the process
// without running the destructors that overran.
fn exit_process(cause: ExitCause, reason: &'static str) {
    tracing::error!(reason, "teardown overran the exit bound; exiting now");
    // stderr is unbuffered; one line, then exit without unwinding or running
    // the remaining destructors (they are the work that overran).
    eprintln!(
        "multiview: teardown overran the {} ms exit bound after {reason}; exiting now",
        EXIT_DEADLINE.as_millis()
    );
    std::process::exit(cause.code());
}

/// Whether this process may be ended by the watchdog. Only the `multiview`
/// binary's `main` enables it; a library user (tests, other binaries) never
/// has its process ended.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Allow [`arm`] to end this process. Called once by the binary's `main`.
pub fn enable() {
    ENABLED.store(true, Ordering::Release);
}

/// Arm the process exit deadline. Idempotent: the first call wins (its cause and
/// deadline stand); later calls do nothing. A no-op unless [`enable`] was
/// called. Never blocks.
pub fn arm(cause: ExitCause, reason: &'static str) {
    if ENABLED.load(Ordering::Acquire) {
        arm_with(cause, reason, EXIT_DEADLINE, exit_process, &ARMED_FLAG);
    }
}

/// The process-wide armed flag (separate from [`ARMED`] so tests can use their
/// own flag).
static ARMED_FLAG: AtomicBool = AtomicBool::new(false);

/// Arm with an explicit deadline, action and flag (the testable core of
/// [`arm`]). Returns `true` if this call armed the watchdog.
pub fn arm_with(
    cause: ExitCause,
    reason: &'static str,
    deadline: Duration,
    on_deadline: ExitFn,
    flag: &'static AtomicBool,
) -> bool {
    if flag.swap(true, Ordering::AcqRel) {
        return false;
    }
    let due = Instant::now() + deadline;
    tracing::info!(
        ?cause,
        reason,
        deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
        "exit deadline armed"
    );
    let spawned = std::thread::Builder::new()
        .name("multiview-exit-watchdog".to_owned())
        .spawn(move || {
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            on_deadline(cause, reason);
        });
    if spawned.is_err() {
        // No thread: nothing can enforce the bound. Say so; the normal
        // teardown still runs.
        tracing::error!("could not start the exit watchdog thread; exit is unbounded");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    static CALLS: AtomicU32 = AtomicU32::new(0);
    static LAST: Mutex<Option<(ExitCause, &'static str, Instant)>> = Mutex::new(None);

    fn record(cause: ExitCause, reason: &'static str) {
        CALLS.fetch_add(1, Ordering::SeqCst);
        *LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((cause, reason, Instant::now()));
    }

    #[test]
    fn exit_codes_follow_the_cause() {
        assert_eq!(ExitCause::Fatal.code(), 1);
        assert_eq!(ExitCause::Stop.code(), 0);
    }

    #[test]
    fn fires_once_at_the_deadline_with_the_first_cause() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let start = Instant::now();
        assert!(arm_with(
            ExitCause::Fatal,
            "first",
            Duration::from_millis(150),
            record,
            &FLAG
        ));
        // A second arm (a stop signal after the fatal) does not move the
        // deadline or change the cause.
        assert!(!arm_with(
            ExitCause::Stop,
            "second",
            Duration::from_millis(10),
            record,
            &FLAG
        ));
        let wait_until = start + Duration::from_secs(5);
        while CALLS.load(Ordering::SeqCst) == 0 && Instant::now() < wait_until {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "fires exactly once");
        let (cause, reason, at) = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expect("fired");
        assert_eq!((cause, reason), (ExitCause::Fatal, "first"));
        let elapsed = at - start;
        assert!(
            elapsed >= Duration::from_millis(150),
            "not before the deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "promptly after it: {elapsed:?}"
        );
    }

    #[test]
    fn the_production_deadline_is_two_seconds() {
        assert_eq!(EXIT_DEADLINE, Duration::from_secs(2));
    }
}
