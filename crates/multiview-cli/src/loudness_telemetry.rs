//! Off-hot-path **program-bus loudness telemetry** (AUD-8): a small, read-only
//! EBU R128 meter that taps the *emitted* (post-loudnorm) program audio and
//! **pushes** a conflated [`multiview_events::AudioLoudness`] sample onto the
//! engine's outbound event stream, so the management UI's live loudness meter
//! lights up with momentary / short-term / integrated LUFS, loudness range, and
//! true-peak (dBTP) against the compliance target.
//!
//! Per ADR-R006 the meter is **read-only and non-blocking**: it runs on the
//! bake-consumer thread (where the program audio is already mixed + normalised +
//! about to be encoded), **never** on the engine output-clock loop. It pushes
//! through the engine's drop-oldest
//! [`EnginePublisher`](multiview_engine::EnginePublisher) event stream, whose
//! `publish` never awaits or blocks a slow subscriber (invariant #10): a stalled
//! UI simply skips loudness samples; it can never back-pressure this meter and
//! this meter can never back-pressure the engine.
//!
//! ## Cadences (ADR-R006)
//! The bake consumer hands one program-audio block per output tick (~25–30 Hz),
//! but the M/S/I/LRA/dBTP compliance lane is the **slow** cadence: every block is
//! metered, but a [`Conflator`](multiview_audio::Conflator) gates the *emit* to
//! ~10 Hz so the wire stays at the documented 10–25 Hz. **Ballistics are applied
//! client-side** (the browser meter): the wire carries the raw measured values.
//!
//! ## Why a dedicated meter (not the loudnorm's internal one)
//! The [`LoudnormProcessor`](multiview_audio::LoudnormProcessor)'s internal meter
//! is built [without true-peak](multiview_audio::LoudnessMeter::without_true_peak)
//! and measures the **pre-gain** bus (it drives the makeup gain). The compliance
//! meter must measure the **emitted** (post-gain, post-limit) program — what the
//! viewer actually hears — and must report dBTP, so it is a separate
//! [`LoudnessMeter`] (true-peak enabled) fed the processed block.

use multiview_audio::{AudioBlock, AudioFormat, Conflator, LoudnessMeter, DISPLAY_HZ};
use multiview_events::AudioLoudness;

/// The compliance-lane emit cadence (Hz): the slow M/S/I/LRA/dBTP cadence per
/// ADR-R006 (`M/S/I/LRA/dBTP at 10 Hz for UI/compliance`). Inside the documented
/// 10–25 Hz wire envelope; the per-tile burned-in PPM meters are a separate
/// faster path (the CLI overlay baker), not this lane.
pub const LOUDNESS_EMIT_HZ: u32 = 10;

/// Number of meter sub-block windows to retain so memory never grows with run
/// length while short-term (3 s) / integrated stay continuous. Mirrors the
/// loudnorm processor's retention multiple over the short-term window.
const RETAIN_SUBBLOCKS: usize = 64;

/// Narrow an `f64` meter reading to the `f32` wire field without an `as` cast
/// (the workspace bans `as_conversions` in non-test code). The values are small
/// bounded loudness/peak magnitudes, so the `f32` mantissa is ample; the
/// round-trip-through-string narrowing is exact-enough and cannot panic — mirrors
/// the `multiview_audio::meterdata` `db_to_f32` helper. A non-finite or
/// unparseable value (never produced by the meter's finite readings) falls back
/// to `0.0` rather than panicking on the audio path (inv #1).
fn db_to_f32(value: f64) -> f32 {
    value.to_string().parse::<f32>().unwrap_or(0.0)
}

/// The off-hot-path program-bus loudness meter + emit conflator.
///
/// Build one per run alongside the program-audio bus + loudnorm processor at the
/// same [`AudioFormat`], then call [`push`](Self::push) on each **emitted**
/// program block (after loudnorm). It returns `Some(AudioLoudness)` at most
/// [`LOUDNESS_EMIT_HZ`] times per second (conflated, latest-wins); the caller
/// publishes that onto the engine event stream.
#[derive(Debug)]
pub struct LoudnessTelemetry {
    /// The compliance meter measuring the EMITTED program bus (true-peak ON, so
    /// it reports dBTP). A reading hiccup never stalls the bus.
    meter: LoudnessMeter,
    /// The program/bus index carried on every sample.
    program: u32,
    /// Compliance reference: the normalisation target (LUFS), true-peak ceiling
    /// (dBTP), and live tolerance (LU). These ride every sample so the browser
    /// meter colours against the same target the loudnorm processor uses.
    target_lufs: f32,
    ceiling_dbtp: f32,
    tolerance_lu: f32,
    /// Rate-bounds the EMIT to [`LOUDNESS_EMIT_HZ`] (latest-wins, drop-oldest):
    /// every block is metered, but only ~10 samples/s are published.
    conflator: Conflator<()>,
    /// The wire-advertised cadence (`LOUDNESS_EMIT_HZ`), carried on every sample.
    sampled_hz: u32,
}

impl LoudnessTelemetry {
    /// Build a loudness telemetry meter for `format` reporting against the
    /// `target_lufs` / `ceiling_dbtp` / `tolerance_lu` compliance reference for
    /// program `program`.
    ///
    /// # Errors
    ///
    /// Returns the [`multiview_audio`] error if the format is unusable (zero
    /// sample rate / channels) — the same contract as [`LoudnessMeter::new`].
    pub fn new(
        format: AudioFormat,
        program: u32,
        target_lufs: f32,
        ceiling_dbtp: f32,
        tolerance_lu: f32,
    ) -> Result<Self, multiview_audio::AudioError> {
        // True-peak ON: the compliance lane reports dBTP for the emitted bus.
        let meter = LoudnessMeter::new(format)?;
        Ok(Self {
            meter,
            program,
            target_lufs,
            ceiling_dbtp,
            tolerance_lu,
            conflator: Conflator::with_rate(LOUDNESS_EMIT_HZ),
            sampled_hz: LOUDNESS_EMIT_HZ,
        })
    }

    /// Meter one **emitted** program block (always) and return a conflated
    /// loudness sample (at most [`LOUDNESS_EMIT_HZ`] per second).
    ///
    /// `gain_db` is the makeup gain the loudnorm processor reported for this
    /// block (`None` when no normaliser is engaged). `now_ns` is the monotonic
    /// time used to rate-bound the emit. Returns `None` when this block is
    /// metered but not yet due to emit (the latest reading conflates forward).
    ///
    /// Does no I/O and cannot block: a meter push hiccup degrades to skipping
    /// this block's measurement, never a panic on the audio path (inv #1).
    pub fn push(
        &mut self,
        block: &AudioBlock,
        gain_db: Option<f32>,
        now_ns: i64,
    ) -> Option<AudioLoudness> {
        // Feed the EMITTED samples to the compliance meter, then bound the
        // history to the short-term window so memory never grows with run length.
        let _ = self.meter.push_interleaved(block.interleaved());
        self.meter.retain_recent(RETAIN_SUBBLOCKS);
        // Mark a fresh reading available; the conflator gates the emit cadence.
        self.conflator.accept(());
        self.conflator.poll(now_ns)?;
        Some(self.assemble(gain_db))
    }

    /// Read the meter's current M/S/I/LRA/dBTP and pack the wire sample. Pure
    /// (no I/O); the gate-`None` readings ride as absent fields (never a
    /// fabricated value). Public for unit testing the mapping.
    #[must_use]
    pub fn assemble(&self, gain_db: Option<f32>) -> AudioLoudness {
        AudioLoudness {
            program: self.program,
            momentary: self.meter.momentary().map(db_to_f32),
            short_term: self.meter.short_term().map(db_to_f32),
            integrated: self.meter.integrated().map(db_to_f32),
            lra: self.meter.loudness_range().map(db_to_f32),
            true_peak_dbtp: self.meter.true_peak_dbtp().map(db_to_f32),
            target_lufs: self.target_lufs,
            ceiling_dbtp: self.ceiling_dbtp,
            tolerance_lu: self.tolerance_lu,
            gain_db,
            sampled_hz: self.sampled_hz,
        }
    }
}

/// The wire cadence reported in [`AudioLoudness::sampled_hz`] for this lane;
/// equals [`LOUDNESS_EMIT_HZ`] but never exceeds the meter draw-data display
/// cadence ([`DISPLAY_HZ`]). A small const guard so the two cadences stay
/// consistent if either is retuned.
#[must_use]
pub const fn wire_hz() -> u32 {
    if LOUDNESS_EMIT_HZ <= DISPLAY_HZ {
        LOUDNESS_EMIT_HZ
    } else {
        DISPLAY_HZ
    }
}

#[cfg(test)]
mod tests {
    // Exact-equality asserts compare against values stored verbatim (the
    // compliance reference), and the tone-generator helper narrows sample-rate /
    // amplitude scalars — both are sound in test-only code.
    #![allow(
        clippy::float_cmp,
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]

    use super::*;
    use multiview_audio::ChannelLayout;

    fn stereo_48k() -> AudioFormat {
        AudioFormat::new(48_000, ChannelLayout::Stereo)
    }

    /// A 100 ms block of a loud-ish stereo sine so the meter has real energy to
    /// measure (above the absolute gate). Amplitude 0.5 → ~ -9 dBFS-ish tone.
    fn tone_block(format: AudioFormat, secs: f64, freq: f64, amp: f64) -> AudioBlock {
        let frames = (f64::from(format.sample_rate()) * secs) as usize;
        let ch = format.channel_count();
        let mut samples = Vec::with_capacity(frames * ch);
        for n in 0..frames {
            let t = n as f64 / f64::from(format.sample_rate());
            let s = (amp * (core::f64::consts::TAU * freq * t).sin()) as f32;
            for _ in 0..ch {
                samples.push(s);
            }
        }
        AudioBlock::from_interleaved(format, samples).expect("valid block")
    }

    #[test]
    fn assemble_carries_the_compliance_reference_always() {
        let tel = LoudnessTelemetry::new(stereo_48k(), 0, -23.0, -1.5, 1.0).unwrap();
        let sample = tel.assemble(Some(0.5));
        assert_eq!(sample.program, 0);
        assert_eq!(sample.target_lufs, -23.0);
        assert_eq!(sample.ceiling_dbtp, -1.5);
        assert_eq!(sample.tolerance_lu, 1.0);
        assert_eq!(sample.gain_db, Some(0.5));
        assert_eq!(sample.sampled_hz, LOUDNESS_EMIT_HZ);
    }

    #[test]
    fn silence_keeps_loudness_fields_absent_not_fabricated() {
        // A meter that has never seen audio above the gate reports no loudness:
        // the fields must be None (absent on the wire), never -inf or 0.
        let tel = LoudnessTelemetry::new(stereo_48k(), 0, -23.0, -1.5, 1.0).unwrap();
        let sample = tel.assemble(None);
        assert_eq!(sample.momentary, None, "no audio → no momentary");
        assert_eq!(sample.short_term, None, "no audio → no short-term");
        assert_eq!(sample.integrated, None, "no audio → no integrated");
        assert_eq!(sample.gain_db, None, "no normaliser → no gain");
    }

    #[test]
    fn real_audio_produces_a_finite_momentary_reading() {
        // After ~0.5 s of a loud tone the momentary (400 ms) window is full and
        // must report a finite LUFS value — proving the meter is actually fed the
        // pushed block, not a tautological always-None.
        let format = stereo_48k();
        let mut tel = LoudnessTelemetry::new(format, 0, -23.0, -1.5, 1.0).unwrap();
        let mut now = 0_i64;
        let mut last: Option<AudioLoudness> = None;
        // Five 100 ms blocks = 500 ms; emit is gated to 10 Hz (100 ms), so this
        // yields several emits; keep the last.
        for _ in 0..5 {
            now += 100_000_000; // 100 ms in ns
            if let Some(s) = tel.push(&tone_block(format, 0.1, 1_000.0, 0.5), Some(0.0), now) {
                last = Some(s);
            }
        }
        let s = last.expect("a 10 Hz emit fired within 500 ms");
        let m = s
            .momentary
            .expect("momentary is measured after 400+ ms of audio");
        assert!(
            m.is_finite() && m < 0.0 && m > -60.0,
            "momentary should be a sane negative LUFS for a -6 dBFS tone, got {m}"
        );
    }

    #[test]
    fn emit_is_rate_bounded_to_ten_hz() {
        // Pushing many blocks within one 100 ms emit window yields at most ONE
        // emit (conflation / drop-oldest): the engine is never flooded (inv #10).
        let format = stereo_48k();
        let mut tel = LoudnessTelemetry::new(format, 0, -23.0, -1.5, 1.0).unwrap();
        let block = tone_block(format, 0.01, 1_000.0, 0.5); // 10 ms blocks
                                                            // 9 blocks spaced 10 ms apart spans 90 ms < the 100 ms emit interval.
        let mut emits = 0;
        let mut now = 0_i64;
        // First poll at t=0 primes the conflator (next_emit_ns starts at i64::MIN
        // so the first due time is immediately): count emits across a 90 ms span.
        for _ in 0..9 {
            now += 10_000_000; // 10 ms
            if tel.push(&block, None, now).is_some() {
                emits += 1;
            }
        }
        assert!(
            emits <= 2,
            "within ~90 ms the 10 Hz lane emits at most ~1-2 samples, got {emits}"
        );
    }

    #[test]
    fn emit_advances_across_a_second() {
        // Over 1 s of 10 ms blocks the 10 Hz lane emits ~10 samples — not 100
        // (the block rate). Proves the conflator actually advances on time.
        let format = stereo_48k();
        let mut tel = LoudnessTelemetry::new(format, 0, -23.0, -1.5, 1.0).unwrap();
        let block = tone_block(format, 0.01, 1_000.0, 0.5);
        let mut emits = 0;
        let mut now = 0_i64;
        for _ in 0..100 {
            now += 10_000_000;
            if tel.push(&block, None, now).is_some() {
                emits += 1;
            }
        }
        assert!(
            (8..=13).contains(&emits),
            "a 10 Hz lane emits ~10 samples over 1 s of 100 blocks, got {emits}"
        );
    }
}
