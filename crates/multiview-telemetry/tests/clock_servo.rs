//! Integration tests for the clock-layer servo telemetry (DEV-C4).
//!
//! These pin the metric model the acceptance soak reads: the disciplined
//! reference servo's offset (ns) and frequency correction (ppb), labelled by
//! the honest `source`, plus the display-audio buffer servo's resample-ratio
//! correction (ppm). The documented pass thresholds are exported as constants
//! the harness compares the 99th-percentile |offset| against.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_telemetry::clock::{
    names, AudioServoGauges, ClockServoGauges, ClockSourceLabel, CHRONY_OFFSET_P99_MAX_NS,
    PTP_OFFSET_P99_MAX_NS, SOAK_WINDOW_SECS,
};
use multiview_telemetry::metrics::{Labels, MetricKind, MetricsRegistry};

#[test]
fn registers_the_clock_servo_series_set() {
    let reg = MetricsRegistry::new();
    let _ = ClockServoGauges::register(&reg);
    let descriptors = reg.series();
    let names: Vec<&str> = descriptors.iter().map(|s| s.name.as_str()).collect();
    for expected in [names::CLOCK_OFFSET_NS, names::CLOCK_FREQUENCY_PPB] {
        assert!(names.contains(&expected), "must register {expected}");
    }
    // Both the ptp and system source legs are registered up-front so a
    // dashboard never shows a missing series when a reference transition
    // happens mid-run.
    for d in &descriptors {
        assert_eq!(d.kind, MetricKind::Gauge, "servo signals are gauges");
    }
}

#[test]
fn offset_and_frequency_are_recorded_per_source() {
    let reg = MetricsRegistry::new();
    let gauges = ClockServoGauges::register(&reg);

    gauges.record(ClockSourceLabel::Ptp, -42_000, 17);

    let ptp_offset = reg.gauge(names::CLOCK_OFFSET_NS, Labels::new().with("source", "ptp"));
    let ptp_ppb = reg.gauge(
        names::CLOCK_FREQUENCY_PPB,
        Labels::new().with("source", "ptp"),
    );
    assert_eq!(ptp_offset.get(), -42_000.0);
    assert_eq!(ptp_ppb.get(), 17.0);

    // The system leg is untouched and stays at its initial zero.
    let sys_offset = reg.gauge(
        names::CLOCK_OFFSET_NS,
        Labels::new().with("source", "system"),
    );
    assert_eq!(sys_offset.get(), 0.0);
}

#[test]
fn recording_a_source_does_not_disturb_the_other_leg() {
    let reg = MetricsRegistry::new();
    let gauges = ClockServoGauges::register(&reg);
    gauges.record(ClockSourceLabel::Ptp, 10, 1);
    gauges.record(ClockSourceLabel::System, 900_000, -3);

    let ptp_offset = reg.gauge(names::CLOCK_OFFSET_NS, Labels::new().with("source", "ptp"));
    let sys_offset = reg.gauge(
        names::CLOCK_OFFSET_NS,
        Labels::new().with("source", "system"),
    );
    assert_eq!(ptp_offset.get(), 10.0, "ptp leg keeps its own value");
    assert_eq!(sys_offset.get(), 900_000.0, "system leg is independent");
}

#[test]
fn clock_source_label_is_stable_and_bounded() {
    assert_eq!(ClockSourceLabel::Ptp.label(), "ptp");
    assert_eq!(ClockSourceLabel::System.label(), "system");
}

#[test]
fn pass_thresholds_match_the_adr_m010_acceptance() {
    // ADR-M010 / display-out §10.5: 99th-pct |offset| <= 100 us (PTP) /
    // <= 1 ms (chrony) over a 24 h soak. Integer ns, never float.
    assert_eq!(PTP_OFFSET_P99_MAX_NS, 100_000);
    assert_eq!(CHRONY_OFFSET_P99_MAX_NS, 1_000_000);
    assert_eq!(SOAK_WINDOW_SECS, 24 * 60 * 60);
    // The PTP bound is exactly one tenth of the chrony bound, the documented
    // "PTP upgrades the tier" relationship.
    assert_eq!(CHRONY_OFFSET_P99_MAX_NS, PTP_OFFSET_P99_MAX_NS * 10);
}

#[test]
fn audio_servo_gauge_records_resample_ppm_per_sink() {
    let reg = MetricsRegistry::new();
    let gauges = AudioServoGauges::register(&reg, "hdmi0");
    gauges.record_resample_ppm(-12.5);
    gauges.record_fill_fraction(0.5);
    gauges.record_skew_ms(2.0);

    let ppm = reg.gauge(
        names::AUDIO_RESAMPLE_PPM,
        Labels::new().with("sink", "hdmi0"),
    );
    let fill = reg.gauge(
        names::AUDIO_FIFO_FILL_FRACTION,
        Labels::new().with("sink", "hdmi0"),
    );
    let skew = reg.gauge(names::AUDIO_SKEW_MS, Labels::new().with("sink", "hdmi0"));
    assert_eq!(ppm.get(), -12.5);
    assert_eq!(fill.get(), 0.5);
    assert_eq!(skew.get(), 2.0);
}
