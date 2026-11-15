//! The WebRTC program-output **egress feed** — the bounded, drop-oldest seam the
//! `webrtc` (WHEP-serve) and `whip_push` outputs pull the **already-encoded**
//! program access units through (ADR-0049 §5).
//!
//! This is the egress twin of the WHIP-ingest [`RtpRing`](crate::transport::RtpRing)
//! and the preview [`SampleFeed`](multiview_preview::whep::transport::SampleFeed):
//! the cli's bake consumer encodes the program **once** (invariant #7) and fans
//! the same coded [`multiview_ffmpeg::EncodedPacket`]s to every transport; a
//! WebRTC output's sink runner re-stamps each into an [`EgressSample`] (the codec
//! AU bytes + its 90 kHz / 48 kHz RTP timestamp + the IDR flag) and **pushes** it
//! here. The native WHEP-serve / WHIP-push driver drains the paired [`EgressFeed`]
//! and str0m sample-writes the bytes into every viewer / the push session.
//!
//! It is **pure** — no str0m, no socket, no `multiview-ffmpeg` — so it lives in the
//! crate's default build and is unit-tested in ordinary CI. The encode-once proof
//! (the SAME AU bytes fan to N feeds) and the invariant-#10 isolation proof (a
//! stalled consumer drops, never grows, never blocks the producer) are both
//! exercised here without a network.
//!
//! ## Isolation (invariant #10)
//!
//! [`EgressSink::push`] is **wait-free and non-blocking**: it takes only the
//! feed's short-lived bookkeeping mutex (never a lock the engine or the encoder
//! awaits) and, when the bounded ring is full, evicts the **oldest** buffered
//! sample. A slow or stalled WHEP player / WHIP target therefore loses *its* media
//! and can never back-pressure the encode-once fan-out, the bake consumer, or the
//! output clock — the engine never awaits a WebRTC consumer.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The bound on buffered egress samples before the oldest is dropped (drop-oldest,
/// invariant #10 / safety rule 5 — a slow viewer never grows memory). Sized to
/// hold a few seconds of program GOPs so a brief network stall absorbs without
/// growth, but bounded so a wedged consumer cannot leak.
pub const MAX_EGRESS: usize = 512;

/// Which elementary stream an [`EgressSample`] carries — and therefore which RTP
/// clock its [`EgressSample::rtp_timestamp`] is expressed in (ADR-0049 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EgressMedia {
    /// An H.264 video access unit. Timestamps ride the **90 kHz** RTP clock.
    Video,
    /// An Opus audio frame. Timestamps ride the **48 kHz** RTP clock (RFC 7587).
    Audio,
}

impl EgressMedia {
    /// The RTP clock rate (Hz) this kind's timestamps are expressed in: 90 kHz
    /// for video, 48 kHz for Opus audio (RFC 7587).
    #[must_use]
    pub const fn rtp_clock_hz(self) -> u32 {
        match self {
            Self::Video => 90_000,
            Self::Audio => 48_000,
        }
    }

    /// Derive the 32-bit wire RTP timestamp for a packet stamped `pts` in
    /// `time_base` (the packet's tick-derived PTS, invariant #3): rescale it
    /// exactly onto this kind's RTP clock ([`Self::rtp_clock_hz`]) and **wrap it
    /// modulo 2^32** (#492).
    ///
    /// RTP timestamps are modular (RFC 3550 §5.1): a receiver unwraps them by
    /// serial-number arithmetic, so the stamp must keep advancing through the
    /// 2^32 boundary — never clamp. A clamp pinned every packet to 0 once the
    /// 64-bit PTS passed `u32::MAX` (video after 2^32 / 90 000 s ≈ 13 h 15 min,
    /// audio after 2^32 / 48 000 s ≈ 24.9 h), freezing every viewer on an engine
    /// that is expected to run forever (invariant #1). The wrap is Euclidean, so a
    /// (pre-roll) negative PTS also maps onto the same modular timeline instead
    /// of collapsing to 0. There is no random initial offset on this path: str0m
    /// writes the timestamp it is given verbatim.
    #[must_use]
    pub fn rtp_timestamp(self, pts: i64, time_base: multiview_core::time::Rational) -> u32 {
        let clock = multiview_core::time::Rational::new(1, i64::from(self.rtp_clock_hz()));
        let ticks = multiview_core::time::rescale(pts, time_base, clock);
        let wrapped = ticks.rem_euclid(1_i64 << 32);
        // `rem_euclid` by 2^32 is always in `0..2^32`, so this never falls back.
        u32::try_from(wrapped).unwrap_or(0)
    }
}

/// One encoded program access unit handed to a WebRTC output's egress.
///
/// Carries the coded bytes (an H.264 Annex-B / AVCC access unit, or one Opus
/// frame), its RTP timestamp **already derived from the output tick counter**
/// (invariant #3 — raw input PTS never reaches a viewer), whether it begins a
/// keyframe (the driver caches SPS/PPS and prepends them at each IDR for late
/// joiners), and which elementary stream it is. The driver packetizes this into
/// SRTP per viewer; nothing here inspects the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressSample {
    /// Which elementary stream this sample is (video on 90 kHz, Opus on 48 kHz).
    pub media: EgressMedia,
    /// Presentation timestamp in RTP clock units **of this sample's kind**
    /// (invariant #3, derived from the output tick — never input PTS).
    pub rtp_timestamp: u32,
    /// Whether this sample begins a keyframe (IDR). Audio frames are
    /// independently decodable, so audio producers set `false`.
    pub keyframe: bool,
    /// The coded payload (an H.264 access unit or one Opus frame).
    pub data: Vec<u8>,
}

/// Shared bounded ring backing an [`EgressSink`]/[`EgressFeed`] pair.
#[derive(Debug, Default)]
struct EgressRing {
    queue: VecDeque<EgressSample>,
    /// Total samples evicted as the oldest because the ring was full (telemetry).
    dropped: u64,
    /// Whether the producer has signalled end-of-stream (the output stopped).
    closed: bool,
}

/// The producer end of a bounded, drop-oldest egress feed.
///
/// The WebRTC output's sink runner pushes each re-stamped program access unit
/// here with [`EgressSink::push`], which is **wait-free and non-blocking** (see
/// the module isolation note). Cheap to clone (an `Arc` of the shared ring).
#[derive(Debug, Clone)]
pub struct EgressSink {
    inner: Arc<Mutex<EgressRing>>,
}

/// The consumer end of a bounded, drop-oldest egress feed.
///
/// The native WHEP-serve / WHIP-push driver drains this with [`EgressFeed::pop`]
/// and sample-writes the bytes into every session. Draining slowly only causes
/// the producer to drop the oldest samples; it never back-pressures the producer.
#[derive(Debug, Clone)]
pub struct EgressFeed {
    inner: Arc<Mutex<EgressRing>>,
}

/// Build a bounded, drop-oldest egress feed, returning the
/// `(EgressSink, EgressFeed)` producer/consumer pair.
#[must_use]
pub fn egress_feed() -> (EgressSink, EgressFeed) {
    let ring = Arc::new(Mutex::new(EgressRing::default()));
    (
        EgressSink {
            inner: Arc::clone(&ring),
        },
        EgressFeed { inner: ring },
    )
}

impl EgressSink {
    /// Push one encoded program access unit, evicting the oldest if the ring is
    /// full (drop-oldest). Wait-free with respect to the consumer: it takes only
    /// the feed's short-lived bookkeeping mutex and never blocks on a WHEP player
    /// / WHIP target. Returns `true` if an older sample was evicted (the consumer
    /// is lagging); `false` otherwise. A poisoned mutex silently drops the sample
    /// (the egress is best-effort and must never propagate a failure outward).
    ///
    /// `#[must_use]`: the returned lag flag is the only synchronous "egress
    /// falling behind" signal at the push site; a caller that does not care binds
    /// it to `_`.
    #[must_use]
    pub fn push(&self, sample: EgressSample) -> bool {
        let Ok(mut ring) = self.inner.lock() else {
            return false;
        };
        let mut evicted = false;
        while ring.queue.len() >= MAX_EGRESS {
            if ring.queue.pop_front().is_some() {
                ring.dropped = ring.dropped.saturating_add(1);
                evicted = true;
            } else {
                break;
            }
        }
        ring.queue.push_back(sample);
        evicted
    }

    /// Signal end-of-stream: the WebRTC output stopped (its packet channel closed
    /// at end-of-program). The driver drains the remaining samples, then tears the
    /// sessions down.
    pub fn close(&self) {
        if let Ok(mut ring) = self.inner.lock() {
            ring.closed = true;
        }
    }

    /// The number of samples currently buffered (bounded by [`MAX_EGRESS`]).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.inner.lock().map_or(0, |r| r.queue.len())
    }
}

impl EgressFeed {
    /// Remove and return the oldest buffered sample, or `None` if empty.
    ///
    /// Non-blocking. Draining slowly only causes the producer to drop the oldest
    /// samples on its next [`EgressSink::push`]; it never back-pressures it.
    #[must_use]
    pub fn pop(&self) -> Option<EgressSample> {
        self.inner.lock().ok().and_then(|mut r| r.queue.pop_front())
    }

    /// The number of samples currently buffered (telemetry / bounded assertion).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |r| r.queue.len())
    }

    /// Whether the feed is currently empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The total number of samples dropped (evicted as oldest) since creation — a
    /// growing count is the operator-visible "WHEP/WHIP egress lagging" signal; it
    /// never indicates engine trouble.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.lock().map_or(0, |r| r.dropped)
    }

    /// Whether end-of-stream was signalled **and** the feed is drained.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.inner
            .lock()
            .map_or(true, |r| r.closed && r.queue.is_empty())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample(ts: u32, keyframe: bool) -> EgressSample {
        EgressSample {
            media: EgressMedia::Video,
            rtp_timestamp: ts,
            keyframe,
            data: vec![0x65, 0xAB],
        }
    }

    #[test]
    fn rtp_clock_rates_are_per_kind() {
        assert_eq!(EgressMedia::Video.rtp_clock_hz(), 90_000);
        assert_eq!(EgressMedia::Audio.rtp_clock_hz(), 48_000);
    }

    /// #492: the video stamp wraps mod 2^32 across the 90 kHz boundary
    /// (≈ 13 h 15 min of program), it is never clamped to 0.
    #[test]
    fn video_rtp_timestamp_wraps_mod_2_32() {
        use multiview_core::time::Rational;
        let tb = Rational::new(1, 90_000);
        let max = i64::from(u32::MAX);
        assert_eq!(
            EgressMedia::Video.rtp_timestamp(max - 2999, tb),
            u32::MAX - 2999
        );
        assert_eq!(EgressMedia::Video.rtp_timestamp(max, tb), u32::MAX);
        assert_eq!(EgressMedia::Video.rtp_timestamp(max + 1, tb), 0);
        assert_eq!(EgressMedia::Video.rtp_timestamp(max + 3001, tb), 3000);
        // A 30 fps encoder time-base (1/30) one frame either side of the wrap:
        // frame n stamps n * 3000 (mod 2^32) on the 90 kHz clock.
        let fps30 = Rational::new(1, 30);
        let n = max / 3000; // last frame before the 90 kHz wrap
        let before = EgressMedia::Video.rtp_timestamp(n, fps30);
        let after = EgressMedia::Video.rtp_timestamp(n + 1, fps30);
        assert_eq!(i64::from(before), n * 3000);
        assert_eq!(
            after.wrapping_sub(before),
            3000,
            "one frame later, mod 2^32"
        );
        assert!(after < before, "the stamp wrapped, it did not clamp");
        assert_ne!(after, 0);
        // Far past the boundary (≈ 3 days) it still advances, never pinned.
        let days3 = 3 * 86_400 * 30;
        assert_eq!(
            EgressMedia::Video
                .rtp_timestamp(days3 + 1, fps30)
                .wrapping_sub(EgressMedia::Video.rtp_timestamp(days3, fps30)),
            3000
        );
    }

    /// #492: the 48 kHz Opus stamp wraps mod 2^32 (≈ 24.9 h of program).
    #[test]
    fn audio_rtp_timestamp_wraps_mod_2_32() {
        use multiview_core::time::Rational;
        let tb = Rational::new(1, 48_000);
        let max = i64::from(u32::MAX);
        let last = max - (max % 960); // last 20 ms Opus frame before the wrap
        let before = EgressMedia::Audio.rtp_timestamp(last, tb);
        let after = EgressMedia::Audio.rtp_timestamp(last + 960, tb);
        assert_eq!(i64::from(before), last);
        assert_eq!(after.wrapping_sub(before), 960);
        assert!(after < before && after != 0, "wrapped, not clamped");
        assert_eq!(EgressMedia::Audio.rtp_timestamp(max + 1, tb), 0);
        assert_eq!(EgressMedia::Audio.rtp_timestamp(max + 961, tb), 960);
    }

    /// Below the boundary the stamp is the exact rescale (no behaviour change),
    /// and a negative pre-roll PTS lands on the same modular timeline.
    #[test]
    fn rtp_timestamp_is_exact_below_the_wrap_and_modular_below_zero() {
        use multiview_core::time::Rational;
        assert_eq!(
            EgressMedia::Video.rtp_timestamp(1, Rational::new(1, 30)),
            3000
        );
        assert_eq!(
            EgressMedia::Video.rtp_timestamp(1, Rational::new(1001, 30_000)),
            3003
        );
        assert_eq!(
            EgressMedia::Audio.rtp_timestamp(960, Rational::new(1, 48_000)),
            960
        );
        let tb = Rational::new(1, 90_000);
        assert_eq!(EgressMedia::Video.rtp_timestamp(-1, tb), u32::MAX);
        assert_eq!(
            EgressMedia::Video
                .rtp_timestamp(0, tb)
                .wrapping_sub(EgressMedia::Video.rtp_timestamp(-3000, tb)),
            3000
        );
    }

    #[test]
    fn push_pop_is_fifo() {
        let (sink, feed) = egress_feed();
        assert!(!sink.push(sample(1, true)));
        assert!(!sink.push(sample(2, false)));
        assert_eq!(feed.pop().unwrap().rtp_timestamp, 1);
        assert_eq!(feed.pop().unwrap().rtp_timestamp, 2);
        assert!(feed.pop().is_none());
    }
}
