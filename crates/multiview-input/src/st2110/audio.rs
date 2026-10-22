//! ST 2110-30 / AES67 PCM-audio **receive producer** (ADR-0033 §3, ADR-T013).
//!
//! The audio analogue of the video
//! [`St2110Producer`](super::transport::St2110Producer): it pulls RTP packet
//! units from a [`PacketSource`](super::transport::PacketSource), depacketizes
//! each with the pure [`V30Payload`] parser, converts the big-endian L16/L24
//! samples to the canonical interleaved `f32`, and yields an [`Aes67AudioFrame`]
//! carrying the **verbatim** 32-bit RTP media timestamp + SSRC + a
//! sequence-gap discontinuity flag.
//!
//! It deliberately does **not** touch the `AudioStore`: `multiview-input` does
//! not depend on `multiview-audio`. The caller (the CLI pipeline) feeds the
//! yielded frame's `(raw_timestamp, ssrc, discontinuity)` through the shared
//! [`RtpAudioRebaser`](crate::rtp_audio) — the one ADR-T013 rebase contract the
//! WebRTC-Opus path also uses — onto an absolute `AudioStore` frame index and
//! `publish_at`s the `f32` block there. Splitting the depacketize (here) from
//! the rebase+publish (the caller) keeps this crate decoupled from the store
//! exactly as the WebRTC audio path is.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! [`Aes67AudioProducer::next_audio`] does **non-blocking pulls only** from the
//! source and never paces the output clock: an empty source yields `Ok(None)`
//! (the caller re-polls next tick) and a malformed payload is skipped, never
//! faulted. The stream is *sampled*, never *pacing* — a stalled or absent RTP
//! source simply stops yielding and the store rides silence-fill.

use super::v30::{Aes3Format, SampleDepth, V30Payload};

/// Convert one depacketized ST 2110-30 payload to interleaved canonical `f32`
/// (frame-major, length `group_count * channels`).
///
/// The exact inverse-scale of the [`Aes67Packetizer`](super::packetize::Aes67Packetizer)
/// encode: **L16** divides by `32768` (`2^15`) and **L24** by `8388608`
/// (`2^23`). The encode scales by `32767` / `8388607` to keep full-scale
/// positive from wrapping to the most-negative code; that ±1-LSB asymmetry is
/// standard AES67 and lossless in the decode direction (a decoded sample lands
/// in `[-1.0, 1.0)`). Never panics and never `as`-casts a raw sample directly.
#[must_use]
pub fn pcm_to_f32(payload: &V30Payload<'_>) -> Vec<f32> {
    let channels = usize::from(payload.format.channels);
    let groups = payload.group_count();
    let depth = payload.format.depth;
    let mut out = Vec::with_capacity(groups.saturating_mul(channels));
    for group in 0..groups {
        for channel in 0..channels {
            // `sample` returns `None` only out of range, which the `0..groups` /
            // `0..channels` bounds never hit; a defensive `unwrap_or(0)` keeps
            // the decode panic-free on the data plane rather than indexing.
            let code = payload.sample(group, channel).unwrap_or(0);
            out.push(code_to_unit_f32(code, depth));
        }
    }
    out
}

/// Convert one signed PCM code to a unit-range `f32` for the given depth
/// (**L16** `/32768`, **L24** `/8388608`).
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: `code` magnitude is <= 2^15 (L16) or <= 2^23 (L24), both < 2^24 and
// therefore exactly representable in `f32`; dividing by the 2^k scale yields a
// value in [-1.0, 1.0). There is no allocation-free fallible `i32 -> f32`; this
// mirrors the reviewed float<->int pattern in `st2110::packetize` and
// `multiview-audio::mixer`.
fn code_to_unit_f32(code: i32, depth: SampleDepth) -> f32 {
    let scale = match depth {
        SampleDepth::L16 => 32_768.0_f32,
        SampleDepth::L24 => 8_388_608.0_f32,
    };
    code as f32 / scale
}

/// One depacketized ST 2110-30 / AES67 audio unit yielded by
/// [`Aes67AudioProducer`].
///
/// Carries the fields the shared [`RtpAudioRebaser`](crate::rtp_audio) consumes
/// (`raw_timestamp`, `ssrc`, `discontinuity`) plus the decoded interleaved
/// `f32` block the caller places on the `AudioStore` at the rebased frame index.
#[derive(Debug, Clone, PartialEq)]
pub struct Aes67AudioFrame {
    /// The verbatim 32-bit RTP media timestamp (audio sample-rate ticks — e.g.
    /// 48 kHz for Class A, advancing by sample-groups per packet). The caller
    /// rebases this via [`RtpAudioRebaser`](crate::rtp_audio).
    pub raw_timestamp: u32,
    /// The RTP synchronization source; a change re-anchors the rebaser.
    pub ssrc: u32,
    /// Whether a sequence gap was observed before this unit (a lost packet the
    /// rebaser treats as a discontinuity).
    pub discontinuity: bool,
    /// The PCM format (channel count + depth) the payload was parsed against.
    pub format: Aes3Format,
    /// Interleaved canonical `f32` samples (frame-major), length a whole
    /// multiple of the channel count.
    pub samples: Vec<f32>,
}

#[cfg(feature = "st2110")]
use crate::error::Result;
#[cfg(feature = "st2110")]
use crate::st2110::transport::PacketSource;

/// A receive producer over a [`PacketSource`](super::transport::PacketSource)
/// that yields [`Aes67AudioFrame`]s for the ADR-T013 rebase seam.
///
/// The audio IN-2 bridge for ST 2110-30 / AES67: non-blocking pulls only, never
/// paces the output clock, and reuses the corrected bounded drop-oldest
/// [`ChannelPacketSource`](super::transport::ChannelPacketSource) for the live
/// socket path (invariants #1 / #10).
#[cfg(feature = "st2110")]
pub struct Aes67AudioProducer {
    source: Box<dyn PacketSource + Send>,
    format: Aes3Format,
    /// The SDP-negotiated RTP payload type this session decodes. A packet whose
    /// payload type differs is a stray / multiplexed stream on the same 5-tuple and
    /// is dropped, not decoded as PCM (RFC 3550 §5.1).
    expected_payload_type: u8,
    /// The SSRC the [`last_sequence`](Self::last_sequence) watermark belongs to.
    /// Sequence numbers are only comparable within one synchronization source, so
    /// a change resets the watermark (a new SSRC is a new sequence space).
    last_ssrc: Option<u32>,
    /// The highest in-stream RTP sequence accepted so far **for `last_ssrc`** (for
    /// gap detection). Reset whenever the SSRC changes.
    last_sequence: Option<u16>,
}

#[cfg(feature = "st2110")]
impl core::fmt::Debug for Aes67AudioProducer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `Box<dyn PacketSource>` is not `Debug` (like the video St2110Producer).
        f.debug_struct("Aes67AudioProducer")
            .field("format", &self.format)
            .field("expected_payload_type", &self.expected_payload_type)
            .field("last_ssrc", &self.last_ssrc)
            .field("last_sequence", &self.last_sequence)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "st2110")]
impl Aes67AudioProducer {
    /// The maximum number of source packets a single [`next_audio`](Self::next_audio)
    /// call pulls before yielding `Ok(None)`.
    ///
    /// A sample must be **bounded work**: a malformed-packet flood must never
    /// make one poll loop until the source drains, which would delay the output
    /// clock (invariant #1). After this many skipped units the call returns
    /// `Ok(None)` and the caller re-polls next tick, so a flood is drained at up
    /// to this rate per tick rather than all at once. Sized well above any real
    /// per-tick packet count (a 1 ms AES67 stream delivers one unit per tick).
    pub const MAX_PACKETS_PER_POLL: usize = 64;

    /// Build a producer over an application-supplied packet source, decoding
    /// against the SDP-negotiated [`Aes3Format`] and filtering to the
    /// SDP-negotiated RTP `payload_type`.
    ///
    /// A packet whose RTP payload type differs from `payload_type` is a stray /
    /// multiplexed stream on the same 5-tuple and is dropped (not decoded as PCM),
    /// exactly like a malformed payload — see [`next_audio`](Self::next_audio).
    #[must_use]
    pub fn new(source: Box<dyn PacketSource + Send>, format: Aes3Format, payload_type: u8) -> Self {
        Self {
            source,
            format,
            expected_payload_type: payload_type,
            last_ssrc: None,
            last_sequence: None,
        }
    }

    /// The PCM format this producer decodes against.
    #[must_use]
    pub const fn format(&self) -> Aes3Format {
        self.format
    }

    /// Pull the next depacketized audio unit, or `Ok(None)` when nothing is
    /// ready this tick.
    ///
    /// Non-blocking: an empty source yields `Ok(None)` (the caller re-polls next
    /// tick) and never blocks the data plane (invariant #1). A packet whose RTP
    /// payload type is not the session's, or whose payload is malformed (not a
    /// whole number of sample groups for the configured format), is **skipped**,
    /// never faulted — a stray/multiplexed stream on the same 5-tuple or a single
    /// bad datagram must not be decoded as PCM or stall the stream (invariants
    /// #1 / #2). A gap against the last accepted RTP sequence sets
    /// [`Aes67AudioFrame::discontinuity`].
    ///
    /// **Bounded work (inv #1):** the skip loop pulls at most
    /// [`MAX_PACKETS_PER_POLL`] units per call. A malformed-packet flood is drained
    /// at that rate per tick rather than all at once, so one call — whether it
    /// runs on the output-sample side or a shared ingest thread — can never do
    /// unbounded work and delay the output clock (panel F1). Budget exhausted
    /// with nothing valid yields `Ok(None)`; the caller re-polls next tick.
    ///
    /// # Errors
    ///
    /// Propagates a source fault (a socket error surfaced by the underlying
    /// [`PacketSource`](super::transport::PacketSource)); the supervisor applies
    /// reconnect backoff rather than crashing the engine.
    pub fn next_audio(&mut self) -> Result<Option<Aes67AudioFrame>> {
        for _ in 0..Self::MAX_PACKETS_PER_POLL {
            let Some(packet) = self.source.poll_packet()? else {
                return Ok(None);
            };
            // A packet from a DIFFERENT RTP payload type is a stray / multiplexed
            // stream on the same 5-tuple — drop it (never decode it as our PCM),
            // like a malformed payload. Counts against the per-poll budget below.
            if packet.payload_type != self.expected_payload_type {
                continue;
            }
            // A malformed / partial-group payload is dropped, not faulted.
            let Ok(payload) = V30Payload::parse(&packet.payload, self.format) else {
                continue;
            };
            let discontinuity = self.observe_sequence(packet.ssrc, packet.sequence);
            let samples = pcm_to_f32(&payload);
            return Ok(Some(Aes67AudioFrame {
                raw_timestamp: packet.timestamp,
                ssrc: packet.ssrc,
                discontinuity,
                format: self.format,
                samples,
            }));
        }
        // Budget exhausted on skipped units this tick: yield and re-poll next
        // tick so a flood can never make one sample unbounded work (inv #1).
        Ok(None)
    }

    /// Update the sequence watermark for `ssrc` and report whether a forward gap
    /// (a lost packet) was observed. A stale reordered packet does not move the
    /// watermark, so a later in-order packet still detects its own gap.
    ///
    /// Sequence numbers are only comparable **within one synchronization
    /// source**, so a change of `ssrc` is a new stream: the watermark resets to
    /// this packet's sequence and no gap is reported (the first packet of a stream
    /// anchors — it is not a lost-packet gap; the shared
    /// [`RtpAudioRebaser`](crate::rtp_audio) re-anchors on the SSRC change
    /// itself). Without this, a new stream whose sequence base RFC 1982 arithmetic
    /// reads as "before" the old watermark would be judged perpetually stale, the
    /// watermark would never advance, and real gaps on the new stream would be
    /// missed (P2-F5).
    fn observe_sequence(&mut self, ssrc: u32, sequence: u16) -> bool {
        if self.last_ssrc != Some(ssrc) {
            self.last_ssrc = Some(ssrc);
            self.last_sequence = Some(sequence);
            return false;
        }
        let gap = self.last_sequence.is_some_and(|prev| {
            sequence != prev.wrapping_add(1) && crate::st2110::rtp::seq_after(prev, sequence)
        });
        let is_stale = self
            .last_sequence
            .is_some_and(|prev| !crate::st2110::rtp::seq_after(prev, sequence));
        if !is_stale {
            self.last_sequence = Some(sequence);
        }
        gap
    }
}
