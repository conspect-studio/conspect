//! Tests for the IDENTIFY (flash-a-tile) operator-locate overlay.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_overlay::identify::Identify;

const SEC: i64 = 1_000_000_000;

fn at(ns_secs_tenths: i64) -> MediaTime {
    // Argument is in tenths of a second so the flash period is easy to express.
    MediaTime::from_nanos(ns_secs_tenths * (SEC / 10))
}

#[test]
fn idle_identify_is_not_visible() {
    let id = Identify::new(at(5).as_nanos(), at(2).as_nanos());
    assert!(!id.is_active(at(0)));
    assert!(!id.is_on(at(0)), "an unstarted identify never flashes");
}

#[test]
fn trigger_makes_it_active_for_the_configured_duration() {
    // period = 1.0s (10 tenths), total duration = 3.0s (30 tenths).
    let mut id = Identify::new(at(10).as_nanos(), at(30).as_nanos());
    id.trigger(at(0));
    assert!(id.is_active(at(0)));
    assert!(id.is_active(at(29)));
    assert!(!id.is_active(at(30)), "expires exactly at total duration");
    assert!(!id.is_active(at(31)));
}

#[test]
fn flash_toggles_on_and_off_each_half_period() {
    // period = 1.0s -> on for the first half (0.5s), off for the second half.
    let mut id = Identify::new(at(10).as_nanos(), at(100).as_nanos());
    id.trigger(at(0));
    // First half-period: ON.
    assert!(id.is_on(at(0)));
    assert!(id.is_on(at(4)));
    // Second half-period: OFF.
    assert!(!id.is_on(at(5)));
    assert!(!id.is_on(at(9)));
    // Next period: ON again.
    assert!(id.is_on(at(10)));
    assert!(!id.is_on(at(15)));
}

#[test]
fn flash_is_off_once_expired_even_if_phase_would_be_on() {
    let mut id = Identify::new(at(10).as_nanos(), at(20).as_nanos());
    id.trigger(at(0));
    // At t=2.0s the identify has expired; even though the phase math would be
    // "on", it must be dark.
    assert!(!id.is_active(at(20)));
    assert!(!id.is_on(at(20)));
}

#[test]
fn retrigger_restarts_the_flash_window() {
    let mut id = Identify::new(at(10).as_nanos(), at(20).as_nanos());
    id.trigger(at(0));
    assert!(id.is_active(at(15)));
    // Re-trigger later resets the clock; it is active for a fresh duration.
    id.trigger(at(100));
    assert!(id.is_active(at(115)));
    assert!(!id.is_active(at(120)));
}

#[test]
fn cancel_stops_the_flash_immediately() {
    let mut id = Identify::new(at(10).as_nanos(), at(50).as_nanos());
    id.trigger(at(0));
    assert!(id.is_active(at(5)));
    id.cancel();
    assert!(!id.is_active(at(5)));
    assert!(!id.is_on(at(5)));
}

#[test]
fn accessibility_badge_text_is_present_when_active() {
    // IDENTIFY conveys "this tile" with a text badge, not colour/flash alone.
    let mut id = Identify::new(at(10).as_nanos(), at(50).as_nanos());
    id.trigger(at(0));
    let badge = id.badge(at(1)).expect("active identify has a badge");
    assert!(badge.to_uppercase().contains("IDENTIFY"));
    assert!(id.badge(at(60)).is_none(), "no badge once expired");
}

#[test]
fn zero_period_is_treated_as_steady_on() {
    // A zero period must not divide-by-zero; it degrades to a steady-on marker.
    let mut id = Identify::new(0, at(20).as_nanos());
    id.trigger(at(0));
    assert!(id.is_on(at(1)));
    assert!(id.is_on(at(5)));
    assert!(!id.is_on(at(20)), "still expires");
}
