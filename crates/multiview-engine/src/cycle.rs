//! **Round-robin tile cycling** and **freeze / reference-still** tile modes, as
//! pure value machines over an injected [`MediaTime`] (broadcast-multiviewer brief
//! §1, ADR-MV001).
//!
//! A cycling tile shows one source from a rotating roster, advancing on a fixed
//! dwell so a single cell can survey many sources. A frozen tile holds a captured
//! reference still instead of live video (for an A/B reference or to park a flaky
//! feed). Both are decisions the engine samples on its control tick and applies at
//! a frame boundary.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! [`RoundRobin::tick`] and [`FreezeTile`] are pure functions over an injected
//! [`MediaTime`]; they **return** the source/freeze state for the engine to apply
//! and never block, `.await`, or reach into the engine. Cycling is deterministic:
//! the same dwell and clock always yield the same rotation.
use multiview_core::time::MediaTime;

/// A round-robin tile cycler: rotate through a roster of source ids on a fixed
/// dwell.
///
/// Construct with [`RoundRobin::new`] (a non-empty roster and a positive dwell),
/// then drive it once per control tick with [`RoundRobin::tick`]. It advances to
/// the next source each time the dwell elapses, wrapping at the end of the roster.
/// `Clone` for lock-free snapshotting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRobin {
    roster: Vec<String>,
    dwell: MediaTime,
    /// Index of the currently-shown source within `roster`.
    index: usize,
    /// When the current source was selected (the dwell is measured from here).
    since: MediaTime,
}

impl RoundRobin {
    /// Construct a cycler over `roster`, dwelling `dwell` on each source, starting
    /// at `start`.
    ///
    /// Returns [`None`] if `roster` is empty or `dwell` is not strictly positive
    /// (a zero/negative dwell would advance unboundedly within one tick).
    #[must_use]
    pub fn new(roster: Vec<String>, dwell: MediaTime, start: MediaTime) -> Option<Self> {
        if roster.is_empty() || dwell.as_nanos() <= 0 {
            return None;
        }
        Some(Self {
            roster,
            dwell,
            index: 0,
            since: start,
        })
    }

    /// The number of sources in the roster.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roster.len()
    }

    /// Whether the roster is empty. Always `false` for a constructed
    /// [`RoundRobin`] (the constructor rejects an empty roster); provided for
    /// clippy/`len` symmetry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roster.is_empty()
    }

    /// The zero-based index of the currently-shown source.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// The currently-shown source id.
    ///
    /// Always `Some` for a constructed cycler (the roster is non-empty); the
    /// `Option` is defensive against an out-of-range index that cannot occur.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.roster.get(self.index).map(String::as_str)
    }

    /// Advance the cycler to media time `now`, returning the source id to show.
    ///
    /// Each time the dwell elapses the cycler advances one step and re-bases its
    /// dwell. A coarse tick that skips several dwell windows advances by the
    /// correct number of steps in one call (catch-up is computed, not replayed
    /// per missed window), wrapping the roster as needed. A non-monotonic `now`
    /// cannot advance early (elapsed is clamped to zero).
    pub fn tick(&mut self, now: MediaTime) -> &str {
        let dwell_ns = self.dwell.as_nanos().max(1);
        let elapsed = now.saturating_sub(self.since).as_nanos().max(0);
        let steps = elapsed / dwell_ns;
        if steps > 0 {
            let len = self.roster.len().max(1);
            let advance = usize_mod(steps, len);
            self.index = (self.index + advance) % len;
            // Re-base `since` forward by the whole windows consumed so the next
            // dwell starts cleanly (no fractional carry, no drift).
            let consumed = steps.saturating_mul(dwell_ns);
            self.since = MediaTime::from_nanos(self.since.as_nanos().saturating_add(consumed));
        }
        // Defensive: index is always in range for a non-empty roster.
        self.roster.get(self.index).map_or("", String::as_str)
    }
}

/// A freeze / reference-still tile mode: hold a captured still instead of live
/// video.
///
/// Construct with [`FreezeTile::live`], then [`FreezeTile::freeze`] to capture a
/// reference at the current instant (carrying an opaque still id the framestore
/// resolves) and [`FreezeTile::thaw`] to return to live. `Copy`-free because the
/// still id is a `String`; `Clone` for snapshotting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FreezeTile {
    /// The captured still id while frozen; `None` when live.
    still: Option<String>,
    /// When the freeze was captured (for display / audit).
    frozen_at: MediaTime,
}

impl FreezeTile {
    /// A tile in the live (not frozen) state.
    #[must_use]
    pub fn live() -> Self {
        Self {
            still: None,
            frozen_at: MediaTime::ZERO,
        }
    }

    /// Whether the tile is currently frozen on a reference still.
    #[must_use]
    pub const fn is_frozen(&self) -> bool {
        self.still.is_some()
    }

    /// The captured still id while frozen, if any.
    #[must_use]
    pub fn still_id(&self) -> Option<&str> {
        self.still.as_deref()
    }

    /// When the current freeze was captured (meaningful only while frozen).
    #[must_use]
    pub const fn frozen_at(&self) -> MediaTime {
        self.frozen_at
    }

    /// Freeze the tile on reference still `still_id`, captured at media time
    /// `now`. Idempotent on the *act* of freezing: re-freezing replaces the held
    /// still and capture time.
    pub fn freeze(&mut self, still_id: impl Into<String>, now: MediaTime) {
        self.still = Some(still_id.into());
        self.frozen_at = now;
    }

    /// Return the tile to live video. Returns `true` if it was frozen (and is now
    /// live); `false` if it was already live (no-op).
    pub fn thaw(&mut self) -> bool {
        if self.still.is_some() {
            self.still = None;
            self.frozen_at = MediaTime::ZERO;
            true
        } else {
            false
        }
    }
}

/// `steps % len` for the cycle advance, computed in `u64` then narrowed to a
/// `usize` modulus (always `< len`, so the conversion cannot lose information).
fn usize_mod(steps: i64, len: usize) -> usize {
    let len_u = u64::try_from(len).unwrap_or(1).max(1);
    let steps_u = u64::try_from(steps).unwrap_or(0);
    let m = steps_u % len_u;
    usize::try_from(m).unwrap_or(0)
}
