//! AES67 / SMPTE **ST 2110-30** PCM-audio RTP **packetizer** (pure egress codec).
//!
//! This is the exact inverse of [`V30Payload::parse`](super::v30::V30Payload):
//! it turns a block of interleaved `f32` PCM samples into the big-endian
//! L16/L24 sample-group bytes that ride directly in an RTP payload
//! (RFC 3190 / AES67). There is no per-payload header — the channel count and
//! sample depth are the SDP-negotiated [`Aes3Format`], supplied at construction.
//! The encoder is **pure**: no I/O, no clock, no socket; it round-trips against
//! the depacketizer entirely offline.
//!
//! It lives next to the depacketizer (rather than in `multiview-output`) so the
//! two share one [`SampleDepth`] / [`Aes3Format`] wire model: the sibling output
//! crate does not depend on this crate, and an RTP transmit path (a later,
//! feature-gated slice) reuses this codec through the gated transport layer
//! here.
//!
//! ## Encoding rules (the load-bearing footgun)
//!
//! - **L16:** clamp each sample to `[-1.0, 1.0]`, multiply by `32767`, round to
//!   the nearest integer, and emit the `i16` big-endian.
//! - **L24:** clamp to `[-1.0, 1.0]`, multiply by **`8388607`** (`2^23 - 1`, NOT
//!   `2^23 = 8388608`), round, then emit the high three bytes of `(value << 8)`
//!   in big-endian order — exactly the layout
//!   [`V30Payload::sample`](super::v30::V30Payload::sample) sign-extends.
//!   Scaling by `2^23` would wrap a full-scale-positive sample (`+1.0`) to the
//!   most-negative code, producing an audible click, so the maximum-magnitude
//!   code is `2^23 - 1`.
//!
//! ## Whole-sample-group framing (RFC 3190)
//!
//! A payload is always a whole number of sample groups (one sample per channel).
//! [`Aes67Packetizer::encode`] rejects an input whose length is not a multiple of
//! the channel count with [`Aes67Error::PartialGroup`], mirroring the
//! depacketizer's [`V30Error::PartialGroup`](super::v30::V30Error) check.

use super::v30::{Aes3Format, SampleDepth, V30Error};

/// Errors raised while packetizing an AES67 / ST 2110-30 audio block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Aes67Error {
    /// The packetizer was constructed with zero channels (surfaced from
    /// [`Aes3Format::new`]).
    #[error("aes67: a packetizer must have at least one channel")]
    ZeroChannels,

    /// The sample block length was not a whole number of sample groups for the
    /// configured channel count (a partial group is a framing error).
    #[error("aes67: sample block {samples} is not a multiple of channel count {channels}")]
    PartialGroup {
        /// The number of `f32` samples supplied.
        samples: usize,
        /// The configured channel count.
        channels: usize,
    },
}

impl From<V30Error> for Aes67Error {
    fn from(value: V30Error) -> Self {
        // `V30Error` is `#[non_exhaustive]`, but it is defined in this crate, so
        // this match is exhaustive over its current variants; a new variant added
        // there forces this mapping to be updated (the desired within-crate
        // behaviour). The only constructor that yields a `V30Error` here is
        // `Aes3Format::new`, which raises `ZeroChannels`.
        match value {
            V30Error::ZeroChannels => Aes67Error::ZeroChannels,
            V30Error::PartialGroup { payload, group } => Aes67Error::PartialGroup {
                samples: payload,
                channels: group,
            },
        }
    }
}

/// An AES67 / ST 2110-30 RTP payload encoder for a fixed PCM [`Aes3Format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aes67Packetizer {
    format: Aes3Format,
}

impl Aes67Packetizer {
    /// Construct an encoder for `channels` interleaved channels at `depth`.
    ///
    /// # Errors
    ///
    /// [`Aes67Error::ZeroChannels`] if `channels` is `0`.
    pub fn new(channels: u8, depth: SampleDepth) -> Result<Self, Aes67Error> {
        let format = Aes3Format::new(channels, depth)?;
        Ok(Self { format })
    }

    /// The configured PCM format.
    #[must_use]
    pub const fn format(&self) -> Aes3Format {
        self.format
    }

    /// Encode a block of interleaved `f32` PCM samples into big-endian L16/L24
    /// RTP payload bytes.
    ///
    /// `samples` is interleaved frame-major (`[ch0, ch1, …, ch0, ch1, …]`); its
    /// length must be a whole multiple of the channel count.
    ///
    /// # Errors
    ///
    /// [`Aes67Error::PartialGroup`] if `samples.len()` is not a multiple of the
    /// channel count.
    pub fn encode(&self, samples: &[f32]) -> Result<Vec<u8>, Aes67Error> {
        let channels = usize::from(self.format.channels);
        if samples.len() % channels != 0 {
            return Err(Aes67Error::PartialGroup {
                samples: samples.len(),
                channels,
            });
        }
        let mut bytes = Vec::with_capacity(samples.len() * self.format.depth.bytes_per_sample());
        match self.format.depth {
            SampleDepth::L16 => {
                for &sample in samples {
                    bytes.extend_from_slice(&encode_l16(sample).to_be_bytes());
                }
            }
            SampleDepth::L24 => {
                for &sample in samples {
                    // The 24-bit code occupies the high three bytes of the
                    // sign-extended `i32` shifted left by 8, exactly matching the
                    // depacketizer's `i32::from_be_bytes([hi, mid, lo, 0]) >> 8`.
                    let be = (encode_l24(sample) << 8).to_be_bytes();
                    // The top three octets are the L24 sample, MSB first.
                    bytes.extend_from_slice(&be[0..3]);
                }
            }
        }
        Ok(bytes)
    }
}

/// Map a unit-range `f32` to a 16-bit PCM code (clamped, round-to-nearest).
///
/// The result is in `[-32767, 32767]`; `-32768` is never produced so the encode
/// is symmetric and the round-trip through the depacketizer preserves sign.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: `sample` is clamped to [-1.0, 1.0] then scaled by 32767 and rounded,
// so the value is exactly within `i16` range — the narrowing cast cannot
// truncate or wrap. There is no allocation-free safe float→int conversion that
// avoids `as`; this mirrors the reviewed pattern in `multiview-audio::mixer`.
fn encode_l16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// Map a unit-range `f32` to a 24-bit PCM code (clamped, round-to-nearest),
/// returned in the low 24 bits of an `i32` (sign-extended).
///
/// Scales by `2^23 - 1` (`8388607`), NOT `2^23`, so full-scale positive does not
/// wrap to the most-negative code.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: `sample` is clamped to [-1.0, 1.0] then scaled by 8_388_607 and
// rounded, so the value is within `[-8_388_607, 8_388_607]` — firmly inside
// `i32`, so the cast cannot truncate or wrap. Same reviewed pattern as
// `encode_l16` / `multiview-audio::mixer`.
fn encode_l24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32
}
