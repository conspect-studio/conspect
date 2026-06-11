//! Alert-card state model. The alert card is a must-never-fail overlay
//! (ADR-R008 SIGNAL LOST class); its visibility is driven by a small,
//! deterministic state machine over a media clock — never by a live input
//! frame (overlays are input-decoupled, ADR-R008).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_overlay::alert::{AlertCard, AlertState, Severity};

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

#[test]
fn new_card_starts_idle_and_invisible() {
    let card = AlertCard::new("tile-3 signal", Severity::Critical);
    assert_eq!(card.state(), AlertState::Idle);
    assert!(!card.is_visible());
    assert_eq!(card.severity(), Severity::Critical);
}

#[test]
fn raise_makes_the_card_active_and_visible() {
    let mut card = AlertCard::new("gpu lost", Severity::Critical);
    card.raise(ms(0));
    assert_eq!(card.state(), AlertState::Active);
    assert!(card.is_visible());
}

#[test]
fn acknowledge_keeps_the_card_visible_but_marks_it_seen() {
    let mut card = AlertCard::new("encoder recycle", Severity::Warning);
    card.raise(ms(0));
    card.acknowledge();
    assert_eq!(card.state(), AlertState::Acknowledged);
    // An acknowledged-but-still-firing alert stays on screen.
    assert!(card.is_visible());
}

#[test]
fn acknowledge_is_a_noop_when_idle() {
    let mut card = AlertCard::new("x", Severity::Info);
    card.acknowledge();
    assert_eq!(card.state(), AlertState::Idle);
    assert!(!card.is_visible());
}

#[test]
fn clear_with_dwell_holds_the_card_then_hides_after_the_dwell_elapses() {
    // Dwell prevents flicker: the card stays visible for `dwell` after the
    // condition clears.
    let mut card =
        AlertCard::new("reconnecting", Severity::Warning).with_clear_dwell(MediaTime::from_nanos(
            500 * 1_000_000, // 500 ms
        ));
    card.raise(ms(0));
    card.clear(ms(1000));
    // Immediately after clearing it is in the Clearing dwell, still visible.
    assert_eq!(card.state(), AlertState::Clearing);
    assert!(card.is_visible());

    // Before the dwell elapses: still visible.
    card.tick(ms(1400));
    assert_eq!(card.state(), AlertState::Clearing);
    assert!(card.is_visible());

    // After the dwell elapses (1000 + 500 = 1500): hidden + idle.
    card.tick(ms(1600));
    assert_eq!(card.state(), AlertState::Idle);
    assert!(!card.is_visible());
}

#[test]
fn clear_with_zero_dwell_hides_immediately_on_next_tick() {
    let mut card = AlertCard::new("done", Severity::Info);
    card.raise(ms(0));
    card.clear(ms(100));
    // Zero dwell: the very moment it is reached, the card is no longer visible.
    card.tick(ms(100));
    assert_eq!(card.state(), AlertState::Idle);
    assert!(!card.is_visible());
}

#[test]
fn re_raising_during_clearing_dwell_returns_to_active() {
    // A flapping condition that recovers then fails again must re-show the card.
    let mut card = AlertCard::new("flap", Severity::Critical)
        .with_clear_dwell(MediaTime::from_nanos(1_000_000_000));
    card.raise(ms(0));
    card.clear(ms(100));
    assert_eq!(card.state(), AlertState::Clearing);
    card.raise(ms(200));
    assert_eq!(card.state(), AlertState::Active);
    assert!(card.is_visible());
    // And a later tick well past the original dwell keeps it active.
    card.tick(ms(5000));
    assert_eq!(card.state(), AlertState::Active);
    assert!(card.is_visible());
}

#[test]
fn tick_before_raise_is_inert() {
    let mut card = AlertCard::new("idle", Severity::Info);
    card.tick(ms(9999));
    assert_eq!(card.state(), AlertState::Idle);
    assert!(!card.is_visible());
}

#[test]
fn raising_an_acknowledged_alert_again_returns_to_active_unacknowledged() {
    // A fresh occurrence of the same condition should re-demand attention.
    let mut card = AlertCard::new("re", Severity::Warning);
    card.raise(ms(0));
    card.acknowledge();
    assert_eq!(card.state(), AlertState::Acknowledged);
    card.raise(ms(10));
    assert_eq!(card.state(), AlertState::Active);
}

#[test]
fn severity_orders_info_below_warning_below_critical() {
    assert!(Severity::Info < Severity::Warning);
    assert!(Severity::Warning < Severity::Critical);
}
