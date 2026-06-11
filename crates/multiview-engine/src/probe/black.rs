//! The **black** picture probe: mean luma below a threshold over a detection
//! zone (ADR-MV001 / broadcast-multiviewer §4).
//!
//! A frame's picture is "black" when the average luma inside the configured
//! detection zone falls at or below a luma threshold. This module computes only
//! the *instantaneous* condition; the dwell-up/dwell-down hysteresis that turns a
//! run of black frames into a raised X.733 alarm (and a run of non-black frames
//! into a clear) lives in [`crate::alarm::state`].
use multiview_core::alarm::AlarmKind;
use serde::{Deserialize, Serialize};

use super::luma::{DetectionZone, LumaView};
use super::ProbeObservation;

/// Configuration for a [`BlackProbe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlackConfig {
    /// Mean-luma threshold (8-bit, `0.0..=255.0`): the picture is considered
    /// black when the zone's mean luma is **at or below** this value.
    ///
    /// A sensible broadcast default is around video black (limited-range Y' 16),
    /// nudged up a little to tolerate noise — see [`BlackConfig::default`].
    pub luma_threshold: f64,
    /// The region of the frame to analyse. Defaults to the whole frame.
    pub zone: DetectionZone,
}

impl Default for BlackConfig {
    /// Threshold 16.0 (limited-range video black) over the full frame.
    fn default() -> Self {
        Self {
            luma_threshold: 16.0,
            zone: DetectionZone::FULL,
        }
    }
}

impl BlackConfig {
    /// Construct a config from a luma threshold over the full frame.
    #[must_use]
    pub fn with_threshold(luma_threshold: f64) -> Self {
        Self {
            luma_threshold,
            zone: DetectionZone::FULL,
        }
    }

    /// Restrict analysis to `zone`.
    #[must_use]
    pub const fn with_zone(mut self, zone: DetectionZone) -> Self {
        self.zone = zone;
        self
    }
}

/// A stateless black-picture detector.
///
/// Construct once from a [`BlackConfig`] and call [`BlackProbe::detect`] per
/// sampled frame; the probe holds no per-frame state, so it neither allocates
/// nor blocks (probe isolation contract — see [`crate::probe`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlackProbe {
    config: BlackConfig,
}

impl BlackProbe {
    /// Create a probe with the given configuration.
    #[must_use]
    pub const fn new(config: BlackConfig) -> Self {
        Self { config }
    }

    /// The probe's configuration.
    #[must_use]
    pub const fn config(&self) -> &BlackConfig {
        &self.config
    }

    /// Evaluate one sampled frame.
    ///
    /// Returns a [`ProbeObservation`] whose `condition_present` is `true` when the
    /// zone's mean luma is **at or below** the configured threshold, and whose
    /// `measured` carries that mean luma (`0.0..=255.0`) for diagnostics.
    #[must_use]
    pub fn detect(&self, luma: &LumaView<'_>) -> ProbeObservation {
        let mean = luma.mean_luma(self.config.zone);
        let present = mean <= self.config.luma_threshold;
        ProbeObservation::new(AlarmKind::Black, present, mean)
    }
}
