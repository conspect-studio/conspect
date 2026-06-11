//! Tests for the M9 high-availability heartbeat health-check state machine and
//! the failover policy: peers exchange heartbeats over an injected `MediaTime`;
//! on a miss-threshold the standby promotes exactly one node; the documented
//! priority + epoch policy admits no split-brain; failover preserves output
//! continuity at the model level (make-before-break). Pure-Rust default build
//! (no cluster transport).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_engine::ha::{
    HaNode, HaStateMachine, Heartbeat, HeartbeatConfig, NodeId, NodeRole, PeerHealth, Priority,
};

/// An active-peer heartbeat from `id` (priority `prio`) at the given epoch/time.
fn active_hb(id: u32, prio: u32, epoch: u64, sent_ns: i64) -> Heartbeat {
    Heartbeat {
        from: NodeId::new(id),
        priority: Priority::new(prio),
        role: NodeRole::Active,
        epoch,
        drives_output: true,
        sent_at: at(sent_ns),
    }
}

fn at(ns: i64) -> MediaTime {
    MediaTime::from_nanos(ns)
}

/// 1 s heartbeat interval, declare a peer dead after 3 consecutive misses.
fn cfg() -> HeartbeatConfig {
    HeartbeatConfig::new(1_000_000_000, 3).unwrap()
}

fn standby(id: u32, prio: u32) -> HaStateMachine {
    HaStateMachine::new(
        HaNode::new(NodeId::new(id), Priority::new(prio)),
        NodeRole::Standby,
        cfg(),
    )
}

fn active(id: u32, prio: u32) -> HaStateMachine {
    HaStateMachine::new(
        HaNode::new(NodeId::new(id), Priority::new(prio)),
        NodeRole::Active,
        cfg(),
    )
}

#[test]
fn fresh_standby_does_not_drive_output() {
    let sm = standby(2, 100);
    assert_eq!(sm.role(), NodeRole::Standby);
    assert!(!sm.drives_output());
    assert_eq!(sm.epoch(), 0);
}

#[test]
fn fresh_active_drives_output_immediately() {
    let sm = active(1, 100);
    assert_eq!(sm.role(), NodeRole::Active);
    assert!(sm.drives_output());
}

#[test]
fn standby_promotes_after_miss_threshold() {
    // Standby (id 2) watching an active peer (id 1). The active heartbeats once,
    // then goes silent; after `miss_threshold` intervals elapse with no fresh
    // heartbeat, the standby promotes itself.
    let mut sm = standby(2, 100);

    // First heartbeat from the active at t=0.
    sm.observe_heartbeat(active_hb(1, 100, 0, 0));
    sm.tick(at(0));
    assert!(
        !sm.drives_output(),
        "must not promote while active is healthy"
    );
    assert_eq!(sm.role(), NodeRole::Standby);

    // 2 s later (2 intervals): below the 3-miss threshold -> still standby.
    sm.tick(at(2_000_000_000));
    assert_eq!(sm.role(), NodeRole::Standby);

    // 3 s later (>= 3 intervals of silence): promote.
    let promo = sm.tick(at(3_000_000_001));
    assert!(promo, "tick at miss-threshold must report a promotion");
    assert_eq!(sm.role(), NodeRole::Active);
    assert!(
        sm.drives_output(),
        "make-before-break: new active drives output"
    );
    assert_eq!(sm.epoch(), 1, "promotion bumps the epoch exactly once");
}

#[test]
fn fresh_heartbeat_resets_the_miss_counter() {
    let mut sm = standby(2, 100);
    sm.observe_heartbeat(active_hb(1, 100, 0, 0));
    // Almost at the threshold...
    sm.tick(at(2_500_000_000));
    assert_eq!(sm.role(), NodeRole::Standby);
    // ...but a fresh heartbeat arrives, resetting the deadline.
    sm.observe_heartbeat(active_hb(1, 100, 0, 2_500_000_000));
    // Even well past the original deadline, the refreshed peer keeps us standby.
    sm.tick(at(4_000_000_000));
    assert_eq!(sm.role(), NodeRole::Standby);
    assert!(!sm.drives_output());
}

#[test]
fn promotion_is_idempotent_across_ticks() {
    let mut sm = standby(2, 100);
    sm.observe_heartbeat(active_hb(1, 100, 0, 0));
    let first = sm.tick(at(5_000_000_000));
    assert!(first);
    assert_eq!(sm.epoch(), 1);
    // A second tick after promotion does not promote again / bump the epoch.
    let second = sm.tick(at(6_000_000_000));
    assert!(!second);
    assert_eq!(sm.epoch(), 1, "epoch only bumps on the promotion edge");
    assert_eq!(sm.role(), NodeRole::Active);
}

#[test]
fn standby_with_no_active_peer_promotes_on_its_own_deadline() {
    // A standby that never hears any active at all (cold start where the active
    // was already dead) promotes once its own start deadline elapses.
    let mut sm = standby(2, 100);
    assert_eq!(sm.role(), NodeRole::Standby);
    sm.tick(at(500_000_000));
    assert_eq!(sm.role(), NodeRole::Standby);
    let promo = sm.tick(at(3_000_000_001));
    assert!(promo);
    assert_eq!(sm.role(), NodeRole::Active);
}

#[test]
fn higher_epoch_active_demotes_a_self_promoted_node() {
    // Anti-split-brain: if a node self-promoted (epoch 1) but then hears a peer
    // claiming Active with a HIGHER epoch, it must yield (demote) — the higher
    // epoch is authoritative.
    let mut sm = standby(2, 100);
    sm.observe_heartbeat(active_hb(1, 100, 0, 0));
    assert!(sm.tick(at(5_000_000_000)));
    assert_eq!(sm.role(), NodeRole::Active);
    assert_eq!(sm.epoch(), 1);

    // A peer comes back online claiming Active at epoch 2 (higher): we yield.
    sm.observe_heartbeat(active_hb(1, 100, 2, 5_500_000_000));
    assert_eq!(
        sm.role(),
        NodeRole::Standby,
        "yield to a higher-epoch active"
    );
    assert!(!sm.drives_output());
}

#[test]
fn lower_epoch_active_does_not_demote_us() {
    // The mirror of the above: a stale peer claiming Active at a LOWER epoch is a
    // zombie and must NOT cause us to yield.
    let mut sm = standby(2, 100);
    sm.observe_heartbeat(active_hb(1, 100, 0, 0));
    assert!(sm.tick(at(5_000_000_000)));
    assert_eq!(sm.epoch(), 1);

    // Stale active at epoch 0 (our promotion superseded it): ignore.
    sm.observe_heartbeat(active_hb(1, 100, 0, 5_500_000_000));
    assert_eq!(
        sm.role(),
        NodeRole::Active,
        "ignore a stale lower-epoch active"
    );
    assert!(sm.drives_output());
}

#[test]
fn equal_epoch_tie_broken_by_priority_then_id() {
    // Two nodes promote on the same epoch (a partition healed). The documented
    // tie-break: higher Priority wins; equal priority -> lower NodeId wins. The
    // loser yields when it hears the winner at the same epoch.
    //
    // Node 2 (priority 100) self-promotes to epoch 1.
    let mut loser = standby(2, 100);
    loser.observe_heartbeat(active_hb(1, 100, 0, 0));
    assert!(loser.tick(at(5_000_000_000)));
    assert_eq!(loser.epoch(), 1);
    assert_eq!(loser.role(), NodeRole::Active);

    // It then hears node 3 (priority 200 — higher) also Active at epoch 1.
    loser.observe_heartbeat(active_hb(3, 200, 1, 5_500_000_000));
    assert_eq!(
        loser.role(),
        NodeRole::Standby,
        "equal epoch, lower priority -> yield"
    );

    // Conversely, the higher-priority node (3) keeps Active when it hears the
    // lower-priority peer at the same epoch.
    let mut winner = standby(3, 200);
    winner.observe_heartbeat(active_hb(1, 100, 0, 0));
    assert!(winner.tick(at(5_000_000_000)));
    assert_eq!(winner.epoch(), 1);
    winner.observe_heartbeat(active_hb(2, 100, 1, 5_500_000_000));
    assert_eq!(
        winner.role(),
        NodeRole::Active,
        "equal epoch, higher priority -> keep"
    );
}

#[test]
fn peer_health_tracks_liveness_against_deadline() {
    let mut health = PeerHealth::new(NodeId::new(1), cfg());
    assert!(!health.is_alive(at(0)), "no heartbeat yet -> not alive");
    health.record(at(1_000_000_000));
    assert!(health.is_alive(at(1_500_000_000)));
    // 3 intervals after the last heartbeat -> dead.
    assert!(!health.is_alive(at(4_000_000_001)));
}

#[test]
fn heartbeat_config_rejects_degenerate_values() {
    assert!(
        HeartbeatConfig::new(0, 3).is_err(),
        "zero interval is invalid"
    );
    assert!(
        HeartbeatConfig::new(-5, 3).is_err(),
        "negative interval is invalid"
    );
    assert!(
        HeartbeatConfig::new(1_000_000, 0).is_err(),
        "zero miss threshold"
    );
}
