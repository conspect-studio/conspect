//! The admission + **resource-adaptive degradation** control loop (invariant
//! #9).
//!
//! A closed control loop runs on the engine's *slow* control tick (not the
//! per-frame output clock):
//!
//! 1. **Sense** — read a normalized `0.0..=1.0` pressure signal. The default
//!    derivation ([`ControlLoop::pressure_from_plan`]) is the worst per-engine
//!    utilization from the [`Planner`]'s cost model (decode/composite/encode
//!    Mpix/s vs budget); a real deployment refines it with measured telemetry.
//! 2. **Estimate** — the [`Planner`] (HAL cost model) turns a candidate plan
//!    into per-stage usage; admission (`admit`) is the hard gate at start time.
//! 3. **Plan** — the [`Hysteresis`](multiview_hal::degradation::Hysteresis) controller (inside the planner) maps the
//!    pressure reading to a single, non-flapping ladder move per tick. Shedding
//!    is prompt; recovery waits out a dwell/cooldown so a noisy signal cannot
//!    oscillate the plan.
//! 4. **Apply** — the active level selects the cumulative
//!    [`DegradationAction`] set, applied **cheapest-impact-first, tile-by-tile,
//!    before the program output everyone sees is ever touched**
//!    (`affects_program()` marks the boundary). Bounded queues drop, never grow.
//!
//! This module is a thin, deterministic orchestrator over `multiview-hal`; all the
//! ladder semantics and the anti-flap math live there and are reused verbatim
//! (no duplication).
use multiview_hal::degradation::{actions_at_level, DegradationAction, LadderMove};
use multiview_hal::planner::{Admission, Plan, Planner};
use multiview_hal::{CostBudget, HysteresisConfig, Stage};

use crate::error::{Error, Result};

/// The outcome of one control-loop step.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlStep {
    /// The ladder move the hysteresis controller applied this tick.
    pub mv: LadderMove,
    /// The degradation level after the move (`0` = full quality).
    pub level: usize,
    /// The cumulative set of actions now active (cheapest-impact-first order).
    pub active: &'static [DegradationAction],
    /// The pressure reading that drove the step.
    pub pressure: f64,
}

impl ControlStep {
    /// Whether any currently-active action degrades the **program output**
    /// everyone sees (rung 4+). While this is `false`, all shedding is confined
    /// to low-priority tiles / shared resources — the invariant-#9 guarantee
    /// that program output is touched last.
    #[must_use]
    pub fn affects_program(&self) -> bool {
        self.active.iter().any(|a| a.affects_program())
    }
}

/// The engine's admission + degradation control loop.
///
/// Wraps a [`Planner`] (HAL). The engine constructs one at start from the cost
/// budget, runs admission once for the chosen plan, then drives
/// [`ControlLoop::step`] on its slow control tick with a sensed pressure signal.
#[derive(Debug, Clone)]
pub struct ControlLoop {
    planner: Planner,
}

impl ControlLoop {
    /// Construct a control loop with the given cost `budget` and the default
    /// hysteresis tuning.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidControlLoop`] if the budget is malformed.
    pub fn new(budget: CostBudget) -> Result<Self> {
        let planner = Planner::new(budget).map_err(|e| Error::InvalidControlLoop(e.to_string()))?;
        Ok(Self { planner })
    }

    /// Construct a control loop with an explicit hysteresis configuration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidControlLoop`] if the budget or hysteresis config
    /// is malformed.
    pub fn with_hysteresis(budget: CostBudget, hysteresis: HysteresisConfig) -> Result<Self> {
        let planner = Planner::with_hysteresis(budget, hysteresis)
            .map_err(|e| Error::InvalidControlLoop(e.to_string()))?;
        Ok(Self { planner })
    }

    /// Admit a candidate `plan`: does it fit within every engine budget?
    ///
    /// This is the start-time hard gate (invariant #6/#9). Returns the per-stage
    /// usage breakdown on success.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidControlLoop`] (wrapping the HAL `BudgetExceeded`)
    /// for the first over-budget stage in pipeline order.
    pub fn admit(&self, plan: &Plan) -> Result<Admission> {
        self.planner
            .admit(plan)
            .map_err(|e| Error::InvalidControlLoop(e.to_string()))
    }

    /// The current degradation level (`0` = full quality).
    #[must_use]
    pub fn level(&self) -> usize {
        self.planner.degradation_level()
    }

    /// The currently-active cumulative action set.
    #[must_use]
    pub fn active_actions(&self) -> &'static [DegradationAction] {
        actions_at_level(self.level())
    }

    /// Derive a normalized `0.0..=1.0` pressure signal from a candidate `plan`'s
    /// utilization of the budget: the **worst** per-engine utilization, clamped.
    ///
    /// This is the "sense" step's default estimator — pressure is the most
    /// loaded stage, because the most-loaded engine is what will stall first. A
    /// utilization `>= 1.0` clamps to `1.0` (fully saturated). A degenerate
    /// reading yields `0.0`.
    #[must_use]
    pub fn pressure_from_plan(&self, plan: &Plan) -> f64 {
        let admission = self.planner.evaluate(plan);
        let mut worst = 0.0_f64;
        for stage in Stage::ALL {
            let util = admission.for_stage(stage).utilization();
            if util.is_finite() && util > worst {
                worst = util;
            } else if util.is_infinite() {
                worst = 1.0;
            }
        }
        worst.clamp(0.0, 1.0)
    }

    /// Run one control-loop step with an already-sensed `pressure` reading.
    ///
    /// Drives the hysteresis controller (plan) and reports the applied move and
    /// the resulting active action set (apply). Non-flapping by construction:
    /// the controller sheds promptly but recovers only after its dwell.
    pub fn step(&mut self, pressure: f64) -> ControlStep {
        let mv = self.planner.next_step(pressure);
        let level = self.planner.degradation_level();
        ControlStep {
            mv,
            level,
            active: actions_at_level(level),
            pressure,
        }
    }

    /// Sense pressure from a `plan` and run one step in a single call.
    pub fn step_from_plan(&mut self, plan: &Plan) -> ControlStep {
        let pressure = self.pressure_from_plan(plan);
        self.step(pressure)
    }
}
