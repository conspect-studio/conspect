//! Monotonic media time and exact rationals.
//!
//! This module owns Multiview's timing **primitives** and enforces invariant #3
//! (the unified timing model): all internal time is integer nanoseconds or
//! exact rationals, and frame rates are carried as exact `num/den`
//! rationals — **never** as floating-point fps, which drifts (~3.6 s/hour for
//! 29.97 represented as `29.97`).
//!
//! The output-clock invariant (#1) computes presentation timestamps as a pure
//! function of the tick counter: `out_pts = f(tick)`. That function lives in
//! [`MediaTime::from_tick`]; it is recomputed from the integer `tick` every
//! time and is **never** accumulated in a float (which would compound rounding
//! error frame after frame).
use serde::{Deserialize, Serialize};

/// Greatest common divisor of two non-negative `i128` values (Euclid).
///
/// Operates on magnitudes; callers normalize sign separately.
const fn gcd_i128(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// An exact rational, e.g. a frame rate or timebase (`num/den`).
///
/// Equality and ordering are by **value** (cross-multiplied), so `2/4` equals
/// `1/2`. Construct with [`Rational::new`]; call [`Rational::reduce`] to obtain
/// the canonical form (reduced fraction, sign carried on the numerator, and a
/// strictly positive denominator).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rational {
    /// Numerator (carries the sign in canonical form).
    pub num: i64,
    /// Denominator (strictly positive in canonical form).
    pub den: i64,
}

impl Rational {
    /// Construct a rational from a numerator and denominator.
    ///
    /// The value is stored verbatim; use [`Rational::reduce`] to normalize.
    #[must_use]
    pub const fn new(num: i64, den: i64) -> Self {
        Self { num, den }
    }

    /// 25 fps (PAL).
    pub const FPS_25: Self = Self::new(25, 1);
    /// 30 fps (integer; not the NTSC family).
    pub const FPS_30: Self = Self::new(30, 1);
    /// 29.97 fps (NTSC, exactly `30000/1001`).
    pub const FPS_29_97: Self = Self::new(30_000, 1001);
    /// 50 fps.
    pub const FPS_50: Self = Self::new(50, 1);
    /// 60 fps (integer).
    pub const FPS_60: Self = Self::new(60, 1);
    /// 59.94 fps (NTSC, exactly `60000/1001`).
    pub const FPS_59_94: Self = Self::new(60_000, 1001);
    /// 23.976 fps (NTSC film, exactly `24000/1001`).
    pub const FPS_23_976: Self = Self::new(24_000, 1001);

    /// Whether the value is exactly zero (numerator is zero).
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.num == 0
    }

    /// Return the canonical (reduced) form: reduced fraction, sign on the
    /// numerator, strictly positive denominator.
    ///
    /// A zero numerator reduces to `0/1`. A zero denominator is left as `0/0`
    /// (degenerate; callers should treat a zero denominator as invalid and use
    /// [`Rational::is_valid`] before relying on the value).
    #[must_use]
    pub fn reduce(self) -> Self {
        if self.den == 0 {
            // Degenerate: cannot normalize. Preserve as-is for the caller to reject.
            return self;
        }
        if self.num == 0 {
            return Self { num: 0, den: 1 };
        }
        // Work in i128 to avoid overflow on `i64::MIN.abs()` (which is `2^63`,
        // one past `i64::MAX`).
        let num = i128::from(self.num);
        let den = i128::from(self.den);
        let sign = if (num < 0) ^ (den < 0) { -1 } else { 1 };
        let num_abs = num.abs();
        let den_abs = den.abs();
        let g = gcd_i128(num_abs, den_abs);
        // `g >= 1` divides both, so the reduced magnitudes only shrink.
        let rn = (num_abs / g) * sign;
        let rd = den_abs / g;
        // `rn` (signed numerator) ALWAYS fits `i64`: `|num| <= 2^63` so
        // `|rn| = |num|/g <= 2^63`, which is in range for either sign
        // (`i64::MIN == -2^63`). `rd` (positive denominator) fits `i64` UNLESS
        // it equals `2^63`, which can happen only for the degenerate input
        // `den == i64::MIN` coprime to the numerator (`g == 1`) — a value with
        // no positive-`i64`-denominator canonical form. In that single case we
        // cannot normalize, so (matching the zero-denominator branch) we return
        // the input verbatim for the caller to reject via [`Rational::is_valid`]
        // rather than fabricating a wrong/sign-flipped result. `unwrap_or` is
        // never reached for `rn`, and for `rd` only on that unrepresentable
        // input, where preserving `self` is the deliberate, documented fallback.
        match (i64::try_from(rn), i64::try_from(rd)) {
            (Ok(num), Ok(den)) => Self { num, den },
            _ => self,
        }
    }

    /// Whether this rational is usable (non-zero, finite denominator).
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.den != 0
    }

    /// The reciprocal `den/num`, in canonical form.
    ///
    /// Returns [`None`] if the numerator is zero (the reciprocal would divide
    /// by zero) or the denominator is zero (the input is degenerate).
    #[must_use]
    pub fn inv(self) -> Option<Self> {
        if self.num == 0 || self.den == 0 {
            return None;
        }
        Some(Self::new(self.den, self.num).reduce())
    }

    /// Multiply two rationals, returning the reduced product.
    ///
    /// Cross-reduces before multiplying to keep intermediates small, then
    /// returns [`None`] if the reduced numerator or denominator still does not
    /// fit in `i64`.
    #[must_use]
    pub fn checked_mul(self, other: Self) -> Option<Self> {
        if self.den == 0 || other.den == 0 {
            return None;
        }
        let an = i128::from(self.num);
        let ad = i128::from(self.den);
        let bn = i128::from(other.num);
        let bd = i128::from(other.den);
        let num = an.checked_mul(bn)?;
        let den = ad.checked_mul(bd)?;
        // Reduce the i128 product, then check it fits i64.
        if num == 0 {
            return Some(Self { num: 0, den: 1 });
        }
        let sign = if (num < 0) ^ (den < 0) { -1 } else { 1 };
        let g = gcd_i128(num.abs(), den.abs());
        let rn = (num.abs() / g) * sign;
        let rd = den.abs() / g;
        Some(Self {
            num: i64::try_from(rn).ok()?,
            den: i64::try_from(rd).ok()?,
        })
    }

    /// Approximate floating value — for display/diagnostics only, never for
    /// timing math.
    ///
    /// Frame-rate/timebase numerators and denominators always fit in `i32`.
    /// Values outside that range (not expected for real timebases) are clamped
    /// to the `i32` bounds before conversion, since the result is purely a
    /// human-readable approximation and must never feed timing arithmetic
    /// (invariant #3).
    #[must_use]
    pub fn as_f64(self) -> f64 {
        // `clamp` keeps the operands inside the lossless `i32 -> f64` domain, so
        // no `as` cast (and no precision loss) is needed — `f64::from` is exact
        // for `i32`.
        let num = self.num.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        let den = self.den.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        let num = i32::try_from(num).map_or(f64::NAN, f64::from);
        let den = i32::try_from(den).map_or(f64::NAN, f64::from);
        num / den
    }

    /// The rounded per-tick **period in nanoseconds** for this cadence — i.e. the
    /// nanosecond span of one tick, `round(NANOS_PER_SEC · den / num)`.
    ///
    /// This is exactly `MediaTime::from_tick(1, self).as_nanos()` — the same value
    /// the output clock uses for `pts_at(1)`. Returns `0` for a degenerate cadence
    /// (`num == 0`/`den == 0`), and rounds to `0` for a sub-nanosecond period.
    /// Diagnostics/accessor only — for the validation bound use
    /// [`Rational::has_subnanosecond_period`], which tests the *exact* period (the
    /// rounded value can be `1` while the exact period is `< 1 ns`, e.g. 1.5 GHz).
    #[must_use]
    pub fn period_nanos(self) -> i64 {
        MediaTime::from_tick(1, self).as_nanos()
    }

    /// Whether one tick's **exact** period is shorter than one nanosecond — i.e.
    /// `den/num · NANOS_PER_SEC < 1`, equivalently `num > NANOS_PER_SEC · den`
    /// (the cadence exceeds 1 GHz).
    ///
    /// Such a cadence is rejected by [`crate::layout::Canvas::validate`] and the
    /// engine's `OutputClock::new`: with a sub-nanosecond exact period, consecutive
    /// ticks can round to the **same** nanosecond, breaking strictly-increasing
    /// output PTS (invariant #3) and the output clock's exact-floor wall-clock
    /// resync (ADR-T018). The test uses the exact period (not the rounded
    /// `period_nanos`), computed in i128 so `NANOS_PER_SEC · den` cannot overflow.
    /// A degenerate non-positive cadence (`num <= 0`/`den <= 0`) is not classified
    /// here — the positive-rational checks reject it first.
    #[must_use]
    pub fn has_subnanosecond_period(self) -> bool {
        if self.num <= 0 || self.den <= 0 {
            return false;
        }
        // period_exact = (NANOS_PER_SEC · den) / num seconds-as-ns; < 1 ns ⇔
        // NANOS_PER_SEC · den < num.
        i128::from(MediaTime::NANOS_PER_SEC).saturating_mul(i128::from(self.den))
            < i128::from(self.num)
    }

    /// Cross-multiplied numerator/denominator in `i128`, sign-normalized so the
    /// effective denominator is positive. Used for `Ord`/`PartialOrd`.
    fn cross_terms(self) -> (i128, i128) {
        let n = i128::from(self.num);
        let d = i128::from(self.den);
        if d < 0 {
            (-n, -d)
        } else {
            (n, d)
        }
    }
}

impl PartialEq for Rational {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == core::cmp::Ordering::Equal
    }
}

impl Eq for Rational {}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Rational {
    /// Compare by cross-multiplication in `i128` (so `i64 * i64` never
    /// overflows), with denominators normalized positive first.
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let (an, ad) = self.cross_terms();
        let (bn, bd) = other.cross_terms();
        // a/ad vs b/bd  with ad,bd >= 0  =>  an*bd vs bn*ad
        (an * bd).cmp(&(bn * ad))
    }
}

/// Rescale an integer `value` measured in units of the `from` timebase into the
/// `to` timebase, rounding to nearest (ties away from zero).
///
/// This is the `av_rescale_q` analogue. A `value` measured in `from`-units
/// equals `value * from` seconds; dividing by `to` gives the count of
/// `to`-units. All arithmetic is performed in `i128`, so it never overflows for
/// `i64` inputs with realistic (`<= i64::MAX`) timebases, and the result is
/// saturated into `i64`.
///
/// A zero (degenerate) `from` or `to` denominator yields `0` rather than
/// panicking; callers should validate timebases with [`Rational::is_valid`].
#[must_use]
pub fn rescale(value: i64, from: Rational, to: Rational) -> i64 {
    if from.den == 0 || to.den == 0 || to.num == 0 {
        return 0;
    }
    // result = value * from.num * to.den / (from.den * to.num)
    let v = i128::from(value);
    let numerator = v
        .saturating_mul(i128::from(from.num))
        .saturating_mul(i128::from(to.den));
    let denominator = i128::from(from.den).saturating_mul(i128::from(to.num));
    if denominator == 0 {
        return 0;
    }
    // Normalize so the denominator is positive (keeps rounding symmetric).
    let (numerator, denominator) = if denominator < 0 {
        (-numerator, -denominator)
    } else {
        (numerator, denominator)
    };
    // Round half away from zero: add/subtract half the (positive) denominator
    // toward the sign of the numerator before truncating.
    let half = denominator / 2;
    let rounded = if numerator >= 0 {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
    };
    i64::try_from(rounded).unwrap_or(if rounded < 0 { i64::MIN } else { i64::MAX })
}

/// A point on the internal monotonic media timeline, in nanoseconds.
///
/// All Multiview time is carried as integer nanoseconds (invariant #3). Build one
/// from a `(tick, cadence)` pair with [`MediaTime::from_tick`] — that is the
/// canonical realization of `out_pts = f(tick)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct MediaTime(i64);

impl MediaTime {
    /// The zero instant.
    pub const ZERO: Self = Self(0);

    /// Number of nanoseconds in one second.
    const NANOS_PER_SEC: i64 = 1_000_000_000;

    /// Construct from a raw nanosecond count.
    #[must_use]
    pub const fn from_nanos(ns: i64) -> Self {
        Self(ns)
    }

    /// The value in nanoseconds.
    #[must_use]
    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    /// Compute the presentation time of output `tick` at a fixed `cadence`
    /// (frames per second, as an exact rational), in nanoseconds.
    ///
    /// This is `out_pts = f(tick)` (invariant #1): `tick / cadence` seconds,
    /// expressed in nanoseconds and computed exactly via [`rescale`] every
    /// time — never accumulated in floating point. One tick spans `1/cadence`
    /// seconds, i.e. a timebase of `cadence.den / cadence.num` seconds per
    /// tick.
    #[must_use]
    pub fn from_tick(tick: i64, cadence: Rational) -> Self {
        // tick is in units of (cadence.den / cadence.num) seconds; convert to
        // units of (1 / NANOS_PER_SEC) seconds.
        let tick_timebase = Rational::new(cadence.den, cadence.num);
        let ns_timebase = Rational::new(1, Self::NANOS_PER_SEC);
        Self(rescale(tick, tick_timebase, ns_timebase))
    }

    /// Convert this nanosecond instant back to a tick index at `cadence`,
    /// rounding to the nearest tick.
    ///
    /// Inverse of [`MediaTime::from_tick`] (exact for instants that land on a
    /// tick boundary; within one tick otherwise).
    #[must_use]
    pub fn to_tick(self, cadence: Rational) -> i64 {
        let ns_timebase = Rational::new(1, Self::NANOS_PER_SEC);
        let tick_timebase = Rational::new(cadence.den, cadence.num);
        rescale(self.0, ns_timebase, tick_timebase)
    }

    /// Saturating addition of two instants/durations (no overflow panic).
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Saturating subtraction of two instants/durations (no underflow panic).
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_subnanosecond_period_is_exact_not_rounded() {
        // Exact period >= 1 ns (fps <= 1 GHz): accepted (not sub-ns).
        assert!(!Rational::new(60, 1).has_subnanosecond_period());
        assert!(!Rational::new(30_000, 1_001).has_subnanosecond_period());
        assert!(!Rational::new(24_000, 1_001).has_subnanosecond_period());
        // Exactly 1 GHz — period is exactly 1 ns, the accepted boundary.
        assert!(!Rational::new(1_000_000_000, 1).has_subnanosecond_period());
        // A non-trivial denominator: 2e9/3 ≈ 0.667 GHz → period 1.5 ns, accepted.
        assert!(!Rational::new(2_000_000_000, 3).has_subnanosecond_period());

        // Just over 1 GHz: the ROUNDED period is still 1 ns, but the EXACT period is
        // < 1 ns — this is the case `period_nanos() < 1` would WRONGLY accept and is
        // exactly why the bound tests the exact period.
        assert_eq!(Rational::new(1_500_000_000, 1).period_nanos(), 1);
        assert!(Rational::new(1_500_000_000, 1).has_subnanosecond_period());
        assert!(Rational::new(1_000_000_001, 1).has_subnanosecond_period());
        assert!(Rational::new(2_000_000_000, 1).has_subnanosecond_period());
        // Absurdly fast: period rounds to 0.
        assert_eq!(Rational::new(10_000_000_000, 1).period_nanos(), 0);
        assert!(Rational::new(10_000_000_000, 1).has_subnanosecond_period());

        // Degenerate/non-positive cadences are not classified here (the
        // positive-rational checks reject them first); never panics.
        assert!(!Rational::new(0, 1).has_subnanosecond_period());
        assert!(!Rational::new(60, 0).has_subnanosecond_period());
        assert!(!Rational::new(-60, 1).has_subnanosecond_period());
        // i64::MAX numerator must not overflow the i128 cross-multiply.
        assert!(Rational::new(i64::MAX, 1).has_subnanosecond_period());
    }

    #[test]
    fn period_nanos_matches_pts_at_one() {
        for cadence in [
            Rational::new(60, 1),
            Rational::new(30_000, 1_001),
            Rational::new(1_000_000_000, 1),
        ] {
            assert_eq!(
                cadence.period_nanos(),
                MediaTime::from_tick(1, cadence).as_nanos()
            );
        }
    }
}
