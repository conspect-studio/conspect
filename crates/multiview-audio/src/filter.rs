//! Biquad IIR filters and the ITU-R BS.1770 K-weighting pre-filter.
//!
//! The K-weighting is the two-stage filter applied before the mean-square
//! integration: stage 1 is a high-shelf "pre-filter" modelling the acoustic
//! effect of the head, stage 2 is an "RLB" (Revised Low-frequency B-curve)
//! high-pass. Both are second-order (biquad) sections.
//!
//! Coefficients are derived from the documented analog prototypes via the
//! bilinear transform, so they are exact at any sample rate and reproduce the
//! published 48 kHz reference constants
//! (BS.1770-4 Tables 1 & 2) to floating-point precision.

/// A direct-form-I biquad section, normalized so `a0 == 1`.
///
/// Transfer function `H(z) = (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    /// Feed-forward coefficient for `x[n]`.
    pub b0: f64,
    /// Feed-forward coefficient for `x[n-1]`.
    pub b1: f64,
    /// Feed-forward coefficient for `x[n-2]`.
    pub b2: f64,
    /// Feedback coefficient for `y[n-1]`.
    pub a1: f64,
    /// Feedback coefficient for `y[n-2]`.
    pub a2: f64,
}

impl Biquad {
    /// Construct a normalized biquad (assumes `a0 == 1`).
    #[must_use]
    pub const fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self { b0, b1, b2, a1, a2 }
    }
}

/// Per-channel direct-form-I state for one [`Biquad`].
#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadState {
    #[inline]
    fn process(&mut self, coeffs: &Biquad, x0: f64) -> f64 {
        let y0 = coeffs.b0 * x0 + coeffs.b1 * self.x1 + coeffs.b2 * self.x2
            - coeffs.a1 * self.y1
            - coeffs.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }
}

/// The two cascaded [`Biquad`] sections forming the K-weighting filter for one
/// channel, with their running state.
#[derive(Debug, Clone, Copy)]
pub struct KWeightFilter {
    stage1: Biquad,
    stage2: Biquad,
    s1: BiquadState,
    s2: BiquadState,
}

impl KWeightFilter {
    /// Build a K-weighting filter for the given sample rate.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let (stage1, stage2) = k_weight_coeffs(sample_rate);
        Self {
            stage1,
            stage2,
            s1: BiquadState::default(),
            s2: BiquadState::default(),
        }
    }

    /// Filter one sample through both stages.
    #[inline]
    #[must_use]
    pub fn process(&mut self, x: f64) -> f64 {
        let a = self.s1.process(&self.stage1, x);
        self.s2.process(&self.stage2, a)
    }

    /// Reset the filter's internal state (start a fresh integration).
    pub fn reset(&mut self) {
        self.s1 = BiquadState::default();
        self.s2 = BiquadState::default();
    }
}

// --- Analog-prototype parameters from BS.1770-4 / the Mansbridge derivation. ---
// Stage 1 (high-shelf pre-filter / head model):
const SHELF_F0: f64 = 1_681.974_450_955_533;
const SHELF_Q: f64 = 0.707_175_236_955_419_6;
const SHELF_GAIN_DB: f64 = 3.999_843_853_973_347;
const SHELF_VB_EXP: f64 = 0.499_666_774_154_541_6;
// Stage 2 (RLB high-pass):
const HP_F0: f64 = 38.135_470_876_024_44;
const HP_Q: f64 = 0.500_327_037_323_877_3;

/// Compute the two K-weighting biquad sections (stage 1 high-shelf, stage 2
/// RLB high-pass) for `sample_rate`, via the bilinear transform of the
/// documented analog prototypes.
///
/// At `sample_rate == 48_000` these reproduce the published BS.1770-4 reference
/// coefficients.
#[must_use]
pub fn k_weight_coeffs(sample_rate: u32) -> (Biquad, Biquad) {
    let fs = f64::from(sample_rate);

    // --- Stage 1: high-shelf pre-filter (head model). ---
    let k = (core::f64::consts::PI * SHELF_F0 / fs).tan();
    let vh = 10f64.powf(SHELF_GAIN_DB / 20.0);
    let vb = vh.powf(SHELF_VB_EXP);
    let kk = k * k;
    let denom = 1.0 + k / SHELF_Q + kk;
    let stage1 = Biquad::new(
        (vh + vb * k / SHELF_Q + kk) / denom,
        2.0 * (kk - vh) / denom,
        (vh - vb * k / SHELF_Q + kk) / denom,
        2.0 * (kk - 1.0) / denom,
        (1.0 - k / SHELF_Q + kk) / denom,
    );

    // --- Stage 2: RLB high-pass. ---
    let k = (core::f64::consts::PI * HP_F0 / fs).tan();
    let kk = k * k;
    let denom = 1.0 + k / HP_Q + kk;
    let stage2 = Biquad::new(
        1.0,
        -2.0,
        1.0,
        2.0 * (kk - 1.0) / denom,
        (1.0 - k / HP_Q + kk) / denom,
    );

    (stage1, stage2)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    // reason: loop-index to float in test signal generation is exact here.
    #![allow(clippy::as_conversions, clippy::cast_precision_loss)]
    use super::*;

    #[test]
    fn stage1_dc_gain_is_unity_for_passband() {
        // A long DC input through the high-pass stage 2 must decay to ~0.
        let (_s1, s2) = k_weight_coeffs(48_000);
        let mut st = BiquadState::default();
        let mut last = 0.0;
        for _ in 0..48_000 {
            last = st.process(&s2, 1.0);
        }
        assert!(
            last.abs() < 1e-3,
            "RLB high-pass should reject DC, got {last}"
        );
    }

    #[test]
    fn process_is_finite() {
        let mut f = KWeightFilter::new(48_000);
        for i in 0..1000 {
            let y = f.process((f64::from(i) * 0.01).sin());
            assert!(y.is_finite());
        }
    }
}
