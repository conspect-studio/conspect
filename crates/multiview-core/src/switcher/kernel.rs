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
