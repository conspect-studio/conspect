//! [`RtspServerSink`] — the [`PacketSink`] that feeds the in-process RTSP server.
//!
//! This is the typed seam between the encode-once-mux-many fan-out
//! ([`crate::fanout`]) and the `GStreamer` `appsrc` that payloads the stream to
//! RTSP clients. The engine routes each pre-encoded canvas packet to this sink on
//! the output clock; [`RtspServerSink::deliver`] performs a **strictly
//! non-blocking** hand-off into a bounded drop-oldest [`BoundedPacketQueue`] and
//! returns immediately (it never `.await`s, never blocks, never back-pressures
//! the engine — invariants #1/#10).
//!
//! The serving side (the `GLib` main loop + `appsrc → h264parse → rtph264pay`
//! pipeline, behind the off-by-default `rtsp-server` feature) drains this same
//! queue with [`RtspServerSink::pop`] and pushes the **already-encoded** NAL
//! bytes into `appsrc` — no re-encode (invariant #7). Because the queue is the
//! decoupling point, the serving side can be slow or absent without ever
//! reaching back to the producer.

use std::sync::Arc;

use crate::fanout::{EncodedPacket, PacketSink};
use crate::rtcp::{SenderReport, SrStamper};

use super::queue::BoundedPacketQueue;

/// A [`PacketSink`] that enqueues encoded packets for the in-process RTSP server.
///
/// Holds a bounded drop-oldest queue (shared via [`Arc`] with the serving side).
/// `deliver` is the producer end; the serving thread is the consumer end.
///
/// The sink optionally carries the program's epoch-fed RTCP
/// [`SrStamper`] (ADR-M010, DEV-C1) so the serving layer obtains Sender
/// Reports stamped from the **same** outbound presentation epoch as the
/// control-WS publication and the HLS `EXT-X-PROGRAM-DATE-TIME` tags — see
/// [`RtspServerSink::sender_report`] and the module docs for the precise
/// wired-vs-seam boundary.
#[derive(Debug)]
pub struct RtspServerSink {
    id: String,
    queue: Arc<BoundedPacketQueue>,
    sr_stamper: Option<SrStamper>,
}

impl RtspServerSink {
    /// Create a sink with id `id` whose bounded drop-oldest buffer holds at most
    /// `capacity` packets (a `capacity` of `0` is clamped to `1` — see
    /// [`BoundedPacketQueue::new`]).
    #[must_use]
    pub fn new(id: impl Into<String>, capacity: usize) -> Self {
        Self {
            id: id.into(),
            queue: Arc::new(BoundedPacketQueue::new(capacity)),
            sr_stamper: None,
        }
    }

    /// Build a sink around an existing shared queue, so the serving side and the
    /// producer share the same buffer.
    #[must_use]
    pub fn with_queue(id: impl Into<String>, queue: Arc<BoundedPacketQueue>) -> Self {
        Self {
            id: id.into(),
            queue,
            sr_stamper: None,
        }
    }

    /// Attach the program's epoch-fed RTCP SR stamper (ADR-M010): the serving
    /// layer then stamps Sender-Report NTP↔RTP pairs from the same outbound
    /// presentation epoch every other surface uses.
    #[must_use]
    pub fn with_sr_stamper(mut self, stamper: SrStamper) -> Self {
        self.sr_stamper = Some(stamper);
        self
    }

    /// Build the epoch-stamped RTCP Sender Report for the wall instant
    /// `wall_ns` with the transport's RTP totals.
    ///
    /// Returns `None` when no stamper is attached **or** no epoch has been
    /// published yet — an honest absence, never a fabricated NTP↔RTP pair.
    /// Read-only over the shared epoch cell: calling this can never influence
    /// the engine or the producer side (invariants #1/#10).
    #[must_use]
    pub fn sender_report(
        &self,
        wall_ns: i64,
        packet_count: u32,
        octet_count: u32,
    ) -> Option<SenderReport> {
        self.sr_stamper
            .as_ref()
            .and_then(|s| s.report(wall_ns, packet_count, octet_count))
    }

    /// A clone of the shared queue handle for the serving side to drain.
    #[must_use]
    pub fn queue(&self) -> Arc<BoundedPacketQueue> {
        Arc::clone(&self.queue)
    }

    /// The number of packets currently buffered (`<= capacity`).
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Pop the oldest buffered packet (the serving side's drain), or `None` when
    /// empty. Never blocks.
    #[must_use]
    pub fn pop(&self) -> Option<Arc<EncodedPacket>> {
        self.queue.pop()
    }

    /// Drain the buffered packets in FIFO order (used by the seam tests and the
    /// serving side's burst drain).
    #[must_use]
    pub fn drain_for_test(&self) -> Vec<Arc<EncodedPacket>> {
        self.queue.drain()
    }
}

impl PacketSink for RtspServerSink {
    fn sink_id(&self) -> &str {
        &self.id
    }

    fn deliver(&self, packet: &Arc<EncodedPacket>) {
        // Non-blocking, bounded, drop-oldest: enqueue the reference and return.
        // The drop flag is intentionally ignored here — the engine must not
        // branch on it on the hot path; a shed metric is the serving side's
        // concern.
        let _shed = self.queue.push(packet);
    }
}
