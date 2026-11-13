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
//! On **Linux** (#311 reopen) [`set_current_thread_slice`] asks the fair
//! scheduler (EEVDF, kernel 6.12+) for a short **request size** for the
//! calling thread via `sched_setattr(SCHED_OTHER, sched_runtime)`. A shorter
//! request gives a waking thread an earlier virtual deadline, so it preempts a
//! CPU-bound neighbour promptly instead of queueing behind that neighbour's
//! default ~3 ms slice. It keeps `SCHED_OTHER` and the thread's weight
//! (niceness), so it cannot take more CPU than before; it needs no privilege.
//! Older kernels ignore the field; the read-back reports that honestly.
//!
//! This leaf crate owns the `unsafe` FFI calls behind a safe API (the engine
//! stays `forbid(unsafe_code)`). Elsewhere both functions are no-ops returning
//! `NotApplicable`; Linux niceness and timer slack use rustix's safe wrappers
//! in the engine itself.

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

/// What [`set_current_thread_slice`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slice {
    /// The kernel now uses this request size (ns) for the calling thread, as
    /// read back with `sched_getattr`.
    Applied(u64),
    /// The kernel accepted the call but reads back no custom request size
    /// (a pre-EEVDF kernel, before 6.12): nothing changed.
    Unsupported,
    /// The OS refused (the errno it returned).
    Refused(i32),
    /// Nothing to do on this platform.
    NotApplicable,
}

impl std::fmt::Display for Slice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Applied(ns) => write!(f, "EEVDF slice {} µs", ns / 1_000),
            Self::Unsupported => f.write_str("default slice (kernel has no custom slice)"),
            Self::Refused(code) => write!(f, "default slice (sched_setattr refused: {code})"),
            Self::NotApplicable => f.write_str("default slice"),
        }
    }
}

/// Ask the fair scheduler for a request size (slice) of `slice_ns` for the
/// calling thread, keeping `SCHED_OTHER` and its current niceness. The kernel
/// clamps the value to 0.1..100 ms. Best effort: never fails the caller.
#[cfg(target_os = "linux")]
#[must_use]
#[allow(unsafe_code)] // reason: two syscalls; see the SAFETY notes.
pub fn set_current_thread_slice(slice_ns: u64) -> Slice {
    let Ok(size) = u32::try_from(std::mem::size_of::<libc::sched_attr>()) else {
        return Slice::Refused(libc::EINVAL);
    };
    // `sched_setattr` also sets the nice value: pass the current one so the
    // call changes only the slice (and never asks for a privileged raise).
    let Some(mut attr) = current_attr(size) else {
        return Slice::Refused(last_errno());
    };
    // The size of the struct libc declares (the kernel accepts any size from
    // VER0, 48 bytes, up; no SCHED_FLAG_UTIL_* flag is passed, so the
    // util-clamp fields are never read). Set explicitly, never inherited.
    attr.size = size;
    attr.sched_policy = 0; // SCHED_OTHER
    attr.sched_flags = 0;
    attr.sched_priority = 0;
    attr.sched_runtime = slice_ns;
    attr.sched_deadline = 0;
    attr.sched_period = 0;
    // SAFETY: `attr` is a fully initialised `sched_attr` whose `size` field is
    // set just above to its own size; pid 0
    // targets the calling thread only; flags 0. The kernel only reads `attr`.
    // A syscall returns a value, it cannot unwind.
    let rc = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, std::ptr::addr_of!(attr), 0_u32) };
    if rc != 0 {
        return Slice::Refused(last_errno());
    }
    // The kernel clamps a custom request to 0.1..100 ms and otherwise reports
    // its base slice, so only an exact read-back of the clamped request means
    // the request is in force (a kernel that ignores the field reads back its
    // default).
    let expected = slice_ns.clamp(SLICE_MIN_NS, SLICE_MAX_NS);
    match current_attr(size) {
        Some(now) if now.sched_runtime == expected => Slice::Applied(now.sched_runtime),
        Some(_) => Slice::Unsupported,
        None => Slice::Refused(last_errno()),
    }
}

/// The kernel's clamp on a custom fair-class request size (Linux 6.12
/// `sched/syscalls.c`: 0.1 ms .. 100 ms).
#[cfg(target_os = "linux")]
const SLICE_MIN_NS: u64 = 100_000;
#[cfg(target_os = "linux")]
const SLICE_MAX_NS: u64 = 100_000_000;

/// The calling thread's current `sched_attr`, `None` on error (see errno).
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // reason: one syscall; see the SAFETY note.
fn current_attr(size: u32) -> Option<libc::sched_attr> {
    // SAFETY: an all-zero `sched_attr` is a valid value (plain integers).
    let mut attr: libc::sched_attr = unsafe { std::mem::zeroed() };
    attr.size = size;
    // SAFETY: `attr` is a writable, correctly sized `sched_attr` and `size`
    // is its size; pid 0 is the calling thread; flags 0. The kernel writes at
    // most `size` bytes into it. A syscall cannot unwind.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sched_getattr,
            0,
            std::ptr::addr_of_mut!(attr),
            size,
            0_u32,
        )
    };
    (rc == 0).then_some(attr)
}

#[cfg(target_os = "linux")]
fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

/// Not Linux: nothing to do.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn set_current_thread_slice(_slice_ns: u64) -> Slice {
    Slice::NotApplicable
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slice is applied (EEVDF kernels) or honestly reported as
    /// unsupported (older kernels); it keeps the thread's niceness, and it is
    /// per thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_slice_changes_only_the_calling_threads_request_size() {
        let size = u32::try_from(std::mem::size_of::<libc::sched_attr>()).unwrap_or(0);
        let outcome = std::thread::spawn(move || {
            let before = current_attr(size).map(|a| a.sched_nice);
            let slice = set_current_thread_slice(300_000);
            let after = current_attr(size);
            (
                slice,
                before,
                after.map(|a| a.sched_nice),
                after.map(|a| a.sched_runtime),
            )
        })
        .join()
        .ok();
        let Some((slice, before, after, runtime)) = outcome else {
            panic!("the probe thread panicked");
        };
        assert!(
            matches!(slice, Slice::Applied(300_000) | Slice::Unsupported),
            "{slice:?}"
        );
        assert!(before.is_some());
        if matches!(slice, Slice::Applied(_)) {
            assert_eq!(runtime, Some(300_000), "the kernel reads back the request");
        }
        assert_eq!(before, after, "the nice value is unchanged");
        if matches!(slice, Slice::Applied(_)) {
            let here = current_attr(size).map(|a| a.sched_runtime);
            assert_ne!(here, Some(300_000), "per thread, not per process");
        }
    }

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
