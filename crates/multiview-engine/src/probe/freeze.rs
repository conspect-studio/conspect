//! The **freeze** picture probe: too few luma samples change between successive
//! frames over a detection zone (ADR-MV001 / broadcast-multiviewer §4).
//!
//! A picture is "frozen" when, comparing the current sampled frame to the
//! previous one, the fraction of luma samples that changed by more than a
//! per-sample tolerance is **at or below** a change threshold. As with the black
//! probe this computes only the instantaneous condition; persistence/recovery
//! hysteresis lives in [`crate::alarm::state`].
//!
//! The probe is **stateless**: the caller supplies both the current and the
//! previous luma view (the engine already retains each tile's last-good frame),
//! so the probe never owns or copies pixels and cannot block.
use multiview_core::alarm::AlarmKind;
use serde::{Deserialize, Serialize};

use super::luma::{DetectionZone, LumaView};
use super::ProbeObservation;

/// Configuration for a [`FreezeProbe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FreezeConfig {
    /// Per-sample 8-bit difference under which two luma samples are deemed
    /// unchanged. Small values tolerate sensor/codec noise; `0` means
    /// bit-identical. Defaults to `2`.
    pub diff_tolerance: u8,
    /// Changed-fraction threshold in `0.0..=1.0`: the picture is "frozen" when
    /// the fraction of changed samples is **at or below** this value. Defaults to
    /// a small fraction so a tiny amount of noise still reads as frozen.
    pub change_threshold: f64,
    /// The region of the frame to analyse. Defaults to the whole frame.
    pub zone: DetectionZone,
}

impl Default for FreezeConfig {
    /// Tolerance 2, change threshold 0.1 %, full frame.
    fn default() -> Self {
        Self {
            diff_tolerance: 2,
            change_threshold: 0.001,
            zone: DetectionZone::FULL,
        }
    }
}

impl FreezeConfig {
    /// Construct from a change threshold (full frame, default tolerance).
    #[must_use]
    pub fn with_change_threshold(change_threshold: f64) -> Self {
        Self {
            change_threshold,
            ..Self::default()
        }
    }

    /// Set the per-sample difference tolerance.
    #[must_use]
    pub const fn with_tolerance(mut self, diff_tolerance: u8) -> Self {
        self.diff_tolerance = diff_tolerance;
        self
    }

    /// Restrict analysis to `zone`.
    #[must_use]
    pub const fn with_zone(mut self, zone: DetectionZone) -> Self {
        self.zone = zone;
        self
    }
}

/// A stateless freeze detector.
///
/// Construct once from a [`FreezeConfig`] and call [`FreezeProbe::detect`] with
/// the current and previous frames' luma views.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreezeProbe {
    config: FreezeConfig,
}

impl FreezeProbe {
    /// Create a probe with the given configuration.
    #[must_use]
    pub const fn new(config: FreezeConfig) -> Self {
        Self { config }
    }

    /// The probe's configuration.
    #[must_use]
    pub const fn config(&self) -> &FreezeConfig {
        &self.config
    }

    /// Evaluate the current frame against the `previous` one.
    ///
    /// Returns a [`ProbeObservation`] whose `condition_present` is `true` when the
    /// changed-sample fraction is **at or below** the configured threshold (the
    /// picture has not meaningfully changed → frozen), and whose `measured`
    /// carries that changed fraction (`0.0..=1.0`).
    ///
    /// If the two views describe different geometry the change fraction is `1.0`
    /// (treated as definitely-not-frozen), so a resolution change cannot be
    /// mistaken for a freeze.
    #[must_use]
    pub fn detect(&self, current: &LumaView<'_>, previous: &LumaView<'_>) -> ProbeObservation {
        let changed =
            current.changed_fraction(previous, self.config.zone, self.config.diff_tolerance);
        let present = changed <= self.config.change_threshold;
        ProbeObservation::new(AlarmKind::Freeze, present, changed)
    }
}
