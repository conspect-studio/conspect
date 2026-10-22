//! PCM sample-format negotiation + float→integer conversion (DEV-B4
//! adversarial review MEDIUM-3).
//!
//! Real HDMI audio devices — x86 HDA codecs, the Pi's vc4-hdmi — typically
//! offer only integer sample formats (S16/S24/S32) and refuse `FLOAT_LE`, so a
//! sink that only ever asks for float opens nothing and runs silent on exactly
//! the hardware we ship to. This module is the **pure** half of the fix, CI
//! -tested without hardware: a preference-ordered negotiation (float first —
//! no conversion at all — then the widest integer first so no precision is
//! given away needlessly) plus sample-accurate float→integer conversion for
//! the write boundary. The feature-gated [`super::alsa`] backend wires
//! [`negotiate_sample_format`] to `snd_pcm_hw_params_test_format` and the
//! converters to the typed `snd_pcm_writei`.

/// A PCM sample format the display-audio sink can drive, covering the formats
/// HDMI/DP audio devices actually expose. `S24` is the 24-bit-in-32-bit
/// container (LSB-justified, ALSA `S24_LE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PcmSampleFormat {
    /// 32-bit float (the pipeline's native format — no conversion).
    Float,
    /// Signed 32-bit integer.
    S32,
    /// Signed 24-bit integer, LSB-justified in a 32-bit container.
    S24,
    /// Signed 16-bit integer.
    S16,
}

/// The negotiation preference order: float first (no conversion at all), then
/// integers widest-first (no precision given away needlessly).
const PREFERENCE: [PcmSampleFormat; 4] = [
    PcmSampleFormat::Float,
    PcmSampleFormat::S32,
    PcmSampleFormat::S24,
    PcmSampleFormat::S16,
];

/// Pick the best sample format a device supports: the first of
/// float → S32 → S24 → S16 for which `supports` returns `true`, or [`None`]
/// when the device offers none of them (it cannot be driven — the sink stays
/// silent rather than feeding it a format it refused).
pub fn negotiate_sample_format(
    mut supports: impl FnMut(PcmSampleFormat) -> bool,
) -> Option<PcmSampleFormat> {
    PREFERENCE.into_iter().find(|format| supports(*format))
}

/// Quantize one float sample against a symmetric integer full scale:
/// round-to-nearest, out-of-range input clips to ±full scale (never wraps),
/// `NaN` degrades to silence. The result is integer-valued and within
/// `[-full_scale, +full_scale]`.
fn quantize(sample: f32, full_scale: f64) -> f64 {
    if sample.is_nan() {
        return 0.0;
    }
    (f64::from(sample) * full_scale)
        .round()
        .clamp(-full_scale, full_scale)
}

/// Convert float samples to signed 16-bit, symmetric full scale
/// (±1.0 ↔ ±32 767), rounding to nearest and clipping out-of-range input.
#[must_use]
pub fn f32_to_s16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| i16_from_quantized(quantize(*s, 32_767.0)))
        .collect()
}

/// Convert float samples to signed 24-bit in a 32-bit container
/// (LSB-justified, ±1.0 ↔ ±8 388 607), rounding to nearest and clipping.
#[must_use]
pub fn f32_to_s24(samples: &[f32]) -> Vec<i32> {
    samples
        .iter()
        .map(|s| i32_from_quantized(quantize(*s, 8_388_607.0)))
        .collect()
}

/// Convert float samples to signed 32-bit (±1.0 ↔ ±2 147 483 647), rounding
/// to nearest and clipping.
#[must_use]
pub fn f32_to_s32(samples: &[f32]) -> Vec<i32> {
    samples
        .iter()
        .map(|s| i32_from_quantized(quantize(*s, 2_147_483_647.0)))
        .collect()
}

/// `f64 → i16` for a [`quantize`]d value.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: `quantize` returns an integer-valued f64 clamped to ±32 767, so the
// cast is in-range and exact; no fallible `TryFrom<f64> for i16` exists.
fn i16_from_quantized(quantized: f64) -> i16 {
    quantized as i16
}

/// `f64 → i32` for a [`quantize`]d value.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: `quantize` returns an integer-valued f64 clamped to at most
// ±2 147 483 647, so the cast is in-range and exact; no fallible
// `TryFrom<f64> for i32` exists.
fn i32_from_quantized(quantized: f64) -> i32 {
    quantized as i32
}
