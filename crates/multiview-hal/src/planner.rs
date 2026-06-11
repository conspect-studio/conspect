//! The planner: admission control over the cost budget + the degradation hook.
//!
//! The planner is the decision seam of invariants #6 and #9. It consumes the
//! [`crate::registry::BackendRegistry`] (what's possible) and a
//! [`crate::cost::CostBudget`] (what fits), and answers two questions:
//!
//! - **Admission** ([`Planner::admit`]): does a proposed [`Plan`] — a set of
//!   per-tile/per-rendition loads at a fixed output cadence — fit within every
//!   engine's budget? Over-budget on any stage is a hard rejection.
//! - **Degradation** ([`Planner::next_step`]): wraps the [`Hysteresis`]
//!   controller so the engine's control loop gets a single, non-flapping ladder
//!   move per tick.
//!
//! Both are pure: no I/O, no native deps, fully deterministic and testable.
use multiview_core::time::Rational;

use crate::capability::Stage;
use crate::cost::{CostBudget, TileLoad};
use crate::degradation::{Hysteresis, HysteresisConfig, LadderMove};
use crate::error::{Error, Result};

/// A proposed pipeline plan: the tile/rendition loads, plus the output cadence.
///
/// The cadence is the single fixed output frame rate (invariant #1/#3); every
/// load is evaluated at it. Tiles are grouped only by [`Stage`] for budgeting —
/// the planner sums each stage independently because the engines are physically
/// distinct (efficiency §4).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The fixed output cadence (frames/sec) as an exact rational.
    pub cadence: Rational,
    /// All per-tile / per-rendition loads in the plan.
    pub loads: Vec<TileLoad>,
}

impl Plan {
    /// Construct a plan.
    #[must_use]
    pub fn new(cadence: Rational, loads: Vec<TileLoad>) -> Self {
        Self { cadence, loads }
    }

    /// Total load on `stage`, in megapixels/sec, summing every matching tile.
    #[must_use]
    pub fn stage_load_mpps(&self, stage: Stage) -> f64 {
        self.loads
            .iter()
            .filter(|load| load.stage == stage)
            .map(|load| load.megapixels_per_sec(self.cadence))
            .sum()
    }
}

/// The outcome of an admission check.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    /// Per-stage `(requested, budget)` load in megapixels/sec, for telemetry.
    pub decode: StageUsage,
    /// Composite stage usage.
    pub composite: StageUsage,
    /// Encode stage usage.
    pub encode: StageUsage,
}

impl Admission {
    /// Usage for a given stage.
    #[must_use]
    pub const fn for_stage(&self, stage: Stage) -> StageUsage {
        match stage {
            Stage::Decode => self.decode,
            Stage::Composite => self.composite,
            Stage::Encode => self.encode,
        }
    }
}

/// Requested vs available load for one engine, in megapixels/sec.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageUsage {
    /// Megapixels/sec requested by the plan on this stage.
    pub requested_mpps: f64,
    /// Megapixels/sec available in the budget on this stage.
    pub budget_mpps: f64,
}

impl StageUsage {
    /// Fraction of the budget the request consumes (`0.0..` ; `> 1.0` means
    /// over-budget). A zero budget yields [`f64::INFINITY`] when anything is
    /// requested, and `0.0` when nothing is.
    #[must_use]
    pub fn utilization(self) -> f64 {
        if self.budget_mpps == 0.0 {
            if self.requested_mpps == 0.0 {
                0.0
            } else {
                f64::INFINITY
            }
        } else {
            self.requested_mpps / self.budget_mpps
        }
    }

    /// Whether the request fits within the budget (`<=`, inclusive).
    #[must_use]
    pub fn fits(self) -> bool {
        self.requested_mpps <= self.budget_mpps
    }
}

/// The admission + degradation planner.
#[derive(Debug, Clone)]
pub struct Planner {
    budget: CostBudget,
    hysteresis: Hysteresis,
}

impl Planner {
    /// Construct a planner with the given budget and the default hysteresis
    /// tuning.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if the budget is malformed
    /// ([`CostBudget::validate`]).
    pub fn new(budget: CostBudget) -> Result<Self> {
        Self::with_hysteresis(budget, HysteresisConfig::new_default())
    }

    /// Construct a planner with an explicit hysteresis configuration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if the budget or the hysteresis
    /// config is malformed.
    pub fn with_hysteresis(budget: CostBudget, hysteresis: HysteresisConfig) -> Result<Self> {
        budget.validate()?;
        hysteresis.validate()?;
        Ok(Self {
            budget,
            hysteresis: Hysteresis::new(hysteresis),
        })
    }

    /// The configured budget.
    #[must_use]
    pub const fn budget(&self) -> CostBudget {
        self.budget
    }

    /// The current degradation level (`0` = full quality).
    #[must_use]
    pub fn degradation_level(&self) -> usize {
        self.hysteresis.level()
    }

    /// Compute the per-stage usage of `plan` against the budget (always
    /// succeeds; inspect [`Admission`] / [`StageUsage::fits`] for the verdict).
    #[must_use]
    pub fn evaluate(&self, plan: &Plan) -> Admission {
        let usage = |stage: Stage| StageUsage {
            requested_mpps: plan.stage_load_mpps(stage),
            budget_mpps: self.budget.for_stage(stage),
        };
        Admission {
            decode: usage(Stage::Decode),
            composite: usage(Stage::Composite),
            encode: usage(Stage::Encode),
        }
    }

    /// Admit `plan` only if it fits within **every** engine budget.
    ///
    /// Returns the [`Admission`] breakdown on success.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExceeded`] for the first stage (in pipeline
    /// order) whose summed load exceeds its budget. An invalid plan cadence
    /// (degenerate rational) yields a zero load and therefore admits trivially;
    /// validate the cadence with [`Rational::is_valid`] upstream.
    pub fn admit(&self, plan: &Plan) -> Result<Admission> {
        let admission = self.evaluate(plan);
        for stage in Stage::ALL {
            let usage = admission.for_stage(stage);
            if !usage.fits() {
                return Err(Error::BudgetExceeded {
                    stage,
                    requested_mpps: usage.requested_mpps,
                    budget_mpps: usage.budget_mpps,
                });
            }
        }
        Ok(admission)
    }

    /// Drive the degradation controller one control tick with a normalized
    /// `pressure` reading and return the ladder move applied.
    ///
    /// This is the loop the engine calls on its slow control tick; the
    /// hysteresis guarantees the level never flaps. After a move, consult
    /// [`Planner::degradation_level`] /
    /// [`crate::degradation::actions_at_level`] for the active levers.
    pub fn next_step(&mut self, pressure: f64) -> LadderMove {
        self.hysteresis.observe(pressure)
    }
}
