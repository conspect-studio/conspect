//! The per-tile failure-ladder state machine (invariant #2).
//!
//! A tile rides the ladder driven purely by **elapsed time since the last fresh
//! frame** measured against three configurable thresholds:
//!
//! ```text
//! LIVE ──(no fresh frame for `hold`)──▶ STALE          (hold last-good frame)
//! STALE ──(elapsed ≥ `stale`)─────────▶ RECONNECTING   (reconnect card)
//! RECONNECTING ──(elapsed ≥ `nosignal`)▶ NO_SIGNAL      (SIGNAL LOST slate)
//! any state ──(fresh frame arrives)───▶ LIVE
//! ```
//!
//! The mapping from elapsed time to state is a **pure function**
//! ([`classify`]), so the whole transition table is exhaustively property- and
//! state-machine-testable with an injected clock — no real time, no sleeps.
//!
//! The canonical [`SourceState`] enum lives in `multiview-core`
//! (`LIVE`/`STALE`/`RECONNECTING`/`NO_SIGNAL`); this module supplies the timing
//! policy that drives transitions between its variants.
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;

use crate::error::{Error, Result};

/// The three staleness thresholds defining a tile's failure ladder.
///
/// Each is the **elapsed time since the last fresh frame** at which the tile
/// crosses into the next-worse state. They must be strictly increasing
/// (`hold < stale < nosignal`) and positive; build with [`TileThresholds::new`]
/// (validating) or [`TileThresholds::from_millis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileThresholds {
    hold: MediaTime,
    stale: MediaTime,
    nosignal: MediaTime,
}

impl TileThresholds {
    /// Construct from three thresholds, validating that they are positive and
    /// strictly increasing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NonPositiveThreshold`] if any threshold is `<= 0`, or
    /// [`Error::NonMonotonicThresholds`] if they are not strictly increasing
    /// (`hold < stale < nosignal`).
    pub fn new(hold: MediaTime, stale: MediaTime, nosignal: MediaTime) -> Result<Self> {
        for t in [hold, stale, nosignal] {
            if t.as_nanos() <= 0 {
                return Err(Error::NonPositiveThreshold(t.as_nanos()));
            }
        }
        if !(hold < stale && stale < nosignal) {
            return Err(Error::NonMonotonicThresholds {
                hold_ns: hold.as_nanos(),
                stale_ns: stale.as_nanos(),
                nosignal_ns: nosignal.as_nanos(),
            });
        }
        Ok(Self {
            hold,
            stale,
            nosignal,
        })
    }

    /// Convenience constructor taking millisecond durations.
    ///
    /// # Errors
    ///
    /// As [`TileThresholds::new`]. Each millisecond value is converted to
    /// nanoseconds with saturation, so absurd inputs cannot overflow.
    pub fn from_millis(hold_ms: i64, stale_ms: i64, nosignal_ms: i64) -> Result<Self> {
        let to_ns = |ms: i64| MediaTime::from_nanos(ms.saturating_mul(1_000_000));
        Self::new(to_ns(hold_ms), to_ns(stale_ms), to_ns(nosignal_ms))
    }

    /// The `hold` threshold: `LIVE -> STALE` after this much starvation.
    #[must_use]
    pub const fn hold(self) -> MediaTime {
        self.hold
    }

    /// The `stale` threshold: `STALE -> RECONNECTING` after this much.
    #[must_use]
    pub const fn stale(self) -> MediaTime {
        self.stale
    }

    /// The `nosignal` threshold: `RECONNECTING -> NO_SIGNAL` after this much.
    #[must_use]
    pub const fn nosignal(self) -> MediaTime {
        self.nosignal
    }
}

impl Default for TileThresholds {
    /// Broadcast-style defaults: hold 500 ms, stale 2 s, no-signal 10 s.
    ///
    /// These are known-valid (positive, strictly increasing), so construction
    /// cannot fail.
    fn default() -> Self {
        Self {
            hold: MediaTime::from_nanos(500_000_000),
            stale: MediaTime::from_nanos(2_000_000_000),
            nosignal: MediaTime::from_nanos(10_000_000_000),
        }
    }
}

/// Classify a tile's [`SourceState`] purely from the elapsed time since its
/// last fresh frame.
///
/// This is the heart of the failure ladder and is intentionally a pure,
/// branch-only function so it can be exhaustively property-tested:
///
/// * `elapsed < hold` → [`SourceState::Live`]
/// * `hold <= elapsed < stale` → [`SourceState::Stale`]
/// * `stale <= elapsed < nosignal` → [`SourceState::Reconnecting`]
/// * `elapsed >= nosignal` → [`SourceState::NoSignal`]
///
/// A negative `elapsed` (clock ran backwards — should not happen on a monotonic
/// timeline) is clamped to `0` and therefore classified [`SourceState::Live`],
/// matching the "fresh frame just arrived" case.
#[must_use]
pub fn classify(elapsed: MediaTime, thresholds: TileThresholds) -> SourceState {
    let e = elapsed.as_nanos().max(0);
    if e < thresholds.hold.as_nanos() {
        SourceState::Live
    } else if e < thresholds.stale.as_nanos() {
        SourceState::Stale
    } else if e < thresholds.nosignal.as_nanos() {
        SourceState::Reconnecting
    } else {
        SourceState::NoSignal
    }
}
