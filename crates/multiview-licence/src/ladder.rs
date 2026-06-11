//! The enforcement ladder as **pure data** (ADR-0050 §2/§4/§6, brief §6, §12).
//!
//! # Never off air (the load-bearing invariant)
//!
//! This module **computes a state**. It holds **no engine handle**, spawns **no
//! task**, performs **no I/O**, and has **no way to stop, stall, or de-pace
//! program output**. The hardest rung the ladder can reach is data that *asks*
//! the engine to lock reconfiguration or stamp a corner watermark — conveniences
//! the engine applies by reading a pre-derived atomic off the hot loop
//! (ADR-0050 §5). Every [`LadderState`] answers `program_stays_on_air() == true`
//! by construction (see [`LadderState::program_stays_on_air`]); the type cannot
//! express "stop output". This is the product promise (invariant #1), and it is
//! enforced here structurally, not by convention.
//!
//! The ladder is computed **off the hot loop** from the lease arithmetic
//! ([`crate::lease`]) + the hardware-class match + GPU usage vs limit + the
//! evaluation day. The engine reads only the two derived booleans
//! ([`LadderOutcome::config_locked`] / [`LadderOutcome::watermark`]); it never
//! runs this computation on the tick path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::constants::{EVALUATION_WATERMARK_DAY, LAPSED_SOFT_MAX_DAYS, LEASE_GRACE_DAYS};
use crate::entitlement::HardwareClass;
use crate::lease::Lease;

/// The computed ladder state — the seven conditions the entitlement plane
/// derives (ADR-0050 §4 / brief §6, §12). Serialised `snake_case` so every
/// surface (engine, API, portals) reads the **same** discriminant.
///
/// `#[non_exhaustive]`: future conditions add without breaking match sites that
/// already handle a wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LadderState {
    /// Lease valid (within term). Maps to the `active` resource level.
    Compliant,
    /// Within the 14-day grace window past expiry. Maps to `warning`.
    Grace,
    /// 15–45 days past expiry: blocks **new** instances + config-lock (as data).
    LapsedSoft,
    /// More than 45 days past expiry: watermark + config-lock (as data).
    LapsedHard,
    /// An evaluation/trial grant within its period; honest watermark from day 31.
    Evaluation,
    /// The detected hardware class differs from the licensed class.
    ClassMismatch,
    /// GPU usage exceeds the entitlement's GPU limit.
    OverGpu,
}

impl LadderState {
    /// **The never-off-air guarantee.** Every state keeps program output on air;
    /// this is a `const true` — the ladder has no rung that takes a running
    /// program off air (ADR-0050 §6.3, invariant #1). It is a method (not an
    /// absent concept) so callers and tests can assert it explicitly.
    #[must_use]
    pub const fn program_stays_on_air(self) -> bool {
        true
    }

    /// Whether this state asks the engine to **deny hot-reconfiguration** (the
    /// running scene keeps playing; you simply cannot reconfigure it). Data only
    /// — the engine reads the derived atomic off the loop (S2).
    #[must_use]
    pub const fn config_locked(self) -> bool {
        matches!(self, LadderState::LapsedSoft | LadderState::LapsedHard)
    }

    /// Whether the state **alone** requests a corner watermark. Lapsed-hard does;
    /// an evaluation grant's watermark depends on the evaluation *day*, so that
    /// is computed in [`compute_ladder_state`] and surfaced on
    /// [`LadderOutcome::watermark`] (not here).
    #[must_use]
    pub const fn watermark(self) -> bool {
        matches!(self, LadderState::LapsedHard)
    }

    /// Whether this state asks the startup gate to **refuse creating a new**
    /// engine instance (S1). A running instance is never affected — this is a
    /// startup-only convenience (ADR-0050 §5). Data only.
    #[must_use]
    pub const fn blocks_new_instances(self) -> bool {
        matches!(self, LadderState::LapsedSoft | LadderState::LapsedHard)
    }
}

/// The inputs to the ladder computation, assembled off the hot loop by the cli
/// from the entitlement resource + sampled GPU usage + the wall clock. All times
/// are explicit instants handed in; this crate never reads a system clock
/// (data minimisation + determinism).
///
/// This is a **local compute input**, not a versioned wire resource, so it is
/// freely constructible by callers (the cli builds and mutates it field by
/// field). The wire resources ([`crate::entitlement::Entitlement`],
/// [`crate::lease::Lease`], the level enums) carry `#[non_exhaustive]`; this
/// input deliberately does not.
#[derive(Debug, Clone)]
pub struct LadderInput {
    /// The current dated lease.
    pub lease: Lease,
    /// The instant to evaluate the ladder at (the cli supplies `Utc::now()`).
    pub now: DateTime<Utc>,
    /// The hardware class the entitlement is licensed for.
    pub licensed_class: HardwareClass,
    /// The hardware class detected on the machine.
    pub detected_class: HardwareClass,
    /// The GPU allowance (count). Usage strictly above this is `over_gpu`.
    pub gpu_limit: u32,
    /// The number of GPUs currently in use.
    pub gpu_in_use: u32,
    /// When the evaluation/trial period started, if this is an eval grant.
    pub evaluation_started_at: Option<DateTime<Utc>>,
}

/// The full computed ladder result: the [`LadderState`] plus the derived
/// convenience booleans the engine seams consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LadderOutcome {
    /// The single computed state every surface renders.
    pub state: LadderState,
    /// Whether a corner watermark is requested (lapsed-hard, or an evaluation
    /// grant from day [`EVALUATION_WATERMARK_DAY`]). The engine reads this as a
    /// pre-derived atomic off the loop (S3).
    watermark: bool,
}

impl LadderOutcome {
    /// See [`LadderState::program_stays_on_air`] — always `true`.
    #[must_use]
    pub const fn program_stays_on_air(&self) -> bool {
        self.state.program_stays_on_air()
    }

    /// See [`LadderState::config_locked`].
    #[must_use]
    pub const fn config_locked(&self) -> bool {
        self.state.config_locked()
    }

    /// Whether a corner watermark is requested. Lapsed-hard always; an evaluation
    /// grant from day [`EVALUATION_WATERMARK_DAY`].
    #[must_use]
    pub const fn watermark(&self) -> bool {
        self.watermark
    }

    /// See [`LadderState::blocks_new_instances`].
    #[must_use]
    pub const fn blocks_new_instances(&self) -> bool {
        self.state.blocks_new_instances()
    }
}

/// Compute the ladder state from its inputs (ADR-0050 §6).
///
/// Precedence — most-actionable first, so the operator sees the reason that most
/// needs fixing:
/// 1. `class_mismatch` (licensed ≠ detected hardware class),
/// 2. `over_gpu` (usage strictly exceeds the limit),
/// 3. `evaluation` (an active eval/trial grant),
/// 4. the time phase: `compliant` → `grace` (≤14d past expiry) → `lapsed_soft`
///    (15–45d) → `lapsed_hard` (>45d).
///
/// It performs no I/O and cannot affect output (never-off-air).
#[must_use]
pub fn compute_ladder_state(input: &LadderInput) -> LadderOutcome {
    let state = primary_state(input);
    let watermark = state.watermark() || evaluation_watermark(input, state);
    LadderOutcome { state, watermark }
}

fn primary_state(input: &LadderInput) -> LadderState {
    if input.licensed_class != input.detected_class {
        return LadderState::ClassMismatch;
    }
    if input.gpu_in_use > input.gpu_limit {
        return LadderState::OverGpu;
    }
    if input.evaluation_started_at.is_some() {
        return LadderState::Evaluation;
    }
    time_phase(&input.lease, input.now)
}

/// The time-based phase from whole days past expiry (the exact boundaries).
fn time_phase(lease: &Lease, now: DateTime<Utc>) -> LadderState {
    let past = lease.days_past_expiry(now);
    if past <= 0 {
        LadderState::Compliant
    } else if past <= LEASE_GRACE_DAYS {
        LadderState::Grace
    } else if past <= LAPSED_SOFT_MAX_DAYS {
        LadderState::LapsedSoft
    } else {
        LadderState::LapsedHard
    }
}

/// An evaluation grant stamps an honest watermark from day
/// [`EVALUATION_WATERMARK_DAY`] (clean for the first 30 elapsed days; the
/// watermark engages once 31 days have elapsed since the grant).
fn evaluation_watermark(input: &LadderInput, state: LadderState) -> bool {
    if state != LadderState::Evaluation {
        return false;
    }
    match input.evaluation_started_at {
        Some(start) => {
            let elapsed_days = (input.now - start).num_days();
            elapsed_days >= EVALUATION_WATERMARK_DAY
        }
        None => false,
    }
}
