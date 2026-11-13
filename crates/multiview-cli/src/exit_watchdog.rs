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

#[cfg(not(all(unix, feature = "ffmpeg")))]
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::thread::Thread;
use std::time::{Duration, Instant};

/// The deadline: the watchdog fires this long after the first exit decision.
/// The process is then gone within [`EXIT_PATH_GRACE`] more
/// (docs/architecture/bounded-exit.md).
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
    /// Deadline, in ns after `origin`; 0 = not armed. Set by the FIRST arm and
    /// never moved: the bound is measured from the first exit decision. A later
    /// Fatal only upgrades the cause (the exit code), not the deadline.
    due_ns: AtomicU64,
    /// The first arm's instant, in ns after `origin` (at least 1); 0 = not
    /// armed. Whoever sets it sets `due_ns`.
    armed_ns: AtomicU64,
    /// First arm -> fire, in ms; set when the deadline fires.
    fired_after_ms: AtomicU64,
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
            armed_ns: AtomicU64::new(0),
            fired_after_ms: AtomicU64::new(0),
            cause: AtomicU8::new(0),
            thread: OnceLock::new(),
        }
    }

    /// Arm: wait-free, allocation-free, never logs. The first arm fixes the
    /// deadline; a later Fatal upgrades a Stop cause. Returns `true` when this
    /// call set the deadline.
    pub fn arm(&self, cause: ExitCause, deadline: Duration) -> bool {
        let now = self.origin.elapsed();
        let now_ns = u64::try_from(now.as_nanos()).unwrap_or(u64::MAX).max(1);
        let due = u64::try_from(now.saturating_add(deadline).as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        self.cause.fetch_max(cause.tag(), Ordering::AcqRel);
        // The first arm claims `armed_ns`, then publishes the deadline; a
        // racing later arm only upgrades the cause.
        let set = self
            .armed_ns
            .compare_exchange(0, now_ns, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if set {
            self.due_ns.store(due, Ordering::Release);
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

    /// First arm -> fire, in ms (0 until the deadline fired).
    #[must_use]
    pub fn fired_after_arm_ms(&self) -> u64 {
        self.fired_after_ms.load(Ordering::Acquire)
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
                // #350: a timed wait can wake late, by more for a longer
                // timeout (macOS timer coalescing; a CI VM fired 92-106 ms
                // late after one long wait). Every wait is at most MAX_PARK,
                // and the last FINAL_APPROACH goes in FINAL_STEP waits. Each
                // wait targets the absolute deadline, so lateness never
                // accumulates: the fire is late by at most the lateness of
                // the one wait that crosses the deadline. Reported with the
                // fire: the worst single-wait lateness, measured tightly
                // around the park, and the longest wait requested.
                let left = due_at - now;
                let step = if left > FINAL_APPROACH {
                    left.min(MAX_PARK)
                } else {
                    left.min(FINAL_STEP)
                };
                MAX_STEP_US.fetch_max(
                    u64::try_from(step.as_micros()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                let parked_at = Instant::now();
                std::thread::park_timeout(step);
                let late = parked_at.elapsed().saturating_sub(step);
                WORST_WAKE_LATE_MS.fetch_max(
                    u64::try_from(late.as_millis()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                continue;
            }
            // Decision (the first arm) -> fire, on the monotonic clock.
            let armed = Duration::from_nanos(self.armed_ns.load(Ordering::Acquire));
            let fired_after =
                u64::try_from(self.origin.elapsed().saturating_sub(armed).as_millis())
                    .unwrap_or(u64::MAX);
            self.fired_after_ms.store(fired_after, Ordering::Release);
            FIRE_AFTER_ARM_MS.store(fired_after, Ordering::Release);
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
            .spawn(move || {
                // #350: on a loaded host a normal-priority sleeper can wake
                // late; the watchdog runs at the same raised class as the
                // output clock (macOS QoS user-interactive; no-op elsewhere).
                // Best effort, and reported.
                let class = multiview_clocksys::raise_current_thread();
                tracing::info!(class = ?class, "exit watchdog scheduling class");
                self.run(on_deadline);
            }) {
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

/// The watchdog's last approach to the deadline is made in [`FINAL_STEP`]
/// waits, which the OS wakes almost on time (#350).
const FINAL_APPROACH: Duration = Duration::from_millis(20);
/// One short wait of the final approach.
const FINAL_STEP: Duration = Duration::from_millis(1);
/// The longest single wait before the final approach.
const MAX_PARK: Duration = Duration::from_millis(20);

/// How long the ordinary exit path may take before the process is killed.
pub const EXIT_PATH_GRACE: Duration = Duration::from_millis(500);

#[allow(clippy::exit)]
// reason: this IS the bounded hard exit (#309). It runs only after the deadline,
// when teardown overran; `process::exit` ends the process without running the
// destructors that overran. The backstop thread starts FIRST: it kills the
// process with SIGKILL if the notice write or the C runtime exit path (atexit
// handlers, stdio flush) has not finished within EXIT_PATH_GRACE. The notice is
// then written on this thread, completely, with a raw write to fd 2 (no std
// stderr lock, no tracing: either may be the stuck part); a blocked stderr is
// cut by the backstop.
fn exit_process(cause: ExitCause) {
    // A stop whose outputs have not finalised by the deadline, or whose run
    // faltered, did not stop cleanly: report it as a failure (#309).
    let cause = effective_cause(
        cause,
        OUTPUTS_OPEN.load(Ordering::Acquire) > 0,
        RUN_FAILED.load(Ordering::Acquire),
    );
    let backstop = std::thread::Builder::new()
        .name("multiview-exit-backstop".to_owned())
        .spawn(|| {
            std::thread::sleep(EXIT_PATH_GRACE);
            kill_self();
        });
    if backstop.is_err() {
        // No backstop: never risk a blocked stderr write; exit at once.
        std::process::exit(cause.code());
    }
    let line = format!(
        "multiview: teardown overran the {} ms exit bound ({cause:?}); exiting with \
         code {} (outputs finalised before the bound are valid; detached sinks may be \
         incomplete; the supervisor restarts the service) \
         [fired_after_decision_ms={} worst_wake_late_ms={} max_wait_ms={}]\n",
        EXIT_DEADLINE.as_millis(),
        cause.code(),
        FIRE_AFTER_ARM_MS.load(Ordering::Acquire),
        WORST_WAKE_LATE_MS.load(Ordering::Relaxed),
        MAX_STEP_US.load(Ordering::Relaxed).div_ceil(1000)
    );
    write_stderr_raw(line.as_bytes());
    std::process::exit(cause.code());
}

/// The exit code's cause: a stop with outputs still open, or after the run
/// faltered (frames were shed), is reported as a failure (#309, #350).
#[must_use]
pub const fn effective_cause(cause: ExitCause, outputs_open: bool, run_failed: bool) -> ExitCause {
    match cause {
        ExitCause::Stop if outputs_open || run_failed => ExitCause::Fatal,
        other => other,
    }
}

/// Write to the already-open fd 2, without the std stderr lock and without
/// reopening its path (a socket stderr — systemd's journal default — cannot be
/// reopened). Best effort.
#[cfg(all(unix, feature = "ffmpeg"))]
fn write_stderr_raw(mut bytes: &[u8]) {
    // Loop on a short write; retry EINTR; give up on any other error.
    while !bytes.is_empty() {
        match rustix::io::write(rustix::stdio::stderr(), bytes) {
            Ok(0) => return,
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

/// Without `rustix`: reopen the stderr path append-only (no socket support).
#[cfg(not(all(unix, feature = "ffmpeg")))]
fn write_stderr_raw(bytes: &[u8]) {
    let handle = std::fs::OpenOptions::new()
        .append(true)
        .open("/dev/stderr")
        .or_else(|_| std::fs::OpenOptions::new().append(true).open("/dev/fd/2"));
    if let Ok(mut err) = handle {
        let _ = err.write_all(bytes);
    }
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

/// Monotonic ms from the first exit decision to the deadline firing,
/// reported in the hard-exit notice (#350).
static FIRE_AFTER_ARM_MS: AtomicU64 = AtomicU64::new(0);
/// The worst lateness of one watchdog wait while armed (#350 diagnostics).
static WORST_WAKE_LATE_MS: AtomicU64 = AtomicU64::new(0);
/// The longest single watchdog wait requested while armed, in µs (#350).
static MAX_STEP_US: AtomicU64 = AtomicU64::new(0);

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
    fn a_stop_after_a_falter_or_with_open_outputs_exits_1() {
        assert_eq!(
            effective_cause(ExitCause::Stop, false, false),
            ExitCause::Stop
        );
        assert_eq!(
            effective_cause(ExitCause::Stop, false, true),
            ExitCause::Fatal
        );
        assert_eq!(
            effective_cause(ExitCause::Stop, true, false),
            ExitCause::Fatal
        );
        assert_eq!(
            effective_cause(ExitCause::Fatal, false, false),
            ExitCause::Fatal
        );
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
        // The reported decision -> fire time is measured from the arm, for any
        // deadline (not derived from EXIT_DEADLINE).
        let reported = w.fired_after_arm_ms();
        assert!(
            (150..400).contains(&reported),
            "reported fired_after_arm_ms {reported}"
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
    fn a_fatal_during_a_stop_upgrades_the_code_and_keeps_the_first_deadline() {
        // #365: deterministic. Both arms happen BEFORE the watchdog thread
        // starts, so the fire cannot race the upgrade. The Fatal asks for a
        // 10 s deadline: if an upgrade moved the deadline, the fire would come
        // 10 s later, not at the first decision's 300 ms.
        static W: OnceLock<Watchdog> = OnceLock::new();
        let w = W.get_or_init(Watchdog::new);
        let start = Instant::now();
        assert!(w.arm(ExitCause::Stop, Duration::from_millis(300)));
        assert!(!w.arm(ExitCause::Fatal, Duration::from_secs(10)));
        assert!(w.start(record2));
        let until = start + Duration::from_secs(8);
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
        assert_eq!(fired.len(), 1, "fired once, at the first deadline");
        let (cause, at) = fired[0];
        assert_eq!(cause, ExitCause::Fatal, "the failure is reported");
        assert!(
            at >= start + Duration::from_millis(300),
            "not before the first deadline"
        );
        assert!(
            at < start + Duration::from_secs(8),
            "the first decision's deadline holds; the upgrade does not extend it to 10 s"
        );
    }

    #[test]
    fn arm_is_a_noop_until_enabled() {
        // `enable()` is never called in the test binary.
        assert!(WATCHDOG.get().is_none());
        arm(ExitCause::Fatal);
        assert_eq!(armed(), None);
    }
}
