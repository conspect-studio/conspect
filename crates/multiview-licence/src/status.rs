//! The published licence status (the S4 hand-off shape) and the canonical
//! `enforcement.level` resource field (ADR-0050 §2/§3/§6.1).
//!
//! [`EnforcementLevel`] is the field every surface renders identically (engine,
//! API, portals) — there is no second opinion. [`LicenceStatus`] is the shape
//! the entitlement plane publishes wait-free for the control plane to read at
//! `GET /api/v1/account/licence` (ADR-0050 §3). This module is pure data: it
//! holds **no** engine handle and cannot affect output (never-off-air).

use serde::{Deserialize, Serialize};

use crate::ladder::{LadderOutcome, LadderState};
use crate::lease::Lease;

/// The canonical enforcement level — the `enforcement.level` resource field
/// (ADR-0050 §2/§6.2). Serialised `kebab-case` to match the resource shape
/// (`config-locked`, `block-new-instance`, `unlicensed-build`). The engine, API,
/// and portals all read this same discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EnforcementLevel {
    /// Lease valid — clean canvas, on air.
    Active,
    /// Nearing expiry / in grace, or a policy reason (class/GPU) the operator
    /// should act on — clean canvas, on air.
    Warning,
    /// Hot-reconfiguration denied; the running scene keeps playing — on air.
    ConfigLocked,
    /// Corner watermark stamped; reconfiguration denied — on air.
    Watermark,
    /// Creating a **new** engine instance is refused; running instances keep
    /// playing — on air.
    BlockNewInstance,
    /// The heartbeat client was compiled out; reported honestly (ADR-0050 §7).
    UnlicensedBuild,
}

impl EnforcementLevel {
    /// Map a computed [`LadderState`] to the canonical resource level
    /// (ADR-0050 §6.2). This is the single mapping every surface shares.
    #[must_use]
    pub const fn from_ladder_state(state: LadderState) -> Self {
        match state {
            LadderState::Compliant => EnforcementLevel::Active,
            // A near-expiry warning, a class mismatch, or an over-GPU condition
            // are all "act on this" warnings that keep the canvas clean.
            LadderState::Grace
            | LadderState::Evaluation
            | LadderState::ClassMismatch
            | LadderState::OverGpu => EnforcementLevel::Warning,
            // Soft lapse blocks new instances + locks config (data only).
            LadderState::LapsedSoft => EnforcementLevel::ConfigLocked,
            // Hard lapse adds the watermark.
            LadderState::LapsedHard => EnforcementLevel::Watermark,
        }
    }

    /// **The never-off-air guarantee** restated at the resource level: no level
    /// takes a running program off air (ADR-0050 §6.3, invariant #1).
    #[must_use]
    pub const fn program_stays_on_air(self) -> bool {
        true
    }

    /// Whether this level asks the engine to **deny hot-reconfiguration** (S2):
    /// the running scene keeps playing, you simply cannot reconfigure it. True
    /// from `config-locked` and on the harder rungs that subsume it (`watermark`,
    /// `block-new-instance`); never on `active`/`warning`/`unlicensed-build`. This
    /// is one of the two cheap booleans an engine seam derives from the single
    /// published level (ADR-0050 §3/§5) — data only, no control flow here.
    #[must_use]
    pub const fn config_locked(self) -> bool {
        matches!(
            self,
            EnforcementLevel::ConfigLocked
                | EnforcementLevel::Watermark
                | EnforcementLevel::BlockNewInstance
        )
    }

    /// Whether this level asks the engine to **stamp a corner watermark** (S3) on
    /// the composited multiview canvas. True for `watermark`, the harder
    /// `block-new-instance` rung that subsumes it, and the honest
    /// `unlicensed-build` watermark (ADR-0050 §7). Marking the canvas never stops
    /// or de-paces output — the watermark rides the overlay sub-pass off the hot
    /// loop (ADR-0050 §5/§6.3, invariant #1). The other cheap boolean an engine
    /// seam derives from the single published level.
    #[must_use]
    pub const fn watermark(self) -> bool {
        matches!(
            self,
            EnforcementLevel::Watermark
                | EnforcementLevel::BlockNewInstance
                | EnforcementLevel::UnlicensedBuild
        )
    }

    /// Whether the startup gate should **refuse creating a NEW engine instance**
    /// (S1). True ONLY at `block-new-instance` — every softer rung allows a start,
    /// and an unlicensed build is reported honestly but never blocked (ADR-0050
    /// §7). A **running** instance never re-enters the startup gate, so this can
    /// never take a running program off air (ADR-0050 §5/§6.3, invariant #1).
    #[must_use]
    pub const fn blocks_new_instances(self) -> bool {
        matches!(self, EnforcementLevel::BlockNewInstance)
    }
}

/// The published licence status — the wait-free hand-off the entitlement plane
/// exposes for the control plane (ADR-0050 §3, S4). It is small, `Clone`, and
/// serde-round-trippable. It carries the level, the machine-readable reasons,
/// and the dated lease bounds; the engine derives only two booleans from it
/// (`config_locked`, `watermark`) off the hot loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LicenceStatus {
    /// The canonical enforcement level (the resource field).
    pub level: EnforcementLevel,
    /// Machine-readable reason codes; the UI renders all of them.
    pub reasons: Vec<String>,
    /// Whether the engine should deny hot-reconfiguration (derived).
    pub config_locked: bool,
    /// Whether the engine should stamp a corner watermark (derived).
    pub watermark: bool,
    /// Whether the startup gate should refuse a new engine instance (derived).
    pub blocks_new_instances: bool,
    /// The dated lease this status reflects.
    pub lease: Lease,
}

impl LicenceStatus {
    /// Build a published status from a computed ladder outcome, the lease, and
    /// the machine-readable reason codes.
    #[must_use]
    pub fn from_outcome(outcome: LadderOutcome, lease: Lease, reasons: Vec<String>) -> Self {
        Self {
            level: EnforcementLevel::from_ladder_state(outcome.state),
            reasons,
            config_locked: outcome.config_locked(),
            watermark: outcome.watermark(),
            blocks_new_instances: outcome.blocks_new_instances(),
            lease,
        }
    }

    /// See [`EnforcementLevel::program_stays_on_air`] — always `true`.
    #[must_use]
    pub const fn program_stays_on_air(&self) -> bool {
        self.level.program_stays_on_air()
    }
}
