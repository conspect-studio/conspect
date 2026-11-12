//! Bounded process exit after a fatal output fault or a stop signal (#309,
//! `docs/architecture/bounded-exit.md`).
//!
//! [`enable`] (called once by the `multiview` binary) starts ONE dedicated OS
//! thread that parks. [`arm`] is wait-free and allocation-free — it stores the
//! deadline and the cause in atomics and unparks the thread — so it is safe on
//! any thread, including the output clock's. The thread then sleeps until the
//! deadline and, if the process is still alive, ends it with the cause's exit
//! code. Teardown that finishes in time is not affected: `main` returns and the
//! process exits normally.

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::thread::Thread;
use std::time::{Duration, Instant};

/// The bound from the exit decision to the process exit.
pub const EXIT_DEADLINE: Duration = Duration::from_secs(2);

/// Why the exit was decided; selects the exit code. A fatal cause overrides a
/// stop cause armed earlier (the run failed during its stop).
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

    const fn tag(self) -> u8 {
        match self {
            Self::Fatal => 2,
            Self::Stop => 1,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            2 => Some(Self::Fatal),
            1 => Some(Self::Stop),
            _ => None,
        }
    }
}

/// The armed state one watchdog thread watches.
#[derive(Debug)]
pub struct Watchdog {
    /// The process-relative origin for [`Self::due_ns`].
    origin: Instant,
    /// Deadline, in ns after `origin`; 0 = not armed. Set by the first arm;
    /// a later Fatal that upgrades a Stop moves it to that Fatal's own
    /// deadline if later, so the failure is reported within the bound of the
    /// fatal decision.
    due_ns: AtomicU64,
    /// [`ExitCause::tag`] of the strongest cause armed so far; 0 = none.
    cause: AtomicU8,
    /// The watchdog thread to unpark on the first arm.
    thread: OnceLock<Thread>,
}

impl Watchdog {
    /// A disarmed watchdog with no thread yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            due_ns: AtomicU64::new(0),
            cause: AtomicU8::new(0),
            thread: OnceLock::new(),
        }
    }

    /// Arm: wait-free, allocation-free, never logs. The first arm fixes the
    /// deadline; a later Fatal upgrades a Stop cause. Returns `true` when this
    /// call set the deadline.
    pub fn arm(&self, cause: ExitCause, deadline: Duration) -> bool {
        let due = u64::try_from(self.origin.elapsed().saturating_add(deadline).as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        let previous = self.cause.fetch_max(cause.tag(), Ordering::AcqRel);
        let set = self
            .due_ns
            .compare_exchange(0, due, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if !set && cause.tag() > previous {
            // An upgrade (Stop → Fatal): give the fatal its own full bound.
            self.due_ns.fetch_max(due, Ordering::AcqRel);
        }
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
        set
    }

    /// Time left until the armed deadline (`None` when not armed).
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        let due = self.due_ns.load(Ordering::Acquire);
        (due != 0).then(|| {
            (self.origin + Duration::from_nanos(due)).saturating_duration_since(Instant::now())
        })
    }

    /// The armed cause, if any.
    #[must_use]
    pub fn cause(&self) -> Option<ExitCause> {
        ExitCause::from_tag(self.cause.load(Ordering::Acquire))
    }

    /// The watchdog thread body: park until armed, sleep until the deadline,
    /// then call `on_deadline` with the (possibly upgraded) cause.
    pub fn run(&self, on_deadline: fn(ExitCause)) {
        loop {
            let due = self.due_ns.load(Ordering::Acquire);
            if due == 0 {
                std::thread::park();
                continue;
            }
            let due_at = self.origin + Duration::from_nanos(due);
            let now = Instant::now();
            if due_at > now {
                // Wake at the deadline, or earlier on an arm that moved it.
                std::thread::park_timeout(due_at - now);
                continue;
            }
            on_deadline(self.cause().unwrap_or(ExitCause::Fatal));
            return;
        }
    }

    /// Start this watchdog's thread (once). Returns `false` if it could not
    /// start; arming then does nothing and the exit stays unbounded.
    pub fn start(&'static self, on_deadline: fn(ExitCause)) -> bool {
        if self.thread.get().is_some() {
            return true;
        }
        match std::thread::Builder::new()
            .name("multiview-exit-watchdog".to_owned())
            .spawn(move || self.run(on_deadline))
        {
            Ok(handle) => {
                let _ = self.thread.set(handle.thread().clone());
                // An arm that raced the spawn may have missed the unpark.
                handle.thread().unpark();
                true
            }
            Err(_) => false,
        }
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

/// How long the ordinary exit path may take before the process is killed.
pub const EXIT_PATH_GRACE: Duration = Duration::from_millis(500);

#[allow(clippy::exit)]
// reason: this IS the bounded hard exit (#309). It runs only after the deadline,
// when teardown overran; `process::exit` ends the process without running the
// destructors that overran. The message is best-effort on a helper thread
// through its own /dev/stderr handle (no std stderr lock, no tracing: either may
// be the stuck part). The C runtime exit path (atexit handlers, stdio flush)
// could itself block, so a backstop thread kills the process with SIGKILL if
// `exit` has not finished within EXIT_PATH_GRACE.
fn exit_process(cause: ExitCause) {
    // A stop whose outputs have not finalised by the deadline did not stop
    // cleanly: report it as a failure (a wedged consumer or sink, #309).
    let cause = if cause == ExitCause::Stop
        && (OUTPUTS_OPEN.load(Ordering::Acquire) > 0 || RUN_FAILED.load(Ordering::Acquire))
    {
        ExitCause::Fatal
    } else {
        cause
    };
    let _ = std::thread::Builder::new()
        .name("multiview-exit-notice".to_owned())
        .spawn(move || {
            // /dev/stderr is Linux; macOS/BSD expose the fd under /dev/fd.
            // Append: a fresh open of a regular-file stderr must not overwrite
            // the start of the log.
            let handle = std::fs::OpenOptions::new()
                .append(true)
                .open("/dev/stderr")
                .or_else(|_| std::fs::OpenOptions::new().append(true).open("/dev/fd/2"));
            if let Ok(mut err) = handle {
                let _ = writeln!(
                    err,
                    "multiview: teardown overran the {} ms exit bound ({cause:?}); exiting with \
                     code {} (outputs finalised before the bound are valid; detached sinks may be \
                     incomplete; the supervisor restarts the service)",
                    EXIT_DEADLINE.as_millis(),
                    cause.code()
                );
            }
        });
    let _ = std::thread::Builder::new()
        .name("multiview-exit-backstop".to_owned())
        .spawn(|| {
            std::thread::sleep(EXIT_PATH_GRACE);
            kill_self();
        });
    // Give the notice a moment; never wait for it.
    std::thread::sleep(Duration::from_millis(20));
    std::process::exit(cause.code());
}

/// End the process unconditionally (no atexit handlers, no stdio flush).
#[cfg(all(unix, feature = "ffmpeg"))]
fn kill_self() {
    use rustix::process::{getpid, kill_process, Signal};
    let _ = kill_process(getpid(), Signal::KILL);
}

/// Without `rustix` in this build: `abort` also skips atexit handlers and stdio
/// flushing (it raises SIGABRT), so the bound still holds.
#[cfg(not(all(unix, feature = "ffmpeg")))]
fn kill_self() {
    std::process::abort();
}

static WATCHDOG: OnceLock<Watchdog> = OnceLock::new();

/// The run's own verdict is a failure (for example it faltered: frames were
/// shed). Does not arm a deadline; only makes a hard exit report code 1.
static RUN_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record that the run's verdict is a failure (wait-free; safe on the clock).
pub fn note_run_failed() {
    RUN_FAILED.store(true, Ordering::Release);
}

/// Live output egresses that have not finalised yet.
static OUTPUTS_OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A live output egress started (its outputs are open until finalised).
pub fn outputs_opened() {
    OUTPUTS_OPEN.fetch_add(1, Ordering::AcqRel);
}

/// A live output egress finalised every output (consumer done, every sink
/// joined, nothing wedged).
pub fn outputs_finalised() {
    let mut current = OUTPUTS_OPEN.load(Ordering::Acquire);
    while current > 0 {
        match OUTPUTS_OPEN.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(seen) => current = seen,
        }
    }
}

/// Start the process exit watchdog. Called once by the `multiview` binary's
/// `main`; a library user (tests, other binaries) never calls it, so [`arm`]
/// can never end their process.
pub fn enable() {
    let watchdog = WATCHDOG.get_or_init(Watchdog::new);
    if !watchdog.start(exit_process) {
        tracing::error!("could not start the exit watchdog thread; the exit is unbounded");
    }
}

/// Arm the process exit deadline ([`EXIT_DEADLINE`] from now). Wait-free and
/// allocation-free: safe on the output clock. A no-op unless [`enable`] ran.
pub fn arm(cause: ExitCause) {
    if let Some(watchdog) = WATCHDOG.get() {
        watchdog.arm(cause, EXIT_DEADLINE);
    }
}

/// The armed state as a cheap poll: `Some(cause)` once any exit was decided.
/// The binary's "exit decided" task polls this (every few ms, off the clock) to
/// start the durable shutdown work — control drain, final Running persist,
/// journal flush — concurrently with the pipeline teardown, inside the bound.
/// (Polling keeps [`arm`] wait-free: it never wakes an async task itself.)
///
/// Time left until the process exit deadline, if one is armed.
#[must_use]
pub fn remaining() -> Option<Duration> {
    WATCHDOG.get().and_then(Watchdog::remaining)
}

/// Whether the exit deadline is armed.
#[must_use]
pub fn armed() -> Option<ExitCause> {
    WATCHDOG.get().and_then(Watchdog::cause)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn exit_codes_follow_the_cause() {
        assert_eq!(ExitCause::Fatal.code(), 1);
        assert_eq!(ExitCause::Stop.code(), 0);
    }

    #[test]
    fn the_production_deadline_is_two_seconds() {
        assert_eq!(EXIT_DEADLINE, Duration::from_secs(2));
    }

    #[test]
    fn first_arm_fixes_the_deadline_and_fatal_upgrades_stop() {
        let w = Watchdog::new();
        assert_eq!(w.cause(), None);
        assert!(w.arm(ExitCause::Stop, Duration::from_secs(10)));
        assert_eq!(w.cause(), Some(ExitCause::Stop));
        // A later fatal keeps the deadline but upgrades the cause.
        assert!(!w.arm(ExitCause::Fatal, Duration::from_millis(1)));
        assert_eq!(w.cause(), Some(ExitCause::Fatal));
        // A later stop never downgrades a fatal.
        assert!(!w.arm(ExitCause::Stop, Duration::from_millis(1)));
        assert_eq!(w.cause(), Some(ExitCause::Fatal));
    }

    static FIRED: Mutex<Vec<(ExitCause, Instant)>> = Mutex::new(Vec::new());
    fn record(cause: ExitCause) {
        FIRED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((cause, Instant::now()));
    }

    #[test]
    fn fires_once_at_the_deadline_and_never_while_disarmed() {
        static W: OnceLock<Watchdog> = OnceLock::new();
        let w = W.get_or_init(Watchdog::new);
        assert!(w.start(record));
        // Disarmed: nothing fires.
        std::thread::sleep(Duration::from_millis(200));
        assert!(FIRED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
        let start = Instant::now();
        w.arm(ExitCause::Fatal, Duration::from_millis(150));
        let until = start + Duration::from_secs(5);
        while FIRED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));
        let fired = FIRED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(fired.len(), 1, "fires exactly once");
        let (cause, at) = fired[0];
        assert_eq!(cause, ExitCause::Fatal);
        let elapsed = at - start;
        assert!(
            elapsed >= Duration::from_millis(150),
            "not before: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "promptly at: {elapsed:?}"
        );
    }

    static FIRED2: Mutex<Vec<(ExitCause, Instant)>> = Mutex::new(Vec::new());
    fn record2(cause: ExitCause) {
        FIRED2
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((cause, Instant::now()));
    }

    #[test]
    fn a_fatal_during_a_stop_gets_its_own_full_bound() {
        static W: OnceLock<Watchdog> = OnceLock::new();
        let w = W.get_or_init(Watchdog::new);
        assert!(w.start(record2));
        let start = Instant::now();
        w.arm(ExitCause::Stop, Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(200));
        let fatal_at = Instant::now();
        w.arm(ExitCause::Fatal, Duration::from_millis(300));
        let until = start + Duration::from_secs(5);
        while FIRED2
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let fired = FIRED2
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(fired.len(), 1);
        let (cause, at) = fired[0];
        assert_eq!(cause, ExitCause::Fatal, "the failure is reported");
        assert!(
            at >= fatal_at + Duration::from_millis(300),
            "the fatal's own bound"
        );
        assert!(at < fatal_at + Duration::from_millis(600), "and not later");
    }

    #[test]
    fn arm_is_a_noop_until_enabled() {
        // `enable()` is never called in the test binary.
        assert!(WATCHDOG.get().is_none());
        arm(ExitCause::Fatal);
        assert_eq!(armed(), None);
    }
}
