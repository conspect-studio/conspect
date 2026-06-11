#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Snapshot-then-delta ordering tests (ADR-RT003): snapshot establishes the
//! per-topic baseline, deltas must strictly advance the `seq`, gaps are
//! detected, and `$resync` rebuilds (resets) the baseline.

use multiview_events::ordering::Accepted;
use multiview_events::{Error, FrameKind, Seq, Topic, TopicCursor};
use proptest::prelude::*;

#[test]
fn delta_before_snapshot_is_rejected() {
    let mut cur = TopicCursor::new(Topic::Tiles);
    let err = cur
        .accept(FrameKind::Delta, Seq::new(1))
        .expect_err("a delta with no prior snapshot must be rejected");
    match err {
        Error::NonMonotonic { topic, .. } => assert_eq!(topic, "tiles"),
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(
        cur.last_seq(),
        None,
        "rejected delta must not move the cursor"
    );
}

#[test]
fn snapshot_then_ordered_deltas_accepted() {
    let mut cur = TopicCursor::new(Topic::Tiles);
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(43)).unwrap(),
        Accepted::SnapshotBaseline { seq: Seq::new(43) }
    );
    assert_eq!(cur.last_seq(), Some(Seq::new(43)));
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(44)).unwrap(),
        Accepted::Delta { seq: Seq::new(44) }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(45)).unwrap(),
        Accepted::Delta { seq: Seq::new(45) }
    );
    assert_eq!(cur.last_seq(), Some(Seq::new(45)));
}

#[test]
fn out_of_order_or_duplicate_delta_is_rejected() {
    let mut cur = TopicCursor::new(Topic::Outputs);
    cur.accept(FrameKind::Snapshot, Seq::new(100)).unwrap();
    cur.accept(FrameKind::Delta, Seq::new(101)).unwrap();

    // Duplicate (== last) is rejected.
    let dup = cur.accept(FrameKind::Delta, Seq::new(101)).unwrap_err();
    assert!(matches!(
        dup,
        Error::NonMonotonic {
            got: 101,
            last: 101,
            ..
        }
    ));
    // Older is rejected.
    let old = cur.accept(FrameKind::Delta, Seq::new(50)).unwrap_err();
    assert!(matches!(
        old,
        Error::NonMonotonic {
            got: 50,
            last: 101,
            ..
        }
    ));
    // Cursor unchanged by rejections.
    assert_eq!(cur.last_seq(), Some(Seq::new(101)));
}

#[test]
fn gap_in_deltas_is_reported() {
    let mut cur = TopicCursor::new(Topic::Tiles);
    cur.accept(FrameKind::Snapshot, Seq::new(10)).unwrap();
    // Jump from 10 to 14 -> 11,12,13 missed (gap == 3) -> warrants re-snapshot.
    let accepted = cur.accept(FrameKind::Delta, Seq::new(14)).unwrap();
    assert_eq!(
        accepted,
        Accepted::DeltaWithGap {
            seq: Seq::new(14),
            gap: 3
        }
    );
    assert_eq!(cur.last_seq(), Some(Seq::new(14)));
}

#[test]
fn resync_snapshot_rebuilds_baseline_with_new_seq() {
    // ADR-RT003 consequence: $resync is a REBUILD, not a merge. A fresh
    // snapshot resets the baseline even to a brand-new (here lower) seq line.
    let mut cur = TopicCursor::new(Topic::Tiles);
    cur.accept(FrameKind::Snapshot, Seq::new(184_250)).unwrap();
    cur.accept(FrameKind::Delta, Seq::new(184_251)).unwrap();

    // Server restarts; new session => new seq baseline starting at 1.
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(1)).unwrap(),
        Accepted::SnapshotBaseline { seq: Seq::new(1) }
    );
    assert_eq!(cur.last_seq(), Some(Seq::new(1)));
    // Deltas after the rebuild flow from the new baseline.
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(2)).unwrap(),
        Accepted::Delta { seq: Seq::new(2) }
    );
}

#[test]
fn frame_kind_roundtrips() {
    for (k, s) in [
        (FrameKind::Snapshot, "snapshot"),
        (FrameKind::Delta, "delta"),
    ] {
        let v = serde_json::to_value(k).unwrap();
        assert_eq!(v, serde_json::json!(s));
        let back: FrameKind = serde_json::from_value(v).unwrap();
        assert_eq!(back, k);
    }
}

proptest! {
    /// A snapshot followed by strictly-increasing deltas is always accepted,
    /// and the cursor always equals the last accepted seq.
    #[test]
    fn snapshot_then_increasing_deltas_always_accepted(
        base in 0u64..1_000_000,
        steps in proptest::collection::vec(1u64..1000, 0..200),
    ) {
        let mut cur = TopicCursor::new(Topic::Tiles);
        cur.accept(FrameKind::Snapshot, Seq::new(base)).unwrap();
        let mut seq = base;
        for step in steps {
            seq += step; // strictly increasing
            let accepted = cur.accept(FrameKind::Delta, Seq::new(seq)).unwrap();
            match accepted {
                Accepted::Delta { seq: s } | Accepted::DeltaWithGap { seq: s, .. } => {
                    prop_assert_eq!(s, Seq::new(seq));
                }
                Accepted::SnapshotBaseline { .. } => prop_assert!(false, "delta misclassified"),
                _ => prop_assert!(false, "unexpected Accepted variant"),
            }
            prop_assert_eq!(cur.last_seq(), Some(Seq::new(seq)));
        }
    }

    /// Any delta at or below the current baseline is always rejected and never
    /// moves the cursor.
    #[test]
    fn non_advancing_delta_always_rejected(
        base in 1u64..1_000_000,
        back in 0u64..1_000_000,
    ) {
        let mut cur = TopicCursor::new(Topic::Outputs);
        cur.accept(FrameKind::Snapshot, Seq::new(base)).unwrap();
        let bad_seq = base.saturating_sub(back % base); // <= base
        let result = cur.accept(FrameKind::Delta, Seq::new(bad_seq));
        prop_assert!(result.is_err(), "seq {} <= baseline {} must reject", bad_seq, base);
        prop_assert_eq!(cur.last_seq(), Some(Seq::new(base)));
    }
}
