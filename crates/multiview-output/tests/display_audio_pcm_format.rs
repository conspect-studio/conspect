//! PCM sample-format negotiation + conversion tests (DEV-B4 adversarial review
//! MEDIUM-3: a hardcoded float PCM format is silent on real HDA/vc4 devices).
//!
//! Real HDMI audio devices (x86 HDA, the Pi's vc4-hdmi) typically offer only
//! integer formats — S16/S24/S32 — and refuse `FLOAT_LE`, so a sink that only
//! ever asks for float opens nothing and stays silent on exactly the hardware
//! we ship to. The fix is a pure preference-ordered negotiation
//! (float → S32 → S24 → S16, widest first so no precision is given away
//! needlessly) plus sample-accurate float→integer conversion at the write
//! boundary. Both halves are pure and CI-tested here; the feature-gated ALSA
//! backend only wires them to `snd_pcm_hw_params_test_format` and the typed
//! `writei`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::display::audio::{
    f32_to_s16, f32_to_s24, f32_to_s32, negotiate_sample_format, PcmSampleFormat,
};

#[test]
fn s16_only_device_negotiates_s16() {
    // The MEDIUM-3 field case: the device offers nothing but S16 (common on
    // older HDA codecs). Negotiation must land on S16, not fail → silence.
    let picked = negotiate_sample_format(|f| f == PcmSampleFormat::S16);
    assert_eq!(
        picked,
        Some(PcmSampleFormat::S16),
        "an S16-only device must negotiate S16"
    );
}

#[test]
fn negotiation_prefers_float_then_widest_integer_first() {
    // Everything offered => float (no conversion at all).
    assert_eq!(
        negotiate_sample_format(|_| true),
        Some(PcmSampleFormat::Float)
    );
    // Integer-only device => the widest integer wins (S32 before S24/S16).
    assert_eq!(
        negotiate_sample_format(|f| f != PcmSampleFormat::Float),
        Some(PcmSampleFormat::S32)
    );
    // No S32 => S24 before S16.
    assert_eq!(
        negotiate_sample_format(|f| matches!(f, PcmSampleFormat::S24 | PcmSampleFormat::S16)),
        Some(PcmSampleFormat::S24)
    );
    // A device offering none of them cannot be driven.
    assert_eq!(negotiate_sample_format(|_| false), None);
}

#[test]
fn f32_to_s16_is_sample_accurate() {
    // Symmetric full-scale: ±1.0 ↔ ±32767, round-to-nearest, out-of-range
    // input clips (never wraps), non-finite input degrades to silence.
    assert_eq!(
        f32_to_s16(&[0.0, 1.0, -1.0, 0.5, 2.0, -2.0, f32::NAN]),
        vec![0, 32_767, -32_767, 16_384, 32_767, -32_767, 0]
    );
}

#[test]
fn f32_to_s24_is_sample_accurate() {
    // S24 (LSB-justified in a 32-bit container): ±1.0 ↔ ±8388607.
    assert_eq!(
        f32_to_s24(&[0.0, 1.0, -1.0, 0.25, 3.0, f32::NEG_INFINITY]),
        vec![0, 8_388_607, -8_388_607, 2_097_152, 8_388_607, -8_388_607]
    );
}

#[test]
fn f32_to_s32_is_sample_accurate() {
    assert_eq!(
        f32_to_s32(&[0.0, 1.0, -1.0, 0.5, -4.0]),
        vec![
            0,
            2_147_483_647,
            -2_147_483_647,
            1_073_741_824,
            -2_147_483_647
        ]
    );
}
