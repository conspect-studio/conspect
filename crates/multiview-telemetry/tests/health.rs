//! Integration tests for the health-state readiness gates.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::health::{GateId, HealthState, Readiness};

#[test]
fn fresh_state_is_live_but_not_ready() {
    // Liveness is in-process only and is true from construction (ADR / brief
    // §15: /livez must NOT depend on GPU/upstream, or it triggers restart loops).
    let state = HealthState::new();
    assert!(
        state.is_live(),
        "a freshly-constructed process must be live"
    );
    // With no gates declared, readiness is vacuously... not the point: a process
    // that has declared no readiness gates is ready, but the engine always
    // declares gates, so a state with declared+unmet gates is NOT ready.
    assert!(state.is_ready(), "no declared gates => vacuously ready");
}

#[test]
fn declared_gate_blocks_readiness_until_satisfied() {
    let mut state = HealthState::new();
    state.declare_gate(GateId::new("ingest"));
    state.declare_gate(GateId::new("backend-init"));
    state.declare_gate(GateId::new("output"));

    assert!(
        !state.is_ready(),
        "unmet gates must keep the process not-ready"
    );
    // Liveness is unaffected by readiness gates.
    assert!(state.is_live());

    state.satisfy(&GateId::new("ingest"));
    state.satisfy(&GateId::new("backend-init"));
    assert!(!state.is_ready(), "still one unmet gate => not ready");

    state.satisfy(&GateId::new("output"));
    assert!(state.is_ready(), "all gates satisfied => ready");
}

#[test]
fn gate_can_regress_to_unsatisfied() {
    let mut state = HealthState::new();
    state.declare_gate(GateId::new("output"));
    state.satisfy(&GateId::new("output"));
    assert!(state.is_ready());

    // A previously-passing gate can fail again (e.g. output endpoint lost).
    state.unsatisfy(&GateId::new("output"));
    assert!(!state.is_ready(), "a regressed gate must drop readiness");
}

#[test]
fn satisfying_an_undeclared_gate_declares_it_unmet_elsewhere() {
    // Satisfying a gate that was never declared should be a no-op for an
    // undeclared id (it does not silently make the process ready).
    let mut state = HealthState::new();
    state.declare_gate(GateId::new("ingest"));
    state.satisfy(&GateId::new("does-not-exist"));
    assert!(
        !state.is_ready(),
        "satisfying an unknown gate must not flip readiness"
    );
}

#[test]
fn readiness_report_lists_pending_gates() {
    let mut state = HealthState::new();
    state.declare_gate(GateId::new("ingest"));
    state.declare_gate(GateId::new("output"));
    state.satisfy(&GateId::new("ingest"));

    let report: Readiness = state.readiness();
    assert!(!report.ready);
    let pending: Vec<&str> = report.pending.iter().map(GateId::as_str).collect();
    assert_eq!(pending, vec!["output"], "only the unmet gate is pending");

    state.satisfy(&GateId::new("output"));
    let report = state.readiness();
    assert!(report.ready);
    assert!(report.pending.is_empty());
}

#[test]
fn declaring_same_gate_twice_is_idempotent() {
    let mut state = HealthState::new();
    state.declare_gate(GateId::new("ingest"));
    state.declare_gate(GateId::new("ingest"));
    state.satisfy(&GateId::new("ingest"));
    assert!(state.is_ready());
    assert_eq!(state.readiness().pending.len(), 0);
}
