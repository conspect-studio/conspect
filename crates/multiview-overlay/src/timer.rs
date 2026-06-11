//! Count-up / count-down / down-then-up timers and round-robin page cycling
//! (broadcast brief §5).
//!
//! Timers and the round-robin cycler are **pure value-machines over an injected
//! [`MediaTime`]** (the engine's media clock) — exactly like
//! [`crate::alert::AlertCard`] and `multiview-framestore`'s state machine. They
//! never read a wall clock, never spawn, and never touch the engine tick loop:
//! the renderer (or control surface) calls [`Timer::display`] / [`Timer::phase`]
//! / [`RoundRobin::page_at`] with the current media time and gets back a value.
//!
//! All arithmetic is integer nanoseconds (invariant #3); the displayed
//! `HH:MM:SS` is whole-second truncation. A count-down clamps at zero (never
//! negative); a down-then-up timer counts the overrun upward past the deadline.
//!
//! **Accessibility:** the warning/expiry state is exposed as a
//! [`TimerPhase`] the renderer reads as text/state — the "wrap colour" of a
//! broadcast timer is a *consequence* of the phase, never the only signal.

use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Nanoseconds in one second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// How a [`Timer`] counts.
///
/// Serialised tagged on `mode` (`snake_case` variant names); never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerMode {
    /// Count elapsed time upward from the start (a stopwatch).
    CountUp,
    /// Count down from `duration` (ns) to zero, then hold at zero.
    CountDown {
        /// Total countdown duration, in nanoseconds.
        duration: i64,
    },
    /// Count down from `duration` (ns) to zero, then count the overrun upward.
    DownThenUp {
        /// Countdown duration before the overrun begins, in nanoseconds.
        duration: i64,
    },
}

impl TimerMode {
    /// The countdown duration (ns), or `None` for [`TimerMode::CountUp`].
    #[must_use]
    pub const fn duration(self) -> Option<i64> {
        match self {
            Self::CountUp => None,
            Self::CountDown { duration } | Self::DownThenUp { duration } => Some(duration),
        }
    }
}

/// The presentation phase of a running timer — read by the renderer to pick the
/// "wrap colour", and exposed as text/state for accessibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerPhase {
    /// Not started (or stopped); the readout holds zero.
    #[default]
    Idle,
    /// Running normally.
    Counting,
    /// Running, inside a count-down's warning window: the remaining time is
    /// strictly less than the configured warn window.
    Warning,
    /// Paused; the readout is frozen at the elapsed value.
    Paused,
    /// A count-down that has reached/passed zero (expired / in overrun).
    Expired,
}

impl TimerPhase {
    /// A short lower-case label for the phase (accessibility text).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Counting => "counting",
            Self::Warning => "warning",
            Self::Paused => "paused",
            Self::Expired => "expired",
        }
    }
}

/// The run-state of a [`Timer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    /// Never started.
    Stopped,
    /// Running since `started_at` media time.
    Running { started_at: MediaTime },
    /// Paused with `elapsed` nanoseconds already accrued.
    Paused { elapsed: i64 },
}

/// A broadcast timer: a [`TimerMode`], an optional count-down warning window,
/// and a run-state advanced by [`start`](Timer::start) / [`pause`](Timer::pause)
/// / [`resume`](Timer::resume), all over an injected media clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timer {
    mode: TimerMode,
    /// Warning window, in nanoseconds before the count-down deadline.
    warn_window: i64,
    state: RunState,
}

impl Timer {
    /// A new, stopped timer of the given mode (no warning window).
    #[must_use]
    pub const fn new(mode: TimerMode) -> Self {
        Self {
            mode,
            warn_window: 0,
            state: RunState::Stopped,
        }
    }

    /// Set the count-down warning window (ns before the deadline), builder-style.
    /// Ignored by [`TimerMode::CountUp`].
    #[must_use]
    pub const fn with_warn_window(mut self, ns: i64) -> Self {
        self.warn_window = ns;
        self
    }

    /// Start (or restart) the timer at media time `now`.
    pub fn start(&mut self, now: MediaTime) {
        self.state = RunState::Running { started_at: now };
    }

    /// Pause the timer at media time `now`, freezing the elapsed value.
    /// A no-op unless the timer is running.
    pub fn pause(&mut self, now: MediaTime) {
        if let RunState::Running { started_at } = self.state {
            let elapsed = now.as_nanos().saturating_sub(started_at.as_nanos()).max(0);
            self.state = RunState::Paused { elapsed };
        }
    }

    /// Resume a paused timer at media time `now`, continuing from the frozen
    /// elapsed value. A no-op unless the timer is paused.
    pub fn resume(&mut self, now: MediaTime) {
        if let RunState::Paused { elapsed } = self.state {
            // Anchor the (re)start so that `now - started_at == elapsed`.
            let started_at = MediaTime::from_nanos(now.as_nanos().saturating_sub(elapsed));
            self.state = RunState::Running { started_at };
        }
    }

    /// The timer mode.
    #[must_use]
    pub const fn mode(&self) -> TimerMode {
        self.mode
    }

    /// Elapsed nanoseconds since start at media time `now` (0 if stopped, frozen
    /// if paused). Never negative.
    fn elapsed_ns(&self, now: MediaTime) -> i64 {
        match self.state {
            RunState::Stopped => 0,
            RunState::Paused { elapsed } => elapsed,
            RunState::Running { started_at } => {
                now.as_nanos().saturating_sub(started_at.as_nanos()).max(0)
            }
        }
    }

    /// The signed nanoseconds the readout should show at `now`:
    /// elapsed for count-up; remaining (clamped at 0) for count-down; remaining
    /// then overrun for down-then-up.
    fn readout_ns(&self, now: MediaTime) -> i64 {
        let elapsed = self.elapsed_ns(now);
        match self.mode {
            TimerMode::CountUp => elapsed,
            TimerMode::CountDown { duration } => duration.saturating_sub(elapsed).max(0),
            TimerMode::DownThenUp { duration } => {
                let remaining = duration.saturating_sub(elapsed);
                remaining.abs()
            }
        }
    }

    /// The current presentation [`TimerPhase`] at media time `now`.
    #[must_use]
    pub fn phase(&self, now: MediaTime) -> TimerPhase {
        match self.state {
            RunState::Stopped => TimerPhase::Idle,
            RunState::Paused { .. } => TimerPhase::Paused,
            RunState::Running { .. } => {
                let elapsed = self.elapsed_ns(now);
                match self.mode.duration() {
                    None => TimerPhase::Counting,
                    Some(duration) => {
                        let remaining = duration.saturating_sub(elapsed);
                        if remaining <= 0 {
                            TimerPhase::Expired
                        } else if self.warn_window > 0 && remaining < self.warn_window {
                            TimerPhase::Warning
                        } else {
                            TimerPhase::Counting
                        }
                    }
                }
            }
        }
    }

    /// The whole-second `HH:MM:SS` readout at media time `now`.
    #[must_use]
    pub fn display(&self, now: MediaTime) -> String {
        format_hms(self.readout_ns(now))
    }
}

/// Format a non-negative nanosecond count as `HH:MM:SS` (whole-second
/// truncation). Hours are not wrapped at 24 — a timer can run arbitrarily long.
fn format_hms(ns: i64) -> String {
    let total_secs = ns.max(0) / NANOS_PER_SEC;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    format!("{hours:02}:{mins:02}:{secs:02}")
}

/// A round-robin page cycler: `pages` displays each shown for `dwell_ns`, then
/// advancing and wrapping. Pure over the injected media clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundRobin {
    pages: usize,
    dwell_ns: i64,
}

impl RoundRobin {
    /// A cycler over `pages` displays with a per-page `dwell_ns` dwell.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTimer`] if `pages == 0` or `dwell_ns <= 0` (a
    /// zero dwell would divide by zero).
    pub fn new(pages: usize, dwell_ns: i64) -> Result<Self> {
        if pages == 0 {
            return Err(Error::InvalidTimer("pages must be > 0".to_owned()));
        }
        if dwell_ns <= 0 {
            return Err(Error::InvalidTimer("dwell must be > 0".to_owned()));
        }
        Ok(Self { pages, dwell_ns })
    }

    /// The number of pages in the cycle.
    #[must_use]
    pub const fn pages(self) -> usize {
        self.pages
    }

    /// The zero-based page index shown at media time `now`, anchored at media
    /// time zero. Wraps after the last page.
    #[must_use]
    pub fn page_at(self, now: MediaTime) -> usize {
        // dwell_ns > 0 and pages > 0 are construction invariants.
        let elapsed = now.as_nanos().max(0);
        // Use i128 for the intermediate to avoid overflow on long uptimes.
        let ticks = i128::from(elapsed) / i128::from(self.dwell_ns);
        let pages = usize_to_i128(self.pages);
        let idx = ticks.rem_euclid(pages);
        i128_to_usize(idx)
    }
}

/// Lossless `usize -> i128` (usize is at most 64 bits on supported platforms).
fn usize_to_i128(value: usize) -> i128 {
    i128::try_from(value).unwrap_or(i128::MAX)
}

/// `i128 -> usize` for a value provably in `0..pages` (`rem_euclid` result).
fn i128_to_usize(value: i128) -> usize {
    usize::try_from(value).unwrap_or(0)
}
