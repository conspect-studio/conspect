//! Clock-layer servo telemetry (DEV-C4; ADR-R012, implementing the ADR-M010
//! sync-acceptance gate): the disciplined-reference servo's **offset (ns)** and
//! **frequency correction (ppb)**, plus the display-audio buffer servo's
//! **resample-ratio correction (ppm)**, exported through the dependency-free
//! [`MetricsRegistry`] as lock-free gauges.
//!
//! ## What the acceptance soak reads
//!
//! ADR-M010 gates the frame-accurate sync claim (Tiers S/A/B) on a 24 h soak.
//! Two of its pass conditions are read straight off these gauges:
//!
//! * the disciplined-reference servo offset, with the documented threshold
//!   **99th-percentile `|offset|` ≤ 100 µs (PTP) / ≤ 1 ms (chrony)** over the
//!   24 h window ([`PTP_OFFSET_P99_MAX_NS`] / [`CHRONY_OFFSET_P99_MAX_NS`]);
//! * the servo frequency correction (ppb), a steady value indicating a
//!   converged loop rather than a hunting one.
//!
//! Thresholds are **integer nanoseconds** (invariant #3) — never float
//! seconds — so the soak harness compares the exported percentile against an
//! exact bound.
//!
//! ## Where the values come from (and why telemetry stays a leaf)
//!
//! Like the rest of this crate this module owns only the **model** — the
//! series names, the bounded label scheme, and the lock-free [`Gauge`]
//! handles. The off-hot-path **1 Hz** timing-status publisher (the same task
//! that derives the outbound epoch in `multiview-cli`) reads the
//! `EpochStatus`'s `offset_ns` / `frequency_ppb` / `source` and calls
//! [`ClockServoGauges::record`]; the display-audio sink's servo tick calls
//! [`AudioServoGauges`]. No engine type is referenced here (telemetry is a
//! leaf), and the update path is a single relaxed atomic store, so recording a
//! servo sample never back-pressures the publisher — which itself never
//! back-pressures the engine (invariant #10).
//!
//! ## Conflation
//!
//! Both servos run off the hot path at low rate (the reference servo is
//! sampled at ~1 Hz; the audio buffer servo at the sink's drain cadence). The
//! gauges are **latest-wins** by construction (a gauge holds one value), so
//! there is nothing to conflate beyond writing the newest sample — exactly the
//! drop-oldest discipline invariant #10 requires for any signal that crosses
//! out of the engine.

use crate::metrics::{Gauge, Labels, MetricsRegistry};

/// Metric series names. Public so a Prometheus exporter / test / soak harness
/// can reference them without re-typing the strings.
pub mod names {
    /// Disciplined-reference servo offset (`local − master`), in **nanoseconds**,
    /// labelled by `source` (`ptp`/`system`). The soak's pass condition reads
    /// the 99th-percentile of `|offset|` against [`super::PTP_OFFSET_P99_MAX_NS`]
    /// / [`super::CHRONY_OFFSET_P99_MAX_NS`].
    pub const CLOCK_OFFSET_NS: &str = "multiview_clock_servo_offset_nanoseconds";
    /// Disciplined-reference servo frequency correction, in **parts per
    /// billion**, labelled by `source`. Positive = the local clock runs fast.
    pub const CLOCK_FREQUENCY_PPB: &str = "multiview_clock_servo_frequency_ppb";
    /// Display-audio buffer servo resample-ratio correction, in **parts per
    /// million**, labelled by `sink`.
    pub const AUDIO_RESAMPLE_PPM: &str = "multiview_audio_servo_resample_ppm";
    /// Display-audio FIFO fill fraction (`0.0..=1.0`), labelled by `sink`.
    pub const AUDIO_FIFO_FILL_FRACTION: &str = "multiview_audio_servo_fifo_fill_fraction";
    /// Display-audio sample-vs-scanout skew, in **milliseconds** (positive =
    /// audio ahead of the flip clock), labelled by `sink`.
    pub const AUDIO_SKEW_MS: &str = "multiview_audio_servo_skew_milliseconds";
}

/// The disciplined-reference soak pass threshold for a **PTP** reference:
/// 99th-percentile `|offset|` ≤ **100 µs** over the 24 h window, in integer
/// nanoseconds (ADR-M010 / display-out §10.5; ptp4l software timestamping on a
/// quiet `GbE` LAN measures ±5–50 µs typical, spec'd as ±100 µs guaranteed).
pub const PTP_OFFSET_P99_MAX_NS: i64 = 100_000;

/// The disciplined-reference soak pass threshold for a **chrony/NTP** system
/// reference: 99th-percentile `|offset|` ≤ **1 ms** over the 24 h window, in
/// integer nanoseconds (ADR-M010; chrony lands ~0.5–1 ms on a `GbE` LAN —
/// ~1/30 of a 60 Hz frame, still frame-accurate Tier B). The PTP bound is
/// exactly one tenth of this — "PTP upgrades the tier" (ADR-M010).
pub const CHRONY_OFFSET_P99_MAX_NS: i64 = 1_000_000;

/// The acceptance soak window, in seconds: **24 hours** (ADR-M010).
pub const SOAK_WINDOW_SECS: u64 = 24 * 60 * 60;

/// The honest `source` label for a disciplined-reference offset/ppb series —
/// which leg currently disciplines the published epoch (ADR-T012). Bounded to
/// the two ADR-M010 reference classes so cardinality stays fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClockSourceLabel {
    /// The ST 2059-2 PTP servo (PHC-disciplined).
    Ptp,
    /// chrony/NTP-disciplined (or undisciplined) system time.
    System,
}

impl ClockSourceLabel {
    /// The bounded, stable lower-case label for this source.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ClockSourceLabel::Ptp => "ptp",
            ClockSourceLabel::System => "system",
        }
    }

    /// The 99th-percentile `|offset|` soak pass bound (integer ns) for this
    /// reference class: the tighter PTP bound, or the chrony/NTP bound for the
    /// system leg.
    #[must_use]
    pub const fn offset_p99_max_ns(self) -> i64 {
        match self {
            ClockSourceLabel::Ptp => PTP_OFFSET_P99_MAX_NS,
            ClockSourceLabel::System => CHRONY_OFFSET_P99_MAX_NS,
        }
    }
}

/// The registered disciplined-reference servo gauges (offset + ppb), one pair
/// per `source` leg.
///
/// Both the `ptp` and `system` legs are registered up-front, so a reference
/// transition mid-run never makes a dashboard series disappear; the leg that
/// is not currently selected simply holds its last value (the publisher writes
/// only the live leg each cycle).
#[derive(Debug, Clone)]
pub struct ClockServoGauges {
    ptp_offset: Gauge,
    ptp_ppb: Gauge,
    system_offset: Gauge,
    system_ppb: Gauge,
}

impl ClockServoGauges {
    /// Register the offset (ns) + frequency (ppb) gauges for both source legs
    /// against `registry`.
    ///
    /// Re-registering the same `(name, labels)` returns the existing handle, so
    /// this is idempotent.
    #[must_use]
    pub fn register(registry: &MetricsRegistry) -> Self {
        let source = |value: &str| Labels::new().with("source", value);
        Self {
            ptp_offset: registry.gauge(names::CLOCK_OFFSET_NS, source("ptp")),
            ptp_ppb: registry.gauge(names::CLOCK_FREQUENCY_PPB, source("ptp")),
            system_offset: registry.gauge(names::CLOCK_OFFSET_NS, source("system")),
            system_ppb: registry.gauge(names::CLOCK_FREQUENCY_PPB, source("system")),
        }
    }

    /// Publish one servo sample for the currently-selected `source`: its
    /// smoothed offset (`local − master`, ns) and frequency correction (ppb).
    ///
    /// Only the named leg is updated; the other holds its last value. The
    /// conversions to `f64` are lossless for any realistic servo offset
    /// (sub-second ns) and frequency (ppb fits comfortably in an f64 mantissa).
    pub fn record(&self, source: ClockSourceLabel, offset_ns: i64, frequency_ppb: i64) {
        let (offset, ppb) = match source {
            ClockSourceLabel::Ptp => (&self.ptp_offset, &self.ptp_ppb),
            ClockSourceLabel::System => (&self.system_offset, &self.system_ppb),
        };
        offset.set(i64_to_f64(offset_ns));
        ppb.set(i64_to_f64(frequency_ppb));
    }

    /// The current published offset (ns) for a leg (test/telemetry convenience).
    #[must_use]
    pub fn offset_ns(&self, source: ClockSourceLabel) -> f64 {
        match source {
            ClockSourceLabel::Ptp => self.ptp_offset.get(),
            ClockSourceLabel::System => self.system_offset.get(),
        }
    }
}

/// The registered display-audio buffer-servo gauges for one sink: the resample
/// ppm the PI controller is demanding, plus the two error inputs (FIFO fill
/// fraction, sample-vs-scanout skew) for diagnosing a hunting loop.
#[derive(Debug, Clone)]
pub struct AudioServoGauges {
    resample_ppm: Gauge,
    fill_fraction: Gauge,
    skew_ms: Gauge,
}

impl AudioServoGauges {
    /// Register the audio buffer-servo gauges for the named `sink` (a stable,
    /// bounded identifier such as the ALSA device, e.g. `hdmi0`).
    #[must_use]
    pub fn register(registry: &MetricsRegistry, sink: impl Into<String>) -> Self {
        let sink = sink.into();
        let labels = || Labels::new().with("sink", sink.clone());
        Self {
            resample_ppm: registry.gauge(names::AUDIO_RESAMPLE_PPM, labels()),
            fill_fraction: registry.gauge(names::AUDIO_FIFO_FILL_FRACTION, labels()),
            skew_ms: registry.gauge(names::AUDIO_SKEW_MS, labels()),
        }
    }

    /// Publish the servo's current resample-ratio correction in ppm.
    pub fn record_resample_ppm(&self, ppm: f64) {
        self.resample_ppm.set(ppm);
    }

    /// Publish the current FIFO fill fraction (`0.0..=1.0`).
    pub fn record_fill_fraction(&self, fraction: f64) {
        self.fill_fraction.set(fraction);
    }

    /// Publish the current sample-vs-scanout skew in milliseconds (positive =
    /// audio ahead of the flip clock).
    pub fn record_skew_ms(&self, skew_ms: f64) {
        self.skew_ms.set(skew_ms);
    }
}

/// Convert an `i64` servo measurement to the gauge's `f64` storage. Servo
/// offsets are sub-second ns and frequencies are small ppb, both far inside the
/// 2^53 exactly-representable range — `i32::MAX`-class magnitudes round-trip
/// exactly and the largest plausible value (a multi-day-stepped offset) loses
/// at most sub-ns precision, irrelevant at telemetry grade.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: no fallible `From<i64> for f64`; the values are telemetry-grade and
// far below 2^53 for any real servo reading.
const fn i64_to_f64(value: i64) -> f64 {
    value as f64
}

#[cfg(test)]
// reason: every gauge value asserted here is an exact integer (0, 500_000)
// round-tripped through `i64_to_f64`, which is lossless for these magnitudes —
// so an exact `assert_eq!` is correct, not flaky float comparison.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn record_writes_only_the_named_leg() {
        let reg = MetricsRegistry::new();
        let gauges = ClockServoGauges::register(&reg);
        gauges.record(ClockSourceLabel::System, 500_000, -2);
        assert_eq!(gauges.offset_ns(ClockSourceLabel::System), 500_000.0);
        // The ptp leg is untouched: still its initial zero.
        assert_eq!(gauges.offset_ns(ClockSourceLabel::Ptp), 0.0);
    }

    #[test]
    fn offset_p99_max_ns_picks_the_right_bound_per_source() {
        assert_eq!(
            ClockSourceLabel::Ptp.offset_p99_max_ns(),
            PTP_OFFSET_P99_MAX_NS
        );
        assert_eq!(
            ClockSourceLabel::System.offset_p99_max_ns(),
            CHRONY_OFFSET_P99_MAX_NS
        );
    }

    #[test]
    fn register_is_idempotent() {
        let reg = MetricsRegistry::new();
        let _a = ClockServoGauges::register(&reg);
        let _b = ClockServoGauges::register(&reg);
        // Two offset series (ptp + system) and two ppb series, no duplicates.
        let offset_series = reg
            .series()
            .into_iter()
            .filter(|s| s.name == names::CLOCK_OFFSET_NS)
            .count();
        assert_eq!(offset_series, 2, "one offset series per source, deduped");
    }
}
