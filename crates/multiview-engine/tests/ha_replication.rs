//! Tests for the M9 state-replication MODEL: the serializable engine-state
//! snapshot the active replicates to a standby, the deltas applied between
//! snapshots, and the standby-side applier that reconstructs an equal snapshot.
//! Round-trip (serde) + delta-apply + monotonic-version property tests. The
//! network transport itself is behind the off-by-default `cluster` feature and
//! is compile-only; this exercises the pure model. Pure-Rust default build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_engine::ha::repl::{
    ApplyError, EngineSnapshot, ReplicaApplier, ReplicationDelta, SnapshotVersion, TileBinding,
};
use proptest::prelude::*;

fn snap_v(v: u64) -> EngineSnapshot {
    EngineSnapshot {
        version: SnapshotVersion::new(v),
        active_layout: "grid-3x3".to_owned(),
        epoch: 1,
        tiles: vec![
            TileBinding {
                tile: 0,
                source: Some("cam-1".to_owned()),
            },
            TileBinding {
                tile: 1,
                source: None,
            },
        ],
    }
}

#[test]
fn snapshot_round_trips_through_json() {
    let snap = snap_v(7);
    let json = serde_json::to_string(&snap).unwrap();
    let back: EngineSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(snap, back);
}

#[test]
fn delta_round_trips_through_json() {
    let delta = ReplicationDelta::SourceRebound {
        from: SnapshotVersion::new(7),
        to: SnapshotVersion::new(8),
        tile: 0,
        source: Some("cam-2".to_owned()),
    };
    let json = serde_json::to_string(&delta).unwrap();
    let back: ReplicationDelta = serde_json::from_str(&json).unwrap();
    assert_eq!(delta, back);
}

#[test]
fn applier_accepts_baseline_snapshot() {
    let mut applier = ReplicaApplier::new();
    assert!(applier.current().is_none());
    applier.install_snapshot(snap_v(7)).unwrap();
    assert_eq!(applier.current().unwrap().version, SnapshotVersion::new(7));
}

#[test]
fn applier_applies_a_contiguous_delta() {
    let mut applier = ReplicaApplier::new();
    applier.install_snapshot(snap_v(7)).unwrap();
    applier
        .apply_delta(ReplicationDelta::SourceRebound {
            from: SnapshotVersion::new(7),
            to: SnapshotVersion::new(8),
            tile: 0,
            source: Some("cam-2".to_owned()),
        })
        .unwrap();
    let cur = applier.current().unwrap();
    assert_eq!(cur.version, SnapshotVersion::new(8));
    assert_eq!(cur.tiles[0].source.as_deref(), Some("cam-2"));
}

#[test]
fn applier_applies_a_layout_swap_delta() {
    let mut applier = ReplicaApplier::new();
    applier.install_snapshot(snap_v(7)).unwrap();
    applier
        .apply_delta(ReplicationDelta::LayoutSwap {
            from: SnapshotVersion::new(7),
            to: SnapshotVersion::new(8),
            layout: "pip".to_owned(),
        })
        .unwrap();
    let cur = applier.current().unwrap();
    assert_eq!(cur.active_layout, "pip");
    assert_eq!(cur.version, SnapshotVersion::new(8));
}

#[test]
fn applier_rejects_a_gapped_delta() {
    // A standby must never apply a delta whose `from` does not match its current
    // version — that would silently diverge the replicated state. It must reject
    // and request a fresh snapshot instead.
    let mut applier = ReplicaApplier::new();
    applier.install_snapshot(snap_v(7)).unwrap();
    let err = applier
        .apply_delta(ReplicationDelta::LayoutSwap {
            from: SnapshotVersion::new(9), // gap: expected 7
            to: SnapshotVersion::new(10),
            layout: "pip".to_owned(),
        })
        .unwrap_err();
    assert!(matches!(
        err,
        ApplyError::VersionGap {
            expected,
            got,
        } if expected == SnapshotVersion::new(7) && got == SnapshotVersion::new(9)
    ));
    // State is unchanged on rejection.
    assert_eq!(applier.current().unwrap().version, SnapshotVersion::new(7));
    assert_eq!(applier.current().unwrap().active_layout, "grid-3x3");
}

#[test]
fn applier_rejects_a_delta_before_any_snapshot() {
    let mut applier = ReplicaApplier::new();
    let err = applier
        .apply_delta(ReplicationDelta::LayoutSwap {
            from: SnapshotVersion::new(0),
            to: SnapshotVersion::new(1),
            layout: "pip".to_owned(),
        })
        .unwrap_err();
    assert!(matches!(err, ApplyError::NoBaseline));
}

#[test]
fn applier_rejects_non_increasing_delta() {
    let mut applier = ReplicaApplier::new();
    applier.install_snapshot(snap_v(7)).unwrap();
    let err = applier
        .apply_delta(ReplicationDelta::LayoutSwap {
            from: SnapshotVersion::new(7),
            to: SnapshotVersion::new(7), // not strictly increasing
            layout: "pip".to_owned(),
        })
        .unwrap_err();
    assert!(matches!(err, ApplyError::NonMonotonic { .. }));
}

#[test]
fn newer_snapshot_replaces_an_older_one() {
    let mut applier = ReplicaApplier::new();
    applier.install_snapshot(snap_v(7)).unwrap();
    applier.install_snapshot(snap_v(12)).unwrap();
    assert_eq!(applier.current().unwrap().version, SnapshotVersion::new(12));
}

#[test]
fn older_snapshot_is_rejected() {
    let mut applier = ReplicaApplier::new();
    applier.install_snapshot(snap_v(12)).unwrap();
    let err = applier.install_snapshot(snap_v(7)).unwrap_err();
    assert!(matches!(err, ApplyError::NonMonotonic { .. }));
    assert_eq!(applier.current().unwrap().version, SnapshotVersion::new(12));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Any snapshot round-trips losslessly through JSON.
    #[test]
    fn snapshot_json_round_trip(
        version in 0u64..1_000_000,
        epoch in 0u64..1000,
        layout in "[a-z0-9-]{1,16}",
        tiles in proptest::collection::vec(
            (0u32..64, proptest::option::of("[a-z0-9-]{1,12}")),
            0..16,
        ),
    ) {
        let snap = EngineSnapshot {
            version: SnapshotVersion::new(version),
            active_layout: layout,
            epoch,
            tiles: tiles
                .into_iter()
                .map(|(tile, source)| TileBinding { tile, source })
                .collect(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: EngineSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    /// Applying a strictly-increasing chain of contiguous deltas to a baseline
    /// snapshot always succeeds and the replica version advances monotonically to
    /// the final delta's `to`.
    #[test]
    fn contiguous_delta_chain_applies_monotonically(
        base in 0u64..1000,
        steps in 1u64..32,
    ) {
        let mut applier = ReplicaApplier::new();
        applier.install_snapshot(snap_v(base)).unwrap();
        let mut version = base;
        for i in 0..steps {
            let from = SnapshotVersion::new(version);
            version = version.saturating_add(1);
            let to = SnapshotVersion::new(version);
            // Alternate the two delta kinds.
            let delta = if i % 2 == 0 {
                ReplicationDelta::LayoutSwap { from, to, layout: format!("l{i}") }
            } else {
                ReplicationDelta::SourceRebound { from, to, tile: 0, source: Some(format!("s{i}")) }
            };
            applier.apply_delta(delta).unwrap();
            prop_assert_eq!(applier.current().unwrap().version, to);
        }
        prop_assert_eq!(
            applier.current().unwrap().version,
            SnapshotVersion::new(base.saturating_add(steps))
        );
    }

    /// A delta whose `from` does not match the current version is ALWAYS rejected
    /// and never mutates replica state (no silent divergence).
    #[test]
    fn gapped_delta_never_mutates(
        base in 1u64..1000,
        wrong_from in 0u64..2000,
    ) {
        prop_assume!(wrong_from != base);
        let mut applier = ReplicaApplier::new();
        applier.install_snapshot(snap_v(base)).unwrap();
        let before = applier.current().unwrap().clone();
        let res = applier.apply_delta(ReplicationDelta::LayoutSwap {
            from: SnapshotVersion::new(wrong_from),
            to: SnapshotVersion::new(wrong_from.saturating_add(1)),
            layout: "x".to_owned(),
        });
        prop_assert!(res.is_err());
        prop_assert_eq!(applier.current().unwrap(), &before);
    }
}
