//! Scheduler tests (ADR-MV001): interval rules fire once per due window (with
//! coalesced catch-up, never a burst) and never early; event rules fire on a
//! matching event; all over an injected `MediaTime`, never blocking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_engine::scheduler::{EventKind, ScheduledAction, Scheduler, TriggerEvent};
use proptest::prelude::*;

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

#[test]
fn interval_rule_fires_at_due_instants() {
    let mut sched = Scheduler::new();
    assert!(sched.every(ms(0), ms(100), ScheduledAction::take_salvo("s")));

    // Base instant t=0 is due immediately.
    assert_eq!(sched.tick(ms(0), &[]).len(), 1);
    // Before next due instant: no fire.
    assert_eq!(sched.tick(ms(50), &[]).len(), 0);
    // At next due instant t=100: fire.
    assert_eq!(sched.tick(ms(100), &[]).len(), 1);
    // Just after: no fire until t=200.
    assert_eq!(sched.tick(ms(150), &[]).len(), 0);
    assert_eq!(sched.tick(ms(200), &[]).len(), 1);
}

#[test]
fn interval_rule_coalesces_missed_windows() {
    let mut sched = Scheduler::new();
    sched.every(ms(0), ms(100), ScheduledAction::take_salvo("s"));
    // t=0 fires.
    assert_eq!(sched.tick(ms(0), &[]).len(), 1);
    // A coarse jump past several windows fires only ONCE (no burst).
    assert_eq!(sched.tick(ms(550), &[]).len(), 1);
    // The cursor advanced past 550, so the next fire is at 600.
    assert_eq!(sched.tick(ms(599), &[]).len(), 0);
    assert_eq!(sched.tick(ms(600), &[]).len(), 1);
}

#[test]
fn interval_rule_does_not_fire_before_base() {
    let mut sched = Scheduler::new();
    sched.every(ms(500), ms(100), ScheduledAction::take_salvo("s"));
    // Before base: never fires.
    assert_eq!(sched.tick(ms(0), &[]).len(), 0);
    assert_eq!(sched.tick(ms(499), &[]).len(), 0);
    // At base: fires.
    assert_eq!(sched.tick(ms(500), &[]).len(), 1);
}

#[test]
fn non_positive_interval_is_rejected() {
    let mut sched = Scheduler::new();
    assert!(!sched.every(ms(0), ms(0), ScheduledAction::take_salvo("s")));
    assert!(!sched.every(ms(0), ms(-5), ScheduledAction::take_salvo("s")));
    assert_eq!(sched.rule_count(), 0);
}

#[test]
fn event_rule_fires_on_matching_event() {
    let mut sched = Scheduler::new();
    sched.on_event(
        EventKind::Alarm,
        "black-cam1",
        ScheduledAction::take_salvo("recover"),
    );

    // No event: no fire.
    assert_eq!(sched.tick(ms(0), &[]).len(), 0);
    // Wrong name: no fire.
    let other = [TriggerEvent::new(EventKind::Alarm, "black-cam2")];
    assert_eq!(sched.tick(ms(0), &other).len(), 0);
    // Wrong kind: no fire.
    let wrong_kind = [TriggerEvent::new(EventKind::Cue, "black-cam1")];
    assert_eq!(sched.tick(ms(0), &wrong_kind).len(), 0);
    // Matching event: fires.
    let matching = [TriggerEvent::new(EventKind::Alarm, "black-cam1")];
    let fired = sched.tick(ms(0), &matching);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0], ScheduledAction::take_salvo("recover"));
}

#[test]
fn fires_in_rule_order() {
    let mut sched = Scheduler::new();
    sched.every(ms(0), ms(100), ScheduledAction::take_salvo("a"));
    sched.on_event(EventKind::Cue, "c", ScheduledAction::take_salvo("b"));
    let fired = sched.tick(ms(0), &[TriggerEvent::new(EventKind::Cue, "c")]);
    assert_eq!(
        fired,
        vec![
            ScheduledAction::take_salvo("a"),
            ScheduledAction::take_salvo("b"),
        ]
    );
}

// ---------- property tests ----------

proptest! {
    /// An interval rule never fires before its base, fires at most once per tick,
    /// and after firing its next due instant is strictly after `now` (so a
    /// monotonic sweep never double-fires the same window).
    #[test]
    fn prop_interval_never_early_and_at_most_once(
        base_ms in 0i64..1000,
        interval_ms in 1i64..500,
        ticks in prop::collection::vec(0i64..5000, 1..40),
    ) {
        let mut sched = Scheduler::new();
        prop_assert!(sched.every(ms(base_ms), ms(interval_ms), ScheduledAction::take_salvo("s")));

        // Drive a MONOTONIC non-decreasing sweep of ticks.
        let mut times: Vec<i64> = ticks;
        times.sort_unstable();

        let mut total_fires = 0u64;
        let mut last = i64::MIN;
        for t in times {
            let fired = sched.tick(ms(t), &[]);
            // At most one fire per tick for a single rule.
            prop_assert!(fired.len() <= 1);
            if !fired.is_empty() {
                // Never before base.
                prop_assert!(t >= base_ms, "fired at {t} before base {base_ms}");
            }
            total_fires += u64::try_from(fired.len()).unwrap_or(0);
            last = last.max(t);
        }

        // Sanity: total fires is bounded by the number of whole windows up to the
        // last tick (coalescing means we never fire MORE than one per window).
        if last >= base_ms {
            let windows = (last - base_ms) / interval_ms + 1;
            let windows = u64::try_from(windows).unwrap_or(u64::MAX);
            prop_assert!(total_fires <= windows,
                "fires {total_fires} exceeds windows {windows}");
        } else {
            prop_assert_eq!(total_fires, 0);
        }
    }

    /// An event rule fires exactly once for each tick whose event list contains a
    /// match, independent of the clock.
    #[test]
    fn prop_event_fires_iff_match_present(
        flags in prop::collection::vec(any::<bool>(), 0..30),
    ) {
        let mut sched = Scheduler::new();
        sched.on_event(EventKind::Gpi, "pin-1", ScheduledAction::take_salvo("s"));
        for (i, &has_match) in flags.iter().enumerate() {
            let events = if has_match {
                vec![
                    TriggerEvent::new(EventKind::Gpi, "pin-0"),
                    TriggerEvent::new(EventKind::Gpi, "pin-1"),
                ]
            } else {
                vec![TriggerEvent::new(EventKind::Gpi, "pin-0")]
            };
            let now = ms(i64::try_from(i).unwrap_or(0));
            let fired = sched.tick(now, &events);
            prop_assert_eq!(fired.len(), usize::from(has_match));
        }
    }
}
