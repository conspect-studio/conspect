//! The SAP **datagram rate limiter** — a pure, fixed-window admission gate for
//! the expensive session-table fold (ADR-0041 §3, panel F4).
//!
//! Every accepted SAP datagram triggers an O(n) read-copy-update clone + publish
//! (and, historically, a purge scan) in the [`SapSessionTable`](super::session::SapSessionTable):
//! the whole snapshot and its SDP buffers are cloned so the wait-free reader is
//! never blocked. That is fine at the RFC 2974 announce rate (≥ ~one announce
//! per 30 s per session) but a **spoofed-origin flood** — distinct `(hash,
//! origin)` pairs at line rate — bypasses the per-origin cap and would force that
//! expensive clone *per datagram*, starving the shared control-plane tokio
//! runtime (a management-plane `DoS`; inv #10 — discovery must never be able to
//! back-pressure or starve the engine's host).
//!
//! [`SapRateLimiter`] bounds the **rate** of expensive folds *before* they run:
//! the [`SapListener`](super::transport::SapListener) consults it per datagram
//! and drops the datagram cheaply (just the `recv`) when the window budget is
//! spent, so the O(n) fold runs at most `burst` times per `window` regardless of
//! the inbound datagram rate. It is a **fixed-window** counter — integer-only,
//! allocation-free, and independent of any wall clock (the caller injects a
//! monotonic `now`), so it is fully deterministic and unit-tested in the default
//! build even though only the feature-gated socket listener drives it.
//!
//! It is **not** a security boundary on its own (SAP is unauthenticated and
//! spoofable regardless); it is the CPU-exhaustion floor that keeps an
//! unauthenticated flood from consuming the runtime. The bound is generous for
//! legitimate multi-session discovery: the table itself caps retained sessions,
//! so a burst large enough to fill it is admitted while a sustained flood is
//! throttled to the steady window rate.

use std::time::Duration;

/// The default number of datagrams admitted into the expensive fold path per
/// [`DEFAULT_ACCEPT_WINDOW`]. Generous for legitimate discovery (a real
/// deployment announces tens of sessions; the table retains at most a few
/// hundred) while throttling a flood to this steady rate.
pub const DEFAULT_ACCEPT_BURST: u32 = 128;

/// The default fixed window over which [`DEFAULT_ACCEPT_BURST`] datagrams are
/// admitted. One second bounds the worst-case expensive-fold rate to roughly
/// `2 × burst` per second (the fixed-window edge effect) — orders of magnitude
/// below the line-rate flood the guard defends against.
pub const DEFAULT_ACCEPT_WINDOW: Duration = Duration::from_secs(1);

/// A fixed-window datagram admission gate for the SAP fold path.
///
/// [`allow`](Self::allow) returns `true` for at most `max_per_window` datagrams
/// within each `window`, then `false` until the window rolls over. Time is
/// supplied by the caller (a monotonic `now`), so the limiter is deterministic
/// and holds no clock of its own.
#[derive(Debug, Clone)]
pub struct SapRateLimiter {
    window: Duration,
    max_per_window: u32,
    /// The start of the current window, or `None` before the first datagram.
    window_start: Option<Duration>,
    /// Datagrams already admitted in the current window.
    used: u32,
}

impl SapRateLimiter {
    /// A limiter admitting at most `max_per_window` datagrams per `window`.
    ///
    /// A `max_per_window` of `0` denies every datagram; a zero `window` makes
    /// every datagram start a fresh window (so at most `max_per_window` are ever
    /// admitted back-to-back before the counter resets on the next call).
    #[must_use]
    pub const fn new(max_per_window: u32, window: Duration) -> Self {
        Self {
            window,
            max_per_window,
            window_start: None,
            used: 0,
        }
    }

    /// A limiter with the default burst and window ([`DEFAULT_ACCEPT_BURST`] per
    /// [`DEFAULT_ACCEPT_WINDOW`]).
    #[must_use]
    pub const fn with_defaults() -> Self {
        Self::new(DEFAULT_ACCEPT_BURST, DEFAULT_ACCEPT_WINDOW)
    }

    /// Whether a datagram observed at `now` may enter the expensive parse+fold
    /// path. Consumes one unit of the current window's budget when it returns
    /// `true`; returns `false` (drop cheaply) once the window budget is spent.
    ///
    /// `now` is a monotonic elapsed time; it must not go backwards within a
    /// window (a backwards step simply keeps the current window open, never
    /// widening the budget).
    pub fn allow(&mut self, now: Duration) -> bool {
        let rolled = match self.window_start {
            Some(start) => now.saturating_sub(start) >= self.window,
            None => true,
        };
        if rolled {
            self.window_start = Some(now);
            self.used = 0;
        }
        if self.used < self.max_per_window {
            self.used = self.used.saturating_add(1);
            true
        } else {
            false
        }
    }
}
