//! Minimal safe wrapper that raises the **output-clock thread's** scheduling
//! class where the platform needs a call to do so (#311, ADR-0109 §2).
//!
//! On macOS a plain thread runs at the default `QoS`, and on a saturated host
//! the scheduler then defers its timer wakeups behind every other runnable
//! thread (CI measured ~20 ms mean lateness and ~20% repeats for a 30 fps clock
//! thread that a `tokio` task on the same host did not suffer). Marking the
//! clock thread `QOS_CLASS_USER_INTERACTIVE` — the class Apple documents for
//! work "interacting with the user", such as animations and event handling —
//! gives it the scheduling and timer-leeway treatment that cadence needs. It is
//! not a real-time policy, needs no privilege, and only affects the calling
//! thread.
//!
//! This leaf crate owns the single `unsafe` FFI call behind a safe API (the
//! engine stays `forbid(unsafe_code)`). On every other target
//! [`raise_current_thread`] is a no-op returning [`Raised::NotApplicable`]: Linux
//! uses rustix's safe `setpriority`/timer-slack wrappers in the engine itself.

/// What [`raise_current_thread`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raised {
    /// The calling thread now runs at macOS `QoS` `USER_INTERACTIVE`.
    UserInteractive,
    /// The OS refused (the errno-style code it returned).
    Refused(i32),
    /// Nothing to do on this platform.
    NotApplicable,
}

impl std::fmt::Display for Raised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserInteractive => f.write_str("QoS user-interactive"),
            Self::Refused(code) => write!(f, "default QoS (user-interactive refused: {code})"),
            Self::NotApplicable => f.write_str("default QoS"),
        }
    }
}

/// Raise the calling thread's scheduling class for the output clock (macOS:
/// `QoS` `USER_INTERACTIVE`). Best effort: never fails the caller.
#[cfg(target_os = "macos")]
#[must_use]
#[allow(unsafe_code)] // reason: the crate's single FFI call; see the SAFETY note.
pub fn raise_current_thread() -> Raised {
    // SAFETY: `pthread_set_qos_class_self_np` takes two plain values (a QoS
    // class enum and a relative priority in [QOS_MIN_RELATIVE_PRIORITY, 0];
    // 0 is always valid), touches no caller memory, and only changes the
    // calling thread's own QoS. It returns 0 or an error code; it cannot unwind.
    let code = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if code == 0 {
        Raised::UserInteractive
    } else {
        Raised::Refused(code)
    }
}

/// Non-macOS: nothing to do.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn raise_current_thread() -> Raised {
    Raised::NotApplicable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raising_a_thread_never_fails_and_reports_the_platform_outcome() {
        let outcome = std::thread::spawn(raise_current_thread).join().ok();
        if cfg!(target_os = "macos") {
            assert_eq!(outcome, Some(Raised::UserInteractive));
        } else {
            assert_eq!(outcome, Some(Raised::NotApplicable));
        }
    }
}
