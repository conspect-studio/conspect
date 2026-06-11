//! AES67 / SMPTE ST 2110-30 RTP audio **packetizer** round-trip tests.
//!
//! The encoder ([`Aes67Packetizer`]) and the decoder ([`V30Payload`]) live in
//! the same crate (`multiview-input::st2110`), sharing one [`SampleDepth`] /
//! [`Aes3Format`] wire model, so an `encode → parse` round-trip proves the byte
//! layout agrees. Pure f32→bytes math: no NIC, no clock, no producer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use multiview_input::st2110::v30::{Aes3Format, SampleDepth, V30Payload};
use multiview_input::st2110::{Aes67Error, Aes67Packetizer};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Silence + sizing
// ---------------------------------------------------------------------------

#[test]
fn encode_l24_stereo_silence_is_all_zero() {
    let enc = Aes67Packetizer::new(2, SampleDepth::L24).expect("valid format");
    let bytes = enc.encode(&[0.0, 0.0, 0.0, 0.0]).expect("valid block");
    assert_eq!(bytes.len(), 4 * 3, "4 samples * 3 bytes per L24 sample");
    assert!(bytes.iter().all(|&b| b == 0), "silence encodes to all zero");
}

#[test]
fn encode_l16_sizing() {
    let enc = Aes67Packetizer::new(2, SampleDepth::L16).expect("valid format");
    let bytes = enc.encode(&[0.0, 0.0, 0.0, 0.0]).expect("valid block");
    assert_eq!(bytes.len(), 4 * 2, "4 samples * 2 bytes per L16 sample");
}

#[test]
fn encode_6ch_l24_group_alignment() {
    let enc = Aes67Packetizer::new(6, SampleDepth::L24).expect("valid format");
    let bytes = enc
        .encode(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6])
        .expect("one full group of six");
    assert_eq!(bytes.len(), 6 * 3);
}

// ---------------------------------------------------------------------------
// Sign correctness — the load-bearing footgun (full-scale must not wrap)
// ---------------------------------------------------------------------------

#[test]
fn l24_full_scale_positive_does_not_sign_flip() {
    let enc = Aes67Packetizer::new(2, SampleDepth::L24).expect("valid format");
    let bytes = enc.encode(&[0.999_999, 0.999_999, 0.5, 0.5]).expect("ok");
    let fmt = Aes3Format::new(2, SampleDepth::L24).expect("ok");
    let decoded = V30Payload::parse(&bytes, fmt).expect("ok");
    assert!(
        decoded.sample(0, 0).expect("ch0") > 0,
        "full-scale positive must stay positive (no 2^23 wrap)"
    );
    assert!(decoded.sample(0, 1).expect("ch1") > 0);
}

#[test]
fn l16_full_scale_negative_stays_negative() {
    let enc = Aes67Packetizer::new(1, SampleDepth::L16).expect("valid format");
    let bytes = enc.encode(&[-1.0]).expect("ok");
    let fmt = Aes3Format::new(1, SampleDepth::L16).expect("ok");
    let decoded = V30Payload::parse(&bytes, fmt).expect("ok");
    assert!(decoded.sample(0, 0).expect("ch0") < 0);
}

#[test]
fn clamp_out_of_range_no_panic() {
    let enc = Aes67Packetizer::new(1, SampleDepth::L16).expect("valid format");
    let bytes = enc.encode(&[2.5, -3.0, 0.0]).expect("clamped, no panic");
    assert_eq!(bytes.len(), 3 * 2);
    let fmt = Aes3Format::new(1, SampleDepth::L16).expect("ok");
    let decoded = V30Payload::parse(&bytes, fmt).expect("ok");
    // +2.5 clamps to +1.0 → +32767; -3.0 clamps to -1.0 → -32767.
    assert!(decoded.sample(0, 0).expect("s0") > 0);
    assert!(decoded.sample(1, 0).expect("s1") < 0);
    assert_eq!(decoded.sample(2, 0).expect("s2"), 0);
}

// ---------------------------------------------------------------------------
// Partial-group framing (RFC 3190 whole-sample-group)
// ---------------------------------------------------------------------------

#[test]
fn reject_partial_group_stereo() {
    let enc = Aes67Packetizer::new(2, SampleDepth::L16).expect("valid format");
    assert!(matches!(
        enc.encode(&[0.1, 0.2, 0.3]),
        Err(Aes67Error::PartialGroup { .. })
    ));
}

#[test]
fn reject_zero_channels() {
    assert!(matches!(
        Aes67Packetizer::new(0, SampleDepth::L24),
        Err(Aes67Error::ZeroChannels)
    ));
}

// ---------------------------------------------------------------------------
// Round-trip property: encode → V30Payload::parse reconstructs within 1 LSB,
// preserves sign and channel order, never panics.
// ---------------------------------------------------------------------------

/// Reconstruct an f32 in `[-1, 1]` from a decoded depth-aware integer sample.
fn to_unit(depth: SampleDepth, raw: i32) -> f32 {
    match depth {
        SampleDepth::L16 => f32::from(i16::try_from(raw).unwrap_or(0)) / 32_767.0,
        // L24 full-scale magnitude is 2^23 - 1.
        SampleDepth::L24 => raw as f32 / 8_388_607.0,
        _ => 0.0,
    }
}

proptest! {
    #[test]
    fn encode_never_panics(
        channels in 1u8..=8,
        l24 in any::<bool>(),
        samples in proptest::collection::vec(-4.0f32..4.0, 0..64),
    ) {
        let depth = if l24 { SampleDepth::L24 } else { SampleDepth::L16 };
        let enc = Aes67Packetizer::new(channels, depth).expect("nonzero channels");
        // Must return a typed error or a byte buffer — never panic.
        let _ = enc.encode(&samples);
    }

    #[test]
    fn round_trip_within_one_lsb(
        channels in 1u8..=8,
        l24 in any::<bool>(),
        groups in 1usize..=16,
        seed in proptest::collection::vec(-1.0f32..=1.0, 1..=128),
    ) {
        let depth = if l24 { SampleDepth::L24 } else { SampleDepth::L16 };
        let total = usize::from(channels) * groups;
        let samples: Vec<f32> = (0..total).map(|i| seed[i % seed.len()]).collect();

        let enc = Aes67Packetizer::new(channels, depth).expect("nonzero channels");
        let bytes = enc.encode(&samples).expect("aligned block encodes");

        let fmt = Aes3Format::new(channels, depth).expect("ok");
        let decoded = V30Payload::parse(&bytes, fmt).expect("encoder output is whole groups");
        prop_assert_eq!(decoded.group_count(), groups);

        let lsb = match depth {
            SampleDepth::L16 => 1.0 / 32_767.0,
            _ => 1.0 / 8_388_607.0,
        };
        for g in 0..groups {
            for ch in 0..usize::from(channels) {
                let raw = decoded.sample(g, ch).expect("in range");
                let recon = to_unit(depth, raw);
                let orig = samples[g * usize::from(channels) + ch];
                // Quantization error is < 1 LSB.
                prop_assert!((orig - recon).abs() <= lsb + 1e-6,
                    "orig {} recon {} depth {:?}", orig, recon, depth);
                // Sign is preserved for non-trivial magnitudes.
                if orig.abs() > 2.0 * lsb {
                    prop_assert_eq!(orig.is_sign_positive(), raw > 0);
                }
            }
        }
    }
}
