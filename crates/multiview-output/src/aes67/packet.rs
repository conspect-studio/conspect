//! AES67 / ST 2110-30 send-side wire framing (ADR-0033 §1): the `f32` → L16/L24
//! big-endian PCM payload encoder and the 12-byte RTP fixed-header builder.
//!
//! The **canonical send packetizer lives here in `multiview-output`** (ADR-0033
//! §1), not in `multiview-input`: the two crates stay decoupled (input owns the
//! depacketize/parse side, output owns the packetize/build side), exactly like
//! the TSL protocol's independent per-crate codecs. A dev-dependency round-trip
//! test pins this encoder byte-identical to `multiview-input`'s `V30Payload`
//! decoder — encode here, decode there, assert equality.
//!
//! ## The encode footgun (RFC 3190 / AES67)
//!
//! - **L16:** clamp to `[-1.0, 1.0]`, scale by `32767`, round, emit `i16`
//!   big-endian.
//! - **L24:** clamp, scale by **`8388607`** (`2^23 - 1`, **not** `2^23`), round,
//!   emit the high three bytes of `(v << 8)` big-endian — the exact layout
//!   `multiview-input::st2110::v30` sign-extends. Scaling by `2^23` would wrap a
//!   full-scale-positive sample to the most-negative code (an audible click).

/// PCM sample depth for an AES67 / ST 2110-30 stream (Class A: L16 or L24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PcmDepth {
    /// 16-bit signed PCM ("L16"), 2 bytes per sample.
    L16,
    /// 24-bit signed PCM ("L24"), 3 bytes per sample.
    L24,
}

impl PcmDepth {
    /// The on-wire byte width of one sample at this depth.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            PcmDepth::L16 => 2,
            PcmDepth::L24 => 3,
        }
    }
}

/// The RTP version this sender emits (RFC 3550 fixes it at `2`).
pub const RTP_VERSION: u8 = 2;

/// The RTP fixed-header length in bytes (no CSRC list, no extension).
pub const RTP_FIXED_HEADER_LEN: usize = 12;

/// Build the 12-byte RTP fixed header for one AES67 audio packet.
///
/// Version 2, **no** padding / extension / CSRC, **marker = 0 on every packet**
/// (Multiview is a continuous, non-silence-suppressed sender — RFC 3551 /
/// ADR-0033 §1, invariant #1), a dynamic `payload_type`, and big-endian
/// `sequence` / `timestamp` / `ssrc`. Array-pattern destructuring keeps it free
/// of slice indexing.
///
/// The payload type occupies 7 bits; [`Aes67Sender`](super::sender::Aes67Sender)
/// validates it to `0..=127` at construction (panel F7), so the `& 0x7f` below is
/// defensive-in-depth for any direct caller and never silently reshapes a
/// sender's already-validated type.
#[must_use]
pub fn build_rtp_header(
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
) -> [u8; RTP_FIXED_HEADER_LEN] {
    let [s0, s1] = sequence.to_be_bytes();
    let [t0, t1, t2, t3] = timestamp.to_be_bytes();
    let [c0, c1, c2, c3] = ssrc.to_be_bytes();
    [
        // V=2 (top 2 bits); P=0, X=0, CC=0.
        RTP_VERSION << 6,
        // M=0 (continuous stream); 7-bit payload type.
        payload_type & 0x7f,
        s0,
        s1,
        t0,
        t1,
        t2,
        t3,
        c0,
        c1,
        c2,
        c3,
    ]
}

/// Encode interleaved unit-range `f32` samples to big-endian L16/L24 RTP payload
/// bytes (the whole-sample-group PCM body that rides directly in an RTP packet).
///
/// `samples` is interleaved frame-major; the caller (the sender) supplies a
/// whole number of sample groups, so no partial-group check is needed here.
#[must_use]
pub fn encode_pcm(samples: &[f32], depth: PcmDepth) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len().saturating_mul(depth.bytes_per_sample()));
    encode_pcm_append(&mut bytes, samples, depth);
    bytes
}

/// Encode interleaved unit-range `f32` samples, **appending** the big-endian
/// L16/L24 bytes to `out` (never clearing it) — the allocation-free core of
/// [`encode_pcm`] that the continuous send path reuses to avoid a per-packet
/// payload allocation (rule 22). The caller reserves/clears `out`.
pub fn encode_pcm_append(out: &mut Vec<u8>, samples: &[f32], depth: PcmDepth) {
    match depth {
        PcmDepth::L16 => {
            for &sample in samples {
                out.extend_from_slice(&encode_l16(sample).to_be_bytes());
            }
        }
        PcmDepth::L24 => {
            for &sample in samples {
                // The 24-bit code occupies the high three octets of the
                // sign-extended `i32` shifted left by 8, exactly matching the
                // decoder's `i32::from_be_bytes([hi, mid, lo, 0]) >> 8`.
                let [hi, mid, lo, _low] = (encode_l24(sample) << 8).to_be_bytes();
                out.extend_from_slice(&[hi, mid, lo]);
            }
        }
    }
}

/// Map a unit-range `f32` to a 16-bit PCM code (clamped, round-to-nearest). The
/// result is in `[-32767, 32767]`; `-32768` is never produced, so the encode is
/// symmetric and the decoder round-trip preserves sign.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: `sample` is clamped to [-1.0, 1.0] then scaled by 32767 and rounded,
// so the value is exactly within `i16` range — the narrowing cast cannot
// truncate or wrap. Mirrors the reviewed pattern in `multiview-input::st2110::packetize`.
fn encode_l16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// Map a unit-range `f32` to a 24-bit PCM code (clamped, round-to-nearest),
/// returned in the low 24 bits of an `i32`. Scales by `2^23 - 1` (`8388607`),
/// **not** `2^23`, so full-scale positive does not wrap to the most-negative
/// code (an audible click).
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: `sample` is clamped to [-1.0, 1.0] then scaled by 8_388_607 and
// rounded, so the value is within `[-8_388_607, 8_388_607]` — firmly inside
// `i32`. Mirrors the reviewed pattern in `multiview-input::st2110::packetize`.
fn encode_l24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32
}
