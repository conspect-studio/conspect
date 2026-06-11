//! Selectable single-channel meter ballistics: PPM (IEC 60268-10
//! Type I/IIa/IIb), VU, sample-peak (IEC TR 60268-18) and true-peak (ITU-R
//! BS.1770).
//!
//! Each [`Ballistics`] meter is a streaming, allocation-free per-sample state
//! machine that turns a stream of linear PCM samples into a single moving meter
//! reading (in dB). The *standardised* difference between meter types is the
//! **integration (attack) and decay (fallback) behaviour**:
//!
//! | Scale | Detector | Attack | Decay (fallback) | Reference |
//! |-------|----------|--------|------------------|-----------|
//! | [`MeterScale::Vu`] | mean-square (RMS) | 300 ms | 300 ms (symmetric) | 0 VU = −18 dBFS |
//! | [`MeterScale::Ppm`] Type I | quasi-peak | 5 ms | ~1.7 s/20 dB | IEC 60268-10 |
//! | [`MeterScale::Ppm`] Type `IIa` | quasi-peak | 10 ms | ~2.8 s/24 dB | IEC 60268-10 (EBU) |
//! | [`MeterScale::Ppm`] Type `IIb` | quasi-peak | 10 ms | ~2.8 s/24 dB | IEC 60268-10 (BBC) |
//! | [`MeterScale::SamplePeak`] sample | instantaneous peak | 0 | peak-hold + decay | IEC TR 60268-18 |
//! | [`MeterScale::SamplePeak`] true-peak | 4× oversampled peak | 0 | peak-hold + decay | ITU-R BS.1770 |
//!
//! Per ADR-R006 the meter runs **read-only and off the hot path**; the engine
//! taps audio into a meter on a separate thread and never blocks on it.
//!
//! All readings are reported in **dB relative to digital full scale** by
//! [`Ballistics::reading_db`]; [`Ballistics::reading_scaled`] additionally
//! exposes the meter's *native* scale (VU units, dBFS sample-peak, etc.) for the
//! UI. Silence floors at [`Ballistics::FLOOR_DB`] rather than `-inf` so the
//! reading is always finite.
use serde::{Deserialize, Serialize};

use crate::truepeak::TruePeakDetector;

/// The peak level (dBFS) of the alignment sine that a [`MeterScale::Vu`] meter
/// maps to **0 VU**.
///
/// Multiview aligns 0 VU to the EBU alignment level of −18 dBFS *peak*. A VU meter
/// is an RMS instrument, so internally 0 VU corresponds to the *RMS* of that
/// alignment sine ([`VU_REFERENCE_RMS_DBFS`], 3.01 dB below the peak). A steady
/// sine at −18 dBFS peak therefore deflects the meter to 0 VU.
pub const VU_REFERENCE_DBFS: f64 = -18.0;

/// The RMS level (dBFS) corresponding to **0 VU**: the RMS of the
/// [`VU_REFERENCE_DBFS`] alignment sine (a sine's RMS is 3.01 dB below its
/// peak).
pub const VU_REFERENCE_RMS_DBFS: f64 = VU_REFERENCE_DBFS - 3.010_299_956_639_812;

/// IEC 60268-10 PPM type (integration/fallback timing class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PpmKind {
    /// Type `I` (DIN/Nordic): 5 ms integration, ~1.7 s/20 dB fallback.
    One,
    /// Type `IIa` (EBU/IEC, "EBU PPM"): 10 ms integration, ~2.8 s/24 dB fallback.
    Iia,
    /// Type `IIb` (BBC PPM): 10 ms integration, ~2.8 s/24 dB fallback.
    Iib,
}

impl PpmKind {
    /// Integration (attack) time to reach −1 dB of a step, in seconds.
    #[must_use]
    pub const fn integration_secs(self) -> f64 {
        match self {
            Self::One => 0.005,
            Self::Iia | Self::Iib => 0.010,
        }
    }

    /// Fallback (decay) rate, in dB per second.
    #[must_use]
    pub const fn fallback_db_per_sec(self) -> f64 {
        match self {
            // ~20 dB in 1.7 s.
            Self::One => 20.0 / 1.7,
            // ~24 dB in 2.8 s (EBU) / 24 dB in 2.8 s (BBC 4 dB/marks ≈ same).
            Self::Iia | Self::Iib => 24.0 / 2.8,
        }
    }
}

/// Sample-domain peak detector mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PeakMode {
    /// Plain sample-peak (max `|x|`), per IEC TR 60268-18.
    Sample,
    /// Oversampled true-peak (dBTP), per ITU-R BS.1770.
    TruePeak,
}

/// Which meter ballistic to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MeterScale {
    /// VU meter (300 ms RMS ballistics, 0 VU = −18 dBFS).
    Vu,
    /// Peak Programme Meter of the given IEC 60268-10 type.
    Ppm(PpmKind),
    /// Digital peak meter (sample-peak or true-peak) with peak-hold + decay.
    SamplePeak(PeakMode),
}

/// A meter reading expressed in its meter's *native* scale.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum SampleScale {
    /// VU units (0 VU at the reference level).
    Vu(f64),
    /// dB relative to full scale (sample-peak / true-peak dBTP).
    DbFs(f64),
    /// dB relative to the PPM alignment level (here, dBFS).
    Ppm(f64),
}

/// Default peak-hold time before a digital peak meter starts to decay (seconds).
const PEAK_HOLD_SECS: f64 = 0.25;
/// Decay rate for the digital peak meter after the hold expires (dB/second).
const PEAK_DECAY_DB_PER_SEC: f64 = 20.0;
/// VU integration time constant (seconds) — the 300 ms standard ballistic.
const VU_TIME_CONSTANT_SECS: f64 = 0.3;

/// IEC 60268-10 defines a PPM's **integration time** as the tone-burst duration
/// at which the meter reaches a fixed deflection 2 dB below the steady reading.
/// The attack is a one-pole envelope that *charges only at the rectified peaks*
/// of the programme (it decays slowly between them), so its effective charge
/// time is slower than a continuously-excited one-pole. Empirically a τ of
/// roughly a third of the nominal integration time puts a spec tone-burst within
/// ~2 dB of the steady deflection — matching the IEC quasi-peak response.
const PPM_TAU_FROM_INTEGRATION: f64 = 1.0 / 3.0;

/// A streaming single-channel meter with selectable ballistics.
#[derive(Debug, Clone)]
pub struct Ballistics {
    scale: MeterScale,
    /// Seconds per sample (`1 / fs`).
    dt: f64,
    /// Current detector value in the **linear power/amplitude** domain used by
    /// the chosen scale: mean-square for VU, linear peak for PPM/peak meters.
    value: f64,
    /// Remaining peak-hold time for the digital peak meters (seconds).
    hold_remaining: f64,
    /// Oversampling true-peak front-end (only for [`PeakMode::TruePeak`]).
    true_peak: Option<TruePeakDetector>,
    /// Per-sample attack coefficient for the one-pole smoother (PPM/VU).
    attack_coeff: f64,
    /// Per-sample VU release coefficient (symmetric one-pole).
    vu_coeff: f64,
}

impl Ballistics {
    /// The dB floor reported for silence (instead of `-inf`).
    pub const FLOOR_DB: f64 = -120.0;

    /// Construct a meter for `sample_rate` Hz running `scale`.
    ///
    /// A zero `sample_rate` is clamped to 1 to keep the per-sample step finite;
    /// callers metering real audio always pass a valid rate.
    #[must_use]
    pub fn new(sample_rate: u32, scale: MeterScale) -> Self {
        let fs = f64::from(sample_rate.max(1));
        let dt = 1.0 / fs;
        let true_peak = match scale {
            MeterScale::SamplePeak(PeakMode::TruePeak) => Some(TruePeakDetector::new()),
            _ => None,
        };
        // One-pole smoother time constant -> per-sample coefficient
        // `c = exp(-dt / tau)`, so the response reaches ~63% in `tau`.
        let attack_coeff = match scale {
            MeterScale::Ppm(kind) => {
                (-dt / (kind.integration_secs() * PPM_TAU_FROM_INTEGRATION)).exp()
            }
            MeterScale::Vu => (-dt / VU_TIME_CONSTANT_SECS).exp(),
            MeterScale::SamplePeak(_) => 0.0,
        };
        let vu_coeff = (-dt / VU_TIME_CONSTANT_SECS).exp();
        Self {
            scale,
            dt,
            value: 0.0,
            hold_remaining: 0.0,
            true_peak,
            attack_coeff,
            vu_coeff,
        }
    }

    /// The meter's scale.
    #[must_use]
    pub const fn scale(&self) -> MeterScale {
        self.scale
    }

    /// Push one linear PCM sample, advancing the ballistic.
    pub fn push(&mut self, x: f64) {
        match self.scale {
            MeterScale::Vu => self.push_vu(x),
            MeterScale::Ppm(kind) => self.push_ppm(x, kind),
            MeterScale::SamplePeak(PeakMode::Sample) => self.push_peak(x.abs()),
            MeterScale::SamplePeak(PeakMode::TruePeak) => {
                // Oversample, then drive the peak ballistic with the running
                // true-peak magnitude for this input sample.
                let mag = if let Some(tp) = self.true_peak.as_mut() {
                    tp.push(x);
                    tp.peak_linear()
                } else {
                    x.abs()
                };
                self.push_peak(mag);
            }
        }
    }

    /// VU: one-pole-smoothed mean square (RMS ballistic, symmetric 300 ms).
    fn push_vu(&mut self, x: f64) {
        let target = x * x;
        self.value = self.vu_coeff * self.value + (1.0 - self.vu_coeff) * target;
    }

    /// PPM: fast one-pole attack toward the rectified sample, slow linear
    /// (dB/s) fallback when the input is below the current reading.
    fn push_ppm(&mut self, x: f64, kind: PpmKind) {
        let rect = x.abs();
        if rect >= self.value {
            // Attack: approach the new peak with the integration time constant.
            self.value = self.attack_coeff * self.value + (1.0 - self.attack_coeff) * rect;
        } else {
            // Fallback: decay at a constant dB/s. `mult = 10^(-rate*dt/20)`.
            let mult = 10f64.powf(-kind.fallback_db_per_sec() * self.dt / 20.0);
            self.value *= mult;
        }
    }

    /// Digital peak: instantaneous capture, peak-hold, then dB/s decay.
    fn push_peak(&mut self, mag: f64) {
        if mag >= self.value {
            self.value = mag;
            self.hold_remaining = PEAK_HOLD_SECS;
        } else if self.hold_remaining > 0.0 {
            self.hold_remaining -= self.dt;
        } else {
            let mult = 10f64.powf(-PEAK_DECAY_DB_PER_SEC * self.dt / 20.0);
            self.value *= mult;
        }
    }

    /// The current reading in **dB relative to full scale**, floored at
    /// [`Ballistics::FLOOR_DB`].
    #[must_use]
    pub fn reading_db(&self) -> f64 {
        let linear = match self.scale {
            // VU/value is a mean square -> amplitude is its square root.
            MeterScale::Vu => self.value.max(0.0).sqrt(),
            // PPM / peak value is already a linear amplitude.
            MeterScale::Ppm(_) | MeterScale::SamplePeak(_) => self.value.max(0.0),
        };
        if linear <= 0.0 {
            return Self::FLOOR_DB;
        }
        (20.0 * linear.log10()).max(Self::FLOOR_DB)
    }

    /// The current reading in the meter's *native* scale.
    #[must_use]
    pub fn reading_scaled(&self) -> SampleScale {
        let db = self.reading_db();
        match self.scale {
            MeterScale::Vu => SampleScale::Vu(db - VU_REFERENCE_RMS_DBFS),
            MeterScale::Ppm(_) => SampleScale::Ppm(db),
            MeterScale::SamplePeak(_) => SampleScale::DbFs(db),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    // reason: synthetic-signal generation uses index/length <-> float casts that
    // are exact for the small ranges used here; test-only.
    #![allow(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    use super::*;
    use core::f64::consts::PI;

    fn sine(fs: u32, freq: f64, amp: f64, secs: f64) -> Vec<f64> {
        let n = (f64::from(fs) * secs).round() as usize;
        let w = 2.0 * PI * freq / f64::from(fs);
        (0..n).map(|i| amp * (w * i as f64).sin()).collect()
    }

    #[test]
    fn vu_rms_of_sine_is_minus_3db_of_peak() {
        // RMS of a sine is peak/sqrt(2) = peak - 3.01 dB.
        let mut m = Ballistics::new(48_000, MeterScale::Vu);
        for x in sine(48_000, 1_000.0, 0.5, 1.0) {
            m.push(x);
        }
        let expected = 20.0 * (0.5 / 2f64.sqrt()).log10();
        approx::assert_abs_diff_eq!(m.reading_db(), expected, epsilon = 0.2);
    }

    #[test]
    fn floor_is_finite() {
        let m = Ballistics::new(48_000, MeterScale::Ppm(PpmKind::Iia));
        assert!(m.reading_db().is_finite());
        approx::assert_abs_diff_eq!(m.reading_db(), Ballistics::FLOOR_DB);
    }
}
