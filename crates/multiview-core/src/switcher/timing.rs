//! Exact-rational switcher timing (ADR-T015).
//!
//! Every switcher duration is an integer count of output frames at the owning
//! program's exact rational cadence ([`FrameCount`]); API milliseconds convert
//! once, at the boundary, with integer arithmetic only; and an in-flight
//! transition's progress is a pure function of the tick index ([`progress`]).
//!
//! Nothing in this module performs floating-point arithmetic. The lone lossy
//! conversion — the per-tile blend weight handed to the compositor — lives in
//! [`crate::switcher::kernel`], at the pixel boundary.
use crate::time::Rational;
use core::hash::{Hash, Hasher};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Why a [`FrameCount`] could not be constructed.
///
/// A frame count is a **duration**: zero frames is the no-op the schema forbids
/// (ADR-T015 §2), so it is rejected at construction rather than silently
/// clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FrameCountError {
    /// The requested duration was zero frames.
    #[error("a switcher duration must be at least 1 frame (ADR-T015)")]
    Zero,
}

/// A switcher duration or rate in whole output frames at the owning program's
/// exact rational cadence (ADR-T015 §1).
///
/// Frame counts are the invariant under a cadence change: a 30-frame mix is
/// 30 frames at any cadence — 1.001 s at `30000/1001`, 0.6 s at `50/1` — so
/// stored documents stay cadence-portable. The value is always at least one
/// frame (a requested duration never silently becomes a 0-frame no-op), which
/// is why [`progress`] can never divide by zero.
///
/// Serialises as a bare integer (`rate_frames = 30`); a document carrying `0` is
/// rejected by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct FrameCount(u32);

impl FrameCount {
    /// One frame: the minimum duration, and the degenerate `cut` window.
    pub const ONE: Self = Self(1);

    /// The frame count as a plain integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Whether this is the degenerate one-frame duration (a `cut`).
    #[must_use]
    pub const fn is_one(self) -> bool {
        self.0 == 1
    }

    /// Convert an API-boundary duration in milliseconds to frames at `cadence`,
    /// rounding half up with a one-frame floor (ADR-T015 §2):
    ///
    /// ```text
    /// frames(ms) = max(1, floor((2·ms·num + 1000·den) / (2000·den)))
    /// ```
    ///
    /// The conversion is exact integer arithmetic and happens exactly once, at
    /// the boundary — durations are never stored in milliseconds and
    /// re-converted (which would double-round).
    ///
    /// A degenerate (non-positive) `cadence` cannot be converted against, so the
    /// one-frame floor is returned; callers validate the cadence separately
    /// ([`Rational::is_valid`]) before relying on the result.
    #[must_use]
    pub fn from_ms(ms: u32, cadence: Rational) -> Self {
        if cadence.num <= 0 || cadence.den <= 0 {
            return Self::ONE;
        }
        let num = i128::from(cadence.num);
        let den = i128::from(cadence.den);
        let ms = i128::from(ms);
        let frames = (2 * ms * num + 1000 * den) / (2000 * den);
        Self(u32::try_from(frames).unwrap_or(u32::MAX).max(1))
    }

    /// The rounded, display-only millisecond echo of this duration at `cadence`
    /// (ADR-T015 §2): `round(frames · 1000 · den / num)`, half up.
    ///
    /// Responses and state snapshots echo both the canonical frame count and
    /// this integer, so clients see the quantisation rather than discovering it.
    /// A degenerate cadence has no wall-clock answer and returns `0`.
    #[must_use]
    pub fn to_ms(self, cadence: Rational) -> i64 {
        if cadence.num <= 0 || cadence.den <= 0 {
            return 0;
        }
        let numerator = i128::from(self.0) * 1000 * i128::from(cadence.den);
        let denominator = i128::from(cadence.num);
        let rounded = (2 * numerator + denominator) / (2 * denominator);
        i64::try_from(rounded).unwrap_or(i64::MAX)
    }
}

impl TryFrom<u32> for FrameCount {
    type Error = FrameCountError;

    /// Build a frame count, rejecting the zero no-op.
    ///
    /// # Errors
    ///
    /// Returns [`FrameCountError::Zero`] when `frames` is `0`.
    fn try_from(frames: u32) -> Result<Self, Self::Error> {
        if frames == 0 {
            Err(FrameCountError::Zero)
        } else {
            Ok(Self(frames))
        }
    }
}

impl From<FrameCount> for u32 {
    fn from(frames: FrameCount) -> Self {
        frames.0
    }
}

/// An exact transition progress, `elapsed / duration` in `[0, 1]`.
///
/// Carried as a **reduced** integer pair — never a float, never a stored
/// elapsed accumulator (ADR-T015 §3). Equality and ordering are by value, so
/// `2/4` equals `1/2`, and [`Progress::COMPLETE`] is exactly one.
/// [`Progress::to_basis_points`] is the integer wire form; the single lossy
/// conversion to the compositor's per-tile parameter lives in
/// [`crate::switcher::kernel`].
#[derive(Debug, Clone, Copy, Eq)]
pub struct Progress {
    elapsed: u32,
    duration: u32,
}

impl Progress {
    /// Zero progress (`0/1`): the transition has not started.
    pub const ZERO: Self = Self {
        elapsed: 0,
        duration: 1,
    };

    /// Complete progress (`1/1`): the incoming state is fully on air.
    pub const COMPLETE: Self = Self {
        elapsed: 1,
        duration: 1,
    };

    /// Exact progress for `elapsed` frames of a `duration`-frame window, with
    /// `elapsed` clamped into `[0, duration]` and the fraction reduced.
    #[must_use]
    pub fn new(elapsed: u32, duration: FrameCount) -> Self {
        let duration = duration.get();
        let elapsed = elapsed.min(duration);
        if elapsed == 0 {
            return Self::ZERO;
        }
        let divisor = gcd_u32(elapsed, duration);
        Self {
            elapsed: elapsed / divisor,
            duration: duration / divisor,
        }
    }

    /// The reduced numerator — the elapsed share of the window.
    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.elapsed
    }

    /// The reduced denominator — the window the numerator is measured against.
    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.duration
    }

    /// Whether progress is exactly zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.elapsed == 0
    }

    /// Whether progress is exactly one (the transition completes here).
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.elapsed == self.duration
    }

    /// Progress as integer **basis points** in `0..=10000`, rounded half up.
    ///
    /// The wire and event form of a progress is an integer, never a float
    /// (ADR-T015 §3): `10000` means complete.
    #[must_use]
    pub fn to_basis_points(self) -> u16 {
        // round_half_up(elapsed · 10000 / duration)
        let numerator = 2 * u64::from(self.elapsed) * 10_000 + u64::from(self.duration);
        let denominator = 2 * u64::from(self.duration);
        u16::try_from(numerator / denominator).unwrap_or(u16::MAX)
    }
}

impl Default for Progress {
    /// Zero progress.
    fn default() -> Self {
        Self::ZERO
    }
}

impl PartialEq for Progress {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == core::cmp::Ordering::Equal
    }
}

impl Hash for Progress {
    /// Hash the canonical (reduced) pair. Comparison is by value and every value
    /// is stored reduced, so structural equality and value equality coincide —
    /// the hash stays consistent with [`Progress`]'s manual `PartialEq`.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.elapsed.hash(state);
        self.duration.hash(state);
    }
}

impl PartialOrd for Progress {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Progress {
    /// Compare by cross-multiplication; `u32·u32` always fits in `u64`.
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let lhs = u64::from(self.elapsed) * u64::from(other.duration);
        let rhs = u64::from(other.elapsed) * u64::from(self.duration);
        lhs.cmp(&rhs)
    }
}

/// Greatest common divisor of two non-zero `u32` values (Euclid).
const fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

/// The exact progress of a transition that started at `start_tick` and runs for
/// `duration` frames, evaluated at `tick_index` (ADR-0055 decision 1):
///
/// ```text
/// progress(tick) = clamp((tick.index − start_tick) / duration, 0, 1)
/// ```
///
/// Pure `f(tick)`: no wall clock, no stored elapsed accumulator, no float. A
/// missed tick cannot desynchronise a transition — progress is recomputed from
/// the index, exactly as `out_pts = f(tick)` is — and the same start tick plus
/// the same duration always yields the same sequence, replayable under a
/// manual time source (ADR-T015 §3).
///
/// Ticks at or before `start_tick` are exactly [`Progress::ZERO`]; ticks at or
/// after `start_tick + duration` are exactly [`Progress::COMPLETE`].
#[must_use]
pub fn progress(tick_index: u64, start_tick: u64, duration: FrameCount) -> Progress {
    if tick_index <= start_tick {
        return Progress::ZERO;
    }
    let elapsed = tick_index - start_tick;
    if elapsed >= u64::from(duration.get()) {
        return Progress::COMPLETE;
    }
    Progress::new(u32::try_from(elapsed).unwrap_or(u32::MAX), duration)
}
