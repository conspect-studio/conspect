//! Process health state for the `/livez` and `/readyz` probes.
//!
//! This module is **pure logic** — there is no HTTP server here. `multiview-control`
//! wires these reports to endpoints. The split mirrors the observability brief
//! (core-engine §15) and ADR-R009:
//!
//! * **Liveness (`/livez`)** is *in-process only*. It must NEVER consult the GPU
//!   driver or an upstream camera, or a transient external fault would trigger a
//!   container restart loop. A process that is running and not wedged is live.
//! * **Readiness (`/readyz`)** verifies that startup prerequisites — ingest,
//!   backend initialization, output endpoints — have come up. It is gated: the
//!   process reports *not ready* until every declared gate is satisfied, and a
//!   gate that regresses drops readiness again.
//!
//! Readiness gates are an ordered set keyed by [`GateId`]; declaring the same id
//! twice is idempotent, and satisfying an id that was never declared is a no-op
//! (it can never make the process spuriously ready).
use std::collections::BTreeMap;

/// A stable identifier for a readiness gate (e.g. `"ingest"`, `"output"`).
///
/// Cheap to clone; compared and rendered by its string value. Kept as an owned
/// `String` so callers can derive ids at runtime (per-output, per-source).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GateId(String);

impl GateId {
    /// Construct a gate id from anything string-like.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for GateId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for GateId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A snapshot of the liveness probe result.
///
/// Liveness is in-process only; this carries no external dependency state by
/// design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Liveness {
    /// Whether the process is live (running and not wedged).
    pub live: bool,
}

/// A snapshot of the readiness probe result, including which gates are pending.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Readiness {
    /// `true` only when every declared gate is satisfied.
    pub ready: bool,
    /// The declared gates that are not yet satisfied, in id order.
    pub pending: Vec<GateId>,
}

/// The mutable health state of the process.
///
/// Single-writer by design: a supervisor owns one `HealthState` and flips gates
/// as subsystems come up or fall over. Probe reads clone a cheap snapshot. There
/// is no interior locking here — wrap it in the caller's preferred sync
/// primitive (e.g. an `arc-swap` or a `RwLock`) at the control-plane boundary.
#[derive(Debug, Clone, Default)]
pub struct HealthState {
    /// Whether the process is live. In-process only; defaults to `true`.
    live: bool,
    /// Declared readiness gates mapped to their satisfied flag. A `BTreeMap`
    /// keeps `pending` deterministic and ordered.
    gates: BTreeMap<GateId, bool>,
    /// Constructed flag distinguishing `new()` from `Default` for liveness.
    initialized: bool,
}

impl HealthState {
    /// Create a fresh health state: **live**, with no readiness gates declared.
    ///
    /// A process with no declared gates is vacuously ready; the engine declares
    /// its gates during startup, which makes it not-ready until they pass.
    #[must_use]
    pub fn new() -> Self {
        Self {
            live: true,
            gates: BTreeMap::new(),
            initialized: true,
        }
    }

    /// Whether the process is live.
    ///
    /// Liveness is independent of readiness gates by design (invariant: external
    /// faults must not restart-loop the process). A `Default`-constructed value
    /// is treated as live once it has been observed via [`HealthState::new`];
    /// the in-process default is live.
    #[must_use]
    pub fn is_live(&self) -> bool {
        // Either explicitly initialized via `new()` or left at the live default.
        self.live || !self.initialized
    }

    /// Mark the process as no longer live (in-process fatal condition only).
    ///
    /// Use sparingly: this is for a genuine internal wedge an orchestrator
    /// should restart, never for an upstream/GPU fault.
    pub fn set_live(&mut self, live: bool) {
        self.initialized = true;
        self.live = live;
    }

    /// Declare a readiness gate. Idempotent: re-declaring keeps the existing
    /// satisfied state, so it never resets a gate that already passed.
    pub fn declare_gate(&mut self, id: GateId) {
        self.gates.entry(id).or_insert(false);
    }

    /// Mark a previously-declared gate as satisfied.
    ///
    /// Satisfying an *undeclared* gate is a deliberate no-op: it must never be
    /// possible to make the process ready by satisfying a gate that was never
    /// part of the readiness contract.
    pub fn satisfy(&mut self, id: &GateId) {
        if let Some(flag) = self.gates.get_mut(id) {
            *flag = true;
        }
    }

    /// Mark a previously-declared gate as no longer satisfied (it regressed).
    pub fn unsatisfy(&mut self, id: &GateId) {
        if let Some(flag) = self.gates.get_mut(id) {
            *flag = false;
        }
    }

    /// Whether every declared readiness gate is satisfied.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.gates.values().all(|satisfied| *satisfied)
    }

    /// A liveness snapshot for the `/livez` probe.
    #[must_use]
    pub fn liveness(&self) -> Liveness {
        Liveness {
            live: self.is_live(),
        }
    }

    /// A readiness snapshot for the `/readyz` probe, listing pending gates.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        let pending: Vec<GateId> = self
            .gates
            .iter()
            .filter(|(_, satisfied)| !**satisfied)
            .map(|(id, _)| id.clone())
            .collect();
        Readiness {
            ready: pending.is_empty(),
            pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_live() {
        // A `Default`-constructed state (e.g. derived inside another struct) is
        // still treated as live — liveness must default true.
        let state = HealthState::default();
        assert!(state.is_live());
        assert!(state.liveness().live);
    }

    #[test]
    fn set_live_false_makes_process_not_live_without_touching_readiness() {
        let mut state = HealthState::new();
        state.declare_gate(GateId::new("ingest"));
        state.satisfy(&GateId::new("ingest"));
        assert!(state.is_ready());

        state.set_live(false);
        assert!(
            !state.is_live(),
            "explicit set_live(false) must take effect"
        );
        assert!(!state.liveness().live);
        // Readiness is orthogonal to liveness.
        assert!(state.is_ready());
    }

    #[test]
    fn unsatisfy_on_undeclared_gate_is_a_no_op() {
        let mut state = HealthState::new();
        state.declare_gate(GateId::new("output"));
        state.satisfy(&GateId::new("output"));
        // Regressing a gate that does not exist must not affect anything.
        state.unsatisfy(&GateId::new("nope"));
        assert!(state.is_ready());
    }

    #[test]
    fn gate_id_roundtrips_through_string_forms() {
        let from_str: GateId = "ingest".into();
        let from_string: GateId = String::from("ingest").into();
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.as_str(), "ingest");
        assert_eq!(from_str.to_string(), "ingest");
    }

    #[test]
    fn liveness_snapshot_matches_is_live() {
        let mut state = HealthState::new();
        assert_eq!(state.liveness().live, state.is_live());
        state.set_live(false);
        assert_eq!(state.liveness().live, state.is_live());
    }
}
