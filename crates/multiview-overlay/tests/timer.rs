//! Tests for count-up/count-down/down-then-up timers and round-robin cycling.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_overlay::timer::{RoundRobin, Timer, TimerMode, TimerPhase};

const SEC: i64 = 1_000_000_000;

fn at(secs: i64) -> MediaTime {
    MediaTime::from_nanos(secs * SEC)
}

#[test]
fn count_up_elapses_from_start() {
    let mut t = Timer::new(TimerMode::CountUp);
    t.start(at(10));
    assert_eq!(t.display(at(10)), "00:00:00");
    assert_eq!(t.display(at(13)), "00:00:03");
    assert_eq!(t.display(at(10 + 3661)), "01:01:01");
}

#[test]
fn count_down_reaches_zero_and_enters_overrun() {
    let mut t = Timer::new(TimerMode::CountDown {
        duration: at(5).as_nanos(),
    });
    t.start(at(0));
    assert_eq!(t.display(at(0)), "00:00:05");
    assert_eq!(t.display(at(2)), "00:00:03");
    assert_eq!(t.phase(at(2)), TimerPhase::Counting);
    // At the deadline it shows zero and flips to expired.
    assert_eq!(t.display(at(5)), "00:00:00");
    assert_eq!(t.phase(at(5)), TimerPhase::Expired);
}

#[test]
fn count_down_does_not_go_negative() {
    let mut t = Timer::new(TimerMode::CountDown {
        duration: at(5).as_nanos(),
    });
    t.start(at(0));
    // Past the deadline the readout stays clamped at zero (no underflow).
    assert_eq!(t.display(at(99)), "00:00:00");
    assert_eq!(t.phase(at(99)), TimerPhase::Expired);
}

#[test]
fn down_then_up_counts_down_then_counts_overrun_up() {
    let mut t = Timer::new(TimerMode::DownThenUp {
        duration: at(5).as_nanos(),
    });
    t.start(at(0));
    assert_eq!(t.display(at(3)), "00:00:02");
    assert_eq!(t.phase(at(3)), TimerPhase::Counting);
    // After the deadline it counts the overrun upward.
    assert_eq!(t.display(at(8)), "00:00:03");
    assert_eq!(t.phase(at(8)), TimerPhase::Expired);
}

#[test]
fn stopped_timer_holds_zero() {
    let t = Timer::new(TimerMode::CountUp);
    // Never started.
    assert_eq!(t.display(at(100)), "00:00:00");
    assert_eq!(t.phase(at(100)), TimerPhase::Idle);
}

#[test]
fn pause_freezes_elapsed_then_resume_continues() {
    let mut t = Timer::new(TimerMode::CountUp);
    t.start(at(0));
    t.pause(at(4));
    // While paused, time does not advance.
    assert_eq!(t.display(at(10)), "00:00:04");
    assert_eq!(t.phase(at(10)), TimerPhase::Paused);
    t.resume(at(10));
    // After resume, only the unpaused time accrues.
    assert_eq!(t.display(at(13)), "00:00:07");
}

#[test]
fn count_down_wrap_colour_signals_warning_window() {
    // A count-down with a warning window flips its wrap colour to "warn" inside
    // the last N seconds, conveyed as a phase the renderer reads (a11y: phase is
    // exposed as text/state, not colour alone).
    let mut t = Timer::new(TimerMode::CountDown {
        duration: at(10).as_nanos(),
    })
    .with_warn_window(at(3).as_nanos());
    t.start(at(0));
    assert_eq!(t.phase(at(2)), TimerPhase::Counting);
    // At t=7s, remaining == 3s == the window edge: still counting (the window is
    // strictly the time *inside* the last 3 seconds).
    assert_eq!(t.phase(at(7)), TimerPhase::Counting);
    // At t=8s, remaining == 2s < 3s: warning.
    assert_eq!(t.phase(at(8)), TimerPhase::Warning);
    assert_eq!(t.phase(at(10)), TimerPhase::Expired);
}

#[test]
fn round_robin_cycles_pages_over_time() {
    // 3 pages, 2 seconds dwell each: page index advances every 2s, wrapping.
    let rr = RoundRobin::new(3, at(2).as_nanos()).expect("valid");
    rr_assert(&rr, at(0), 0);
    rr_assert(&rr, at(1), 0);
    rr_assert(&rr, at(2), 1);
    rr_assert(&rr, at(3), 1);
    rr_assert(&rr, at(4), 2);
    rr_assert(&rr, at(6), 0);
}

fn rr_assert(rr: &RoundRobin, now: MediaTime, expected: usize) {
    assert_eq!(rr.page_at(now), expected, "page at {:?}", now.as_nanos());
}

#[test]
fn round_robin_rejects_zero_pages_or_zero_dwell() {
    assert!(RoundRobin::new(0, at(1).as_nanos()).is_err());
    assert!(RoundRobin::new(3, 0).is_err());
}

#[test]
fn serde_round_trips_timer_mode_tagged() {
    let mode = TimerMode::CountDown {
        duration: at(5).as_nanos(),
    };
    let json = serde_json::to_string(&mode).expect("serialize");
    // Tagged on "mode" with snake_case names — never untagged.
    assert!(json.contains("count_down"));
    let back: TimerMode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(mode, back);
}
