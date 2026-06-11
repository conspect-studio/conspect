//! The fixed-cadence monotonic **output clock** — the heart of invariant #1.
//!
//! One clock emits exactly **one valid, correctly-timestamped frame per tick,
//! forever**, independent of any input. The presentation timestamp of a tick is
//! a pure function of the integer tick counter and the fixed output cadence:
//!
//! ```text
//! out_pts = MediaTime::from_tick(tick, cadence)
//! ```
//!
//! computed exactly via [`multiview_core::time::rescale`] every time — **never
//! float-accumulated** (which would compound rounding error frame after frame
//! and drift ~3.6 s/hour for 29.97) and **never derived from an input** (a
//! stalled, bursting, or wrong-fps source can neither stall nor speed up the
//! output, per ADR-T001/R001).
//!
//! ## Injected time source
//!
//! The wall-clock seam is the [`TimeSource`] trait. Production wires a
//! [`MonotonicTimeSource`] (`Instant`-backed, `CLOCK_MONOTONIC`); tests inject a
//! [`ManualTimeSource`] so the whole clock is **deterministic** with no real
//! sleeps. The clock computes, for each tick, the absolute deadline at which
//! that frame is due (`tick / cadence` seconds after the seed instant); a driver
//! waits until that deadline. Because deadlines are absolute (recomputed from
//! the tick counter, not accumulated), OS sleep jitter cannot cause cumulative
//! drift.
use core::time::Duration;

use multiview_core::time::{MediaTime, Rational};

use crate::error::{Error, Result};

/// The injectable monotonic wall-clock seam.
///
/// The output clock never reads `Instant::now()` directly; it asks a
/// `TimeSource` for "nanoseconds since this source started". Production uses
/// [`MonotonicTimeSource`]; tests use [`ManualTimeSource`] to advance time by
/// hand, making the entire clock deterministic.
///
/// Implementations must be **monotonic** (never report a smaller value than a
/// previous call) so the clock's deadline arithmetic stays sane.
pub trait TimeSource: Send + Sync {
    /// Nanoseconds elapsed since this source's fixed origin. Monotonic
    /// non-decreasing.
    fn now_nanos(&self) -> i64;
}

/// A real monotonic time source backed by [`std::time::Instant`]
/// (`CLOCK_MONOTONIC` on Linux, `mach_continuous_time` on macOS).
#[derive(Debug, Clone)]
pub struct MonotonicTimeSource {
    origin: std::time::Instant,
}

impl MonotonicTimeSource {
    /// Create a source whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for MonotonicTimeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for MonotonicTimeSource {
    fn now_nanos(&self) -> i64 {
        // `Instant` is monotonic by contract. The elapsed duration cannot exceed
        // `i64::MAX` ns (~292 years) in any realistic run; saturate defensively
        // rather than risk an `as`-cast or a panic on overflow.
        let nanos = self.origin.elapsed().as_nanos();
        i64::try_from(nanos).unwrap_or(i64::MAX)
    }
}

/// A manually-advanced time source for deterministic tests.
///
/// Time only moves when the test calls [`ManualTimeSource::advance`] (or
/// [`ManualTimeSource::set`]); [`TimeSource::now_nanos`] returns the current
/// stored value. This is what lets the clock's tick-pacing be tested with zero
/// real sleeps and zero flakiness.
#[derive(Debug)]
pub struct ManualTimeSource {
    now_nanos: core::sync::atomic::AtomicI64,
}

impl ManualTimeSource {
    /// Create a source positioned at `t = 0`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            now_nanos: core::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Advance the clock by `delta` (saturating; never moves backwards).
    pub fn advance(&self, delta: Duration) {
        let add = i64::try_from(delta.as_nanos()).unwrap_or(i64::MAX);
        self.now_nanos
            .fetch_add(add, core::sync::atomic::Ordering::AcqRel);
    }

    /// Set the clock to an absolute nanosecond value (must not move backwards;
    /// a smaller value is clamped to the current value to preserve monotonicity).
    pub fn set(&self, nanos: i64) {
        let cur = self.now_nanos.load(core::sync::atomic::Ordering::Acquire);
        self.now_nanos
            .store(nanos.max(cur), core::sync::atomic::Ordering::Release);
    }
}

impl Default for ManualTimeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for ManualTimeSource {
    fn now_nanos(&self) -> i64 {
        self.now_nanos.load(core::sync::atomic::Ordering::Acquire)
    }
}

/// One output tick: a strictly increasing integer index plus its exact
/// presentation timestamp.
///
/// The timestamp is `out_pts = f(tick)` (invariant #1). A `Tick` is the unit the
/// drive loop consumes; the output stage re-stamps every packet from
/// [`Tick::pts`] (invariant #3 — raw input PTS never reaches a muxer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    /// The monotonic tick index (`0`-based, strictly increasing, never reused).
    pub index: u64,
    /// The exact presentation timestamp for this tick on the internal timeline.
    pub pts: MediaTime,
}

/// The fixed-cadence monotonic output clock.
///
/// Holds the immutable output `cadence` (an exact rational — never a float fps)
/// and the running integer tick counter. [`OutputClock::pts_at`] is the pure
/// `out_pts = f(tick)` function; [`OutputClock::tick`] advances the counter and
/// returns the next [`Tick`]; [`OutputClock::deadline_nanos`] gives the absolute
/// instant a tick is due, for a pacing driver.
///
/// The clock owns no inputs and never blocks: it is a counter and an arithmetic
/// function, nothing more. That is precisely what makes the output bulletproof.
#[derive(Debug, Clone)]
pub struct OutputClock {
    cadence: Rational,
    /// The number of ticks already emitted; the index of the next tick.
    next_index: u64,
}

impl OutputClock {
    /// Construct a clock at the given fixed output `cadence` (frames/sec).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCadence`] if the cadence is not a positive,
    /// non-degenerate rational (`num > 0`, `den > 0`) — the clock must have an
    /// exact, usable frame rate (invariant #1/#3).
    pub fn new(cadence: Rational) -> Result<Self> {
        if !cadence.is_valid() || cadence.num <= 0 || cadence.den <= 0 {
            return Err(Error::invalid_cadence(cadence));
        }
        Ok(Self {
            cadence,
            next_index: 0,
        })
    }

    /// The fixed output cadence (exact rational).
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// The index the next [`OutputClock::tick`] call will emit.
    #[must_use]
    pub const fn next_index(&self) -> u64 {
        self.next_index
    }

    /// The exact presentation timestamp of tick `index` at this clock's cadence.
    ///
    /// This is the canonical `out_pts = f(tick)` (invariant #1): recomputed from
    /// the integer `index` every time via [`MediaTime::from_tick`], never
    /// accumulated. Pure and total.
    #[must_use]
    pub fn pts_at(&self, index: u64) -> MediaTime {
        // Tick indices are non-negative and bounded far below `i64::MAX` in any
        // realistic run; saturate rather than risk an `as`-cast.
        let tick = i64::try_from(index).unwrap_or(i64::MAX);
        MediaTime::from_tick(tick, self.cadence)
    }

    /// The absolute deadline (nanoseconds on the [`TimeSource`] timeline) at
    /// which tick `index` is due, given the `seed` instant the clock started.
    ///
    /// `deadline = seed + pts_at(index)`. A pacing driver waits until its time
    /// source reaches this value before emitting the tick. Because the deadline
    /// is derived from the tick counter (not accumulated), sleep jitter cannot
    /// cause cumulative drift (ADR-T001).
    #[must_use]
    pub fn deadline_nanos(&self, index: u64, seed_nanos: i64) -> i64 {
        seed_nanos.saturating_add(self.pts_at(index).as_nanos())
    }

    /// Advance the counter and return the next [`Tick`].
    ///
    /// Infallible and non-blocking: the clock always produces a tick. The index
    /// is strictly increasing and the pts is strictly monotonic (for a positive
    /// cadence). Saturates the counter at [`u64::MAX`] rather than wrapping — at
    /// 60 fps that bound is ~9.7 billion years away, so it is unreachable, but
    /// saturation keeps the function total and panic-free on the hot path.
    pub fn tick(&mut self) -> Tick {
        let index = self.next_index;
        let pts = self.pts_at(index);
        self.next_index = self.next_index.saturating_add(1);
        Tick { index, pts }
    }

    /// One tick period as a [`Duration`] (`1/cadence` seconds), for pacing.
    ///
    /// Returns [`Duration::ZERO`] for a degenerate cadence (which
    /// [`OutputClock::new`] already rejects).
    #[must_use]
    pub fn tick_period(&self) -> Duration {
        // period_ns = pts_at(1) - pts_at(0); exact, derived from the cadence.
        let ns = self.pts_at(1).as_nanos().max(0);
        Duration::from_nanos(u64::try_from(ns).unwrap_or(0))
    }
}
