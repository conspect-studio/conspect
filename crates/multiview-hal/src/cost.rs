//! The cost model: per-tile load and per-engine budgets in **megapixels/sec**.
//!
//! Per invariant #6 and [efficiency §1,§4](../../../docs/research/efficiency.md),
//! decode/composite/encode load is budgeted in *decoded megapixels per second*
//! — never stream count — because a 4K tile costs ~9x a 720p tile. Each engine
//! (decode media-engine, GPU compositor, encode media-engine) carries an
//! independent budget; they live on physically distinct hardware and must be
//! accounted separately.
//!
//! All figures here are pure data: the planner sums per-tile loads and compares
//! them against the budget. The *measured* per-frame costs that refine these
//! priors come from telemetry in another crate; this module models the static
//! plan.
use multiview_core::time::Rational;

use crate::capability::{Resolution, Stage};
use crate::error::{Error, Result};

/// The per-engine throughput budgets, in megapixels per second.
///
/// One value per pipeline [`Stage`]. A plan is admissible only if every stage's
/// summed tile load is `<=` its budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostBudget {
    /// Decode engine budget (decoded Mpix/s, summed over input tiles).
    pub decode_mpps: f64,
    /// Composite engine budget (composited Mpix/s, the canvas plus per-tile
    /// sampling work).
    pub composite_mpps: f64,
    /// Encode engine budget (encoded Mpix/s, summed over renditions).
    pub encode_mpps: f64,
}

impl CostBudget {
    /// Construct a budget.
    #[must_use]
    pub const fn new(decode_mpps: f64, composite_mpps: f64, encode_mpps: f64) -> Self {
        Self {
            decode_mpps,
            composite_mpps,
            encode_mpps,
        }
    }

    /// The budget for a single [`Stage`].
    #[must_use]
    pub const fn for_stage(&self, stage: Stage) -> f64 {
        match stage {
            Stage::Decode => self.decode_mpps,
            Stage::Composite => self.composite_mpps,
            Stage::Encode => self.encode_mpps,
        }
    }

    /// Validate that all budgets are finite and non-negative.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if any budget is negative or
    /// non-finite (NaN/infinite).
    pub fn validate(&self) -> Result<()> {
        for value in [self.decode_mpps, self.composite_mpps, self.encode_mpps] {
            if !value.is_finite() || value < 0.0 {
                return Err(Error::InvalidCapability(
                    "cost budget must be finite and non-negative",
                ));
            }
        }
        Ok(())
    }
}

/// The load one tile (or rendition) imposes on a single engine, in
/// megapixels/sec.
///
/// Computed as `resolution.megapixels() * fps`, where `fps` is carried as an
/// exact [`Rational`] (invariant #3 — never a float fps). The conversion to a
/// floating Mpix/s figure happens once, here, for budget comparison only — the
/// timing math upstream stays exact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileLoad {
    /// The stage this load applies to.
    pub stage: Stage,
    /// The tile/rendition resolution.
    pub resolution: Resolution,
}

impl TileLoad {
    /// Construct a tile load for a stage at a resolution.
    #[must_use]
    pub const fn new(stage: Stage, resolution: Resolution) -> Self {
        Self { stage, resolution }
    }

    /// The load in megapixels/sec at the given `cadence` (frames/sec).
    ///
    /// Returns `0.0` for a degenerate (zero-denominator) cadence; callers
    /// should validate the cadence with [`Rational::is_valid`] first.
    #[must_use]
    pub fn megapixels_per_sec(&self, cadence: Rational) -> f64 {
        if !cadence.is_valid() {
            return 0.0;
        }
        let fps = cadence.as_f64();
        if !fps.is_finite() || fps < 0.0 {
            return 0.0;
        }
        self.resolution.megapixels() * fps
    }
}
