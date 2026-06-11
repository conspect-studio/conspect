//! Meter → overlay **draw-data** bridge: turn the pure-DSP meter readings
//! ([`crate::ballistics`], [`crate::correlation`]) into the small, render-ready
//! numeric values the compositor's overlay sub-pass draws as geometry
//! (overlay-rendering.md §4.2: "meters are geometry, not pictures").
//!
//! The engine taps the meters **read-only and off the hot path** (ADR-R006) and
//! conflates them to a low display rate (~30 Hz) — there is no point producing
//! draw-data faster than a screen refreshes, and conflation guarantees the
//! meter tap can never back-pressure the engine (invariant #10). This module is
//! pure: it carries no GPU/overlay dependency and emits plain `f32` deflections
//! and goniometer coordinates that `multiview-compositor`'s meter primitives
//! consume.
//!
//! The dB→deflection *mapping* (the meter scale + peak-hold) lives in the
//! compositor so the CPU reference and any GPU path agree; here we only sample
//! the meters and **conflate** their latest values.

use crate::ballistics::Ballistics;
use crate::correlation::{CorrelationMeter, GonioPoint};

/// The display cadence the meter draw-data is conflated to (Hz). A meter shown
/// faster than this is wasted work and risks coupling the tap to the engine; 30
/// Hz is a smooth, refresh-bounded rate.
pub const DISPLAY_HZ: u32 = 30;

/// One conflated draw-data sample for a single meter channel: the latest
/// reading in **dBFS** (what the compositor maps to a track deflection) plus the
/// channel's [`crate::correlation`] coordinate when stereo metering is active.
///
/// "Conflated" means the engine overwrites this with the newest reading at most
/// [`DISPLAY_HZ`] times a second and never queues older values — the display
/// always reflects *now*, and a slow display consumer drops samples rather than
/// stalling the meter (invariant #10).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterSample {
    /// The latest meter reading in dB relative to full scale.
    pub db: f64,
}

impl MeterSample {
    /// Read the current dBFS value of a [`Ballistics`] meter into a sample.
    #[must_use]
    pub fn from_ballistics(meter: &Ballistics) -> Self {
        Self {
            db: meter.reading_db(),
        }
    }

    /// The reading as an `f32` (the compositor's meter scale works in `f32`).
    #[must_use]
    pub fn db_f32(self) -> f32 {
        db_to_f32(self.db)
    }
}

/// A conflated stereo draw-data sample: the per-channel dBFS levels, the phase
/// correlation in `[-1, +1]`, and the latest goniometer point. Everything the
/// renderer needs to draw a pair of meter bars, a correlation indicator, and a
/// goniometer dot for one display frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoMeterSample {
    /// Left-channel level (dBFS).
    pub left: MeterSample,
    /// Right-channel level (dBFS).
    pub right: MeterSample,
    /// Phase correlation coefficient in `[-1, +1]`.
    pub correlation: f64,
    /// Side (L−R) goniometer coordinate.
    pub gonio_x: f64,
    /// Mid (L+R) goniometer coordinate.
    pub gonio_y: f64,
}

impl StereoMeterSample {
    /// Sample a left/right [`Ballistics`] pair, a [`CorrelationMeter`], and the
    /// latest [`GonioPoint`] into one conflated draw-data value.
    #[must_use]
    pub fn capture(
        left: &Ballistics,
        right: &Ballistics,
        correlation: &CorrelationMeter,
        gonio: GonioPoint,
    ) -> Self {
        Self {
            left: MeterSample::from_ballistics(left),
            right: MeterSample::from_ballistics(right),
            correlation: correlation.correlation(),
            gonio_x: gonio.x,
            gonio_y: gonio.y,
        }
    }

    /// The goniometer point as `(x, y)` `f32` (the compositor draws dots in
    /// `f32`).
    #[must_use]
    pub fn gonio_f32(self) -> (f32, f32) {
        (db_to_f32(self.gonio_x), db_to_f32(self.gonio_y))
    }
}

/// A wall-clock conflator: it accepts meter samples at any rate and yields a new
/// draw-data value at most every `1 / DISPLAY_HZ` of injected media time,
/// dropping (overwriting) the intervening ones. Pure and time-injected so it is
/// deterministically testable and never sleeps.
///
/// `T` is the conflated value (e.g. [`MeterSample`] / [`StereoMeterSample`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conflator<T> {
    /// Minimum nanoseconds between emitted samples (`1 / DISPLAY_HZ`).
    interval_ns: i64,
    /// The next media-time (ns) at which a sample may be emitted.
    next_emit_ns: i64,
    /// The most recent value accepted (always overwritten; never queued).
    latest: Option<T>,
}

impl<T: Copy> Conflator<T> {
    /// A conflator emitting at most [`DISPLAY_HZ`] times a second.
    #[must_use]
    pub fn new() -> Self {
        Self::with_rate(DISPLAY_HZ)
    }

    /// A conflator emitting at most `hz` times a second (clamped to ≥ 1).
    #[must_use]
    pub fn with_rate(hz: u32) -> Self {
        let hz = hz.max(1);
        Self {
            interval_ns: 1_000_000_000 / i64::from(hz),
            next_emit_ns: i64::MIN,
            latest: None,
        }
    }

    /// Overwrite the latest value (the tap always keeps only the newest reading).
    pub fn accept(&mut self, value: T) {
        self.latest = Some(value);
    }

    /// If at least one display interval has elapsed since the last emission,
    /// return the latest value and arm the next interval; otherwise [`None`].
    ///
    /// Conflation in action: many [`Self::accept`] calls between two `poll`s
    /// collapse to the single newest value, so a slow display never causes a
    /// backlog and the meter tap is decoupled from the engine cadence.
    pub fn poll(&mut self, now_ns: i64) -> Option<T> {
        if now_ns < self.next_emit_ns {
            return None;
        }
        let value = self.latest?;
        self.next_emit_ns = now_ns.saturating_add(self.interval_ns);
        Some(value)
    }
}

impl<T: Copy> Default for Conflator<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Narrow a bounded, finite `f64` meter value to `f32` without an `as` cast
/// (the workspace lints deny `as_conversions`).
///
/// There is no `From<f64> for f32`, so we round-trip through the decimal string
/// the formatter produces, which `f32::from_str` parses with the standard
/// round-to-nearest. Meter values (dB readings, unit goniometer coordinates) are
/// small-magnitude and finite, so this is exact to `f32` precision; a non-finite
/// input (impossible for the meters) maps to `0.0`. This runs only on conflated
/// (~30 Hz) draw-data, never on the hot path.
fn db_to_f32(value: f64) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    value
        .to_string()
        .parse::<f32>()
        .unwrap_or(if value > 0.0 { f32::MAX } else { f32::MIN })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]
    use super::*;
    use crate::ballistics::{MeterScale, PeakMode};

    #[test]
    fn sample_reads_the_meter_dbfs() {
        // A silent sample-peak meter reads its floor; the draw-data carries it.
        let meter = Ballistics::new(48_000, MeterScale::SamplePeak(PeakMode::Sample));
        let sample = MeterSample::from_ballistics(&meter);
        approx::assert_abs_diff_eq!(sample.db, Ballistics::FLOOR_DB);
        // db_f32 narrows without an `as` cast.
        approx::assert_abs_diff_eq!(
            f64::from(sample.db_f32()),
            Ballistics::FLOOR_DB,
            epsilon = 1e-3
        );
    }

    #[test]
    fn conflator_collapses_bursts_to_the_newest_value() {
        // At 30 Hz the interval is 1/30 s ≈ 33.33 ms. Many accepts between polls
        // collapse to the single newest value; a poll before the interval yields
        // nothing (the meter tap can never back up — invariant #10).
        let mut conflator: Conflator<MeterSample> = Conflator::with_rate(30);
        let interval = 1_000_000_000_i64 / 30;

        conflator.accept(MeterSample { db: -20.0 });
        conflator.accept(MeterSample { db: -10.0 });
        conflator.accept(MeterSample { db: -6.0 });

        // First poll at t=0 emits the NEWEST accepted value, not the first.
        let first = conflator.poll(0).expect("a value is available at t=0");
        approx::assert_abs_diff_eq!(first.db, -6.0);

        // A poll before the next interval yields nothing.
        assert!(
            conflator.poll(interval - 1).is_none(),
            "conflated: too soon"
        );

        // Accept more in the meantime; the next due poll emits only the newest.
        conflator.accept(MeterSample { db: -3.0 });
        conflator.accept(MeterSample { db: -1.0 });
        let second = conflator.poll(interval).expect("due at one interval");
        approx::assert_abs_diff_eq!(second.db, -1.0, epsilon = 1e-9);
    }

    #[test]
    fn empty_conflator_yields_nothing() {
        let mut conflator: Conflator<MeterSample> = Conflator::new();
        assert!(conflator.poll(0).is_none(), "no samples accepted yet");
    }
}
