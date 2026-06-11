//! Stereo phase-correlation meter, goniometer (Lissajous) points, and ITU-R
//! BS.775 surround → stereo downmix metering (Lo/Ro and Lt/Rt).
//!
//! * [`CorrelationMeter`] — the **phase-correlation** coefficient in `[-1, +1]`:
//!   `+1` is fully mono/in-phase, `0` is decorrelated, `-1` is fully
//!   anti-phase (mono-collapse risk). It is the Pearson correlation of the
//!   left/right channels, accumulated over a sliding window so it is stable for
//!   a meter display.
//! * [`GonioPoint`] — one Lissajous/goniometer point, rotated 45° so a mono
//!   (in-phase) signal lies on the vertical (M) axis and an anti-phase signal on
//!   the horizontal (S) axis, the conventional broadcast goniometer orientation.
//! * [`SurroundDownmix`] — ITU-R BS.775 5.1 → stereo **Lo/Ro** (stereo
//!   compatible) and **Lt/Rt** (matrix-surround compatible) downmix, for
//!   downmix-compatibility metering.
//!
//! All of this is pure DSP, read-only, and off the hot path (ADR-R006).
use serde::{Deserialize, Serialize};

/// Window length (samples) over which the correlation statistics decay. A
/// ~400 ms window at 48 kHz gives a steady meter without lagging dynamics.
const DEFAULT_WINDOW: usize = 19_200;

/// A streaming stereo phase-correlation meter.
///
/// Accumulates the running sums needed for the Pearson correlation between the
/// left and right channels over an exponentially-weighted window, so the
/// reading reflects recent program rather than the whole history.
#[derive(Debug, Clone)]
pub struct CorrelationMeter {
    /// Per-sample retention factor `α = 1 − 1/window`.
    alpha: f64,
    sum_ll: f64,
    sum_rr: f64,
    sum_lr: f64,
}

impl Default for CorrelationMeter {
    fn default() -> Self {
        Self::with_window(DEFAULT_WINDOW)
    }
}

impl CorrelationMeter {
    /// A correlation meter with the default (~400 ms @ 48 kHz) window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A correlation meter whose statistics decay over roughly `window`
    /// samples. A `window` of 0 is treated as 1.
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)] // reason: window length is small (< 2^53); lossless.
    #[must_use]
    pub fn with_window(window: usize) -> Self {
        let w = window.max(1) as f64;
        Self {
            alpha: 1.0 - 1.0 / w,
            sum_ll: 0.0,
            sum_rr: 0.0,
            sum_lr: 0.0,
        }
    }

    /// Push one stereo frame `(left, right)`.
    pub fn push(&mut self, left: f64, right: f64) {
        self.sum_ll = self.alpha * self.sum_ll + left * left;
        self.sum_rr = self.alpha * self.sum_rr + right * right;
        self.sum_lr = self.alpha * self.sum_lr + left * right;
    }

    /// The current correlation coefficient in `[-1, +1]`.
    ///
    /// Returns `0.0` when either channel carries no energy (silence), so the
    /// reading is always finite.
    #[must_use]
    pub fn correlation(&self) -> f64 {
        let denom = (self.sum_ll * self.sum_rr).sqrt();
        if denom <= f64::EPSILON {
            return 0.0;
        }
        (self.sum_lr / denom).clamp(-1.0, 1.0)
    }
}

/// One goniometer (Lissajous) point in the conventional 45°-rotated display
/// space: the vertical axis is the **mid** (M = L+R) component, the horizontal
/// axis the **side** (S = L−R) component.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GonioPoint {
    /// Horizontal (side / stereo-width) coordinate.
    pub x: f64,
    /// Vertical (mid / mono) coordinate.
    pub y: f64,
}

impl GonioPoint {
    /// Map a left/right sample pair to a rotated goniometer point.
    ///
    /// `x = (L − R)/√2` (side), `y = (L + R)/√2` (mid). An in-phase (mono)
    /// signal `L == R` lands on the vertical axis (`x == 0`); a fully anti-phase
    /// signal `L == −R` lands on the horizontal axis (`y == 0`).
    #[must_use]
    pub fn from_lr(left: f64, right: f64) -> Self {
        const INV_SQRT2: f64 = core::f64::consts::FRAC_1_SQRT_2;
        Self {
            x: (left - right) * INV_SQRT2,
            y: (left + right) * INV_SQRT2,
        }
    }
}

/// ITU-R BS.775 5.1 → stereo downmix coefficients.
///
/// Channel order is the BS.1770/5.1 ordering `L, R, C, LFE, Ls, Rs`. The centre
/// and surround down-mix gains default to the BS.775 −3 dB value; the LFE is
/// excluded from the stereo downmix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SurroundDownmix {
    /// Gain applied to the centre channel in both downmix legs (linear).
    pub center_gain: f64,
    /// Gain applied to each surround channel (linear).
    pub surround_gain: f64,
}

impl Default for SurroundDownmix {
    fn default() -> Self {
        // BS.775 attenuation of −3 dB for centre and surrounds.
        let minus_3db = 10f64.powf(-3.0 / 20.0);
        Self {
            center_gain: minus_3db,
            surround_gain: minus_3db,
        }
    }
}

impl SurroundDownmix {
    /// Stereo-compatible **Lo/Ro** downmix of a 5.1 frame `[L, R, C, LFE, Ls,
    /// Rs]`.
    ///
    /// `Lo = L + g_c·C + g_s·Ls`, `Ro = R + g_c·C + g_s·Rs` (BS.775). Frames
    /// shorter than six channels treat the missing channels as silent.
    #[must_use]
    pub fn lo_ro(&self, frame: &[f64]) -> (f64, f64) {
        let g = |i: usize| frame.get(i).copied().unwrap_or(0.0);
        let (l, r, c, ls, rs) = (g(0), g(1), g(2), g(4), g(5));
        let lo = l + self.center_gain * c + self.surround_gain * ls;
        let ro = r + self.center_gain * c + self.surround_gain * rs;
        (lo, ro)
    }

    /// Matrix-surround-compatible **Lt/Rt** downmix of a 5.1 frame
    /// `[L, R, C, LFE, Ls, Rs]`.
    ///
    /// Per BS.775 the surrounds are matrix-encoded in antiphase:
    /// `Lt = L + g_c·C − g_s·(Ls + Rs)`,
    /// `Rt = R + g_c·C + g_s·(Ls + Rs)`. A surround-only feed therefore appears
    /// with opposite sign in Lt and Rt, recoverable by a matrix decoder.
    #[must_use]
    pub fn lt_rt(&self, frame: &[f64]) -> (f64, f64) {
        let g = |i: usize| frame.get(i).copied().unwrap_or(0.0);
        let (l, r, c, ls, rs) = (g(0), g(1), g(2), g(4), g(5));
        let surround = self.surround_gain * (ls + rs);
        let lt = l + self.center_gain * c - surround;
        let rt = r + self.center_gain * c + surround;
        (lt, rt)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn mid_only_when_balanced() {
        let pt = GonioPoint::from_lr(0.5, 0.5);
        approx::assert_abs_diff_eq!(pt.x, 0.0, epsilon = 1e-12);
        assert!(pt.y > 0.0);
    }

    #[test]
    fn empty_meter_is_zero() {
        let m = CorrelationMeter::new();
        approx::assert_abs_diff_eq!(m.correlation(), 0.0);
    }
}
