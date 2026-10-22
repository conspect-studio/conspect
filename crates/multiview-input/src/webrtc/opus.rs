//! The pure **Opus RTP depacketizer** (RFC 7587) — feature `webrtc`.
//!
//! RFC 7587 §4.2 pins the payload structure: an RTP payload **MUST contain
//! exactly one Opus packet** — there is no fragmentation, no aggregation, and
//! no payload header. Depacketization is therefore a validation + pass-through:
//! the payload bytes *are* the codec frame the decoder consumes.
//!
//! ## Timing (invariant #3)
//!
//! RFC 7587 §4.1 fixes the RTP timestamp clock at **48 kHz regardless of the
//! encoded audio bandwidth or sampling rate** ([`AUDIO_CLOCK_RATE`]). The
//! depacketizer surfaces the 32-bit RTP timestamp **verbatim** as the raw PTS;
//! downstream rebase (wrap unwrap via
//! [`WrapBits::Rtp32`](crate::normalize::WrapBits::Rtp32), anchor,
//! discontinuity re-anchor, reorder placement, silence-fill) is the shared
//! ADR-T013 contract — never re-implemented here (ADR-T014 §5).
//!
//! ## Resilience (invariants #1 / #2 / #5)
//!
//! A sequence gap (lost packet) — detected with the same RFC 1982 serial
//! arithmetic the other RTP ingests use — surfaces as a **discontinuity flag**
//! on the next emitted frame, as does a locally dropped packet (empty or
//! over-cap payload). The depacketizer is a pure state machine over injected
//! packets: bounded, allocation-light (one buffer clone per emitted frame,
//! nothing retained), and it never panics on empty or garbage payloads — it
//! drops and counts them instead. It never reads a socket and never blocks.

use crate::webrtc::transport::{RtpFrame, SequenceTracker};

/// The RTP media clock rate WebRTC Opus audio rides on.
///
/// RFC 7587 §4.1: the RTP timestamp clock for Opus is **always 48 kHz**, even
/// when the encoded bandwidth is narrower. (The video clock is the separate
/// 90 kHz [`VIDEO_CLOCK_RATE`](crate::webrtc::transport::VIDEO_CLOCK_RATE);
/// both are 32-bit RTP clocks —
/// [`WrapBits::Rtp32`](crate::normalize::WrapBits::Rtp32).)
pub const AUDIO_CLOCK_RATE: u32 = 48_000;

/// An upper bound on one Opus packet's payload bytes.
///
/// RFC 6716 framing tops out near 61.4 KiB (up to 48 frames of at most 1275
/// bytes each, plus the TOC/length overhead), so 64 KiB admits every conformant
/// packet. Anything larger is hostile or corrupt and is **dropped, never
/// buffered** — bounded memory regardless of the input (invariant #5).
pub const MAX_OPUS_PACKET_BYTES: usize = 64 * 1024;

/// One depacketized Opus frame.
///
/// `data` is the Opus packet bytes **verbatim** (RFC 7587: the RTP payload is
/// exactly one Opus packet); `raw_pts` is the 32-bit RTP timestamp on the
/// 48 kHz clock, surfaced untouched for the shared rebase path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusFrame {
    /// The Opus packet bytes, exactly as carried in the RTP payload.
    pub data: Vec<u8>,
    /// The 32-bit RTP timestamp (48 kHz clock) as a producer-timebase raw PTS
    /// ([`WrapBits::Rtp32`](crate::normalize::WrapBits::Rtp32)). Always `Some`
    /// (every RTP packet is timestamped); typed `Option` to match the
    /// raw-PTS plumbing downstream.
    pub raw_pts: Option<i64>,
    /// Whether a decode gap precedes this frame: a sequence gap (lost packet)
    /// was observed, or a preceding packet was locally dropped (empty /
    /// over-cap payload). The downstream rebase re-anchors on it.
    pub discontinuity: bool,
}

/// The Opus RTP depacketizer (RFC 7587): one RTP payload == one Opus frame.
///
/// A pure, bounded state machine over injected packets. Tracks the RTP
/// sequence watermark for forward-gap detection, validates the payload (a
/// valid Opus packet carries at least its TOC byte and at most
/// [`MAX_OPUS_PACKET_BYTES`]), and emits each valid payload verbatim as an
/// [`OpusFrame`]. Invalid payloads are dropped and counted; the decode gap
/// they create flags the next emitted frame. Never panics, never blocks,
/// never grows (invariants #1 / #2 / #5).
#[derive(Debug, Default)]
pub struct OpusDepacketizer {
    /// The sequence watermark for forward-gap (loss) detection.
    sequence: SequenceTracker,
    /// Whether a packet was dropped (or a gap absorbed into a drop) since the
    /// last emitted frame; surfaces on the next emit.
    pending_discontinuity: bool,
    /// Count of packets dropped for an invalid payload (empty or over-cap).
    dropped: u64,
}

impl OpusDepacketizer {
    /// Construct a depacketizer with no sequence observed yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sequence: SequenceTracker::new(),
            pending_discontinuity: false,
            dropped: 0,
        }
    }

    /// The number of packets dropped for an invalid payload (empty or over
    /// [`MAX_OPUS_PACKET_BYTES`]).
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Push one decrypted RTP packet, returning the Opus frame it carries.
    ///
    /// Returns `None` for an invalid payload (empty, or larger than
    /// [`MAX_OPUS_PACKET_BYTES`]): the packet is dropped and counted, and the
    /// decode gap flags the next emitted frame's `discontinuity`. The RTP
    /// marker bit (first packet after a DTX silence period, RFC 7587 §4.1) is
    /// intentionally ignored — silence-fill is the downstream rebase
    /// contract's job, not the depacketizer's. Never blocks and never panics
    /// on garbage.
    pub fn push(&mut self, packet: &RtpFrame) -> Option<OpusFrame> {
        // Track the watermark on EVERY packet (dropped ones included) so a
        // later in-order packet does not mis-flag its own gap.
        let gap = self.sequence.note(packet.sequence);
        if packet.payload.is_empty() || packet.payload.len() > MAX_OPUS_PACKET_BYTES {
            // Not a valid Opus packet (RFC 6716: at least the TOC byte; the
            // cap bounds memory). Drop, count, and surface the decode gap on
            // the next emitted frame — `gap` is absorbed into the same flag.
            self.dropped = self.dropped.saturating_add(1);
            self.pending_discontinuity = true;
            return None;
        }
        let pending = core::mem::take(&mut self.pending_discontinuity);
        Some(OpusFrame {
            data: packet.payload.clone(),
            raw_pts: Some(i64::from(packet.timestamp)),
            discontinuity: gap || pending,
        })
    }
}
