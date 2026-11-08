//! Atomic encoded-window transport with bounded, nonblocking admission.
//!
//! A window contains every packet emitted by a single encoder/audio collection
//! interval, including zero or many packets per stream. PTS/DTS remain those of
//! the encoded packets; no frame-per-tick assumption is made here. The queue
//! counts referenced payload bytes per admitted window, not distinct allocations:
//! `Arc` sharing avoids copying coded data, but each queue independently owns
//! its reservation until the consumer drops its lease.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_ffmpeg::{EncodedPacket, StreamKind};

/// Maximum encoded packets in a single atomic collection window.
pub const MAX_BATCH_PACKETS: usize = 256;
/// Maximum referenced coded bytes in one collection window.
pub const MAX_BATCH_BYTES: usize = 12 * 1024 * 1024;
/// Maximum time represented by one collection window.
pub const MAX_BATCH_SPAN: Duration = Duration::from_millis(250);
/// Maximum packets owned by one queue, including consumer-held leases.
pub const MAX_QUEUE_PACKETS: usize = 512;
/// Maximum referenced coded bytes owned by one queue, including leases.
pub const MAX_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum time since admission for a *queued*, unconsumed window.
pub const MAX_QUEUE_AGE: Duration = Duration::from_secs(2);

/// A window failed construction; no packet entered a queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchError {
    /// The packet count exceeds the per-window cap.
    PacketCount,
    /// Payload-byte summation overflowed or exceeded the per-window cap.
    PayloadBytes,
    /// The supplied collection interval exceeds the per-window cap.
    MediaSpan,
}

/// Immutable, fully collected video and audio emission for one interval.
///
/// The individual encoded packets retain their original stream identity,
/// timestamps and ref-counted payload. Consumers may obtain mux-owned packet
/// copies with `EncodedPacket::to_owned_packet`.
pub struct PacketBatch {
    packets: Vec<EncodedPacket>,
    payload_bytes: usize,
    media_span: Duration,
}

impl PacketBatch {
    /// Validate the complete window before making it available for admission.
    pub fn try_new(packets: Vec<EncodedPacket>, media_span: Duration) -> Result<Self, BatchError> {
        if packets.len() > MAX_BATCH_PACKETS {
            return Err(BatchError::PacketCount);
        }
        if media_span > MAX_BATCH_SPAN {
            return Err(BatchError::MediaSpan);
        }
        let payload_bytes = checked_payload_bytes(packets.iter().map(EncodedPacket::len))?;
        if payload_bytes > MAX_BATCH_BYTES {
            return Err(BatchError::PayloadBytes);
        }
        Ok(Self {
            packets,
            payload_bytes,
            media_span,
        })
    }

    /// Every encoded packet, in encoder emission order.
    #[must_use]
    pub fn packets(&self) -> &[EncodedPacket] {
        &self.packets
    }

    /// Number of packets, including all audio streams.
    #[must_use]
    pub fn packet_count(&self) -> usize {
        self.packets.len()
    }

    /// Sum of referenced payload lengths, not allocated memory or Arc overhead.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    /// Collection interval supplied by the producer, not inferred from mixed time bases.
    #[must_use]
    pub fn media_span(&self) -> Duration {
        self.media_span
    }

    fn begins_with_video_keyframe(&self) -> bool {
        self.packets
            .iter()
            .find(|p| matches!(p.kind(), StreamKind::Video))
            .is_some_and(EncodedPacket::is_keyframe)
    }
}

fn checked_payload_bytes(lengths: impl IntoIterator<Item = usize>) -> Result<usize, BatchError> {
    lengths.into_iter().try_fold(0usize, |sum, length| {
        sum.checked_add(length).ok_or(BatchError::PayloadBytes)
    })
}

/// Reason the complete window was rejected without partial A/V admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferError {
    /// The batch itself exceeds queue limits (including configured limits).
    Oversized,
    /// Existing queued and in-flight reservations leave insufficient capacity.
    Occupied,
    /// A queued window exceeded the maximum time since admission.
    OldestAge,
    /// Producer's `Instant` moved backwards relative to a queued offer.
    ClockReversed,
    /// A loss requires a video keyframe before admission resumes.
    NeedsKeyframe,
}

/// A loss notification; consumers must reset mux continuity and await an IDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Discontinuity {
    /// New packet generation after loss.
    pub generation: u64,
    /// Initial reason this generation was invalidated.
    pub reason: OfferError,
}

#[derive(Default)]
struct Reservations {
    packets: AtomicUsize,
    bytes: AtomicUsize,
    generation: AtomicU64,
}

struct Queued {
    batch: Arc<PacketBatch>,
    offered_at: Instant,
    generation: u64,
}

/// One queue's locally owned admission state. Operations never wait for clients.
///
/// The queue is exclusively owned by its producer/consumer scheduler (`&mut
/// self`); leases can move to mux workers and release capacity on `Drop`.
/// Dropping a queue does not invalidate an existing lease's reservation.
pub struct BoundedBatchQueue {
    entries: VecDeque<Queued>,
    reservations: Arc<Reservations>,
    needs_keyframe: bool,
    pending_discontinuity: Option<Discontinuity>,
    max_packets: usize,
    max_bytes: usize,
    max_age: Duration,
}

impl Default for BoundedBatchQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl BoundedBatchQueue {
    /// Create a queue with the broadcast-admission defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(MAX_QUEUE_PACKETS, MAX_QUEUE_BYTES, MAX_QUEUE_AGE)
    }

    /// Set tighter admission limits. Zero capacity always rejects nonempty batches.
    #[must_use]
    pub fn with_limits(max_packets: usize, max_bytes: usize, max_age: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            reservations: Arc::new(Reservations::default()),
            needs_keyframe: false,
            pending_discontinuity: None,
            max_packets: max_packets.min(MAX_QUEUE_PACKETS),
            max_bytes: max_bytes.min(MAX_QUEUE_BYTES),
            max_age: max_age.min(MAX_QUEUE_AGE),
        }
    }

    /// Atomically admit all packet streams or reject the entire window.
    ///
    /// `now` is a monotonic `Instant` sampled by the caller. Age starts at
    /// successful admission, not media PTS, packet DTS or wall-clock time.
    /// Old queued windows are discarded before admission; their loss is
    /// reported as `OldestAge`. In-flight windows remain reserved until drop.
    pub fn try_offer(&mut self, batch: Arc<PacketBatch>, now: Instant) -> Result<(), OfferError> {
        if batch.packet_count() > self.max_packets || batch.payload_bytes() > self.max_bytes {
            self.invalidate(OfferError::Oversized);
            return Err(OfferError::Oversized);
        }
        if self
            .entries
            .iter()
            .any(|entry| now.checked_duration_since(entry.offered_at).is_none())
        {
            self.invalidate(OfferError::ClockReversed);
            return Err(OfferError::ClockReversed);
        }
        if self
            .entries
            .iter()
            .any(|entry| now.duration_since(entry.offered_at) > self.max_age)
        {
            self.invalidate(OfferError::OldestAge);
            return Err(OfferError::OldestAge);
        }
        if self.needs_keyframe && !batch.begins_with_video_keyframe() {
            return Err(OfferError::NeedsKeyframe);
        }
        let packets = self
            .reservations
            .packets
            .load(Ordering::Acquire)
            .checked_add(batch.packet_count())
            .filter(|&n| n <= self.max_packets);
        let bytes = self
            .reservations
            .bytes
            .load(Ordering::Acquire)
            .checked_add(batch.payload_bytes())
            .filter(|&n| n <= self.max_bytes);
        let (Some(packets), Some(bytes)) = (packets, bytes) else {
            self.invalidate(OfferError::Occupied);
            return Err(OfferError::Occupied);
        };
        // Only this queue mutates admission totals; leases exclusively subtract.
        // A lease cannot be dropped simultaneously with this `&mut self` access
        // except from a mux worker; its atomic subtraction is always safe.
        self.reservations
            .packets
            .fetch_add(batch.packet_count(), Ordering::AcqRel);
        self.reservations
            .bytes
            .fetch_add(batch.payload_bytes(), Ordering::AcqRel);
        debug_assert!(self.reservations.packets.load(Ordering::Acquire) <= packets);
        debug_assert!(self.reservations.bytes.load(Ordering::Acquire) <= bytes);
        self.needs_keyframe = false;
        self.entries.push_back(Queued {
            batch,
            offered_at: now,
            generation: self.generation(),
        });
        Ok(())
    }

    /// Take the oldest window without releasing its admission reservation.
    pub fn pop(&mut self) -> Option<BatchLease> {
        self.entries.pop_front().map(|entry| BatchLease {
            batch: entry.batch,
            generation: entry.generation,
            reservations: Arc::clone(&self.reservations),
        })
    }

    /// The generation currently accepted by this queue.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.reservations.generation.load(Ordering::Acquire)
    }

    /// Consume a loss event. Do not interpret a missing event as a clean GOP:
    /// compare each lease's generation with the queue generation as well.
    pub fn take_discontinuity(&mut self) -> Option<Discontinuity> {
        self.pending_discontinuity.take()
    }

    /// Total owned packet and referenced payload-byte reservations, including leases.
    #[must_use]
    pub fn reserved(&self) -> (usize, usize) {
        (
            self.reservations.packets.load(Ordering::Acquire),
            self.reservations.bytes.load(Ordering::Acquire),
        )
    }

    /// Number of windows still waiting to be popped (not in-flight leases).
    #[must_use]
    pub fn queued_windows(&self) -> usize {
        self.entries.len()
    }

    fn invalidate(&mut self, reason: OfferError) {
        if !self.needs_keyframe {
            let generation = self.reservations.generation.fetch_add(1, Ordering::AcqRel) + 1;
            self.pending_discontinuity = Some(Discontinuity { generation, reason });
        }
        self.needs_keyframe = true;
        while let Some(entry) = self.entries.pop_front() {
            self.reservations
                .packets
                .fetch_sub(entry.batch.packet_count(), Ordering::AcqRel);
            self.reservations
                .bytes
                .fetch_sub(entry.batch.payload_bytes(), Ordering::AcqRel);
        }
    }
}

impl Drop for BoundedBatchQueue {
    fn drop(&mut self) {
        while let Some(entry) = self.entries.pop_front() {
            self.reservations
                .packets
                .fetch_sub(entry.batch.packet_count(), Ordering::AcqRel);
            self.reservations
                .bytes
                .fetch_sub(entry.batch.payload_bytes(), Ordering::AcqRel);
        }
    }
}

/// An in-flight whole window. Dropping it releases packet and byte capacity.
pub struct BatchLease {
    batch: Arc<PacketBatch>,
    generation: u64,
    reservations: Arc<Reservations>,
}

impl BatchLease {
    /// Access the immutable window with all of its audio/video packets.
    #[must_use]
    pub fn batch(&self) -> &PacketBatch {
        &self.batch
    }

    /// Iterate original encoded packets without copying their payloads.
    pub fn packets(&self) -> &[EncodedPacket] {
        self.batch.packets()
    }

    /// This lease's generation at admission.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// False after a loss invalidated this lease while a worker held it.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.generation == self.reservations.generation.load(Ordering::Acquire)
    }
}

impl Drop for BatchLease {
    fn drop(&mut self) {
        self.reservations
            .packets
            .fetch_sub(self.batch.packet_count(), Ordering::AcqRel);
        self.reservations
            .bytes
            .fetch_sub(self.batch.payload_bytes(), Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg_next::codec::packet::{Flags, Packet};

    fn packet(kind: StreamKind, len: usize, key: bool) -> EncodedPacket {
        let mut raw = Packet::new(len);
        if key {
            raw.set_flags(Flags::KEY);
        }
        EncodedPacket::with_kind(raw, kind)
    }
    fn window(packets: Vec<EncodedPacket>) -> Arc<PacketBatch> {
        Arc::new(PacketBatch::try_new(packets, Duration::from_millis(33)).unwrap())
    }
    fn key() -> Arc<PacketBatch> {
        window(vec![packet(StreamKind::Video, 1, true)])
    }

    #[test]
    fn healthy_burst_exceeds_four_and_preserves_tracks_and_zero_emission() {
        let mut q = BoundedBatchQueue::new();
        let now = Instant::now();
        let burst = window(
            (0..9)
                .map(|i| {
                    packet(
                        if i % 2 == 0 {
                            StreamKind::Video
                        } else {
                            StreamKind::AudioTrack(i)
                        },
                        1024,
                        i == 0,
                    )
                })
                .collect(),
        );
        q.try_offer(burst, now).unwrap();
        q.try_offer(window(vec![]), now).unwrap();
        let lease = q.pop().unwrap();
        assert_eq!(lease.packets().len(), 9);
        assert_eq!(lease.packets()[1].kind(), StreamKind::AudioTrack(1));
        assert!(lease.is_current());
        assert_eq!(q.pop().unwrap().packets().len(), 0);
        assert_eq!(q.reserved(), (9, 9216));
        drop(lease);
        assert_eq!(q.reserved(), (0, 0));
    }

    #[test]
    fn complete_rejection_never_partially_admits_audio_or_video() {
        let now = Instant::now();
        let mut q = BoundedBatchQueue::with_limits(2, 10, MAX_QUEUE_AGE);
        let av = window(vec![
            packet(StreamKind::Video, 7, true),
            packet(StreamKind::Audio, 7, false),
        ]);
        assert_eq!(q.try_offer(av, now), Err(OfferError::Oversized));
        assert_eq!(q.queued_windows(), 0);
        assert_eq!(q.reserved(), (0, 0));
        assert_eq!(q.take_discontinuity().unwrap().generation, 1);
        assert_eq!(
            q.try_offer(window(vec![packet(StreamKind::Audio, 1, false)]), now),
            Err(OfferError::NeedsKeyframe)
        );
        q.try_offer(key(), now).unwrap();
    }

    #[test]
    fn large_idr_and_in_flight_capacity_release() {
        let now = Instant::now();
        let mut q = BoundedBatchQueue::with_limits(3, MAX_QUEUE_BYTES, MAX_QUEUE_AGE);
        let large = window(vec![packet(StreamKind::Video, 11 * 1024 * 1024, true)]);
        q.try_offer(large, now).unwrap();
        let lease = q.pop().unwrap();
        assert_eq!(q.reserved(), (1, 11 * 1024 * 1024));
        assert_eq!(
            q.try_offer(
                window(vec![packet(StreamKind::Video, 6 * 1024 * 1024, true)]),
                now
            ),
            Err(OfferError::Occupied)
        );
        assert!(!lease.is_current());
        drop(lease);
        q.try_offer(key(), now).unwrap();
        assert_eq!(q.reserved(), (1, 1));
    }

    #[test]
    fn age_expiry_flushes_whole_queue_and_requires_keyframe() {
        let now = Instant::now();
        let mut q = BoundedBatchQueue::new();
        q.try_offer(key(), now).unwrap();
        assert_eq!(
            q.try_offer(key(), now + MAX_QUEUE_AGE + Duration::from_nanos(1)),
            Err(OfferError::OldestAge)
        );
        assert_eq!(q.reserved(), (0, 0));
        assert_eq!(
            q.take_discontinuity().unwrap().reason,
            OfferError::OldestAge
        );
        q.try_offer(key(), now + MAX_QUEUE_AGE + Duration::from_nanos(1))
            .unwrap();
    }

    #[test]
    fn overflow_and_hard_limits_cannot_allocate_unbounded_queues() {
        assert_eq!(
            checked_payload_bytes([usize::MAX, 1]),
            Err(BatchError::PayloadBytes)
        );
        let invalid = (0..257)
            .map(|_| packet(StreamKind::Audio, 0, false))
            .collect();
        assert!(matches!(
            PacketBatch::try_new(invalid, Duration::ZERO),
            Err(BatchError::PacketCount)
        ));
        assert!(matches!(
            PacketBatch::try_new(vec![], Duration::from_secs(1)),
            Err(BatchError::MediaSpan)
        ));
        let mut q = BoundedBatchQueue::with_limits(usize::MAX, usize::MAX, Duration::MAX);
        let now = Instant::now();
        for _ in 0..MAX_QUEUE_PACKETS {
            q.try_offer(key(), now).unwrap();
        }
        assert_eq!(q.queued_windows(), MAX_QUEUE_PACKETS);
        assert_eq!(q.try_offer(key(), now), Err(OfferError::Occupied));
        assert_eq!(q.reserved(), (0, 0));
        assert_eq!(q.queued_windows(), 0);
    }
}
