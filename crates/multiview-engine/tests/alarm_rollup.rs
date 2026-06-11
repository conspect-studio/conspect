//! Severity roll-up (max) and Boolean virtual-alarm tests (ADR-MV001).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::PerceivedSeverity;
use multiview_engine::alarm::rollup::{BoolOp, RollupNode, VirtualAlarm};
use proptest::prelude::*;

use PerceivedSeverity::{Cleared, Critical, Major, Minor, Warning};

#[test]
fn leaf_rolls_up_to_own_severity() {
    let leaf = RollupNode::leaf("probe-a", Major);
    assert_eq!(leaf.rolled_up(), Major);
}

#[test]
fn parent_is_max_over_children() {
    // tile = max(probe black=Minor, probe freeze=Major) = Major
    let tile = RollupNode::group(
        "tile-0",
        vec![
            RollupNode::leaf("black", Minor),
            RollupNode::leaf("freeze", Major),
        ],
    );
    assert_eq!(tile.rolled_up(), Major);

    // system = max(tile-0=Major, tile-1 with Critical) = Critical
    let system = RollupNode::group(
        "system",
        vec![
            tile,
            RollupNode::group("tile-1", vec![RollupNode::leaf("silence", Critical)]),
        ],
    );
    assert_eq!(system.rolled_up(), Critical);
}

#[test]
fn empty_group_rolls_up_to_cleared() {
    let g = RollupNode::group("empty", vec![]);
    assert_eq!(g.rolled_up(), Cleared);
}

#[test]
fn own_severity_can_dominate_children() {
    let node = RollupNode::leaf("x", Critical).with_child(RollupNode::leaf("y", Warning));
    assert_eq!(node.rolled_up(), Critical);
}

#[test]
fn bool_op_and_requires_all_active() {
    assert!(BoolOp::And.evaluate([true, true, true]));
    assert!(!BoolOp::And.evaluate([true, false, true]));
    // Empty AND is false: nothing is "all active".
    assert!(!BoolOp::And.evaluate([]));
}

#[test]
fn bool_op_or_requires_any_active() {
    assert!(BoolOp::Or.evaluate([false, false, true]));
    assert!(!BoolOp::Or.evaluate([false, false, false]));
    assert!(!BoolOp::Or.evaluate([]));
}

#[test]
fn bool_op_xor_is_parity() {
    assert!(BoolOp::Xor.evaluate([true, false, false])); // 1 active -> odd
    assert!(!BoolOp::Xor.evaluate([true, true, false])); // 2 active -> even
    assert!(BoolOp::Xor.evaluate([true, true, true])); // 3 active -> odd
    assert!(!BoolOp::Xor.evaluate([]));
}

#[test]
fn virtual_alarm_fires_and_reports_configured_severity() {
    let va = VirtualAlarm::new(
        "all-feeds-black",
        BoolOp::And,
        vec!["a".into(), "b".into(), "c".into()],
        Critical,
    );
    assert!(va.evaluate([true, true, true]));
    assert_eq!(va.severity_for([true, true, true]), Critical);
    assert_eq!(va.severity_for([true, false, true]), Cleared);
}

#[test]
fn virtual_alarm_missing_inputs_treated_inactive() {
    let va = VirtualAlarm::new("any", BoolOp::Or, vec!["a".into(), "b".into()], Warning);
    // Fewer active values than inputs: the missing one is inactive.
    assert!(!va.evaluate([false]));
    assert!(va.evaluate([false, true]));
}

// Roll-up equals the maximum severity present anywhere in the tree.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn rollup_equals_global_max(sevs in proptest::collection::vec(0_u8..6, 0..12)) {
        let to_sev = |n: u8| match n {
            0 => Cleared,
            1 => PerceivedSeverity::Indeterminate,
            2 => Warning,
            3 => Minor,
            4 => Major,
            _ => Critical,
        };
        let children: Vec<RollupNode> = sevs
            .iter()
            .enumerate()
            .map(|(i, &n)| RollupNode::leaf(format!("p{i}"), to_sev(n)))
            .collect();
        let expected = sevs.iter().map(|&n| to_sev(n)).max().unwrap_or(Cleared);
        let system = RollupNode::group("system", children);
        prop_assert_eq!(system.rolled_up(), expected);
    }

    #[test]
    fn xor_matches_count_parity(flags in proptest::collection::vec(any::<bool>(), 0..10)) {
        let active = flags.iter().filter(|&&b| b).count();
        prop_assert_eq!(BoolOp::Xor.evaluate(flags.clone()), active % 2 == 1);
        prop_assert_eq!(BoolOp::Or.evaluate(flags.clone()), active >= 1);
        prop_assert_eq!(BoolOp::And.evaluate(flags.clone()), !flags.is_empty() && active == flags.len());
    }
}
