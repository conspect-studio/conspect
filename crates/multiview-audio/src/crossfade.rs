//! Per-output take crossfade on the global sample counter (ADR-0108 §5.2-§5.3).
//!
//! A take that changes an output's audio crossfades **inside that output's
//! own encoded audio bus** from the old to the new audio:
//! `out = old · cos θ + new · sin θ`, θ rising from 0 to π/2 over exactly one
//! output tick of samples, `L = total_at(Tm + 1) − total_at(Tm)` (1601 or 1602
//! at 48 kHz and 30000/1001 — integer [`SampleClock`] arithmetic, never a
//! float frame rate). The fade starts at the **grid boundary**
//! `G = ceil(total_at(Tm) / 1024) · 1024`, the first AAC frame boundary of the
//! run's one sample counter at or after the video move tick `Tm`.
//!
//! The envelope is [`GainRamp`]'s equal-power pair (the one ADR-0104 §4 uses
//! for AUTO): the falling ramp is `cos θ`, the rising ramp `sin θ`.
//!
//! **Peak bound.** An equal-power sum of two *correlated* signals rises up to
//! √2 (+3 dB) mid-fade. A take never may push a sample above what either side
//! carried, so each output sample is clamped to `max(|old|, |new|)` of its
//! own instant. For uncorrelated material (the normal take) the clamp almost
//! never engages; for identical material it returns exactly that material.

use crate::cadence::SampleClock;
use crate::mixer::GainRamp;

/// Samples per AAC frame: the global encoded-audio grid unit.
pub const AAC_FRAME_SAMPLES: u64 = 1024;

/// The first multiple of `frame` at or after `samples` (`frame == 0` returns
/// `samples` unchanged).
#[must_use]
pub fn grid_ceil(samples: u64, frame: u64) -> u64 {
    if frame == 0 {
        return samples;
    }
    samples.div_ceil(frame).saturating_mul(frame)
}

/// The fade length of a take moving at tick `tick`: one tick of samples,
/// `total_at(tick + 1) − total_at(tick)`.
#[must_use]
pub fn take_fade_len(clock: &SampleClock, tick: u64) -> u64 {
    clock
        .total_at(tick.saturating_add(1))
        .saturating_sub(clock.total_at(tick))
}

/// One take's equal-power crossfade, placed on the absolute sample counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeCrossfade {
    /// First sample of the fade (`G`, a grid boundary).
    pub start: u64,
    /// Fade length in samples (one tick).
    pub len: u64,
}

impl TakeCrossfade {
    /// The crossfade of a move at tick `move_tick`: starts at the first
    /// `frame` boundary at or after `total_at(move_tick)` and lasts
    /// [`take_fade_len`] samples.
    #[must_use]
    pub fn at_move(clock: &SampleClock, move_tick: u64, frame: u64) -> Self {
        Self {
            start: grid_ceil(clock.total_at(move_tick), frame),
            len: take_fade_len(clock, move_tick),
        }
    }

    /// One past the last faded sample: from here on, the new audio only.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// `(old gain, new gain)` at absolute sample `sample`: `(1, 0)` before the
    /// fade (and exactly at its first sample, θ = 0), `(0, 1)` from
    /// [`end`](Self::end), the equal-power pair in between.
    #[must_use]
    pub fn gains(&self, sample: u64) -> (f32, f32) {
        if sample < self.start {
            return (1.0, 0.0);
        }
        let done = sample - self.start;
        if done >= self.len {
            return (0.0, 1.0);
        }
        let (Ok(frame), Ok(total)) = (usize::try_from(done), usize::try_from(self.len)) else {
            return (0.0, 1.0);
        };
        (
            GainRamp::down(total).envelope_at(frame),
            GainRamp::up(total).envelope_at(frame),
        )
    }

    /// Mix one block. `old` and `new` are interleaved with `channels`
    /// channels; their first frame is absolute sample `block_start`. Appends
    /// `min(old, new)` whole frames to `out`. A short or absent side is
    /// silence, never a panic.
    pub fn mix_into(
        &self,
        old: &[f32],
        new: &[f32],
        channels: usize,
        block_start: u64,
        out: &mut Vec<f32>,
    ) {
        if channels == 0 {
            return;
        }
        let frames = old.len().max(new.len()) / channels;
        out.reserve(frames.saturating_mul(channels));
        for frame in 0..frames {
            let at = block_start.saturating_add(u64::try_from(frame).unwrap_or(u64::MAX));
            let (g_old, g_new) = self.gains(at);
            let base = frame * channels;
            for channel in 0..channels {
                let o = old.get(base + channel).copied().unwrap_or(0.0);
                let n = new.get(base + channel).copied().unwrap_or(0.0);
                let peak = o.abs().max(n.abs());
                out.push(o.mul_add(g_old, n * g_new).clamp(-peak, peak));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::cast_precision_loss
    )]
    use super::*;
    use multiview_core::time::Rational;

    fn ntsc() -> SampleClock {
        SampleClock::new(48_000, Rational::new(30_000, 1_001))
    }

    #[test]
    fn fade_length_is_one_tick_of_samples_for_many_ticks_at_ntsc() {
        let clock = ntsc();
        let mut seen = std::collections::BTreeSet::new();
        for tick in 0..20_000_u64 {
            let fade = TakeCrossfade::at_move(&clock, tick, AAC_FRAME_SAMPLES);
            assert_eq!(fade.len, clock.total_at(tick + 1) - clock.total_at(tick));
            assert!(
                fade.len == 1601 || fade.len == 1602,
                "tick {tick}: {}",
                fade.len
            );
            seen.insert(fade.len);
            assert_eq!(fade.start % 1024, 0, "grid boundary");
            assert!(fade.start >= clock.total_at(tick) && fade.start < clock.total_at(tick) + 1024);
        }
        assert_eq!(seen.len(), 2, "both 1601 and 1602 occur");
        // Five ticks are exactly 8008 samples (the 30000/1001 pattern).
        for t in (0..20_000_u64).step_by(5) {
            let five: u64 = (t..t + 5).map(|x| take_fade_len(&clock, x)).sum();
            assert_eq!(five, 8008);
        }
    }

    #[test]
    fn equal_power_endpoints_are_exact_and_the_span_is_len_samples() {
        let fade = TakeCrossfade {
            start: 4096,
            len: 1601,
        };
        assert_eq!(fade.gains(4095), (1.0, 0.0));
        assert_eq!(fade.gains(4096), (1.0, 0.0), "theta = 0 at G");
        assert_eq!(fade.gains(fade.end()), (0.0, 1.0));
        assert_eq!(fade.gains(fade.end() + 10), (0.0, 1.0));
        let (o, n) = fade.gains(fade.end() - 1);
        assert!(n < 1.0 && o > 0.0, "still fading at the last faded sample");
        // Exactly len samples are not pure old-or-new... minus θ=0 itself.
        let partial = (fade.start..fade.end() + 5)
            .filter(|&s| {
                let g = fade.gains(s);
                g != (1.0, 0.0) && g != (0.0, 1.0)
            })
            .count();
        assert_eq!(partial, 1600);
        for s in fade.start..fade.end() {
            let (o, n) = fade.gains(s);
            assert!((o * o + n * n - 1.0).abs() < 1e-5, "equal power at {s}");
        }
    }

    #[test]
    fn no_output_sample_exceeds_either_input_peak() {
        let fade = TakeCrossfade {
            start: 2048,
            len: 1602,
        };
        let channels = 2;
        let frames = 4800;
        // Correlated (identical) and anti-correlated and unrelated pairs.
        let tone = |hz: f32, amp: f32| -> Vec<f32> {
            (0..frames)
                .flat_map(|i| {
                    let v = amp
                        * (std::f32::consts::TAU * hz * f32::from(u16::try_from(i).unwrap())
                            / 48_000.0)
                            .sin();
                    [v, v]
                })
                .collect::<Vec<f32>>()
        };
        for (old, new) in [
            (tone(1000.0, 0.9), tone(1000.0, 0.9)),
            (tone(440.0, 0.7), tone(1000.0, 0.8)),
            (tone(440.0, 1.0), tone(440.0, -1.0)),
        ] {
            let peak = old.iter().chain(&new).fold(0.0_f32, |m, v| m.max(v.abs()));
            let mut out = Vec::new();
            fade.mix_into(&old, &new, channels, 1000, &mut out);
            assert_eq!(out.len(), old.len());
            assert!(out.iter().all(|v| v.abs() <= peak), "over peak");
            // Before G: old exactly; after the fade: new exactly.
            assert_eq!(out[..(2048 - 1000) * 2], old[..(2048 - 1000) * 2]);
            let after = usize::try_from(fade.end() - 1000).unwrap() * 2;
            assert_eq!(out[after..], new[after..]);
        }
    }

    #[test]
    fn grid_ceil_rounds_up_to_the_frame() {
        assert_eq!(grid_ceil(0, 1024), 0);
        assert_eq!(grid_ceil(1, 1024), 1024);
        assert_eq!(grid_ceil(1024, 1024), 1024);
        assert_eq!(grid_ceil(1025, 1024), 2048);
        assert_eq!(grid_ceil(7, 0), 7);
    }
}
