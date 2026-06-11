#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Sequence-cursor monotonicity and overflow tests (ADR-RT002/RT003).

use multiview_events::{Error, Seq, SeqCounter};
use proptest::prelude::*;

#[test]
fn fresh_counter_starts_at_zero_and_increments() {
    let mut c = SeqCounter::new();
    assert_eq!(c.peek(), Some(Seq::ZERO));
    assert_eq!(c.issue().unwrap(), Seq::ZERO);
    assert_eq!(c.issue().unwrap(), Seq::new(1));
    assert_eq!(c.issue().unwrap(), Seq::new(2));
    assert_eq!(c.peek(), Some(Seq::new(3)));
}

#[test]
fn resuming_after_continues_from_successor() {
    let mut c = SeqCounter::resuming_after(Seq::new(184_250)).unwrap();
    assert_eq!(c.issue().unwrap(), Seq::new(184_251));
    assert_eq!(c.issue().unwrap(), Seq::new(184_252));
}

#[test]
fn resuming_after_max_is_overflow() {
    let err = SeqCounter::resuming_after(Seq::new(u64::MAX)).unwrap_err();
    assert_eq!(err, Error::SeqOverflow);
}

#[test]
fn counter_overflows_at_u64_max_instead_of_wrapping() {
    // Start one below the max; the max is issuable, then the next call errors.
    let mut c = SeqCounter::resuming_after(Seq::new(u64::MAX - 1)).unwrap();
    assert_eq!(c.issue().unwrap(), Seq::new(u64::MAX));
    assert_eq!(c.peek(), None, "counter is exhausted at the max");
    assert_eq!(c.issue().unwrap_err(), Error::SeqOverflow);
    // Still overflowing on a subsequent call (idempotent failure).
    assert_eq!(c.issue().unwrap_err(), Error::SeqOverflow);
}

#[test]
fn gap_to_counts_skipped_frames() {
    assert_eq!(Seq::new(10).gap_to(Seq::new(11)), Some(0)); // adjacent: no gap
    assert_eq!(Seq::new(10).gap_to(Seq::new(14)), Some(3)); // 11,12,13 missed
    assert_eq!(Seq::new(10).gap_to(Seq::new(10)), None); // not strictly after
    assert_eq!(Seq::new(10).gap_to(Seq::new(9)), None); // earlier
}

proptest! {
    /// Every issued sequence is strictly greater than the previous one, with no
    /// repeats and no decreases, for an arbitrary number of issuances from an
    /// arbitrary resume point.
    #[test]
    fn issued_sequences_are_strictly_monotonic(
        start in 0u64..(u64::MAX - 4096),
        count in 1usize..2048,
    ) {
        let mut c = SeqCounter::resuming_after(Seq::new(start)).unwrap();
        let mut prev: Option<Seq> = None;
        for _ in 0..count {
            let cur = c.issue().unwrap();
            if let Some(p) = prev {
                prop_assert!(cur > p, "seq must strictly increase: {:?} !> {:?}", cur, p);
                prop_assert_eq!(cur.get(), p.get() + 1, "seq must advance by exactly one");
            } else {
                prop_assert_eq!(cur, Seq::new(start + 1));
            }
            prev = Some(cur);
        }
    }

    /// `peek` always equals the value the next `issue` returns.
    #[test]
    fn peek_matches_next_issue(start in 0u64..(u64::MAX - 64), count in 0usize..64) {
        let mut c = SeqCounter::resuming_after(Seq::new(start)).unwrap();
        for _ in 0..count {
            let peeked = c.peek();
            let issued = c.issue().unwrap();
            prop_assert_eq!(peeked, Some(issued));
        }
    }
}
