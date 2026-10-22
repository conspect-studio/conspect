//! Acceptance-soak analyzer (DEV-C4; ADR-R012, implementing the ADR-M010
//! sync-acceptance gate): the pure pass/fail logic the
//! `scripts/soak-acceptance.sh` harness runs over a captured metrics series at
//! the end of a run. It is intentionally dependency-free and side-effect-free so
//! the same code is unit-tested in CI and invoked for real (via `cargo xtask
//! soak-report`) against a hardware soak capture.
//!
//! Two checks make up a verdict:
//!
//! * **Disciplined-offset percentile** — the 99th-percentile of `|offset|` per
//!   clock-source leg, compared against the per-source bound from
//!   [`ClockSourceLabel::offset_p99_max_ns`] (PTP 100 µs / chrony 1 ms over the
//!   24 h [`super::clock::SOAK_WINDOW_SECS`] window). Read straight off the
//!   [`super::clock::names::CLOCK_OFFSET_NS`] gauge samples.
//! * **Cadence continuity (invariant #1)** — across a deliberate PTP/WS kill
//!   window, the output-tick counter must keep advancing at least the per-sample
//!   floor. A healthy node free-runs on the held epoch; a stalled output clock
//!   shows a flat delta and fails. This is the chaos extension's assertion.

use crate::clock::ClockSourceLabel;

/// The nearest-rank 99th-percentile of `|offset|` over `samples`, in nanoseconds
/// (`None` for an empty series). Nearest-rank with `ceil(0.99 · n)` matches the
/// ADR-M010 "99th-pct |offset|" pass condition exactly for any sample count.
#[must_use]
pub fn p99_abs_offset_ns(samples: &[i64]) -> Option<i64> {
    if samples.is_empty() {
        return None;
    }
    let mut abs: Vec<u64> = samples.iter().map(|s| s.unsigned_abs()).collect();
    abs.sort_unstable();
    let n = abs.len();
    // rank = ceil(0.99 · n) in integer maths; index = rank − 1 (1-based → 0-based).
    let rank = (99usize.saturating_mul(n).saturating_add(99)) / 100;
    let index = rank.saturating_sub(1).min(n.saturating_sub(1));
    abs.get(index)
        .map(|v| i64::try_from(*v).unwrap_or(i64::MAX))
}

/// The per-leg offset verdict: the measured p99 of `|offset|`, the source's
/// threshold, and whether it passed (boundary is inclusive — `p99 == threshold`
/// passes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetVerdict {
    /// The disciplined clock-source leg this verdict covers.
    pub source: ClockSourceLabel,
    /// The measured 99th-percentile of `|offset|`, in nanoseconds.
    pub p99_abs_ns: i64,
    /// The per-source pass threshold, in nanoseconds.
    pub threshold_ns: i64,
    /// Whether `p99_abs_ns <= threshold_ns`.
    pub pass: bool,
    /// The number of offset samples the verdict was computed over.
    pub samples: usize,
}

/// Evaluate one clock-source leg's offset series against its threshold (`None`
/// for an empty series — a leg with no samples produces no verdict).
#[must_use]
pub fn evaluate_offset(source: ClockSourceLabel, samples: &[i64]) -> Option<OffsetVerdict> {
    let p99_abs_ns = p99_abs_offset_ns(samples)?;
    let threshold_ns = source.offset_p99_max_ns();
    Some(OffsetVerdict {
        source,
        p99_abs_ns,
        threshold_ns,
        pass: p99_abs_ns <= threshold_ns,
        samples: samples.len(),
    })
}

/// Invariant-#1 chaos assertion: given monotonic output-tick counts sampled at a
/// fixed wall interval, every consecutive window must have advanced at least
/// `expected_min_delta` ticks. A PTP/WS kill landing inside the series must NOT
/// produce a stall (flat delta) — the output clock free-runs regardless. Fewer
/// than two samples is vacuously continuous.
#[must_use]
pub fn cadence_uninterrupted(tick_samples: &[u64], expected_min_delta: u64) -> bool {
    tick_samples.windows(2).all(|w| match w {
        [prev, next] => next.saturating_sub(*prev) >= expected_min_delta,
        _ => true,
    })
}

/// The aggregate soak verdict: every offset leg plus the cadence-continuity
/// check. A report passes only when the cadence held, at least one leg was
/// measured, and every measured leg passed its threshold.
#[derive(Debug, Default)]
pub struct SoakReport {
    offsets: Vec<OffsetVerdict>,
    cadence_ok: Option<bool>,
}

impl SoakReport {
    /// Record one clock-source leg's offset verdict.
    pub fn add_offset(&mut self, verdict: OffsetVerdict) {
        self.offsets.push(verdict);
    }

    /// Record the invariant-#1 cadence-continuity result for the chaos window.
    pub fn set_cadence(&mut self, ok: bool) {
        self.cadence_ok = Some(ok);
    }

    /// The per-leg offset verdicts recorded so far.
    #[must_use]
    pub fn offsets(&self) -> &[OffsetVerdict] {
        &self.offsets
    }

    /// Whether the cadence-continuity check has been recorded and held.
    #[must_use]
    pub fn cadence_ok(&self) -> Option<bool> {
        self.cadence_ok
    }

    /// The overall soak verdict: cadence held, ≥1 leg measured, every leg passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.cadence_ok == Some(true)
            && !self.offsets.is_empty()
            && self.offsets.iter().all(|v| v.pass)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn p99_single_sample_is_that_sample() {
        assert_eq!(p99_abs_offset_ns(&[42]), Some(42));
    }

    #[test]
    fn cadence_is_vacuously_continuous_for_short_series() {
        assert!(cadence_uninterrupted(&[], 30));
        assert!(cadence_uninterrupted(&[5], 30));
    }
}
