//! The pure origin pacer (design 4.1): turns the run's output timeline into the
//! tick the player stamps next and the instant it may release a frame.
//!
//! It reads no clock. The executor passes `run_now` (`time.now - seed`, the same
//! value `LiveStamper::stamp` uses) in, or `None` while the output origin is not
//! seeded yet. Tests inject `run_now` directly, so every assertion is on tick
//! counts and event order.

use multiview_core::time::{MediaTime, Rational};

/// How many output ticks ahead of `run_now` a frame is stamped (`LEAD`).
pub const STAMP_LEAD_TICKS: i64 = 2;

/// Output-anchored pacing for one player. Plain data: no clock, no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginPacer {
    cadence: Rational,
    lead: i64,
}

impl OriginPacer {
    /// A pacer for the run `cadence` with the standard [`STAMP_LEAD_TICKS`].
    #[must_use]
    pub const fn new(cadence: Rational) -> Self {
        Self {
            cadence,
            lead: STAMP_LEAD_TICKS,
        }
    }

    /// The smallest tick whose instant is at or after `run_now`.
    #[must_use]
    pub fn ceil_tick(&self, run_now: MediaTime) -> i64 {
        let mut k = run_now.to_tick(self.cadence);
        while MediaTime::from_tick(k.saturating_sub(1), self.cadence) >= run_now {
            k = k.saturating_sub(1);
        }
        while MediaTime::from_tick(k, self.cadence) < run_now {
            k = k.saturating_add(1);
        }
        k
    }

    /// The tick a frame must be stamped at or after, so it is never in the past:
    /// `ceil_tick(run_now) + LEAD`. Used for the first publish, for
    /// `MediaPlayer::catch_up`, and as the commit tick.
    #[must_use]
    pub fn min_tick(&self, run_now: MediaTime) -> i64 {
        self.ceil_tick(run_now).saturating_add(self.lead)
    }

    /// The first tick to publish at, or `None` while the origin is unseeded
    /// (publish nothing until it is).
    #[must_use]
    pub fn first_tick(&self, run_now: Option<MediaTime>) -> Option<i64> {
        run_now.map(|now| self.min_tick(now))
    }

    /// Whether a frame stamped at `tick` may be released at `run_now`: true
    /// once `run_now >= from_tick(tick - LEAD)`.
    #[must_use]
    pub fn may_release(&self, tick: i64, run_now: MediaTime) -> bool {
        run_now >= MediaTime::from_tick(tick.saturating_sub(self.lead), self.cadence)
    }
}
