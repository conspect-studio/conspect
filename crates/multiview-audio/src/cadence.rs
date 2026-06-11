//! The per-output-tick audio sample budget (AUD-3, [`SampleClock`]).
//!
//! The output clock emits one video frame per fixed tick (invariant #1). Audio
//! must ride that same clock: each tick the program bus advances by exactly
//! `sample_rate / fps` samples. For integer-divisible rates that is a constant
//! (25 fps @ 48 kHz = 1920), but for the NTSC `1001`-denominator family it is
//! fractional (30000/1001 @ 48 kHz = 1601.6 samples/tick), so a single tick must
//! emit a whole number of samples while the **long-run** total stays an exact
//! function of the tick count — the audio analogue of invariant #3's "never
//! float fps". [`SampleClock`] does this with pure integer (rational) remainder
//! accumulation: never a float, never an `as` truncation, so the cumulative
//! sample position tracks the ideal real position to within one sample forever
//! and never drifts (ADR-R005 §4.1).

use multiview_core::time::Rational;

/// The per-tick audio sample-budget accumulator.
///
/// Construct one per run with [`SampleClock::new`] from the output sample rate
/// and the exact output cadence (`num/den` fps); call [`SampleClock::next_tick`]
/// once per output tick to get the exact number of samples to mix/emit for that
/// tick. The running total after `t` ticks is exactly
/// `floor(t · sample_rate · den / num)` — gap-free and never ahead of real time.
///
/// ## Tick-index-driven catch-up (RT-8a / invariant #3)
/// The clock also exposes its position as a **pure function of an absolute tick
/// index**: [`total_at`](SampleClock::total_at) is the exact cumulative sample
/// total after a given number of ticks, and [`advance_to`](SampleClock::advance_to)
/// returns the samples owed to jump the clock to an absolute output tick index —
/// catching up across any ticks the consumer skipped (e.g. a `DropOnOverload`
/// gap). Because the position is `floor(t · step_num / step_den)` for the tick
/// count `t` — never paced by surviving frames — audio can never drift away from
/// the video tick timeline under overload. `next_tick()` is exactly
/// `advance_to(current_tick + 1)`.
#[derive(Debug, Clone)]
pub struct SampleClock {
    /// Samples-per-tick numerator increment: `sample_rate · fps.den`.
    step_num: u64,
    /// Samples-per-tick denominator: `fps.num` (the divisor). Always `>= 1`.
    step_den: u64,
    /// The number of ticks the clock has advanced through so far. The cumulative
    /// sample position is a pure function of this counter (invariant #3): exactly
    /// `floor(ticks · step_num / step_den)`.
    ticks: u64,
}

impl SampleClock {
    /// Build a sample clock for `sample_rate` Hz output paced by `fps`.
    ///
    /// `fps` is the exact output cadence (e.g. `30000/1001`); its denominator is
    /// canonically strictly positive. A degenerate cadence (zero numerator) is
    /// clamped to a divisor of 1 so the clock still advances deterministically
    /// rather than dividing by zero (the caller validates real cadences upstream).
    #[must_use]
    pub fn new(sample_rate: u32, fps: Rational) -> Self {
        let den = fps.den.unsigned_abs();
        let num = fps.num.unsigned_abs().max(1);
        Self {
            step_num: u64::from(sample_rate).saturating_mul(den),
            step_den: num,
            ticks: 0,
        }
    }

    /// The number of output ticks the clock has advanced through so far.
    #[must_use]
    pub const fn tick_count(&self) -> u64 {
        self.ticks
    }

    /// The exact cumulative sample total after `tick_count` ticks —
    /// `floor(tick_count · step_num / step_den)`.
    ///
    /// This is the **drift-free ideal**: the running sample position is a pure
    /// function of the tick count (invariant #3), so audio that re-stamps from
    /// this position can never trail or lead the video tick timeline. The
    /// intermediate product is computed in `u128` so a long-running show (large
    /// `tick_count`) cannot overflow.
    #[must_use]
    pub fn total_at(&self, tick_count: u64) -> u64 {
        let product = u128::from(tick_count).saturating_mul(u128::from(self.step_num));
        let total = product / u128::from(self.step_den.max(1));
        u64::try_from(total).unwrap_or(u64::MAX)
    }

    /// The number of samples (per channel) to emit for the next output tick.
    ///
    /// Exact: the per-tick budget is `total_at(t) − total_at(t−1)`, the integer
    /// difference of the drift-free cumulative position. Equivalent to the
    /// classic remainder-accumulation (1601/1602 NTSC alternation) but expressed
    /// against the absolute tick counter so it agrees with
    /// [`advance_to`](SampleClock::advance_to) exactly. Advances the clock by one
    /// tick.
    pub fn next_tick(&mut self) -> usize {
        self.advance_to(self.ticks.saturating_add(1))
    }

    /// Advance the clock to the absolute output **tick index** `target_tick` and
    /// return the samples owed for the whole span since the clock's current
    /// position — catching up across any skipped ticks.
    ///
    /// This is the tick-index-driven entry point (RT-8a): the consumer carries
    /// the output tick index on each item and drives the clock by it, so a
    /// `DropOnOverload` gap (several ticks with no surviving video frame) is
    /// caught up in one call and the `SampleClock` stays a pure function of the
    /// tick counter — audio cannot drift from video (invariant #3). A
    /// `target_tick` at or behind the current position is a monotonic no-op
    /// (returns 0, never rewinds), so a duplicated or out-of-order tick index can
    /// never make the clock emit negative or rewound audio.
    pub fn advance_to(&mut self, target_tick: u64) -> usize {
        if target_tick <= self.ticks {
            return 0;
        }
        let owed = self
            .total_at(target_tick)
            .saturating_sub(self.total_at(self.ticks));
        self.ticks = target_tick;
        usize::try_from(owed).unwrap_or(usize::MAX)
    }
}
