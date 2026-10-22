//! Adaptive (ratio-driven) resampler — the runtime-varying half of the resample
//! machinery (ADR-R005, display-out §5).
//!
//! `multiview-audio` already resamples every decoded source to the canonical
//! 48 kHz float format (the `decode` module, a fixed-ratio libav `swr`
//! resampler behind the off-by-default `ffmpeg` feature). The HDMI
//! display-audio servo
//! (`multiview-output::display::audio`) needs the *complementary* capability:
//! resample with a ratio it can **vary per servo tick** within a clamped ±ppm
//! band, to track the display's scanout clock (the mpv/Kodi "display-resample"
//! technique). That ratio-driven resampler lives here — the home of all
//! resampling — so the servo *drives* it rather than reimplementing resampling.
//!
//! It is pure Rust, hardware-free, and operates on the canonical
//! [`AudioBlock`]/[`AudioFormat`] types, so it is fully unit-testable in CI.
//!
//! ## Method
//!
//! A correction this small (≤ a few hundred ppm in steady state) is a *time
//! stretch*, not a sample-rate change: input and output are both 48 kHz; the
//! resampler emits `frames · (1 + ppm/1e6)` output frames carrying the same
//! signal. It uses **linear interpolation** with a carried fractional read
//! phase, so the cumulative output position tracks the ideal ratio to within one
//! frame of phase forever — never the per-block rounding that would desync audio
//! over a long show. (Insert/drop-sample is the cruder fallback the brief notes;
//! fractional interpolation is the primary path here.) At the tiny ratios the
//! servo applies, linear interpolation's passband error is far below audibility;
//! a higher-order kernel is a future refinement, not a correctness concern.

use crate::format::{AudioBlock, AudioFormat};

/// A resample-ratio correction expressed in parts-per-million, clamped to the
/// audible-artefact band.
///
/// `ppm > 0` means *emit more output frames than input* (audio plays slightly
/// faster — used to drain a too-full FIFO); `ppm < 0` emits fewer (slower —
/// fills a too-empty FIFO). The applied magnitude never exceeds
/// [`RatioPpm::MAX_PPM`]: beyond that, pitch error becomes audible, so the servo
/// must lean on insert/drop instead (the resampler stays transparent).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatioPpm(f64);

impl RatioPpm {
    /// The ±band the applied ratio is clamped to (parts per million).
    ///
    /// A few thousand ppm is already far past what any clock pair needs (real
    /// crystal drift is tens of ppm); the wide band exists only so a transient
    /// servo demand is satisfied by the resampler before insert/drop is needed,
    /// while still bounding the worst-case pitch shift well under perceptual
    /// thresholds for the brief windows it is held.
    pub const MAX_PPM: f64 = 5_000.0;

    /// A zero correction (transparent identity).
    pub const ZERO: Self = Self(0.0);

    /// Build a correction from a ppm value, clamped to ±[`MAX_PPM`](Self::MAX_PPM).
    ///
    /// A non-finite input is treated as zero (the servo never benefits from a
    /// NaN/∞ demand, and the resampler must never produce non-finite output).
    #[must_use]
    pub fn from_ppm(ppm: f64) -> Self {
        if ppm.is_finite() {
            Self(ppm.clamp(-Self::MAX_PPM, Self::MAX_PPM))
        } else {
            Self(0.0)
        }
    }

    /// The clamped ppm value.
    #[must_use]
    pub const fn ppm(self) -> f64 {
        self.0
    }

    /// The output-per-input frame ratio this correction implies
    /// (`1 + ppm/1e6`).
    #[must_use]
    pub fn frame_ratio(self) -> f64 {
        1.0 + self.0 / 1_000_000.0
    }

    /// The per-output-frame input-read step (`1 / frame_ratio`): how far the
    /// read phase advances through the input per output frame.
    #[must_use]
    pub fn input_step(self) -> f64 {
        1.0 / self.frame_ratio()
    }
}

/// A streaming linear-interpolating resampler whose ratio the caller varies per
/// block (the display-audio servo's applicator).
///
/// Construct one per audio stream with [`AdaptiveResampler::new`]; set the
/// current correction with [`set_ratio`](Self::set_ratio) each servo tick and
/// feed blocks through [`process`](Self::process). The fractional read phase and
/// one trailing input frame per channel are carried across calls so a long run
/// stays phase-accurate.
#[derive(Debug, Clone)]
pub struct AdaptiveResampler {
    format: AudioFormat,
    ratio: RatioPpm,
    /// The last input frame of the previous block, per channel — the left
    /// anchor for an output sample whose read phase began before this block.
    carry: Vec<f32>,
    /// Whether `carry` holds a real previous frame (false before the first
    /// block).
    have_carry: bool,
    /// Fractional read position within the *current* input block at the start
    /// of the next output frame, relative to the carry frame at index `-1`.
    /// Always in `[0, 1)` at block boundaries.
    phase: f64,
}

impl AdaptiveResampler {
    /// A fresh resampler for `format`, starting at unity ratio with no carry.
    #[must_use]
    pub fn new(format: AudioFormat) -> Self {
        let channels = format.channel_count().max(1);
        Self {
            format,
            ratio: RatioPpm::ZERO,
            carry: vec![0.0; channels],
            have_carry: false,
            phase: 0.0,
        }
    }

    /// The format every block this resampler emits is in (unchanged from input).
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// The current ratio correction.
    #[must_use]
    pub const fn ratio(&self) -> RatioPpm {
        self.ratio
    }

    /// Set the ratio correction applied to subsequent [`process`](Self::process)
    /// calls.
    pub fn set_ratio(&mut self, ratio: RatioPpm) {
        self.ratio = ratio;
    }

    /// Resample `block` at the current ratio, returning a block in the same
    /// format with `≈ frames · frame_ratio` output frames.
    ///
    /// A block in a different format than this resampler is configured for is
    /// returned unchanged (the caller is expected to keep them aligned; this is
    /// a defensive no-op rather than a panic). An empty block yields an empty
    /// block.
    #[must_use]
    pub fn process(&mut self, block: &AudioBlock) -> AudioBlock {
        let channels = self.format.channel_count();
        if block.format() != self.format || channels == 0 {
            return block.clone();
        }
        let input = block.interleaved();
        let in_frames = block.frame_count();
        if in_frames == 0 {
            return AudioBlock::silence(self.format, 0);
        }

        let step = self.ratio.input_step();
        // Reading frame index `f` (a float) means interpolating between input
        // frames `floor(f)` and `floor(f)+1`. Frame index `-1` is the carry
        // frame (the previous block's last frame); index `0..in_frames` are this
        // block's frames. The phase walks from `self.phase` upward by `step` per
        // output frame, and we stop emitting once the right neighbour would fall
        // beyond the last input frame of this block — that tail is carried into
        // the next call via `carry` + a renormalised `phase`.
        let mut out: Vec<f32> = Vec::with_capacity(input.len() + channels);
        // `pos` is the read position measured from frame index -1 (the carry).
        let mut pos = self.phase;
        // The highest readable left-anchor index (relative to -1) is the last
        // real frame: index `in_frames - 1` maps to pos `in_frames` (since -1 is
        // pos 0). We emit while `pos < in_frames` (the position == in_frames is
        // the synthetic boundary that becomes the next carry).
        let last_pos = usize_to_f64(in_frames);
        while pos < last_pos {
            let left_rel = pos.floor();
            let frac = f64_to_f32(pos - left_rel);
            // `left_rel` is the integer position measured from -1; the input
            // frame index is `left_rel - 1`. `left_rel >= 0` here.
            let left_origin = f64_to_usize(left_rel);
            for ch in 0..channels {
                let l = self.sample_at(input, in_frames, channels, left_origin, ch);
                let r = self.sample_at(input, in_frames, channels, left_origin + 1, ch);
                out.push(l + (r - l) * frac);
            }
            pos += step;
        }

        // Carry the last real input frame and renormalise the phase to be
        // measured from the *next* block's frame -1 (== this block's last
        // frame). `pos` is measured from this block's frame -1; subtract
        // `in_frames` to rebase onto the next block's carry origin.
        for ch in 0..channels {
            // `in_frames` (>= 1) as a left-origin reads input frame
            // `in_frames - 1`: the last real frame. `carry` has exactly
            // `channels` entries, so `get_mut(ch)` always hits.
            let sample = self.sample_at(input, in_frames, channels, in_frames, ch);
            if let Some(slot) = self.carry.get_mut(ch) {
                *slot = sample;
            }
        }
        self.have_carry = true;
        self.phase = (pos - last_pos).max(0.0);

        // `out.len()` is always a whole multiple of `channels` by construction.
        AudioBlock::from_interleaved(self.format, out).unwrap_or_else(|_| {
            // Unreachable in practice (length is a channel multiple); degrade to
            // silence rather than panic on the hot path.
            AudioBlock::silence(self.format, 0)
        })
    }

    /// Read input for channel `ch` at `origin`, the read position measured from
    /// frame index `-1` (the carry frame). `origin == 0` is the carry frame;
    /// `origin == k` is input frame `k - 1`. Out-of-range positions clamp to the
    /// nearest edge so a boundary interpolation never reads past the buffer.
    #[inline]
    fn sample_at(
        &self,
        input: &[f32],
        in_frames: usize,
        channels: usize,
        origin: usize,
        ch: usize,
    ) -> f32 {
        if origin == 0 {
            // The carry frame (previous block's last frame); anchor on this
            // block's first frame before the very first block so the start is
            // continuous rather than a click from silence.
            return if self.have_carry {
                self.carry.get(ch).copied().unwrap_or(0.0)
            } else {
                input.get(ch).copied().unwrap_or(0.0)
            };
        }
        let frame = (origin - 1).min(in_frames.saturating_sub(1));
        input.get(frame * channels + ch).copied().unwrap_or(0.0)
    }
}

/// `usize → f64` for frame counts (always `< 2^53` in any real run, so exact).
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: audio frame counts per block are tiny (< 2^53); the cast is lossless
// and no fallible `From<usize> for f64` exists.
#[inline]
fn usize_to_f64(n: usize) -> f64 {
    n as f64
}

/// `f64 → usize` for a non-negative `floor`ed read position. Negative or
/// non-finite inputs (never produced here) clamp to 0.
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
// reason: `pos.floor()` is a non-negative integer-valued f64 bounded by the
// block frame count (tiny); the truncation is exact and there is no fallible
// `TryFrom<f64> for usize`.
#[inline]
fn f64_to_usize(x: f64) -> usize {
    if x.is_finite() && x >= 0.0 {
        x as usize
    } else {
        0
    }
}

/// `f64 → f32` for an interpolation fraction in `[0, 1)`; the narrowing is
/// bounded and the audible error negligible.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: the value is a fraction in [0,1); f64->f32 narrowing is bounded and
// exact-enough, matching the crate's mixer idiom.
#[inline]
fn f64_to_f32(x: f64) -> f32 {
    x as f32
}
