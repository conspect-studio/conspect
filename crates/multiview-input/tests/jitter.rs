//! Integration tests for the bounded reorder / jitter buffer.
//!
//! The buffer sorts by PTS within a bounded window, drops late packets, and is
//! strictly bounded: it drops, it NEVER grows (invariant: bounded queues drop,
//! never grow). Output is emitted in non-decreasing PTS order.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_input::jitter::{ReorderBuffer, ReorderOutcome};

fn mt(ns: i64) -> MediaTime {
    MediaTime::from_nanos(ns)
}

#[test]
fn reorders_out_of_order_packets_by_pts() {
    let mut b: ReorderBuffer<&str> = ReorderBuffer::new(8);
    // Push out of order.
    assert_eq!(b.push(mt(30), "c"), ReorderOutcome::Buffered);
    assert_eq!(b.push(mt(10), "a"), ReorderOutcome::Buffered);
    assert_eq!(b.push(mt(20), "b"), ReorderOutcome::Buffered);
    // Drain releases in PTS order.
    let drained: Vec<&str> = b.drain_ready(MediaTime::from_nanos(i64::MAX)).collect();
    assert_eq!(drained, vec!["a", "b", "c"]);
}

#[test]
fn pop_returns_smallest_pts_first() {
    let mut b: ReorderBuffer<u32> = ReorderBuffer::new(4);
    b.push(mt(50), 5);
    b.push(mt(10), 1);
    b.push(mt(40), 4);
    assert_eq!(b.pop().map(|(_, v)| v), Some(1));
    assert_eq!(b.pop().map(|(_, v)| v), Some(4));
    assert_eq!(b.pop().map(|(_, v)| v), Some(5));
    assert_eq!(b.pop(), None);
}

#[test]
fn drops_late_packet_below_release_watermark() {
    let mut b: ReorderBuffer<&str> = ReorderBuffer::new(8);
    b.push(mt(100), "first");
    // Release everything up to ts=100.
    let _ = b.drain_ready(mt(100)).collect::<Vec<_>>();
    // A packet that arrives with pts < the last released watermark is TOO LATE.
    assert_eq!(b.push(mt(90), "late"), ReorderOutcome::DroppedLate);
    // It must not appear in any future drain.
    let rest: Vec<&str> = b.drain_ready(mt(i64::MAX)).collect();
    assert!(rest.is_empty());
}

#[test]
fn capacity_is_never_exceeded_drop_oldest() {
    let cap = 4;
    let mut b: ReorderBuffer<i64> = ReorderBuffer::new(cap);
    // Push more than capacity; with drop-oldest, the smallest-PTS entry is
    // evicted to make room for the newer one.
    for ts in [10_i64, 20, 30, 40] {
        assert_eq!(b.push(mt(ts), ts), ReorderOutcome::Buffered);
    }
    assert_eq!(b.len(), cap);
    // 6th push at higher pts evicts the oldest (10) and is buffered.
    assert_eq!(b.push(mt(50), 50), ReorderOutcome::DroppedToMakeRoom);
    assert_eq!(b.len(), cap, "buffer must never exceed capacity");
    let drained: Vec<i64> = b.drain_ready(mt(i64::MAX)).collect();
    // Oldest (10) was evicted; 20..=50 remain in order.
    assert_eq!(drained, vec![20, 30, 40, 50]);
}

#[test]
fn drain_ready_only_releases_at_or_before_watermark() {
    let mut b: ReorderBuffer<i64> = ReorderBuffer::new(8);
    for ts in [10_i64, 20, 30, 40] {
        b.push(mt(ts), ts);
    }
    let ready: Vec<i64> = b.drain_ready(mt(25)).collect();
    assert_eq!(ready, vec![10, 20]);
    // 30 and 40 are held back until the watermark advances.
    assert_eq!(b.len(), 2);
    let later: Vec<i64> = b.drain_ready(mt(40)).collect();
    assert_eq!(later, vec![30, 40]);
}

#[test]
fn drain_ready_output_is_non_decreasing() {
    let mut b: ReorderBuffer<i64> = ReorderBuffer::new(16);
    for ts in [5_i64, 3, 9, 1, 7, 2, 8, 4, 6] {
        b.push(mt(ts), ts);
    }
    let out: Vec<i64> = b.drain_ready(mt(i64::MAX)).collect();
    let mut sorted = out.clone();
    sorted.sort_unstable();
    assert_eq!(out, sorted, "drain must be non-decreasing");
}

#[test]
fn duplicate_pts_keeps_both_in_order() {
    let mut b: ReorderBuffer<&str> = ReorderBuffer::new(8);
    b.push(mt(10), "a1");
    b.push(mt(10), "a2");
    b.push(mt(5), "z");
    let out: Vec<&str> = b.drain_ready(mt(i64::MAX)).collect();
    assert_eq!(out[0], "z");
    assert_eq!(out.len(), 3);
}

#[test]
fn empty_buffer_pops_none() {
    let mut b: ReorderBuffer<u8> = ReorderBuffer::new(4);
    assert!(b.is_empty());
    assert_eq!(b.pop(), None);
    assert_eq!(b.len(), 0);
}
