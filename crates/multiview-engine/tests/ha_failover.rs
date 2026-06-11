//! Tests for the M9 failover **policy** (which instance drives output, with
//! make-before-break handover) and the **no-split-brain** guarantee under the
//! documented priority + epoch policy. Includes property tests: a heartbeat-miss
//! promotes exactly one standby, and failover preserves output continuity at the
//! model level (some node always drives output). Pure-Rust default build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_engine::ha::{
    Cluster, FailoverDecision, FailoverPolicy, HaNode, Heartbeat, HeartbeatConfig, NodeId,
    NodeRole, Priority,
};
use proptest::prelude::*;

fn at(ns: i64) -> MediaTime {
    MediaTime::from_nanos(ns)
}

fn cfg() -> HeartbeatConfig {
    HeartbeatConfig::new(1_000_000_000, 3).unwrap()
}

fn node(id: u32, prio: u32) -> HaNode {
    HaNode::new(NodeId::new(id), Priority::new(prio))
}

#[test]
fn policy_keeps_a_healthy_active_driving() {
    // Three-node cluster (1 active + 2 standby). Active is healthy -> nobody else
    // should drive output.
    let mut cluster = Cluster::new(
        node(1, 100),
        vec![node(2, 90), node(3, 80)],
        FailoverPolicy::default(),
        cfg(),
    );
    cluster.record_heartbeat(NodeId::new(1), at(0));
    // From the perspective of standby node 2, at t just after a heartbeat:
    let decision = cluster.evaluate(NodeId::new(2), at(500_000_000));
    assert_eq!(decision, FailoverDecision::Hold);
}

#[test]
fn highest_priority_live_standby_promotes_on_active_loss() {
    // Active (1) dies. Standbys 2 (prio 90) and 3 (prio 80) are both alive. The
    // policy elects the highest-priority live standby (node 2) to promote; node 3
    // holds.
    let policy = FailoverPolicy::default();
    let mut cluster = Cluster::new(node(1, 100), vec![node(2, 90), node(3, 80)], policy, cfg());
    // All heartbeat at t=0; then only the standbys keep beating.
    cluster.record_heartbeat(NodeId::new(1), at(0));
    cluster.record_heartbeat(NodeId::new(2), at(0));
    cluster.record_heartbeat(NodeId::new(3), at(0));
    cluster.record_heartbeat(NodeId::new(2), at(3_000_000_000));
    cluster.record_heartbeat(NodeId::new(3), at(3_000_000_000));

    // Active is now dead (silent since t=0, >3 intervals). Node 2 should promote.
    let now = at(3_500_000_000);
    assert_eq!(
        cluster.evaluate(NodeId::new(2), now),
        FailoverDecision::Promote
    );
    // Node 3 (lower priority) must hold — exactly one promotes.
    assert_eq!(
        cluster.evaluate(NodeId::new(3), now),
        FailoverDecision::Hold
    );
}

#[test]
fn dead_standby_is_skipped_in_election() {
    // Active (1) and the higher-priority standby (2) are both dead; only the
    // lower-priority standby (3) is alive. Node 3 must promote despite its lower
    // priority — a dead candidate cannot drive output.
    let mut cluster = Cluster::new(
        node(1, 100),
        vec![node(2, 90), node(3, 80)],
        FailoverPolicy::default(),
        cfg(),
    );
    cluster.record_heartbeat(NodeId::new(3), at(3_000_000_000));
    let now = at(3_500_000_000);
    assert_eq!(
        cluster.evaluate(NodeId::new(3), now),
        FailoverDecision::Promote
    );
}

#[test]
fn promoted_node_is_treated_as_active_by_peers() {
    // After node 2 promotes (records an Active heartbeat), peers see a live active
    // again and stop electing.
    let mut cluster = Cluster::new(
        node(1, 100),
        vec![node(2, 90), node(3, 80)],
        FailoverPolicy::default(),
        cfg(),
    );
    // Original active died; node 2 promoted and now beats as Active.
    cluster.observe(Heartbeat {
        from: NodeId::new(2),
        priority: Priority::new(90),
        role: NodeRole::Active,
        epoch: 1,
        drives_output: true,
        sent_at: at(4_000_000_000),
    });
    cluster.record_heartbeat(NodeId::new(3), at(4_000_000_000));
    // Node 3 now sees a healthy active (2) -> holds.
    assert_eq!(
        cluster.evaluate(NodeId::new(3), at(4_200_000_000)),
        FailoverDecision::Hold
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// No-split-brain: for any combination of which nodes are alive at evaluation
    /// time, AT MOST ONE node is told to Promote (and exactly one drives output
    /// overall — the live active, or the single elected promoter).
    #[test]
    fn at_most_one_node_promotes(
        // 4 nodes total: active id 1 + three standbys. `alive` flags choose which
        // ones have a fresh heartbeat. Distinct priorities avoid the id tie-break
        // ambiguity for this invariant (covered separately).
        active_alive in any::<bool>(),
        s2_alive in any::<bool>(),
        s3_alive in any::<bool>(),
        s4_alive in any::<bool>(),
    ) {
        let mut cluster = Cluster::new(
            node(1, 100),
            vec![node(2, 90), node(3, 80), node(4, 70)],
            FailoverPolicy::default(),
            cfg(),
        );
        let fresh = at(10_000_000_000);
        let now = at(10_500_000_000);
        if active_alive { cluster.record_heartbeat(NodeId::new(1), fresh); }
        if s2_alive { cluster.record_heartbeat(NodeId::new(2), fresh); }
        if s3_alive { cluster.record_heartbeat(NodeId::new(3), fresh); }
        if s4_alive { cluster.record_heartbeat(NodeId::new(4), fresh); }

        let ids = [NodeId::new(1), NodeId::new(2), NodeId::new(3), NodeId::new(4)];
        let promoters = ids
            .iter()
            .filter(|id| cluster.evaluate(**id, now) == FailoverDecision::Promote)
            .count();
        prop_assert!(promoters <= 1, "at most one node may promote, got {promoters}");

        // Output continuity (model level): if ANY node is alive, exactly one node
        // drives output — either the healthy active holds, or one standby promotes.
        let any_alive = active_alive || s2_alive || s3_alive || s4_alive;
        let drivers = if active_alive { 1 } else { promoters };
        if any_alive {
            prop_assert_eq!(drivers, 1, "some live node must drive output");
        }
    }

    /// Determinism / agreement: the same cluster health yields the SAME elected
    /// promoter regardless of which surviving node asks — every node computes the
    /// same winner, so they cannot disagree (the root of no-split-brain).
    #[test]
    fn all_survivors_agree_on_the_promoter(
        s2_alive in any::<bool>(),
        s3_alive in any::<bool>(),
        s4_alive in any::<bool>(),
    ) {
        let mut cluster = Cluster::new(
            node(1, 100),
            vec![node(2, 90), node(3, 80), node(4, 70)],
            FailoverPolicy::default(),
            cfg(),
        );
        let fresh = at(10_000_000_000);
        let now = at(10_500_000_000);
        // Active is dead (no heartbeat). Standbys per flags.
        if s2_alive { cluster.record_heartbeat(NodeId::new(2), fresh); }
        if s3_alive { cluster.record_heartbeat(NodeId::new(3), fresh); }
        if s4_alive { cluster.record_heartbeat(NodeId::new(4), fresh); }

        // The cluster exposes the single elected promoter directly; each live
        // standby's local decision must match it.
        let elected = cluster.elected_promoter(now);
        for (alive, id) in [(s2_alive, 2u32), (s3_alive, 3), (s4_alive, 4)] {
            if alive {
                let promotes = cluster.evaluate(NodeId::new(id), now) == FailoverDecision::Promote;
                prop_assert_eq!(promotes, elected == Some(NodeId::new(id)));
            }
        }
    }
}
