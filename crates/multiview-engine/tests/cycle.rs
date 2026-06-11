//! Round-robin + freeze tile tests (ADR-MV001): the cycler advances deterministically
//! on the dwell (coalescing missed windows, wrapping the roster) and the freeze
//! tile holds/thaws a reference still — pure value machines over an injected
//! `MediaTime`, never blocking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_engine::cycle::{FreezeTile, RoundRobin};
use proptest::prelude::*;

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

fn roster() -> Vec<String> {
    vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
}

#[test]
fn round_robin_rejects_empty_or_nonpositive() {
    assert!(RoundRobin::new(Vec::new(), ms(100), ms(0)).is_none());
    assert!(RoundRobin::new(roster(), ms(0), ms(0)).is_none());
    assert!(RoundRobin::new(roster(), ms(-1), ms(0)).is_none());
}

#[test]
fn round_robin_advances_on_dwell_and_wraps() {
    let mut rr = RoundRobin::new(roster(), ms(100), ms(0)).unwrap();
    assert_eq!(rr.tick(ms(0)), "a");
    assert_eq!(rr.tick(ms(50)), "a"); // within dwell
    assert_eq!(rr.tick(ms(100)), "b"); // dwell elapsed
    assert_eq!(rr.tick(ms(200)), "c");
    assert_eq!(rr.tick(ms(300)), "a"); // wrapped
    assert_eq!(rr.index(), 0);
}

#[test]
fn round_robin_coalesces_missed_windows() {
    let mut rr = RoundRobin::new(roster(), ms(100), ms(0)).unwrap();
    assert_eq!(rr.tick(ms(0)), "a");
    // Jump 5 windows: 5 % 3 = 2 steps -> index 2 ("c").
    assert_eq!(rr.tick(ms(550)), "c");
    assert_eq!(rr.index(), 2);
    // Next single window advances by one.
    assert_eq!(rr.tick(ms(650)), "a");
}

#[test]
fn round_robin_non_monotonic_does_not_advance() {
    let mut rr = RoundRobin::new(roster(), ms(100), ms(500)).unwrap();
    assert_eq!(rr.tick(ms(500)), "a");
    // Clock goes backwards: clamped, no advance.
    assert_eq!(rr.tick(ms(100)), "a");
    assert_eq!(rr.index(), 0);
}

#[test]
fn freeze_tile_holds_and_thaws() {
    let mut tile = FreezeTile::live();
    assert!(!tile.is_frozen());
    assert!(tile.still_id().is_none());

    tile.freeze("still-7", ms(42));
    assert!(tile.is_frozen());
    assert_eq!(tile.still_id(), Some("still-7"));
    assert_eq!(tile.frozen_at(), ms(42));

    // Re-freeze replaces the still.
    tile.freeze("still-8", ms(99));
    assert_eq!(tile.still_id(), Some("still-8"));

    assert!(tile.thaw());
    assert!(!tile.is_frozen());
    // Thawing an already-live tile is a no-op.
    assert!(!tile.thaw());
}

// ---------- property tests ----------

proptest! {
    /// After driving a monotonic sweep of ticks, the cycler's index always equals
    /// the total number of elapsed dwell windows (mod roster length) — fully
    /// deterministic and equal to a closed-form computation. The current source
    /// always matches the index.
    #[test]
    fn prop_index_is_deterministic_window_count(
        len in 1usize..6,
        dwell_ms in 1i64..200,
        end_ms in 0i64..5000,
    ) {
        let names: Vec<String> = (0..len).map(|i| format!("s{i}")).collect();
        let mut rr = RoundRobin::new(names.clone(), ms(dwell_ms), ms(0)).unwrap();

        // Single jump to end_ms from base 0.
        let current = rr.tick(ms(end_ms)).to_owned();

        let windows = end_ms / dwell_ms;
        let len_i = i64::try_from(len).unwrap_or(1).max(1);
        let expected_index = usize::try_from(windows % len_i).unwrap_or(0);
        prop_assert_eq!(rr.index(), expected_index);
        prop_assert_eq!(&current, &names[expected_index]);
    }

    /// Stepping window-by-window and jumping straight to the same time land on the
    /// SAME index (catch-up coalescing is exact, never drifts).
    #[test]
    fn prop_stepwise_equals_jump(
        len in 1usize..6,
        dwell_ms in 1i64..200,
        steps in 0i64..50,
    ) {
        let names: Vec<String> = (0..len).map(|i| format!("s{i}")).collect();

        let mut step_rr = RoundRobin::new(names.clone(), ms(dwell_ms), ms(0)).unwrap();
        for k in 0..=steps {
            step_rr.tick(ms(k.saturating_mul(dwell_ms)));
        }

        let mut jump_rr = RoundRobin::new(names, ms(dwell_ms), ms(0)).unwrap();
        jump_rr.tick(ms(steps.saturating_mul(dwell_ms)));

        prop_assert_eq!(step_rr.index(), jump_rr.index());
    }
}
