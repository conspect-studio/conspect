//! IDENTIFY (flash-a-tile) operator-locate overlay (broadcast brief §5).
//!
//! When an operator hits "identify" on a tile in the control surface, that tile
//! flashes a high-visibility marker so they can find it on the wall. This is a
//! pure value-machine over the engine's media clock — exactly like
//! [`crate::alert::AlertCard`] — so it never reads a wall clock and never
//! touches the engine tick loop: the renderer calls [`Identify::is_on`] /
//! [`Identify::is_active`] / [`Identify::badge`] with the current
//! [`MediaTime`] and gets back a value.
//!
//! The flash is a square wave: **on** for the first half of each `period`,
//! **off** for the second half, repeating until the total `duration` elapses.
//! All arithmetic is integer nanoseconds; a zero period degrades to a steady-on
//! marker (no divide-by-zero).
//!
//! **Accessibility:** while active the overlay also carries a text
//! [`badge`](Identify::badge) ("IDENTIFY"), so the "find this tile" signal reads
//! as text — not flash/colour alone.

use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

/// The fixed text badge shown while an identify is active.
const IDENTIFY_BADGE: &str = "IDENTIFY";

/// The flash-a-tile IDENTIFY overlay state: a flash `period` and total
/// `duration` (both ns), plus the trigger time once armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identify {
    /// Full flash period in nanoseconds (on for the first half, off the second).
    period_ns: i64,
    /// Total duration the identify stays active, in nanoseconds.
    duration_ns: i64,
    /// When the current flash window started; `None` while idle.
    triggered_at: Option<MediaTime>,
}

impl Identify {
    /// A new, idle identify with the given flash `period_ns` and total active
    /// `duration_ns`. Negative inputs are clamped to zero.
    #[must_use]
    pub const fn new(period_ns: i64, duration_ns: i64) -> Self {
        Self {
            period_ns: if period_ns < 0 { 0 } else { period_ns },
            duration_ns: if duration_ns < 0 { 0 } else { duration_ns },
            triggered_at: None,
        }
    }

    /// Arm (or re-arm) the identify at media time `now`, restarting the flash
    /// window from the start.
    pub fn trigger(&mut self, now: MediaTime) {
        self.triggered_at = Some(now);
    }

    /// Cancel an active identify immediately (the operator dismissed it).
    pub fn cancel(&mut self) {
        self.triggered_at = None;
    }

    /// The flash period in nanoseconds.
    #[must_use]
    pub const fn period_ns(&self) -> i64 {
        self.period_ns
    }

    /// The total active duration in nanoseconds.
    #[must_use]
    pub const fn duration_ns(&self) -> i64 {
        self.duration_ns
    }

    /// Nanoseconds since the trigger at `now`, or `None` if idle or before the
    /// trigger instant.
    fn since_trigger(&self, now: MediaTime) -> Option<i64> {
        let started = self.triggered_at?;
        let delta = now.as_nanos().saturating_sub(started.as_nanos());
        if delta < 0 {
            None
        } else {
            Some(delta)
        }
    }

    /// Whether the identify is within its active window at media time `now`.
    #[must_use]
    pub fn is_active(&self, now: MediaTime) -> bool {
        match self.since_trigger(now) {
            Some(delta) => delta < self.duration_ns,
            None => false,
        }
    }

    /// Whether the flash marker is lit (on) at media time `now`.
    ///
    /// `false` whenever the identify is inactive. While active, the marker is on
    /// for the first half of each [`period`](Identify::period_ns) and off for
    /// the second; a zero period is steady-on.
    #[must_use]
    pub fn is_on(&self, now: MediaTime) -> bool {
        let Some(delta) = self.since_trigger(now) else {
            return false;
        };
        if delta >= self.duration_ns {
            return false;
        }
        if self.period_ns <= 0 {
            // Degenerate period: steady-on while active.
            return true;
        }
        let phase = delta.rem_euclid(self.period_ns);
        // On for the first half of the period.
        phase * 2 < self.period_ns
    }

    /// The accessibility text badge while active, or `None` once expired/idle.
    #[must_use]
    pub fn badge(&self, now: MediaTime) -> Option<&'static str> {
        if self.is_active(now) {
            Some(IDENTIFY_BADGE)
        } else {
            None
        }
    }
}
