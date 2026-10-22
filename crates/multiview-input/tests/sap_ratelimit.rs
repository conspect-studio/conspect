//! Pure SAP datagram rate-limiter tests (ADR-0041 §3, panel F4).
//!
//! Every accepted SAP datagram triggers an O(n) RCU clone + publish in the
//! session table; a spoofed-origin flood at line rate would otherwise force
//! that expensive work per datagram and could starve the shared control-plane
//! runtime (inv #10). [`SapRateLimiter`] bounds the *rate* of expensive folds
//! before they run — these tests pin that bound deterministically with injected
//! time (no socket, no wall-clock).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use multiview_input::sap::ratelimit::SapRateLimiter;

#[test]
fn admits_up_to_the_burst_then_denies_within_the_window() {
    let mut limiter = SapRateLimiter::new(3, Duration::from_secs(1));
    let now = Duration::from_millis(0);
    assert!(limiter.allow(now), "1st is within the burst");
    assert!(limiter.allow(now), "2nd is within the burst");
    assert!(limiter.allow(now), "3rd is within the burst");
    assert!(
        !limiter.allow(now),
        "the 4th within one window is rate-limited"
    );
    assert!(
        !limiter.allow(Duration::from_millis(999)),
        "still denied later in the same window"
    );
}

#[test]
fn refills_at_the_next_window() {
    let mut limiter = SapRateLimiter::new(2, Duration::from_secs(1));
    assert!(limiter.allow(Duration::from_millis(0)));
    assert!(limiter.allow(Duration::from_millis(0)));
    assert!(
        !limiter.allow(Duration::from_millis(500)),
        "burst exhausted in this window"
    );
    assert!(
        limiter.allow(Duration::from_secs(1)),
        "a new window resets the budget"
    );
    assert!(limiter.allow(Duration::from_millis(1500)));
    assert!(
        !limiter.allow(Duration::from_millis(1600)),
        "the new window's burst is exhausted too"
    );
}

#[test]
fn a_same_window_flood_admits_exactly_the_burst() {
    // 10_000 datagrams arriving within one window admit at most `burst`, so the
    // expensive fold rate is bounded regardless of the inbound datagram rate.
    let mut limiter = SapRateLimiter::new(64, Duration::from_secs(1));
    let admitted = (0..10_000)
        .filter(|_| limiter.allow(Duration::from_millis(1)))
        .count();
    assert_eq!(admitted, 64, "a same-window flood admits exactly the burst");
}

#[test]
fn zero_burst_denies_every_datagram() {
    let mut limiter = SapRateLimiter::new(0, Duration::from_secs(1));
    assert!(!limiter.allow(Duration::from_millis(0)));
    assert!(!limiter.allow(Duration::from_secs(5)));
}
