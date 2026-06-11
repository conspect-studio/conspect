//! X.733 alarm state-machine tests (ADR-MV001): dwell-up/dwell-down hysteresis,
//! latch, acknowledge, and synthetic black/freeze fault injection — all over an
//! injected `MediaTime`, with no sleeps.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::{AlarmId, AlarmKind, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;
use multiview_engine::alarm::state::{AlarmHysteresis, AlarmStateMachine, AlarmTransition, Phase};
use proptest::prelude::*;

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

fn machine(hys: AlarmHysteresis) -> AlarmStateMachine {
    AlarmStateMachine::new(
        AlarmId::new("probe-black-tile-0"),
        AlarmKind::Black,
        AlarmScope::Tile { index: 0 },
        PerceivedSeverity::Major,
        hys,
    )
}

#[test]
fn raises_only_after_dwell_up_elapses() {
    // dwell-up 100 ms, dwell-down 0.
    let mut m = machine(AlarmHysteresis::new(ms(100), ms(0)));
    assert_eq!(m.phase(), Phase::Clear);
    assert!(!m.is_active());

    // Fault appears at t=0: pending, not yet raised.
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Pending { .. }));
    assert!(!m.is_active());
    assert_eq!(m.current_severity(), PerceivedSeverity::Cleared);

    // Still within the dwell at t=99 ms.
    assert_eq!(m.observe(true, ms(99)), AlarmTransition::None);
    assert!(!m.is_active());

    // At exactly the dwell deadline it raises.
    assert_eq!(m.observe(true, ms(100)), AlarmTransition::Raised);
    assert!(m.is_active());
    assert_eq!(m.current_severity(), PerceivedSeverity::Major);
}

#[test]
fn fault_clearing_before_dwell_up_does_not_raise() {
    let mut m = machine(AlarmHysteresis::new(ms(100), ms(0)));
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Pending { .. }));
    // Fault disappears before the dwell elapses: back to Clear, no raise.
    assert_eq!(m.observe(false, ms(50)), AlarmTransition::None);
    assert_eq!(m.phase(), Phase::Clear);
    assert!(!m.is_active());
}

#[test]
fn clears_only_after_dwell_down_elapses() {
    // dwell-up 0 (raise instantly), dwell-down 200 ms.
    let mut m = machine(AlarmHysteresis::new(ms(0), ms(200)));
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::Raised);
    assert!(m.is_active());

    // Condition gone at t=1000: enters Clearing, still active.
    assert_eq!(m.observe(false, ms(1000)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Clearing { .. }));
    assert!(m.is_active());
    assert_eq!(m.current_severity(), PerceivedSeverity::Major);

    // Still within dwell-down.
    assert_eq!(m.observe(false, ms(1199)), AlarmTransition::None);
    assert!(m.is_active());

    // Deadline reached: clears.
    assert_eq!(m.observe(false, ms(1200)), AlarmTransition::Cleared);
    assert_eq!(m.phase(), Phase::Clear);
    assert!(!m.is_active());
    assert_eq!(m.current_severity(), PerceivedSeverity::Cleared);
}

#[test]
fn fault_returning_within_dwell_down_snaps_back_to_raised_no_flap() {
    let mut m = machine(AlarmHysteresis::new(ms(0), ms(200)));
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::Raised);
    assert_eq!(m.observe(false, ms(100)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Clearing { .. }));
    // Fault returns mid-dwell: snap back to Raised, never emitting a Cleared.
    assert_eq!(m.observe(true, ms(150)), AlarmTransition::None);
    assert_eq!(m.phase(), Phase::Raised);
    assert!(m.is_active());
}

#[test]
fn latched_alarm_stays_active_until_reset() {
    let mut m = machine(AlarmHysteresis::new(ms(0), ms(0))).latching();
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::Raised);
    assert!(m.is_latched());

    // Condition clears, but a latched alarm holds Raised forever.
    for t in [10, 100, 1000, 100_000] {
        assert_eq!(m.observe(false, ms(t)), AlarmTransition::None);
        assert_eq!(m.phase(), Phase::Raised);
        assert!(m.is_active());
    }

    // Explicit reset returns it to Clear and unlatches.
    m.reset();
    assert_eq!(m.phase(), Phase::Clear);
    assert!(!m.is_latched());
    assert!(!m.is_active());
}

#[test]
fn acknowledge_only_when_active_and_recorded() {
    let mut m = machine(AlarmHysteresis::new(ms(0), ms(0)));
    // Cannot ack a clear alarm.
    m.acknowledge("alice", ms(0));
    assert!(!m.ack().is_acked());

    m.observe(true, ms(0));
    m.acknowledge("bob", ms(5));
    assert!(m.ack().is_acked());

    let rec = m.record(ms(10));
    assert!(rec.ack.is_acked());
    assert_eq!(rec.severity, PerceivedSeverity::Major);
    assert_eq!(rec.kind, AlarmKind::Black);
    assert_eq!(rec.scope, AlarmScope::Tile { index: 0 });
}

#[test]
fn record_dwell_grows_while_active_and_is_zero_when_clear() {
    let mut m = machine(AlarmHysteresis::new(ms(0), ms(0)));
    assert_eq!(m.record(ms(0)).dwell, MediaTime::ZERO);
    m.observe(true, ms(100));
    assert_eq!(m.record(ms(100)).raised_at, ms(100));
    assert_eq!(m.record(ms(450)).dwell, ms(350));
    // Clear it; dwell resets to zero.
    m.observe(false, ms(500));
    assert_eq!(m.record(ms(900)).dwell, MediaTime::ZERO);
}

#[test]
fn backwards_clock_does_not_prematurely_raise() {
    // A non-monotonic step must not shorten the dwell.
    let mut m = machine(AlarmHysteresis::new(ms(100), ms(0)));
    m.observe(true, ms(1000));
    // Clock jumps backwards: elapsed clamps to 0, still pending.
    assert_eq!(m.observe(true, ms(900)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Pending { .. }));
    // Forward past the real deadline raises.
    assert_eq!(m.observe(true, ms(1100)), AlarmTransition::Raised);
}

// ---- Synthetic-fault property: a fault present continuously for >= dwell_up
// raises; absent continuously for >= dwell_down clears; and the machine never
// reports active for a fault shorter than dwell_up (anti-flap). ----
proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn raise_clear_obey_dwells(
        dwell_up_ms in 0_i64..500,
        dwell_down_ms in 0_i64..500,
        fault_len_ms in 1_i64..1000,
        recover_len_ms in 1_i64..1000,
        step_ms in 1_i64..50,
    ) {
        let hys = AlarmHysteresis::new(ms(dwell_up_ms), ms(dwell_down_ms));
        let mut m = machine(hys);

        // Phase 1: inject the fault continuously for `fault_len_ms`. Observe at
        // every `step`, then force an observation exactly at the window end so a
        // dwell deadline between steps is still evaluated while the run holds (the
        // machine acts only when observed).
        let fault_end = fault_len_ms;
        let mut raised_at: Option<i64> = None;
        let mut t = 0_i64;
        while t < fault_end {
            if m.observe(true, ms(t)) == AlarmTransition::Raised {
                raised_at = Some(t);
            }
            t += step_ms;
        }
        if m.observe(true, ms(fault_end)) == AlarmTransition::Raised {
            raised_at = Some(fault_end);
        }

        if fault_len_ms >= dwell_up_ms {
            // The fault was present long enough: it must have raised, and not
            // before the dwell-up deadline.
            prop_assert!(m.is_active(), "fault held >= dwell_up must raise");
            let r = raised_at.expect("must have raised");
            prop_assert!(r >= dwell_up_ms, "raised at {r} before dwell_up {dwell_up_ms}");
        }

        if !m.is_active() {
            // Never raised — nothing more to clear. Done.
            return Ok(());
        }

        // Phase 2: recover continuously for `recover_len_ms`.
        let recover_start = fault_end + 1;
        let recover_end = recover_start + recover_len_ms;
        let mut cleared_at: Option<i64> = None;
        let mut t = recover_start;
        while t < recover_end {
            if m.observe(false, ms(t)) == AlarmTransition::Cleared {
                cleared_at = Some(t);
            }
            t += step_ms;
        }
        if m.observe(false, ms(recover_end)) == AlarmTransition::Cleared {
            cleared_at = Some(recover_end);
        }

        if recover_len_ms >= dwell_down_ms {
            prop_assert!(!m.is_active(), "recovery held >= dwell_down must clear");
            let c = cleared_at.expect("must have cleared");
            prop_assert!(
                c - recover_start >= dwell_down_ms,
                "cleared too early: {} < dwell_down {dwell_down_ms}",
                c - recover_start
            );
        }
    }
}
