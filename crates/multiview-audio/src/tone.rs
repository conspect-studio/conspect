//! The synthetic **line-up tone** generator (AUD-5): a phase-continuous 1 kHz
//! reference sine, the audible companion to colour bars.
//!
//! A SMPTE/EBU bars-and-tone line-up signal pairs the colour-bar test card with a
//! steady 1 kHz reference tone at the alignment level. This module is the audio
//! half: a pure, deterministic generator that fills successive
//! [`AudioBlock`]s with a 1 kHz sine at the EBU alignment level
//! ([`VU_REFERENCE_DBFS`](crate::ballistics::VU_REFERENCE_DBFS), −18 dBFS peak),
//! identical to every channel, in the canonical 48 kHz program format the bus
//! mixes.
//!
//! ## Drift-free phase (invariant #3/#6)
//! The instantaneous sample at absolute frame index `n` is
//! `amplitude · sin(2π · freq · n / sample_rate)`. The argument is periodic in
//! `n` with period `sample_rate` (adding `sample_rate` to `n` adds an integer
//! number of `2π·freq` turns to the phase, leaving the sine unchanged), so the
//! generator carries phase as the **integer** frame counter reduced modulo the
//! sample rate — never a float phase accumulator. Across a multi-hour show the
//! integer counter never accumulates rounding error, so the tone never drifts in
//! pitch and never develops a click at a block seam (the timing discipline the
//! output clock demands). `f64::sin` is evaluated once per frame from that exact
//! integer phase; the only narrowing is the clamped `f64 → f32` sample helper
//! ([`clamp_sample`]), never a raw `as`.
//!
//! ## Cheap + off the hot path (invariant #1/#10)
//! [`ToneGenerator::next_block`] is a tight per-frame `sin` loop with no
//! allocation beyond the one output `Vec`; it is generated on the synthetic
//! source's own render/ingest thread (the peer of a decode thread) and only ever
//! **writes** the lock-free [`AudioStore`](crate::store::AudioStore) the program
//! bus samples — it can neither pace nor stall the output clock, nor
//! back-pressure the engine.

use crate::ballistics::VU_REFERENCE_DBFS;
use crate::error::Result;
use crate::format::{AudioBlock, AudioFormat};

/// The reference line-up tone frequency: 1 kHz (the SMPTE/EBU bars-and-tone
/// standard alignment tone).
pub const REFERENCE_TONE_HZ: u32 = 1_000;

/// The line-up tone's **peak** level in dBFS: the EBU alignment level of
/// −18 dBFS, matching [`VU_REFERENCE_DBFS`](crate::ballistics::VU_REFERENCE_DBFS)
/// so a VU meter on the tone reads exactly 0 VU.
pub const LINE_UP_TONE_PEAK_DBFS: f64 = VU_REFERENCE_DBFS;

/// The linear peak amplitude of the line-up tone: `10^(dBFS / 20)`.
///
/// At −18 dBFS this is ≈ 0.1259. Computed from [`LINE_UP_TONE_PEAK_DBFS`] so the
/// amplitude and the documented level can never drift apart.
#[must_use]
pub fn line_up_tone_amplitude() -> f64 {
    dbfs_to_linear(LINE_UP_TONE_PEAK_DBFS)
}

/// Convert a dBFS level to a linear amplitude in `[0, 1]`: `10^(dbfs / 20)`.
#[must_use]
fn dbfs_to_linear(dbfs: f64) -> f64 {
    10.0_f64.powf(dbfs / 20.0)
}

/// Hard-limit a generated `f64` sample to the `[-1.0, 1.0]` `f32` sample domain.
///
/// The tone amplitude is well below unity so the clamp never actually fires; it
/// is the belt-and-braces narrowing the workspace requires (mirrors
/// [`crate::mixer`]'s `clamp_sample`) so the `f64 → f32` conversion is never a
/// raw `as` on an unbounded value.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // reason: value is clamped to [-1,1]; f64->f32 narrowing is bounded.
fn clamp_sample(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}

/// A frame count (`usize`) as a `u64`, saturating, without a raw `as` cast.
#[must_use]
fn frame_as_u64(frames: usize) -> u64 {
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// The exact ratio `turns / rate` as `f64`, both reduced into `[0, rate)`, without
/// a raw `as` cast. Both operands are `< sample_rate` (≤ 192 kHz), comfortably
/// within `f64`'s exact-integer range, so the division is exact to `f64`.
#[must_use]
fn turns_ratio(turns: u64, rate: u64) -> f64 {
    let num = u32::try_from(turns).map_or(f64::from(u32::MAX), f64::from);
    let den = u32::try_from(rate.max(1)).map_or(f64::from(u32::MAX), f64::from);
    num / den
}

/// A phase-continuous sine-tone generator at a fixed frequency and level.
///
/// Build one per synthetic source with [`ToneGenerator::new`], then call
/// [`next_block`](ToneGenerator::next_block) once per tick with that tick's
/// frame budget. Successive blocks are seamless (the generator carries an
/// integer phase counter across calls), so the concatenation of every block is
/// one continuous sine — no click at a block boundary, no pitch drift over time.
#[derive(Debug, Clone)]
pub struct ToneGenerator {
    format: AudioFormat,
    /// The tone frequency in Hz.
    freq_hz: u32,
    /// The linear peak amplitude (`10^(dBFS/20)`).
    amplitude: f64,
    /// The absolute frame index of the NEXT frame to emit, reduced modulo the
    /// sample rate so it never grows without bound and never loses precision.
    /// Carrying phase as this integer (not an accumulated float) is the
    /// drift-free discipline (invariant #3/#6).
    phase_frame: u64,
}

impl ToneGenerator {
    /// Build a tone generator at `format` emitting a `freq_hz` sine whose **peak**
    /// is `peak_dbfs` dBFS, identical on every channel, starting at phase zero.
    #[must_use]
    pub fn new(format: AudioFormat, freq_hz: u32, peak_dbfs: f64) -> Self {
        Self {
            format,
            freq_hz,
            amplitude: dbfs_to_linear(peak_dbfs),
            phase_frame: 0,
        }
    }

    /// Build the canonical 1 kHz / −18 dBFS line-up tone at `format` — the bars
    /// companion tone (AUD-5).
    #[must_use]
    pub fn line_up(format: AudioFormat) -> Self {
        Self::new(format, REFERENCE_TONE_HZ, LINE_UP_TONE_PEAK_DBFS)
    }

    /// This generator's audio format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Seek the generator's phase to the sine value at **absolute frame index**
    /// `frame`, so the next [`next_block`](Self::next_block) emits the tone as it
    /// would have been at that frame.
    ///
    /// Because the sine is a pure function of the absolute frame index (the phase
    /// is `2π · freq · frame / rate`, periodic in `frame` with period `rate`),
    /// seeking is exact and is the right way to *skip forward* over a span that
    /// will never be read — e.g. when a downstream reader's cursor has jumped
    /// ahead (a `DropOnOverload` catch-up) — instead of generating every
    /// intervening sample. The phase stays on the same continuous sine, so the
    /// tone is still phase-correct (and click-free) at the seam.
    pub fn seek_to_frame(&mut self, frame: u64) {
        let rate = u64::from(self.format.sample_rate().max(1));
        self.phase_frame = frame % rate;
    }

    /// Generate the next `frames` frames of the tone as an [`AudioBlock`],
    /// advancing the internal phase so the *next* call continues the same sine
    /// without a seam.
    ///
    /// The same sample is written to every channel (a centred, in-phase mono
    /// tone across the layout). Returns a block of exactly `frames` frames.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`](crate::error::AudioError::RaggedBlock)
    /// only on the impossible event that the exact-length buffer is rejected by
    /// [`AudioBlock::from_interleaved`]; constructed lengths are always whole
    /// frames, so this never fires in practice.
    pub fn next_block(&mut self, frames: usize) -> Result<AudioBlock> {
        let channels = self.format.channel_count();
        let rate = u64::from(self.format.sample_rate().max(1));
        let freq = u64::from(self.freq_hz);
        let mut samples = vec![0.0f32; frames.saturating_mul(channels)];

        for frame in 0..frames {
            // Absolute frame index of this frame, reduced modulo the sample rate.
            // `2π · freq · n / rate` is periodic in `n` with period `rate`, so the
            // reduced index gives an identical sine value with no precision loss —
            // the integer phase never grows unbounded and never drifts.
            let n = self.phase_frame.saturating_add(frame_as_u64(frame)) % rate;
            let turns = freq.saturating_mul(n) % rate; // phase numerator in [0, rate)
            let theta = std::f64::consts::TAU * turns_ratio(turns, rate);
            let sample = clamp_sample(self.amplitude * theta.sin());
            let base = frame.saturating_mul(channels);
            for ch in 0..channels {
                if let Some(slot) = samples.get_mut(base.saturating_add(ch)) {
                    *slot = sample;
                }
            }
        }

        // Advance the integer phase counter, keeping it reduced modulo the rate so
        // it is bounded forever (drift-free; invariant #3).
        self.phase_frame = self
            .phase_frame
            .saturating_add(frame_as_u64(frames))
            .checked_rem(rate)
            .unwrap_or(0);

        AudioBlock::from_interleaved(self.format, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::ChannelLayout;
    use crate::loudness::LoudnessMeter;

    fn stereo() -> AudioFormat {
        AudioFormat::new(AudioFormat::CANONICAL_RATE, ChannelLayout::Stereo)
    }

    /// A count as `f64` without a raw `as` cast (test helper; the workspace bans
    /// `as` even in unit tests for the numeric-precision lints).
    fn count_f64(n: usize) -> f64 {
        f64::from(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// An absolute frame index (`u64`) as `f64` without a raw `as` cast.
    fn frame_f64(n: u64) -> f64 {
        f64::from(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// The mean square of a block's samples (energy proxy). Zero iff silence.
    fn mean_square(block: &AudioBlock) -> f64 {
        let s = block.interleaved();
        if s.is_empty() {
            return 0.0;
        }
        let sum: f64 = s.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        sum / count_f64(s.len())
    }

    #[test]
    fn tone_is_not_silence() {
        let mut gen = ToneGenerator::line_up(stereo());
        let block = gen.next_block(1920).expect("block");
        assert_eq!(block.frame_count(), 1920);
        assert!(
            mean_square(&block) > 1.0e-6,
            "the line-up tone must carry real energy, not silence"
        );
    }

    #[test]
    fn tone_peak_is_minus_18_dbfs() {
        let mut gen = ToneGenerator::line_up(stereo());
        // A whole number of full cycles (1 kHz at 48 kHz = 48 samples/cycle) so the
        // observed peak is the true sine peak.
        let block = gen.next_block(48_000).expect("block");
        let peak = block
            .interleaved()
            .iter()
            .fold(0.0_f32, |m, &x| m.max(x.abs()));
        let expected = line_up_tone_amplitude();
        assert!(
            (f64::from(peak) - expected).abs() < 1.0e-3,
            "peak amplitude {peak} must match the -18 dBFS line-up level {expected}"
        );
    }

    #[test]
    fn tone_is_phase_continuous_across_blocks() {
        // 1 kHz at 48 kHz is 48 samples/cycle. Generate two adjacent blocks whose
        // lengths are NOT a multiple of the period so the seam is a non-trivial
        // phase, then assert the sine continues smoothly: the predicted next sample
        // (from the analytic sine at the seam frame) matches the first sample of
        // block N+1, i.e. there is no discontinuity ("click") at the boundary.
        let fmt = stereo();
        let amp = line_up_tone_amplitude();
        let mut gen = ToneGenerator::new(fmt, REFERENCE_TONE_HZ, LINE_UP_TONE_PEAK_DBFS);
        let n0 = 1601usize; // deliberately not a multiple of 48
        let first = gen.next_block(n0).expect("first");
        let second = gen.next_block(64).expect("second");

        // The analytic value the sine should take at absolute frame n0 (channel 0).
        let theta = std::f64::consts::TAU * (f64::from(REFERENCE_TONE_HZ) * count_f64(n0))
            / f64::from(AudioFormat::CANONICAL_RATE);
        let predicted = amp * theta.sin();
        let actual = f64::from(*second.interleaved().first().expect("sample"));
        assert!(
            (actual - predicted).abs() < 1.0e-4,
            "phase must be continuous across the block seam: predicted {predicted}, got {actual}"
        );

        // And the last sample of block N is the sample just before, also on-curve.
        let last = f64::from(*first.interleaved().get((n0 - 1) * 2).expect("last sample"));
        let theta_last = std::f64::consts::TAU * (f64::from(REFERENCE_TONE_HZ) * count_f64(n0 - 1))
            / f64::from(AudioFormat::CANONICAL_RATE);
        assert!(
            (last - amp * theta_last.sin()).abs() < 1.0e-4,
            "block N's last sample must be on the sine curve"
        );
    }

    #[test]
    fn tone_does_not_drift_over_many_blocks() {
        // After a long run of odd-length blocks, the generator's sample at a given
        // absolute frame must still equal the analytic sine at that frame — proof
        // the integer phase never accumulates error (invariant #3 — no float drift).
        let fmt = stereo();
        let amp = line_up_tone_amplitude();
        let mut gen = ToneGenerator::new(fmt, REFERENCE_TONE_HZ, LINE_UP_TONE_PEAK_DBFS);
        let mut absolute = 0u64;
        // ~ 100k frames of history in 1601/1602-ish chunks.
        let chunks = [1601usize, 1602, 1601, 1601, 1602];
        for _ in 0..12 {
            for &c in &chunks {
                let block = gen.next_block(c).expect("block");
                // Check the first sample of this block against the analytic value.
                let theta = std::f64::consts::TAU
                    * (f64::from(REFERENCE_TONE_HZ) * frame_f64(absolute))
                    / f64::from(AudioFormat::CANONICAL_RATE);
                let predicted = amp * theta.sin();
                let actual = f64::from(*block.interleaved().first().expect("sample"));
                assert!(
                    (actual - predicted).abs() < 1.0e-4,
                    "no drift at frame {absolute}: predicted {predicted}, got {actual}"
                );
                absolute = absolute.saturating_add(u64::try_from(c).unwrap_or(u64::MAX));
            }
        }
    }

    #[test]
    fn tone_momentary_loudness_is_near_minus_21_lufs() {
        // A −18 dBFS *peak* sine has RMS 3.01 dB below peak (≈ −21.01 dBFS RMS).
        // The BS.1770 K-weighting is ~0 dB at 1 kHz, and a centred stereo tone is
        // summed across channels (the channel weights for L+R are 1.0 each, so a
        // dual-mono tone reads +3.01 dB over a single channel's RMS). Net momentary
        // loudness ≈ −21.01 + 3.01 = −18.0 LUFS for the dual-mono −18 dBFS-peak tone.
        let fmt = stereo();
        let mut gen = ToneGenerator::line_up(fmt);
        let mut meter = LoudnessMeter::new(fmt).expect("meter");
        // Feed ~1 s so the momentary (400 ms) window is well filled.
        for _ in 0..30 {
            let block = gen.next_block(1920).expect("block");
            meter.push_interleaved(block.interleaved()).expect("push");
        }
        let momentary = meter.momentary().expect("momentary reading");
        assert!(
            (momentary - (-18.0)).abs() < 0.5,
            "momentary loudness {momentary} LUFS must be the expected line-up level (±0.5 LU)"
        );
    }

    #[test]
    fn seek_to_frame_resumes_on_the_analytic_sine() {
        // Seeking forward must produce the tone as it would be at that absolute
        // frame (a pure function of the index), so skipping a never-read span stays
        // phase-correct rather than restarting at phase 0.
        let fmt = stereo();
        let amp = line_up_tone_amplitude();
        let mut gen = ToneGenerator::line_up(fmt);
        let target: u64 = 123_457; // arbitrary, not a multiple of the 48-sample period
        gen.seek_to_frame(target);
        let block = gen.next_block(1).expect("block");
        let theta = std::f64::consts::TAU * (f64::from(REFERENCE_TONE_HZ) * frame_f64(target))
            / f64::from(AudioFormat::CANONICAL_RATE);
        let predicted = amp * theta.sin();
        let actual = f64::from(*block.interleaved().first().expect("sample"));
        assert!(
            (actual - predicted).abs() < 1.0e-4,
            "after seek_to_frame({target}) the tone must equal the analytic sine: \
             predicted {predicted}, got {actual}"
        );
    }
}
