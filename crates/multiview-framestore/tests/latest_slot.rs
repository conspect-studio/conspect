#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Behavioural tests for the lock-free single-slot store.

use std::sync::Arc;

use multiview_framestore::LatestSlot;

#[test]
fn empty_slot_loads_none() {
    let slot: LatestSlot<u32> = LatestSlot::new();
    assert!(slot.is_empty());
    assert!(slot.load().is_none());
    assert_eq!(slot.sequence(), 0);
}

#[test]
fn publish_then_load_returns_value() {
    let slot = LatestSlot::new();
    let seq = slot.publish(42_u32);
    assert_eq!(seq, 1);
    assert!(!slot.is_empty());
    assert_eq!(*slot.load().unwrap(), 42);
    assert_eq!(slot.sequence(), 1);
}

#[test]
fn newest_wins_overwrite_drops_previous() {
    let slot = LatestSlot::new();
    slot.publish(1_u32);
    slot.publish(2_u32);
    let s3 = slot.publish(3_u32);
    // The reader only ever sees the latest published value.
    assert_eq!(*slot.load().unwrap(), 3);
    // Sequence numbers are strictly increasing per publish.
    assert_eq!(s3, 3);
    assert_eq!(slot.sequence(), 3);
}

#[test]
fn sequence_strictly_increases_across_publishes() {
    let slot = LatestSlot::new();
    let mut last = 0;
    for v in 0..100_u32 {
        let s = slot.publish(v);
        assert!(s > last, "sequence must strictly increase: {s} !> {last}");
        last = s;
    }
    assert_eq!(slot.sequence(), 100);
    assert_eq!(*slot.load().unwrap(), 99);
}

#[test]
fn take_clears_slot_but_not_sequence() {
    let slot = LatestSlot::new();
    slot.publish(7_u32);
    slot.publish(8_u32);
    let taken = slot.take().unwrap();
    assert_eq!(*taken, 8);
    assert!(slot.is_empty());
    assert!(slot.load().is_none());
    // Sequence is monotonic for the life of the slot — taking does not reset it.
    assert_eq!(slot.sequence(), 2);
    // A subsequent publish keeps increasing from where we were.
    let s = slot.publish(9_u32);
    assert_eq!(s, 3);
}

#[test]
fn with_value_pre_populates_and_stamps_sequence_one() {
    let slot = LatestSlot::with_value(String::from("hello"));
    assert!(!slot.is_empty());
    assert_eq!(slot.load().unwrap().as_str(), "hello");
    assert_eq!(slot.sequence(), 1);
}

#[test]
fn publish_arc_avoids_reallocation_and_shares_pointer() {
    let slot = LatestSlot::new();
    let arc = Arc::new(vec![1_u8, 2, 3]);
    slot.publish_arc(Arc::clone(&arc));
    let loaded = slot.load().unwrap();
    // The slot stored the very same allocation we handed it.
    assert!(Arc::ptr_eq(&arc, &loaded));
}

#[test]
fn an_in_flight_reader_keeps_its_value_alive_after_overwrite() {
    // Holding a loaded Arc must remain valid even after the slot is overwritten
    // (the no-tearing / no-use-after-free guarantee).
    let slot = LatestSlot::new();
    slot.publish(String::from("first"));
    let held = slot.load().unwrap();
    slot.publish(String::from("second"));
    // `held` still points at the original value; it was not freed or mutated.
    assert_eq!(held.as_str(), "first");
    assert_eq!(slot.load().unwrap().as_str(), "second");
}
