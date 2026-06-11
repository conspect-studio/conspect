//! Integration tests for the supervised-reconnect backoff policy.
//!
//! Exponential backoff with full jitter, capped at a maximum delay. The jitter
//! source is INJECTED as a raw unit value in `[0, JITTER_SCALE]` so the schedule
//! is deterministically testable (no `f64` rounding in the contract). A reset
//! (successful connection) returns the schedule to the base delay.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use core::time::Duration;
use multiview_input::reconnect::{Backoff, BackoffConfig, JITTER_SCALE};

fn cfg() -> BackoffConfig {
    BackoffConfig {
        base: Duration::from_millis(100),
        max: Duration::from_secs(30),
        factor: 2,
    }
}

#[test]
fn backoff_grows_exponentially_at_full_jitter() {
    let mut b = Backoff::new(cfg());
    // jitter = JITTER_SCALE means "use the full computed ceiling": the pure
    // exponential schedule 100, 200, 400, 800 ms ...
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(100));
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(200));
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(400));
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(800));
}

#[test]
fn backoff_is_capped_at_max() {
    let mut b = Backoff::new(cfg());
    let mut last = Duration::ZERO;
    for _ in 0..40 {
        last = b.next_delay(JITTER_SCALE);
        assert!(
            last <= Duration::from_secs(30),
            "delay {last:?} exceeds cap"
        );
    }
    assert_eq!(last, Duration::from_secs(30));
}

#[test]
fn full_jitter_scales_within_zero_and_ceiling() {
    let mut b = Backoff::new(cfg());
    // jitter 0 -> zero delay (full-jitter lower bound).
    assert_eq!(b.next_delay(0), Duration::ZERO);
    // Attempt counter still advanced: next ceiling 200ms; at half scale -> 100ms.
    assert_eq!(b.next_delay(JITTER_SCALE / 2), Duration::from_millis(100));
    // Next ceiling 400ms; at quarter scale -> 100ms.
    assert_eq!(b.next_delay(JITTER_SCALE / 4), Duration::from_millis(100));
}

#[test]
fn jitter_value_above_scale_is_clamped_to_ceiling() {
    let mut b = Backoff::new(cfg());
    // A raw value above JITTER_SCALE is clamped to the full ceiling, never over.
    let d = b.next_delay(JITTER_SCALE * 4);
    assert_eq!(d, Duration::from_millis(100));
}

#[test]
fn reset_returns_to_base() {
    let mut b = Backoff::new(cfg());
    let _ = b.next_delay(JITTER_SCALE);
    let _ = b.next_delay(JITTER_SCALE);
    let _ = b.next_delay(JITTER_SCALE);
    b.reset();
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(100));
}

#[test]
fn attempt_count_tracks_failures() {
    let mut b = Backoff::new(cfg());
    assert_eq!(b.attempts(), 0);
    let _ = b.next_delay(JITTER_SCALE);
    let _ = b.next_delay(JITTER_SCALE);
    assert_eq!(b.attempts(), 2);
    b.reset();
    assert_eq!(b.attempts(), 0);
}

#[test]
fn factor_of_three_grows_faster() {
    let mut b = Backoff::new(BackoffConfig {
        base: Duration::from_millis(10),
        max: Duration::from_secs(60),
        factor: 3,
    });
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(10));
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(30));
    assert_eq!(b.next_delay(JITTER_SCALE), Duration::from_millis(90));
}

#[test]
fn ceiling_is_observable_without_consuming_attempt() {
    let b = Backoff::new(cfg());
    // Before any attempt the ceiling is the base.
    assert_eq!(b.ceiling(), Duration::from_millis(100));
}
