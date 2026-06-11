//! Salvo engine tests (ADR-MV001): arm → take returns the whole atomic batch
//! exactly once (all-or-nothing), take is idempotent and never double-applies,
//! cancel discards an armed salvo, and a not-armed take applies nothing — a pure
//! value machine, never blocking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::tally::TallyColor;
use multiview_engine::salvo::{Salvo, SalvoChange, SalvoPhase};
use proptest::prelude::*;

fn sample_changes() -> Vec<SalvoChange> {
    vec![
        SalvoChange::Layout {
            head: "main".to_owned(),
            layout: "grid-9".to_owned(),
        },
        SalvoChange::SourceBind {
            tile: 0,
            source: Some("cam-1".to_owned()),
        },
        SalvoChange::Tally {
            tile: 0,
            color: TallyColor::Red,
        },
        SalvoChange::Umd {
            tile: 0,
            text: "CAM 1".to_owned(),
        },
    ]
}

#[test]
fn arm_then_take_returns_whole_batch_once() {
    let mut salvo = Salvo::new("preset-a", sample_changes());
    assert_eq!(salvo.phase(), SalvoPhase::Idle);

    // Take before arm is a no-op (applies nothing).
    assert!(salvo.take().is_none());
    assert_eq!(salvo.phase(), SalvoPhase::Idle);

    // Arm, then take returns the full batch.
    assert!(salvo.arm());
    assert!(salvo.is_armed());
    let batch = salvo.take().expect("armed take returns a batch");
    assert_eq!(batch.name, "preset-a");
    assert_eq!(batch.changes, sample_changes());
    assert_eq!(batch.len(), 4);
    assert_eq!(salvo.phase(), SalvoPhase::Taken);
}

#[test]
fn take_is_idempotent_no_double_apply() {
    let mut salvo = Salvo::new("preset-a", sample_changes());
    salvo.arm();
    assert!(salvo.take().is_some());
    // A second take of the same armed-then-taken salvo applies nothing.
    assert!(salvo.take().is_none());
    assert!(salvo.take().is_none());
    assert_eq!(salvo.phase(), SalvoPhase::Taken);
}

#[test]
fn cancel_discards_armed_salvo() {
    let mut salvo = Salvo::new("preset-a", sample_changes());
    salvo.arm();
    assert!(salvo.cancel());
    assert_eq!(salvo.phase(), SalvoPhase::Idle);
    // After cancel, a take applies nothing.
    assert!(salvo.take().is_none());
    // Cancel when not armed is a no-op.
    assert!(!salvo.cancel());
}

#[test]
fn arm_is_idempotent() {
    let mut salvo = Salvo::new("preset-a", sample_changes());
    assert!(salvo.arm()); // first arm changes phase
    assert!(!salvo.arm()); // already armed -> no change
    assert!(salvo.is_armed());
}

#[test]
fn rearm_after_take_allows_another_take() {
    let mut salvo = Salvo::new("preset-a", sample_changes());
    salvo.arm();
    assert!(salvo.take().is_some());
    // Re-arm a taken salvo and take again.
    assert!(salvo.arm());
    assert!(salvo.take().is_some());
}

#[test]
fn empty_salvo_batch_is_empty() {
    let mut salvo = Salvo::new("noop", Vec::new());
    salvo.arm();
    let batch = salvo.take().unwrap();
    assert!(batch.is_empty());
}

// ---------- property tests ----------

/// A symbolic operation applied to a salvo.
#[derive(Debug, Clone, Copy)]
enum Op {
    Arm,
    Cancel,
    Take,
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::Arm), Just(Op::Cancel), Just(Op::Take)]
}

proptest! {
    /// Across any sequence of arm/cancel/take, a take returns a batch IFF the
    /// salvo was armed at that instant (all-or-nothing), and every returned batch
    /// is the salvo's complete, unmodified change set (atomic). The taken-count
    /// never exceeds the armed-then-not-cancelled count (no double-apply).
    #[test]
    fn prop_take_is_all_or_nothing_and_atomic(ops in prop::collection::vec(arb_op(), 0..40)) {
        let changes = sample_changes();
        let mut salvo = Salvo::new("p", changes.clone());
        for op in ops {
            match op {
                Op::Arm => { salvo.arm(); }
                Op::Cancel => { salvo.cancel(); }
                Op::Take => {
                    let armed = salvo.is_armed();
                    let result = salvo.take();
                    // All-or-nothing: a batch is returned exactly when armed.
                    prop_assert_eq!(result.is_some(), armed);
                    if let Some(batch) = result {
                        // Atomic + unmodified: the whole declared change set.
                        prop_assert_eq!(&batch.changes, &changes);
                        prop_assert_eq!(salvo.phase(), SalvoPhase::Taken);
                    }
                }
            }
        }
    }

    /// The phase is always one of the three lifecycle states and `is_armed`
    /// agrees with the phase.
    #[test]
    fn prop_phase_invariants(ops in prop::collection::vec(arb_op(), 0..40)) {
        let mut salvo = Salvo::new("p", sample_changes());
        for op in ops {
            match op {
                Op::Arm => { salvo.arm(); }
                Op::Cancel => { salvo.cancel(); }
                Op::Take => { salvo.take(); }
            }
            let phase = salvo.phase();
            prop_assert!(matches!(phase, SalvoPhase::Idle | SalvoPhase::Armed | SalvoPhase::Taken));
            prop_assert_eq!(salvo.is_armed(), matches!(phase, SalvoPhase::Armed));
            // Changes are immutable regardless of lifecycle.
            let expected = sample_changes();
            prop_assert_eq!(salvo.changes(), expected.as_slice());
        }
    }
}
