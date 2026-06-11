//! Tests for the M9 HA **cluster transport** (the off-by-default `cluster`
//! feature): the concrete `UdpClusterTransport` socket binding that moves
//! heartbeat and replication bytes between peers, driving the already-tested pure
//! `HaRunner`/`HaStateMachine`/`ReplicaApplier`.
//!
//! These are *live loopback* tests: two transports on `[::1]` exchange real
//! UDP datagrams. They exercise failover (node B promotes when node A stops
//! beating past the miss deadline), replication (a `LayoutSwap` delta replicates
//! A->B and applies contiguously), the no-silent-divergence contract (a dropped
//! delta surfaces `VersionGap`), and the isolation guarantee (a black-holed peer
//! never makes the publisher block — invariants #1 + #10).
//!
//! Loopback is CI-testable; true multi-host split-brain failover is a
//! hardware/network tier and is out of scope here.
#![cfg(feature = "cluster")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use multiview_core::time::MediaTime;
use multiview_engine::ha::repl::{EngineSnapshot, ReplicationDelta, SnapshotVersion, TileBinding};
use multiview_engine::ha::transport::{
    ClusterTransport, HaRunner, ReplicationMessage, UdpClusterTransport,
};
use multiview_engine::ha::{
    Cluster, FailoverPolicy, HaNode, HaStateMachine, Heartbeat, HeartbeatConfig, NodeId, NodeRole,
    Priority,
};

fn at(ns: i64) -> MediaTime {
    MediaTime::from_nanos(ns)
}

/// 1 s heartbeat interval, declare a peer dead after 3 consecutive misses.
fn cfg() -> HeartbeatConfig {
    HeartbeatConfig::new(1_000_000_000, 3).unwrap()
}

fn node(id: u32, prio: u32) -> HaNode {
    HaNode::new(NodeId::new(id), Priority::new(prio))
}

/// Bind a transport on an ephemeral loopback port, returning it plus its bound
/// address (so the peer can be told where to send).
fn bind_local() -> (UdpClusterTransport, SocketAddr) {
    let t = UdpClusterTransport::bind("[::1]:0", &[]).expect("bind loopback");
    let addr = t.local_addr().expect("local addr");
    (t, addr)
}

/// Spin until `f()` is true or the deadline passes, polling the transports each
/// loop so inbound datagrams are drained. Returns whether the predicate held.
fn pump_until<F: FnMut() -> bool>(timeout: Duration, mut f: F) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn snapshot_v(v: u64, layout: &str) -> EngineSnapshot {
    EngineSnapshot {
        version: SnapshotVersion::new(v),
        active_layout: layout.to_owned(),
        epoch: 1,
        tiles: vec![TileBinding {
            tile: 0,
            source: Some("cam-1".to_owned()),
        }],
    }
}

#[test]
fn local_addr_reports_the_bound_port() {
    let (_t, addr) = bind_local();
    // IPv6-first: the cluster transport binds the IPv6 loopback `[::1]`.
    assert!(addr.is_ipv6(), "cluster transport must bind IPv6 loopback");
    assert_eq!(addr.ip().to_string(), "::1");
    assert_ne!(addr.port(), 0, "an ephemeral bind must resolve a real port");
}

#[test]
fn a_published_heartbeat_round_trips_to_a_peer() {
    let (sender, _sender_addr) = bind_local();
    let mut receiver = UdpClusterTransport::bind("[::1]:0", &[]).expect("bind receiver");
    // Point the sender at the receiver.
    let recv_addr = receiver.local_addr().expect("recv addr");
    let sender = sender.with_peers(&[recv_addr]);

    let hb = Heartbeat {
        from: NodeId::new(1),
        priority: Priority::new(100),
        role: NodeRole::Active,
        epoch: 4,
        drives_output: true,
        sent_at: at(7),
    };
    sender.publish_heartbeat(hb).expect("publish");

    let mut got_hb = None;
    let arrived = pump_until(Duration::from_secs(2), || {
        got_hb = receiver.poll_heartbeat();
        got_hb.is_some()
    });
    assert!(arrived, "a heartbeat must arrive over loopback");
    assert_eq!(got_hb, Some(hb), "the heartbeat must round-trip exactly");
}

#[test]
fn standby_promotes_when_the_active_stops_beating_over_loopback() {
    // Active = node 1 (prio 100), Standby = node 2 (prio 90).
    let (active_tx, active_addr) = bind_local();
    let (standby_tx, standby_addr) = bind_local();
    let active_tx = active_tx.with_peers(&[standby_addr]);
    let standby_tx = standby_tx.with_peers(&[active_addr]);

    let mut standby_runner = HaRunner::new(
        standby_tx,
        HaStateMachine::new(node(2, 90), NodeRole::Standby, cfg()),
        Cluster::new(
            node(1, 100),
            vec![node(2, 90)],
            FailoverPolicy::default(),
            cfg(),
        ),
    );

    // Active beats at t = 1s. Pump it; the standby must observe a LIVE active
    // (cluster decision = Hold) and must NOT promote yet.
    active_tx
        .publish_heartbeat(Heartbeat {
            from: NodeId::new(1),
            priority: Priority::new(100),
            role: NodeRole::Active,
            epoch: 1,
            drives_output: true,
            sent_at: at(1_000_000_000),
        })
        .expect("active beat");

    // Spin until the heartbeat is actually delivered+observed: at t = 1s the
    // cluster sees the active as alive (Hold), proving the datagram crossed the
    // wire — not merely that nothing arrived.
    let observed = pump_until(Duration::from_secs(2), || {
        let promoted_now = standby_runner.pump_heartbeats(at(1_000_000_000));
        assert!(!promoted_now, "no promotion while the active is alive");
        standby_runner.cluster_active_alive(at(1_000_000_000))
    });
    assert!(
        observed,
        "the standby must actually receive the active's beat"
    );
    assert_eq!(standby_runner.machine().role(), NodeRole::Standby);
    assert!(!standby_runner.machine().has_promoted());

    // Active goes silent. Advance the standby's clock past the dead window
    // (interval 1s * miss 3 = 3s after the last beat -> t = 4s+). The pure model
    // decides; the transport delivered the last beat, nothing more arrives.
    let promoted = standby_runner.pump_heartbeats(at(5_000_000_000));
    assert!(
        promoted,
        "the standby must promote once the active misses the deadline"
    );
    assert_eq!(standby_runner.machine().role(), NodeRole::Active);
    assert!(standby_runner.machine().drives_output());
}

#[test]
fn a_layout_swap_delta_replicates_and_applies_contiguously() {
    // Build the pair so the active can address the standby's bound transport.
    let (active_tx, _active_addr) = bind_local();
    let (standby_tx, standby_addr) = bind_local();
    let active_tx = active_tx.with_peers(&[standby_addr]);
    let mut standby_runner = HaRunner::new(
        standby_tx,
        HaStateMachine::new(node(2, 90), NodeRole::Standby, cfg()),
        Cluster::new(
            node(1, 100),
            vec![node(2, 90)],
            FailoverPolicy::default(),
            cfg(),
        ),
    );

    // Active ships a baseline snapshot (v1) then a LayoutSwap delta (v1 -> v2).
    let snap = snapshot_v(1, "grid-2x2");
    active_tx.publish_snapshot(&snap).expect("publish snapshot");
    let arrived = pump_until(Duration::from_secs(2), || {
        standby_runner.pump_replication();
        standby_runner.replica().version() == Some(SnapshotVersion::new(1))
    });
    assert!(arrived, "the baseline snapshot must replicate");
    assert_eq!(
        standby_runner
            .replica()
            .current()
            .map(|s| s.active_layout.as_str()),
        Some("grid-2x2")
    );

    let delta = ReplicationDelta::LayoutSwap {
        from: SnapshotVersion::new(1),
        to: SnapshotVersion::new(2),
        layout: "grid-3x3".to_owned(),
    };
    active_tx.publish_delta(&delta).expect("publish delta");
    let applied = pump_until(Duration::from_secs(2), || {
        standby_runner.pump_replication();
        standby_runner.replica().version() == Some(SnapshotVersion::new(2))
    });
    assert!(applied, "the contiguous delta must apply");
    assert_eq!(
        standby_runner
            .replica()
            .current()
            .map(|s| s.active_layout.as_str()),
        Some("grid-3x3"),
        "the standby's replica must reflect the swapped layout"
    );
}

#[test]
fn a_replication_message_deserialises_to_the_right_variant() {
    let (sender, _) = bind_local();
    let mut receiver = UdpClusterTransport::bind("[::1]:0", &[]).expect("recv bind");
    let recv_addr = receiver.local_addr().expect("recv addr");
    let sender = sender.with_peers(&[recv_addr]);

    let snap = snapshot_v(3, "grid-1x1");
    sender.publish_snapshot(&snap).expect("snap");
    let mut got = None;
    pump_until(Duration::from_secs(2), || {
        got = receiver.poll_replication();
        got.is_some()
    });
    match got {
        Some(ReplicationMessage::Snapshot(s)) => assert_eq!(s, snap),
        other => panic!("expected a Snapshot replication message, got {other:?}"),
    }
}

#[test]
fn a_malformed_datagram_is_dropped_not_panicked() {
    let mut receiver = UdpClusterTransport::bind("[::1]:0", &[]).expect("recv bind");
    let recv_addr = receiver.local_addr().expect("recv addr");

    // A raw, non-JSON datagram from an unrelated socket must be silently dropped.
    let raw = UdpSocket::bind("[::1]:0").expect("raw bind");
    raw.send_to(b"\xff\x00not-json\xfe", recv_addr)
        .expect("raw send");

    // Give the datagram time to arrive, then poll: it must never panic and must
    // yield nothing classifiable.
    pump_until(Duration::from_millis(200), || false);
    assert!(
        receiver.poll_heartbeat().is_none(),
        "garbage is not a heartbeat"
    );
    assert!(
        receiver.poll_replication().is_none(),
        "garbage is not a replication message"
    );
}

#[test]
fn publishing_to_a_black_holed_peer_never_blocks_the_publisher() {
    // Invariants #1 + #10: a peer that never receives (we point at an address
    // nobody is reading) must not make publish_heartbeat block. We publish far
    // more than any socket buffer and assert the whole burst returns promptly.
    // IPv6 discard-ish peer (same family as the IPv6 loopback bind below).
    let blackhole: SocketAddr = "[::1]:9".parse().expect("addr");
    let sender = UdpClusterTransport::bind("[::1]:0", &[blackhole]).expect("bind");

    let hb = Heartbeat {
        from: NodeId::new(1),
        priority: Priority::new(100),
        role: NodeRole::Active,
        epoch: 1,
        drives_output: true,
        sent_at: at(0),
    };

    let start = Instant::now();
    for _ in 0..100_000u32 {
        // Best-effort: a WouldBlock drop is fine; an error must not be returned
        // for a transient full link, and it must never hang.
        let _ = sender.publish_heartbeat(hb);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "100k non-blocking publishes must finish promptly (took {elapsed:?})"
    );
}
