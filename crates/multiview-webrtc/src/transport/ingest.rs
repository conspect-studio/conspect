//! WHIP ingest session driver — **feature `native`** (ADR-T014 §4, ADR-0048 §7).
//!
//! A WHIP publisher (OBS, a browser, `GStreamer` `whipclientsink`) `POST`s an SDP
//! offer; the endpoint answers `201` and then receives ICE/DTLS/SRTP media. This
//! module owns the *ingest half* of that: an RTP-mode [`Session`] (str0m
//! decrypts SRTP and surfaces raw RTP) whose received packets are routed across
//! a **bounded, drop-oldest** ring into the consumer's
//! [`multiview_input::webrtc::transport::MediaEngine`] pull — the existing pure,
//! keyframe-gated `H264Depacketizer` / `OpusDepacketizer` is the one canonical
//! depacketization path, never str0m's sample API.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The publisher can never pace or back-pressure the engine. Received RTP
//! crosses only the [`RtpRing`] (drop-oldest, bounded by
//! [`MAX_INGRESS_RTP`]); the consumer's [`RtpRingEngine::poll_rtp`] is a
//! non-blocking pop that yields `None` when the ring is empty (the tile holds
//! last-good). The socket-driving loop (the live endpoint,
//! [`WhipEndpoint`](crate::transport::WhipEndpoint)) never `.await`s the
//! publisher — UDP send is non-blocking; a full ring drops.
//! Nothing here is on the output-clock tick path.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use multiview_input::webrtc::transport::{MediaEngine, RtpFrame};

use crate::transport::{ReceivedRtp, Session};

/// The bound on RTP packets buffered between the socket-driving loop and the
/// ingest consumer's pull. Drop-oldest past this, never grows (invariant #10 /
/// safety rule 5). Sized to absorb a burst of MTU-sized packets (several large
/// access units) without stalling the driver.
pub const MAX_INGRESS_RTP: usize = 2048;

/// A bounded, drop-oldest ring of decrypted RTP packets shared between the
/// socket-driving loop (producer) and the ingest [`MediaEngine`] consumer.
///
/// Lock-guarded (a short critical section that neither side holds across I/O or
/// an `.await`): the producer pushes one packet per received RTP, the consumer
/// pops one per `poll_rtp`. A producer that outruns the consumer drops the
/// **oldest** packet rather than growing the ring — a slow consumer (or a
/// degraded tile) can never grow memory or back-pressure the socket.
#[derive(Debug, Clone, Default)]
pub struct RtpRing {
    inner: Arc<Mutex<RtpRingInner>>,
}

#[derive(Debug, Default)]
struct RtpRingInner {
    queue: VecDeque<ReceivedRtp>,
    /// Total packets dropped because the ring was full (telemetry).
    dropped: u64,
    /// Whether the producer side has signalled end-of-stream (publisher gone).
    closed: bool,
}

impl RtpRing {
    /// A fresh empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one decrypted RTP packet (the socket-driving loop's write). Drops the
    /// oldest packet when the ring is at [`MAX_INGRESS_RTP`] (never grows).
    pub fn push(&self, packet: ReceivedRtp) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.queue.len() >= MAX_INGRESS_RTP {
                let _ = inner.queue.pop_front();
                inner.dropped = inner.dropped.saturating_add(1);
            }
            inner.queue.push_back(packet);
        }
    }

    /// Pop the oldest packet (the consumer's read). `None` when empty.
    #[must_use]
    pub fn pop(&self) -> Option<ReceivedRtp> {
        self.inner.lock().ok().and_then(|mut i| i.queue.pop_front())
    }

    /// The number of packets currently buffered (telemetry / bounded assertion).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |i| i.queue.len())
    }

    /// Whether the ring is currently empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many packets have been dropped for a full ring (telemetry).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.lock().map_or(0, |i| i.dropped)
    }

    /// Signal end-of-stream: the publisher session ended (DELETE / ICE timeout /
    /// idle GC). The consumer drains the remaining buffered packets, then sees
    /// clean EOS.
    pub fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.closed = true;
        }
    }

    /// Whether end-of-stream was signalled **and** the ring is drained.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.inner
            .lock()
            .map_or(true, |i| i.closed && i.queue.is_empty())
    }

    /// Drain every RTP packet a [`Session`] has surfaced into the ring (the
    /// socket-driving loop calls this after each drive pass). Returns how many
    /// were moved.
    pub fn drain_from(&self, session: &mut Session) -> usize {
        let mut moved = 0;
        while let Some(packet) = session.take_received_rtp() {
            self.push(packet);
            moved += 1;
        }
        moved
    }
}

/// A [`MediaEngine`] over an [`RtpRing`] — the consumer-side seam the WHIP
/// ingest source's `WebRtcProducer` is built on.
///
/// Each [`poll_rtp`](MediaEngine::poll_rtp) pops one decrypted RTP packet (as
/// the `multiview_input` [`RtpFrame`]) or yields `Ok(None)` when nothing is
/// ready (the tile holds last-good — never blocks; inv #1/#10), or after the
/// publisher has gone and the ring is drained (clean end-of-stream, which the
/// supervisor treats as "ride STALE → `NO_SIGNAL` until the next publisher").
#[derive(Debug)]
pub struct RtpRingEngine {
    ring: RtpRing,
}

impl RtpRingEngine {
    /// Build a media engine that pulls from `ring`.
    #[must_use]
    pub fn new(ring: RtpRing) -> Self {
        Self { ring }
    }
}

impl MediaEngine for RtpRingEngine {
    fn poll_rtp(&mut self) -> multiview_input::error::Result<Option<RtpFrame>> {
        match self.ring.pop() {
            Some(packet) => Ok(Some(RtpFrame {
                payload_type: packet.payload_type,
                sequence: packet.sequence,
                timestamp: packet.timestamp,
                marker: packet.marker,
                payload: packet.payload,
            })),
            // Nothing buffered. If the publisher has gone and the ring is drained,
            // this is a clean end-of-stream (`Ok(None)`); otherwise it is simply
            // "nothing ready this poll" (also `Ok(None)` — the producer re-polls).
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )]

    use super::*;
    use crate::transport::ReceivedRtp;

    fn pkt(seq: u16, ts: u32) -> ReceivedRtp {
        ReceivedRtp {
            payload_type: 96,
            sequence: seq,
            timestamp: ts,
            marker: false,
            ssrc: 1,
            payload: vec![0x65, 0xAB, 0xCD],
        }
    }

    #[test]
    fn ring_is_fifo_and_bounded_drop_oldest() {
        let ring = RtpRing::new();
        // Push past the cap; the oldest are dropped, the newest survive in order.
        for i in 0..(MAX_INGRESS_RTP as u32 + 100) {
            ring.push(pkt(i as u16, i));
        }
        assert!(ring.len() <= MAX_INGRESS_RTP, "ring must be bounded");
        assert_eq!(ring.dropped(), 100, "exactly the overflow was dropped");
        // The oldest surviving packet is the 100th pushed (timestamp 100).
        let first = ring.pop().unwrap();
        assert_eq!(first.timestamp, 100, "drop-oldest kept the newest tail");
    }

    #[test]
    fn engine_yields_rtpframe_then_none_when_empty() {
        let ring = RtpRing::new();
        ring.push(pkt(7, 90_000));
        let mut engine = RtpRingEngine::new(ring.clone());
        let frame = engine.poll_rtp().unwrap().expect("one frame");
        assert_eq!(frame.sequence, 7);
        assert_eq!(frame.timestamp, 90_000);
        assert_eq!(frame.payload, vec![0x65, 0xAB, 0xCD]);
        // Drained: the next poll is a non-blocking `None` (hold last-good).
        assert!(engine.poll_rtp().unwrap().is_none());
    }

    #[test]
    fn close_then_drain_is_end_of_stream() {
        let ring = RtpRing::new();
        ring.push(pkt(1, 1));
        ring.close();
        assert!(!ring.is_ended(), "not ended while packets remain");
        assert!(ring.pop().is_some());
        assert!(ring.is_ended(), "ended once closed and drained");
    }
}
