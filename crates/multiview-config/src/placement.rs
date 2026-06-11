//! The GPU work-placement policy as **config data** (ADR-0018 §consequences).
//!
//! ADR-0018 is explicit that placement tuning is *data, not magic constants*:
//! the scoring weights, the reserve-headroom, and the migration anti-storm
//! parameters are all author-controlled. This module models that document block
//! plus the per-source / per-output **GPU pin** (by stable [`DevicePin`], never
//! the volatile enumeration index — ADR-0018 §2.1).
//!
//! It is *pure schema*: validation guarantees a config that parses cannot carry
//! an out-of-range weight, headroom, or budget into the engine. The engine
//! (`multiview-engine`) maps these into its `PlacementControllerConfig` +
//! `multiview_hal::select::PlacementPolicy`; this crate keeps no dependency on
//! the HAL types (config stays a leaf).
//!
//! All unions are internally tagged (`#[serde(tag = ...)]`), never `untagged`
//! (ADR-0010).

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// A **stable** GPU identity an operator pins a source's decode or an output's
/// encode to (ADR-0018 §2.1).
///
/// `stable_id` is the vendor's stable handle — an NVML UUID, a PCI bus id, or a
/// Metal registryID — **never** the enumeration index (which reorders across
/// reboots / `CUDA_VISIBLE_DEVICES`). It mirrors `multiview_hal::DeviceId`'s
/// identity (`vendor` + `stable_id`); the engine resolves it to a live
/// `DeviceId` at admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DevicePin {
    /// The vendor family (`nvidia` / `intel` / `amd` / `apple`).
    pub vendor: PinVendor,
    /// The vendor's stable device handle (UUID / PCI bus id / registryID).
    pub stable_id: String,
}

impl DevicePin {
    /// Construct a device pin.
    #[must_use]
    pub fn new(vendor: PinVendor, stable_id: impl Into<String>) -> Self {
        Self {
            vendor,
            stable_id: stable_id.into(),
        }
    }

    /// Validate the pin: the stable id must be non-empty (an empty handle could
    /// never resolve to a real device, so it is rejected at config time rather
    /// than silently failing the pin at admission).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] if `stable_id` is empty.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.stable_id.trim().is_empty() {
            return Err(ConfigError::Validation(
                "a gpu_pin.stable_id is empty (must be the vendor's stable handle, \
                 never the enumeration index)"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// The GPU vendor family a [`DevicePin`] names (mirrors `multiview_hal::Vendor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PinVendor {
    /// NVIDIA.
    Nvidia,
    /// Intel.
    Intel,
    /// AMD.
    Amd,
    /// Apple.
    Apple,
}

/// The per-resource scoring weights for the dominant-resource placement score
/// (ADR-0018 §2.3 — VRAM dominant, the others lighter).
///
/// Mirrors `multiview_hal::select::LoadWeights`. Each weight is a non-negative
/// finite fraction; [`PlacementWeights::validate`] rejects a NaN/negative weight
/// (which would silently corrupt the dominant-resource `max`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PlacementWeights {
    /// VRAM used-fraction weight (primary; highest).
    pub vram: f32,
    /// Encoder-ASIC busy-fraction weight.
    pub enc_util: f32,
    /// Decoder-ASIC busy-fraction weight.
    pub dec_util: f32,
    /// NVENC session used-fraction weight.
    pub nvenc_session: f32,
    /// Compute / compositor-pressure busy-fraction weight.
    pub compute: f32,
}

impl PlacementWeights {
    /// The ADR-0018 default weighting (VRAM dominant), matching the HAL
    /// `LoadWeights` defaults.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            vram: 1.0,
            enc_util: 0.6,
            dec_util: 0.6,
            nvenc_session: 0.5,
            compute: 0.4,
        }
    }

    /// Validate every weight is finite and `>= 0.0`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for the first non-finite or negative
    /// weight (a corrupt weight must never reach the score).
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in [
            ("vram", self.vram),
            ("enc_util", self.enc_util),
            ("dec_util", self.dec_util),
            ("nvenc_session", self.nvenc_session),
            ("compute", self.compute),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(ConfigError::Validation(format!(
                    "placement.weights.{name} = {value} must be a finite, non-negative weight"
                )));
            }
        }
        Ok(())
    }
}

impl Default for PlacementWeights {
    fn default() -> Self {
        Self::new_default()
    }
}

/// The migration anti-storm policy (ADR-0018 §4.6) — conservative defaults that
/// bias the loop toward holding/shedding over churning a live pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MigrationPolicy {
    /// Per-pipeline cooldown (control ticks) after any migration before another
    /// is permitted.
    pub cooldown_ticks: u32,
    /// Maximum migrations touching one GPU (as source or destination) within
    /// `budget_window_ticks`.
    pub per_gpu_budget: u32,
    /// The rolling window (control ticks) over which `per_gpu_budget` counts.
    pub budget_window_ticks: u32,
    /// The minimum dominant-share improvement a migration must buy to be taken.
    pub min_gain: f32,
    /// Freeze placement entirely (no migrations proposed) — the operator escape
    /// hatch for a known-hostile co-tenant (ADR-0018 §5 risk 5).
    #[serde(default)]
    pub freeze: bool,
}

impl MigrationPolicy {
    /// The ADR-0018 conservative defaults: a 10-tick cooldown, a 2-per-GPU
    /// budget over a 60-tick window, a `0.1` min-gain, placement not frozen.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            cooldown_ticks: 10,
            per_gpu_budget: 2,
            budget_window_ticks: 60,
            min_gain: 0.1,
            freeze: false,
        }
    }

    /// Validate the migration policy: `min_gain` finite + `>= 0.0`, and the
    /// budget window positive (a zero window would make every GPU instantly
    /// over budget, freezing placement by accident).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] on a non-finite/negative `min_gain`
    /// or a zero `budget_window_ticks`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.min_gain.is_finite() || self.min_gain < 0.0 {
            return Err(ConfigError::Validation(format!(
                "placement.migration.min_gain = {} must be finite and non-negative",
                self.min_gain
            )));
        }
        if self.budget_window_ticks == 0 {
            return Err(ConfigError::Validation(
                "placement.migration.budget_window_ticks must be >= 1 (a zero window \
                 would freeze placement by exhausting every GPU's budget)"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for MigrationPolicy {
    fn default() -> Self {
        Self::new_default()
    }
}

/// The whole-document GPU work-placement policy (ADR-0018 §consequences).
///
/// Absent from a config ⇒ the engine uses its conservative built-in defaults
/// (single-GPU hosts add zero behaviour). Present ⇒ these author-controlled
/// values steer scoring, reserve room for fluctuating external load, and bound
/// migration frequency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PlacementConfig {
    /// Per-resource reserve-headroom fraction (`0.0..=1.0`): a GPU whose
    /// dominant resource would exceed `1.0 - reserve_headroom` after admitting a
    /// pipeline is rejected/degraded, keeping room for other tenants. The engine
    /// maps this to the HAL `headroom_ceiling` (`1.0 - reserve_headroom`).
    #[serde(default = "default_reserve_headroom")]
    pub reserve_headroom: f32,
    /// The dominant-resource scoring weights.
    #[serde(default)]
    pub weights: PlacementWeights,
    /// The migration anti-storm policy.
    #[serde(default)]
    pub migration: MigrationPolicy,
}

/// The default reserve-headroom (`0.15`) — the complement of the ADR-0018
/// `0.85` headroom ceiling.
const fn default_reserve_headroom() -> f32 {
    0.15
}

impl PlacementConfig {
    /// The ADR-0018 conservative defaults.
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            reserve_headroom: default_reserve_headroom(),
            weights: PlacementWeights::new_default(),
            migration: MigrationPolicy::new_default(),
        }
    }

    /// The headroom ceiling the HAL selector uses: `1.0 - reserve_headroom`,
    /// clamped to `0.0..=1.0`.
    #[must_use]
    pub fn headroom_ceiling(&self) -> f32 {
        (1.0 - self.reserve_headroom).clamp(0.0, 1.0)
    }

    /// Validate the placement policy: reserve-headroom in `0.0..=1.0`, weights
    /// non-negative, migration policy sane.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the first violated field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.reserve_headroom.is_finite() || !(0.0..=1.0).contains(&self.reserve_headroom) {
            return Err(ConfigError::Validation(format!(
                "placement.reserve_headroom = {} must lie within 0.0..=1.0",
                self.reserve_headroom
            )));
        }
        self.weights.validate()?;
        self.migration.validate()?;
        Ok(())
    }
}

impl Default for PlacementConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::float_cmp
    )]
    use super::*;

    #[test]
    fn defaults_validate_and_match_adr_0018() {
        let p = PlacementConfig::new_default();
        p.validate().expect("conservative defaults are valid");
        // reserve 0.15 -> ceiling 0.85 (the ADR-0018 default headroom ceiling).
        assert!((p.headroom_ceiling() - 0.85).abs() < 1e-6);
        assert_eq!(p.weights.vram, 1.0);
        assert_eq!(p.migration.per_gpu_budget, 2);
        assert!(!p.migration.freeze);
    }

    #[test]
    fn reserve_headroom_out_of_range_is_rejected() {
        let mut p = PlacementConfig::new_default();
        p.reserve_headroom = 1.5;
        assert!(p.validate().is_err(), "headroom > 1.0 rejected");
        p.reserve_headroom = -0.1;
        assert!(p.validate().is_err(), "negative headroom rejected");
        p.reserve_headroom = f32::NAN;
        assert!(p.validate().is_err(), "NaN headroom rejected");
    }

    #[test]
    fn negative_weight_is_rejected() {
        let mut p = PlacementConfig::new_default();
        p.weights.enc_util = -0.5;
        assert!(
            p.validate().is_err(),
            "a negative weight corrupts the score and must be rejected"
        );
    }

    #[test]
    fn zero_budget_window_is_rejected() {
        let mut p = PlacementConfig::new_default();
        p.migration.budget_window_ticks = 0;
        assert!(
            p.validate().is_err(),
            "a zero window would freeze placement by accident"
        );
    }

    #[test]
    fn empty_pin_stable_id_is_rejected() {
        let pin = DevicePin::new(PinVendor::Nvidia, "   ");
        assert!(pin.validate().is_err(), "an empty stable id can't resolve");
        let pin = DevicePin::new(PinVendor::Nvidia, "GPU-uuid-1");
        pin.validate().expect("a real handle validates");
    }

    #[test]
    fn placement_block_round_trips_through_toml() {
        let toml = r"
            reserve_headroom = 0.2

            [weights]
            vram = 1.0
            enc_util = 0.5
            dec_util = 0.5
            nvenc_session = 0.5
            compute = 0.3

            [migration]
            cooldown_ticks = 15
            per_gpu_budget = 3
            budget_window_ticks = 90
            min_gain = 0.2
            freeze = true
        ";
        let parsed: PlacementConfig = toml::from_str(toml).expect("valid placement block");
        parsed.validate().expect("valid");
        assert!((parsed.reserve_headroom - 0.2).abs() < 1e-6);
        assert_eq!(parsed.migration.cooldown_ticks, 15);
        assert!(parsed.migration.freeze);
        // Round-trip back to TOML and re-parse equal.
        let back = toml::to_string(&parsed).expect("serialize");
        let again: PlacementConfig = toml::from_str(&back).expect("re-parse");
        assert_eq!(parsed, again);
    }

    #[test]
    fn device_pin_round_trips_and_is_vendor_keyed() {
        let pin = DevicePin::new(PinVendor::Amd, "0000:03:00.0");
        let json = serde_json::to_string(&pin).expect("serialize");
        let back: DevicePin = serde_json::from_str(&json).expect("parse");
        assert_eq!(pin, back);
        assert_eq!(back.vendor, PinVendor::Amd);
        assert_eq!(back.stable_id, "0000:03:00.0");
    }

    #[test]
    fn unknown_field_in_placement_is_rejected() {
        // deny_unknown_fields: a typo'd key fails at parse, never silently
        // ignored.
        let toml = "reserve_headroom = 0.1\nreserv_headroom = 0.2\n";
        assert!(toml::from_str::<PlacementConfig>(toml).is_err());
    }
}
