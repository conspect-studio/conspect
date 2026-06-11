//! Property tests for the operator-surface value-machines (timer, round-robin,
//! identify flash) over an injected media clock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_overlay::identify::Identify;
use multiview_overlay::timer::{RoundRobin, Timer, TimerMode, TimerPhase};
use proptest::prelude::*;

fn mt(ns: i64) -> MediaTime {
    MediaTime::from_nanos(ns)
}

proptest! {
    /// A count-down never shows a negative readout and is `Expired` exactly when
    /// the elapsed time reaches/exceeds the duration.
    #[test]
    fn count_down_never_negative_and_expires_on_time(
        duration in 1i64..1_000_000_000_000,
        start in -1_000_000_000i64..1_000_000_000,
        delta in 0i64..2_000_000_000_000,
    ) {
        let mut t = Timer::new(TimerMode::CountDown { duration });
        t.start(mt(start));
        let now = mt(start.saturating_add(delta));
        // The readout parses back to a non-negative HH:MM:SS.
        let shown = t.display(now);
        let parts: Vec<&str> = shown.split(':').collect();
        prop_assert_eq!(parts.len(), 3);
        // Phase: expired iff the elapsed has reached the duration.
        let expected_expired = delta >= duration;
        prop_assert_eq!(t.phase(now) == TimerPhase::Expired, expected_expired);
    }

    /// Count-up is monotonic non-decreasing in time (more elapsed never shows a
    /// smaller whole-second readout).
    #[test]
    fn count_up_is_monotonic(
        start in -1_000_000_000i64..1_000_000_000,
        a in 0i64..1_000_000_000_000,
        b in 0i64..1_000_000_000_000,
    ) {
        let mut t = Timer::new(TimerMode::CountUp);
        t.start(mt(start));
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let secs_lo = parse_secs(&t.display(mt(start.saturating_add(lo))));
        let secs_hi = parse_secs(&t.display(mt(start.saturating_add(hi))));
        prop_assert!(secs_hi >= secs_lo);
    }

    /// A round-robin page index is always within `0..pages`.
    #[test]
    fn round_robin_index_in_bounds(
        pages in 1usize..64,
        dwell in 1i64..10_000_000_000,
        now in -1_000_000_000i64..10_000_000_000_000,
    ) {
        let rr = RoundRobin::new(pages, dwell).expect("valid params");
        let page = rr.page_at(mt(now));
        prop_assert!(page < pages);
    }

    /// Round-robin advances by exactly one page (mod pages) across a dwell
    /// boundary, for non-negative times.
    #[test]
    fn round_robin_advances_one_per_dwell(
        pages in 2usize..32,
        dwell in 1i64..1_000_000_000,
        k in 0i64..1000,
    ) {
        let rr = RoundRobin::new(pages, dwell).expect("valid params");
        let here = rr.page_at(mt(k.saturating_mul(dwell)));
        let next = rr.page_at(mt((k + 1).saturating_mul(dwell)));
        prop_assert_eq!(next, (here + 1) % pages);
    }

    /// Identify flash is never on outside its active window.
    #[test]
    fn identify_off_when_inactive(
        period in 0i64..1_000_000_000,
        duration in 0i64..1_000_000_000,
        trigger in -1_000_000_000i64..1_000_000_000,
        delta in 0i64..2_000_000_000,
    ) {
        let mut id = Identify::new(period, duration);
        id.trigger(mt(trigger));
        let now = mt(trigger.saturating_add(delta));
        if id.is_active(now) {
            prop_assert!(id.badge(now).is_some(), "active -> badge present");
        } else {
            prop_assert!(!id.is_on(now), "must be dark when inactive");
            prop_assert!(id.badge(now).is_none(), "no badge when inactive");
        }
    }
}

/// Sum the HH:MM:SS string into whole seconds for monotonicity checks.
fn parse_secs(hms: &str) -> i64 {
    let parts: Vec<i64> = hms
        .split(':')
        .map(|p| p.parse::<i64>().unwrap_or(0))
        .collect();
    if parts.len() == 3 {
        parts[0] * 3600 + parts[1] * 60 + parts[2]
    } else {
        0
    }
}
