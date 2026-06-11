//! Integration seam between the declarative config probe layer
//! (`multiview_config::probe`) and the engine X.733 alarm state machine
//! (ADR-MV001).
//!
//! These tests pin the contract that a config-authored [`Probe`] — its
//! millisecond [`Dwell`] windows, its `ProbeKind`, its `severity`, its `latched`
//! flag and the cell it watches — is turned faithfully into a driveable
//! [`AlarmStateMachine`]. This is the wiring the probe-stub modules used to defer
//! to "a later wave".
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::probe::{DetectionZone, Dwell, Probe, ProbeKind};
use multiview_core::alarm::{AlarmKind, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;
use multiview_engine::alarm::state::{AlarmHysteresis, AlarmStateMachine, AlarmTransition, Phase};

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

fn black_probe(dwell: Dwell, severity: PerceivedSeverity, latched: bool) -> Probe {
    Probe::new(
        "probe-1",
        "cam-1",
        ProbeKind::black(16, DetectionZone::default()),
        dwell,
        severity,
        latched,
    )
}

#[test]
fn hysteresis_from_dwell_converts_milliseconds_to_media_time() {
    let hys = AlarmHysteresis::from_dwell(Dwell::new(250, 750));
    assert_eq!(hys.dwell_up(), ms(250));
    assert_eq!(hys.dwell_down(), ms(750));
}

#[test]
fn from_probe_maps_kind_scope_severity_and_dwell() {
    let probe = black_probe(Dwell::new(100, 200), PerceivedSeverity::Major, false);
    let m = AlarmStateMachine::from_probe(&probe);

    // Identity, taxonomy and scope are carried from the declaration.
    assert_eq!(m.id().as_str(), "probe-1");
    assert_eq!(m.kind(), AlarmKind::Black);
    assert_eq!(
        m.scope(),
        &AlarmScope::Probe {
            id: "probe-1".to_owned()
        }
    );
    // A fresh machine is clear and reports the cleared severity.
    assert_eq!(m.phase(), Phase::Clear);
    assert_eq!(m.current_severity(), PerceivedSeverity::Cleared);
    assert!(!m.is_latched());
}

#[test]
fn from_probe_respects_the_declared_dwell_up() {
    let probe = black_probe(Dwell::new(100, 0), PerceivedSeverity::Major, false);
    let mut m = AlarmStateMachine::from_probe(&probe);

    // Condition present from t=0: still pending until 100 ms elapse.
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Pending { .. }));
    assert_eq!(m.observe(true, ms(99)), AlarmTransition::None);
    assert!(!m.is_active());

    // At the dwell-up boundary it raises at the declared severity.
    assert_eq!(m.observe(true, ms(100)), AlarmTransition::Raised);
    assert!(m.is_active());
    assert_eq!(m.current_severity(), PerceivedSeverity::Major);
}

#[test]
fn from_probe_respects_the_declared_dwell_down() {
    let probe = black_probe(Dwell::new(0, 300), PerceivedSeverity::Minor, false);
    let mut m = AlarmStateMachine::from_probe(&probe);

    // Zero dwell-up: raises on the first present sample.
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::Raised);
    assert!(m.is_active());

    // Condition clears at t=10 ms but holds Raised through the 300 ms dwell-down.
    assert_eq!(m.observe(false, ms(10)), AlarmTransition::None);
    assert!(matches!(m.phase(), Phase::Clearing { .. }));
    assert_eq!(m.observe(false, ms(309)), AlarmTransition::None);
    assert!(m.is_active());

    // At the dwell-down boundary (since=10) it clears.
    assert_eq!(m.observe(false, ms(310)), AlarmTransition::Cleared);
    assert_eq!(m.phase(), Phase::Clear);
    assert_eq!(m.current_severity(), PerceivedSeverity::Cleared);
}

#[test]
fn from_probe_latched_holds_through_clear() {
    let probe = black_probe(Dwell::new(0, 0), PerceivedSeverity::Critical, true);
    let mut m = AlarmStateMachine::from_probe(&probe);
    assert!(!m.is_latched());

    // Raise (zero dwell-up).
    assert_eq!(m.observe(true, ms(0)), AlarmTransition::Raised);
    assert!(m.is_latched());

    // The condition clears but the latch holds the alarm active despite zero
    // dwell-down — only an explicit reset clears a latched alarm.
    assert_eq!(m.observe(false, ms(50)), AlarmTransition::None);
    assert!(m.is_active());
    assert_eq!(m.observe(false, ms(1_000)), AlarmTransition::None);
    assert!(m.is_active());

    m.reset();
    assert_eq!(m.phase(), Phase::Clear);
    assert!(!m.is_latched());
}

#[test]
fn from_probe_maps_each_probe_kind_to_its_alarm_kind() {
    let cases = [
        (
            ProbeKind::black(16, DetectionZone::default()),
            AlarmKind::Black,
        ),
        (
            ProbeKind::freeze(5, DetectionZone::default()),
            AlarmKind::Freeze,
        ),
        (ProbeKind::silence(-60.0), AlarmKind::Silence),
    ];
    for (kind, expected) in cases {
        let probe = Probe::new(
            "p",
            "c",
            kind,
            Dwell::default(),
            PerceivedSeverity::Warning,
            false,
        );
        assert_eq!(AlarmStateMachine::from_probe(&probe).kind(), expected);
    }
}
