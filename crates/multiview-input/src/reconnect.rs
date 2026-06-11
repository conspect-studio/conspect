//! Supervised-reconnect backoff policy.
//!
//! When an ingest source fails, the supervisor re-establishes it with capped
//! exponential backoff plus **full jitter** — the AWS-recommended scheme that
//! spreads reconnection attempts across many concurrent inputs to avoid a
//! thundering herd. The delay before attempt `n` is drawn from
//! `[0, ceiling_n]`, where `ceiling_n = min(max, base * factor^(n-1))`.
//!
//! The jitter source is **injected** and integer-valued: [`Backoff::next_delay`]
//! takes a raw value in `[0, JITTER_SCALE]`, so the policy is exact integer math
//! and is deterministically testable (no float rounding in the contract). A live
//! supervisor passes a fresh uniform random value each call (e.g.
//! `rng.gen_range(0..=JITTER_SCALE)`); tests pass fixed values.
use core::time::Duration;

/// Full-scale value for the injected jitter fraction. A `jitter` argument of
/// `JITTER_SCALE` selects the full ceiling; `0` selects no delay; intermediate
/// values interpolate linearly. Values above `JITTER_SCALE` are clamped.
pub const JITTER_SCALE: u64 = 1_000_000;

/// Backoff tuning.
///
/// A plain configuration record: callers construct it with all fields (or via
/// [`BackoffConfig::default`]). It is intentionally *not* `#[non_exhaustive]`
/// so callers and tests can build it directly.
#[derive(Debug, Clone, Copy)]
pub struct BackoffConfig {
    /// The base (first-attempt) ceiling.
    pub base: Duration,
    /// The maximum ceiling; the exponential schedule is clamped to this.
    pub max: Duration,
    /// The multiplicative growth factor (typically `2`). Values below `1` are
    /// treated as `1` (no growth).
    pub factor: u32,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(250),
            max: Duration::from_secs(30),
            factor: 2,
        }
    }
}

/// Capped exponential backoff with full jitter.
///
/// Construct with [`Backoff::new`]; call [`Backoff::next_delay`] on each failed
/// attempt to obtain the wait before retrying, and [`Backoff::reset`] once a
/// connection succeeds.
#[derive(Debug)]
pub struct Backoff {
    config: BackoffConfig,
    attempts: u32,
}

impl Backoff {
    /// Construct a backoff policy with the given configuration.
    #[must_use]
    pub fn new(config: BackoffConfig) -> Self {
        Self {
            config,
            attempts: 0,
        }
    }

    /// The number of failed attempts recorded since the last [`Backoff::reset`].
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Reset the schedule after a successful connection: the next
    /// [`Backoff::next_delay`] starts again at the base ceiling.
    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    /// The current ceiling (without consuming an attempt): `min(max, base *
    /// factor^attempts)`, saturating so growth never overflows.
    #[must_use]
    pub fn ceiling(&self) -> Duration {
        nanos_to_duration(self.ceiling_ns())
    }

    /// The current ceiling in nanoseconds, saturating at the configured max.
    fn ceiling_ns(&self) -> u64 {
        let base_ns = duration_to_nanos(self.config.base);
        let max_ns = duration_to_nanos(self.config.max);
        let factor = u64::from(self.config.factor.max(1));
        let mut ceiling = base_ns;
        for _ in 0..self.attempts {
            ceiling = ceiling.saturating_mul(factor);
            if ceiling >= max_ns {
                return max_ns;
            }
        }
        ceiling.min(max_ns)
    }

    /// Record one failed attempt and return the delay before the next retry.
    ///
    /// `jitter` selects a point within the full-jitter window `[0, ceiling]`,
    /// scaled by [`JITTER_SCALE`] (so `jitter == JITTER_SCALE` is the full
    /// ceiling, `0` is zero delay). Values above [`JITTER_SCALE`] are clamped.
    /// Pass a fresh uniform random value in production; pass a fixed value in
    /// tests for determinism.
    #[must_use]
    pub fn next_delay(&mut self, jitter: u64) -> Duration {
        let ceiling_ns = self.ceiling_ns();
        self.attempts = self.attempts.saturating_add(1);
        let frac = jitter.min(JITTER_SCALE);
        // delay = ceiling * frac / JITTER_SCALE, computed in u128 (exact, no
        // overflow for any u64 ceiling and u64 frac <= JITTER_SCALE).
        let delay_ns =
            u128::from(ceiling_ns).saturating_mul(u128::from(frac)) / u128::from(JITTER_SCALE);
        let delay_ns = u64::try_from(delay_ns).unwrap_or(u64::MAX);
        nanos_to_duration(delay_ns)
    }
}

/// Convert a [`Duration`] to nanoseconds, saturating at `u64::MAX`.
fn duration_to_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Convert a nanosecond count to a [`Duration`].
fn nanos_to_duration(ns: u64) -> Duration {
    Duration::from_nanos(ns)
}
