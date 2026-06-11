//! Pure availability / error-second accounting for a monitoring point.
//!
//! Broadcast NMS and SLA reporting expect per-source availability accounting in
//! the style of ITU-T G.826 / G.821: count seconds of service, the subset that
//! were *alarmed*, the subset that were *errored* (any active fault), and the
//! subset that were *severely errored* (a service-affecting fault). From those a
//! simple **availability ratio** falls out.
//!
//! This module owns only the **pure counter model** — no clock, no thread, no
//! I/O. The caller (the engine's monitoring loop, off the protected output path)
//! advances it one second at a time with the current rolled-up
//! [`PerceivedSeverity`] for the monitored object, and reads an immutable
//! [`AvailabilitySnapshot`] whenever it exports metrics. Handles are cheap
//! [`Arc`] clones over atomics, mirroring [`crate::metrics`], so reporting never
//! locks or back-pressures (invariant #10).
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_core::alarm::PerceivedSeverity;

/// A clonable handle to a set of availability counters.
///
/// Each [`tick`](AvailabilityCounters::tick) records one elapsed second at a
/// given severity; clones share the same underlying storage, so several
/// subsystems can observe the same accumulated totals.
#[derive(Debug, Clone, Default)]
pub struct AvailabilityCounters {
    inner: Arc<Inner>,
}

/// The atomic storage behind [`AvailabilityCounters`].
#[derive(Debug, Default)]
struct Inner {
    /// Total in-service seconds observed (every `tick`).
    uptime: AtomicU64,
    /// Seconds during which any alarm was active (severity != `Cleared`).
    alarmed: AtomicU64,
    /// Errored seconds: seconds with any active fault (equal to `alarmed` here,
    /// kept distinct so the two can diverge if "alarm" later includes
    /// non-errored advisory states).
    errored: AtomicU64,
    /// Severely-errored seconds: seconds with a service-affecting fault
    /// (`Major` or `Critical`).
    severely_errored: AtomicU64,
}

impl AvailabilityCounters {
    /// Create a fresh, all-zero set of counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record **one** elapsed second observed at the given rolled-up severity.
    pub fn tick(&self, severity: PerceivedSeverity) {
        self.tick_n(severity, 1);
    }

    /// Record `seconds` elapsed seconds observed at the given rolled-up
    /// severity. Saturating, so a long run can never wrap a counter.
    pub fn tick_n(&self, severity: PerceivedSeverity, seconds: u64) {
        saturating_add(&self.inner.uptime, seconds);
        if severity.is_active() {
            saturating_add(&self.inner.alarmed, seconds);
            saturating_add(&self.inner.errored, seconds);
            if is_severely_errored(severity) {
                saturating_add(&self.inner.severely_errored, seconds);
            }
        }
    }

    /// Take an immutable point-in-time snapshot.
    #[must_use]
    pub fn snapshot(&self) -> AvailabilitySnapshot {
        AvailabilitySnapshot {
            uptime_seconds: self.inner.uptime.load(Ordering::Relaxed),
            alarm_seconds: self.inner.alarmed.load(Ordering::Relaxed),
            error_seconds: self.inner.errored.load(Ordering::Relaxed),
            severely_errored_seconds: self.inner.severely_errored.load(Ordering::Relaxed),
        }
    }
}

/// Saturating add into one counter (lock-free).
fn saturating_add(counter: &AtomicU64, delta: u64) {
    // `fetch_update` lets us saturate rather than wrap; the closure is pure and
    // the CAS retries on contention.
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
    });
}

/// Whether a severity represents a **service-affecting** (severely-errored)
/// fault: `Major` or `Critical`. `Warning`/`Minor`/`Indeterminate` are errored
/// but not severely errored; `Cleared` is neither.
const fn is_severely_errored(severity: PerceivedSeverity) -> bool {
    matches!(
        severity,
        PerceivedSeverity::Major | PerceivedSeverity::Critical
    )
}

/// An immutable snapshot of the availability counters.
///
/// `#[non_exhaustive]` so new accounting fields can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AvailabilitySnapshot {
    /// Total in-service seconds observed.
    pub uptime_seconds: u64,
    /// Seconds during which an alarm was active.
    pub alarm_seconds: u64,
    /// Errored seconds (seconds with any active fault).
    pub error_seconds: u64,
    /// Severely-errored seconds (seconds with a service-affecting fault).
    pub severely_errored_seconds: u64,
}

impl AvailabilitySnapshot {
    /// The availability ratio in `0.0..=1.0`: the fraction of observed seconds
    /// that were **not** unavailable, where an unavailable second is a
    /// severely-errored second.
    ///
    /// With no observed time at all the object is treated as fully available
    /// (`1.0`) rather than dividing by zero.
    #[must_use]
    pub fn availability_ratio(&self) -> f64 {
        if self.uptime_seconds == 0 {
            return 1.0;
        }
        let available = self
            .uptime_seconds
            .saturating_sub(self.severely_errored_seconds);
        // u64 -> f64 widening: the magnitudes here (seconds of uptime) are far
        // below f64's exact-integer range, and we avoid a lossy `as` cast.
        let available_f = round_to_f64(available);
        let total_f = round_to_f64(self.uptime_seconds);
        if total_f == 0.0 {
            return 1.0;
        }
        available_f / total_f
    }
}

/// Convert a `u64` second-count to `f64` without an `as` cast.
///
/// `u32::try_from` splits the value so each half goes through the lossless
/// `f64::from(u32)`; the recombination is exact for the second-magnitudes we
/// track (always well within `2^53`).
fn round_to_f64(value: u64) -> f64 {
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}
