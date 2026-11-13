//! The one lossy bridge from exact switcher progress to the compositor.
//!
//! Pixels are approximate; timing is exact (ADR-T015). Every timing decision is
//! made on integers in [`crate::switcher::timing`]; the only place a transition
//! scalar becomes a float is here, at the per-tile blend weight handed to the
//! CPU fold and the `composite.wgsl` blend step — the same boundary discipline
//! as the clock-hand angles of ADR-0047.
use crate::switcher::timing::Progress;

/// The per-tile blend weight for `progress`, in the compositor's parameter
/// space.
///
/// This is the **only** float conversion in the switcher, and it belongs at the
/// kernel boundary — never in time math. `Progress::ZERO` maps to `0.0` and
/// `Progress::COMPLETE` to `1.0`.
#[must_use]
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: the deliberate, documented narrowing of an exact rational progress
// (`elapsed/duration`) to the compositor's per-tile opacity; `as` is the only
// available `f64 -> f32` conversion and the rounding is the intended boundary
// behaviour (`f64::from(u32)` is exact, so the ratio is computed losslessly).
pub fn opacity(progress: Progress) -> f32 {
    let elapsed = f64::from(progress.numerator());
    let duration = f64::from(progress.denominator());
    (elapsed / duration) as f32
}

/// The two blend weights of a DIP at `elapsed` of `duration` frames, split at
/// the exact rational `switch_point` in `(0, 1)` (ADR-0111 §2).
///
/// Returns `(dip, incoming)`: before the switch point the dip colour is drawn
/// over the outgoing picture at `dip = t / s` and the incoming picture is not
/// drawn (`0`); from the switch point on the dip colour is opaque (`1`) and the
/// incoming picture is drawn over it at `(t - s) / (1 - s)`, where
/// `t = elapsed / duration`. The phase decision is exact integer arithmetic;
/// only the two returned weights are floats (the kernel boundary).
///
/// A degenerate `switch_point` (not strictly inside `(0, 1)`) or a zero
/// `duration` returns `(0, 1)` — show the incoming picture — rather than
/// dividing by zero; the machine refuses such descriptors at arm time.
#[must_use]
#[allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
// reason: the deliberate, documented narrowing of exact integer ratios to the
// compositor's per-tile opacity, as in `opacity`. Every operand is at most
// `u32::MAX * i64::MAX`-bounded by the checked i128 products below, and the
// division happens in f64 before the one narrowing to f32.
pub fn dip_weights(elapsed: u32, duration: u32, switch_point: crate::time::Rational) -> (f32, f32) {
    let (num, den) = (i128::from(switch_point.num), i128::from(switch_point.den));
    if duration == 0 || num <= 0 || den <= 0 || num >= den {
        return (0.0, 1.0);
    }
    let e = i128::from(elapsed.min(duration));
    let n = i128::from(duration);
    // t < s  <=>  e * den < num * n
    let at = e * den;
    let split = num * n;
    let ratio = |a: i128, b: i128| -> f32 { ((a as f64) / (b as f64)) as f32 };
    if at < split {
        (ratio(at, split), 0.0)
    } else {
        (1.0, ratio(at - split, (den - num) * n))
    }
}
