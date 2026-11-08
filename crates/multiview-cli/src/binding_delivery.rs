//! Atomic, byte/count/age-bounded delivery of one tick's coded packets to each
//! output binding (broadcast output contract, slice 2 delivery half).
//!
//! This replaces the previous blocking fan-out (`send_bounded`: "a full queue
//! paces the CONSUMER up to `SINK_WEDGE_GRACE`"). That pacing was the #202
//! mechanism: one wedged file/HLS sink could stall the shared bake consumer and
//! starve every other output, including a healthy SRT peer. Here the consumer
//! **never waits**: it stages the tick's complete video+audio packet set per
//! binding and admits it with one atomic
//! [`BoundedBatchQueue::offer_emitted`](multiview_output::batch::BoundedBatchQueue::offer_emitted)
//! transaction whose only limits are referenced bytes, packet count and
//! encoded-time age. A binding that is full, stale or oversized loses the
//! *whole* batch (never a partial A/V burst), records an explicit discontinuity
//! and requires a fresh random-access point; every other binding and the program
//! clock continue untouched.
//!
//! Each binding has its own pump thread, so the sink's blocking write happens
//! off the consumer entirely and is bounded by [`PUMP_WEDGE_GRACE`]. The retained
//! ownership of a batch is the queue reservation plus the in-flight
//! [`BatchLease`], both counted by the carrier; the handoff to the existing
//! per-sink packet channel is one packet deep.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use multiview_core::time::Rational;
use multiview_ffmpeg::{EncodedPacket, StreamKind};
use multiview_output::batch::{
    BatchLease, BoundedBatchQueue, EmissionError, OfferError, MAX_QUEUE_AGE, MAX_QUEUE_BYTES,
    MAX_QUEUE_PACKETS,
};
use multiview_output::push_link::PushLinkStatus;

/// Grace a binding pump gives a sink that has stopped draining before it
/// abandons that binding. Bounded: a wedged sink can never pin a pump thread or
/// its in-flight lease forever.
pub const PUMP_WEDGE_GRACE: Duration = Duration::from_secs(2);

/// Depth (in packets) of the pump → sink handoff. One packet: the pump blocks
/// on the sink instead of buffering a second copy of a potentially huge IDR.
pub const HANDOFF_PACKETS: usize = 1;

/// Depth (in leases) of the consumer → pump handoff. A lease owns its batch's
/// reservation until the mux drops it, so this is a bounded in-flight window,
/// not a second unbounded queue.
pub const HANDOFF_LEASES: usize = 2;

/// Total trailer deadline for draining already-admitted windows at
/// [`BindingDelivery::finish`]. Bounded, per the contract's 2 s sink
/// lifecycle/flush limit.
pub const FINISH_DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// How a binding's delivery treats a full or stale batch queue.
#[derive(Clone)]
pub enum BindingPolicy {
    /// A local artifact sink (file/HLS): a lost batch fails this binding
    /// visibly; it is never silently dropped and never delays another binding.
    Bounded,
    /// A resilient live push sink: losses are counted on its existing link
    /// status, preserving the `/status` counters the UI already reads.
    Push(Arc<PushLinkStatus>),
}

/// Why a batch was not admitted, for status/telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// The complete window breached a per-window limit (count/bytes/span).
    InvalidWindow,
    /// Existing queued + in-flight reservations leave no room: overload.
    Occupied,
    /// A queued window exceeded the admission age bound.
    OldestAge,
    /// The producer's monotonic timestamp moved backwards.
    ClockReversed,
    /// Delivery loss occurred: a strict random-access point is required first.
    NeedsRecoveryPoint,
    /// The queue rejected the program shape (audio-only queue with video).
    WrongProgramShape,
    /// The binding's sink wedged for the whole grace and was abandoned.
    SinkWedged,
}

impl RejectionReason {
    fn from_offer(error: OfferError) -> Self {
        match error {
            OfferError::Oversized => Self::InvalidWindow,
            OfferError::Occupied => Self::Occupied,
            OfferError::OldestAge => Self::OldestAge,
            OfferError::ClockReversed => Self::ClockReversed,
            OfferError::NeedsRecoveryPoint => Self::NeedsRecoveryPoint,
            OfferError::WrongProgramShape => Self::WrongProgramShape,
            OfferError::Closed | OfferError::ProducerLoss => Self::NeedsRecoveryPoint,
        }
    }

    /// A short, stable label for logs and status payloads.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InvalidWindow => "invalid-window",
            Self::Occupied => "queue-occupied",
            Self::OldestAge => "queue-age",
            Self::ClockReversed => "clock-reversed",
            Self::NeedsRecoveryPoint => "needs-recovery-point",
            Self::WrongProgramShape => "wrong-program-shape",
            Self::SinkWedged => "sink-wedged",
        }
    }
}

/// Per-binding delivery truth: admitted and rejected windows/packets/bytes,
/// discontinuity generation and the current retained reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingStatus {
    /// Windows admitted atomically.
    pub admitted_batches: u64,
    /// Packets inside admitted windows.
    pub admitted_packets: u64,
    /// Referenced coded bytes inside admitted windows.
    pub admitted_bytes: u64,
    /// Windows rejected atomically (whole-window loss).
    pub rejected_batches: u64,
    /// Packets lost with those rejected windows.
    pub rejected_packets: u64,
    /// Bytes lost with those rejected windows.
    pub rejected_bytes: u64,
    /// The last rejection reason.
    pub last_rejection: Option<RejectionReason>,
    /// Carrier discontinuity generation (bumped on every loss).
    pub generation: u64,
    /// Windows still queued (not in-flight leases).
    pub queued_windows: usize,
    /// Packet reservations owned by the queue **including** in-flight leases.
    pub reserved_packets: usize,
    /// Byte reservations owned by the queue **including** in-flight leases.
    pub reserved_bytes: usize,
    /// The sink wedged past the grace (or hung up) and this binding was
    /// abandoned; the run's other bindings continue.
    pub failed: bool,
}

/// Admission limits. Defaults are the carrier's broadcast ceilings; tests and
/// capacity measurement may tighten them, never to paper over overload.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryLimits {
    /// Maximum packets owned by one binding, including in-flight leases.
    pub max_packets: usize,
    /// Maximum referenced coded bytes owned by one binding.
    pub max_bytes: usize,
    /// Maximum time since admission for a queued, unconsumed window.
    pub max_age: Duration,
}

impl Default for DeliveryLimits {
    fn default() -> Self {
        Self {
            max_packets: MAX_QUEUE_PACKETS,
            max_bytes: MAX_QUEUE_BYTES,
            max_age: MAX_QUEUE_AGE,
        }
    }
}

/// The exact duration of one program tick, as a non-negative `Duration`.
///
/// Exact integer arithmetic from the rational cadence: a float fps would drift,
/// and this value bounds every batch's encoded-time span.
#[must_use]
pub fn tick_span(cadence: Rational) -> Duration {
    if cadence.num <= 0 || cadence.den <= 0 {
        return Duration::ZERO;
    }
    let (num, den) = (
        u128::try_from(cadence.num).unwrap_or(1),
        u128::try_from(cadence.den).unwrap_or(1),
    );
    let nanos = den.saturating_mul(1_000_000_000) / num;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

struct Binding {
    policy: BindingPolicy,
    queue: BoundedBatchQueue,
    lease_tx: SyncSender<BatchLease>,
    /// A lease popped but not yet handed to the pump (at most one).
    pending: Option<BatchLease>,
    staged: Vec<EncodedPacket>,
    wedged: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
    admitted_batches: u64,
    admitted_packets: u64,
    admitted_bytes: u64,
    rejected_batches: u64,
    rejected_packets: u64,
    rejected_bytes: u64,
    last_rejection: Option<RejectionReason>,
    failed: bool,
}

/// Atomic delivery for every binding of one program.
pub struct BindingDelivery {
    bindings: Vec<Binding>,
    limits: DeliveryLimits,
}

impl BindingDelivery {
    /// Build one independently bounded binding per policy, returning the
    /// per-sink packet receivers the existing sink runners consume.
    ///
    /// `gate` seeds every binding's recovery policy. It must describe the codec
    /// and framing of the packets actually offered: with no gate the carrier
    /// never re-opens after a loss (it refuses to trust a bare KEY flag), so a
    /// non-NAL codec is better served by rebuilding the binding than by
    /// guessing.
    #[must_use]
    pub fn new(
        policies: Vec<BindingPolicy>,
        limits: DeliveryLimits,
        gate: Option<multiview_output::batch::RecoveryGate>,
    ) -> (Self, Vec<Receiver<EncodedPacket>>) {
        let mut bindings = Vec::with_capacity(policies.len());
        let mut receivers = Vec::with_capacity(policies.len());
        for policy in policies {
            let (packet_tx, packet_rx) = mpsc::sync_channel::<EncodedPacket>(HANDOFF_PACKETS);
            let (lease_tx, lease_rx) = mpsc::sync_channel::<BatchLease>(HANDOFF_LEASES);
            let wedged = Arc::new(AtomicBool::new(false));
            let pump_wedged = Arc::clone(&wedged);
            let pump = thread::Builder::new()
                .name("multiview-binding-pump".to_owned())
                .spawn(move || {
                    while let Ok(lease) = lease_rx.recv() {
                        for packet in lease.packets() {
                            if !send_bounded_packet(&packet_tx, packet.clone()) {
                                // The sink hung up or stopped draining for the
                                // whole grace: this binding is finished. Drop
                                // the lease (releasing its reservation) and end
                                // the pump; no other binding is affected.
                                pump_wedged.store(true, Ordering::Release);
                                return;
                            }
                        }
                    }
                })
                .ok();
            let mut queue = BoundedBatchQueue::with_limits(
                limits.max_packets,
                limits.max_bytes,
                limits.max_age,
            );
            if let Some(gate) = gate {
                queue = queue.with_recovery_gate(gate);
            }
            bindings.push(Binding {
                policy,
                queue,
                lease_tx,
                pending: None,
                staged: Vec::new(),
                wedged,
                pump,
                admitted_batches: 0,
                admitted_packets: 0,
                admitted_bytes: 0,
                rejected_batches: 0,
                rejected_packets: 0,
                rejected_bytes: 0,
                last_rejection: None,
                failed: false,
            });
            receivers.push(packet_rx);
        }
        (
            Self { bindings, limits },
            receivers,
        )
    }

    /// The admission limits in force (for the run report).
    #[must_use]
    pub const fn limits(&self) -> DeliveryLimits {
        self.limits
    }

    /// Stage one emitted packet group (a tick's video, program audio or discrete
    /// tracks) for every live binding. Cloning an [`EncodedPacket`] is a
    /// ref-counted copy of the coded payload, exactly as the previous fan-out.
    ///
    /// A push binding whose producer-side gate is closed until a post-loss
    /// keyframe simply does not stage the packet — its link status already
    /// records that gate state.
    pub fn stage(&mut self, packets: &[EncodedPacket]) {
        if packets.is_empty() {
            return;
        }
        for binding in &mut self.bindings {
            if binding.failed {
                continue;
            }
            for packet in packets {
                let video_keyframe = matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe();
                if let BindingPolicy::Push(link) = &binding.policy {
                    if !link.admit_upstream(video_keyframe) {
                        continue;
                    }
                }
                binding.staged.push(packet.clone());
            }
        }
    }

    /// Atomically admit the staged batch of the **previous** tick to every
    /// binding. Never waits: the only outcome of a full or stale binding is a
    /// whole-batch loss recorded against that binding.
    pub fn flush_tick(&mut self, now: Instant, media_span: Duration) {
        for binding in &mut self.bindings {
            if binding.failed {
                binding.staged.clear();
                continue;
            }
            if binding.staged.is_empty() {
                continue;
            }
            let staged = std::mem::take(&mut binding.staged);
            let packets = staged.len();
            let bytes = staged.iter().map(EncodedPacket::len).sum::<usize>();
            let keyframes: Vec<bool> = staged
                .iter()
                .map(|packet| matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe())
                .collect();
            match binding.queue.offer_emitted(staged, media_span, now) {
                Ok(()) => {
                    binding.admitted_batches = binding.admitted_batches.saturating_add(1);
                    binding.admitted_packets = binding.admitted_packets.saturating_add(packets as u64);
                    binding.admitted_bytes = binding.admitted_bytes.saturating_add(bytes as u64);
                    if let BindingPolicy::Push(link) = &binding.policy {
                        for keyframe in &keyframes {
                            link.queued_upstream(*keyframe);
                        }
                    }
                }
                Err(error) => {
                    let reason = match error {
                        EmissionError::InvalidWindow(_) => RejectionReason::InvalidWindow,
                        EmissionError::Admission(offer) => RejectionReason::from_offer(offer),
                    };
                    binding.rejected_batches = binding.rejected_batches.saturating_add(1);
                    binding.rejected_packets = binding.rejected_packets.saturating_add(packets as u64);
                    binding.rejected_bytes = binding.rejected_bytes.saturating_add(bytes as u64);
                    binding.last_rejection = Some(reason);
                    if let BindingPolicy::Push(link) = &binding.policy {
                        for _ in &keyframes {
                            link.shed_upstream();
                        }
                    }
                }
            }
            // A wedged pump means this binding is over: report it and stop
            // offering (its queue keeps draining until the lease expires).
            if binding.wedged.load(Ordering::Acquire) {
                binding.failed = true;
                binding.last_rejection = Some(RejectionReason::SinkWedged);
            }
            Self::pump_binding(binding, now);
        }
    }

    /// Hand at most one pending lease and then every immediately available
    /// queued window to the binding's pump without waiting. A full lease
    /// handoff leaves the window queued (and age-bounded) rather than blocking.
    fn pump_binding(binding: &mut Binding, now: Instant) {
        if binding.failed {
            while binding.queue.pop_at(now).ok().flatten().is_some() {}
            return;
        }
        if let Some(lease) = binding.pending.take() {
            match binding.lease_tx.try_send(lease) {
                Ok(()) => {}
                Err(TrySendError::Full(lease)) => {
                    binding.pending = Some(lease);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    binding.failed = true;
                    return;
                }
            }
        }
        loop {
            match binding.queue.pop_at(now) {
                Ok(Some(lease)) => match binding.lease_tx.try_send(lease) {
                    Ok(()) => {}
                    Err(TrySendError::Full(lease)) => {
                        binding.pending = Some(lease);
                        return;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        binding.failed = true;
                        return;
                    }
                },
                Ok(None) => return,
                Err(error) => {
                    binding.last_rejection = Some(RejectionReason::from_offer(error));
                    return;
                }
            }
        }
    }

    /// Stop delivery **after serving every admitted window**.
    ///
    /// Callers flush the final tick and the encoder tail first. This then hands
    /// every remaining queued window to its pump until the queues are empty or
    /// the bounded trailer deadline expires (a live sink drains promptly; a
    /// wedged one has already been abandoned), and only then closes the handoff
    /// and joins the pumps. Dropping admitted windows here would be silent
    /// media loss, which the contract forbids.
    pub fn finish(mut self) {
        let deadline = Instant::now() + FINISH_DRAIN_DEADLINE;
        loop {
            let now = Instant::now();
            let mut handed_off = true;
            for binding in &mut self.bindings {
                Self::pump_binding(binding, now);
                if binding.pending.is_some() || binding.queue.queued_windows() > 0 {
                    handed_off = false;
                }
            }
            if handed_off || now >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        for binding in &mut self.bindings {
            // Replace the sender with a closed one so the pump's `recv()` ends
            // once it has forwarded every lease it already owns.
            let (closed, _) = mpsc::sync_channel::<BatchLease>(0);
            let sender = std::mem::replace(&mut binding.lease_tx, closed);
            drop(sender);
            drop(binding.pending.take());
            if let Some(pump) = binding.pump.take() {
                let _ = pump.join();
            }
        }
    }

    /// A status snapshot per binding, in configured output order.
    #[must_use]
    pub fn statuses(&self) -> Vec<BindingStatus> {
        self.bindings
            .iter()
            .map(|binding| {
                let (reserved_packets, reserved_bytes) = binding.queue.reserved();
                BindingStatus {
                    admitted_batches: binding.admitted_batches,
                    admitted_packets: binding.admitted_packets,
                    admitted_bytes: binding.admitted_bytes,
                    rejected_batches: binding.rejected_batches,
                    rejected_packets: binding.rejected_packets,
                    rejected_bytes: binding.rejected_bytes,
                    last_rejection: binding.last_rejection,
                    generation: binding.queue.generation(),
                    queued_windows: binding.queue.queued_windows(),
                    reserved_packets,
                    reserved_bytes,
                    failed: binding.failed,
                }
            })
            .collect()
    }

    /// True when every binding is drained (no queued and no in-flight window).
    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.bindings.iter().all(|binding| binding.queue.is_drained())
    }
}

/// Send one packet to a sink, waiting at most [`PUMP_WEDGE_GRACE`] for a slot.
/// Returns `false` when the sink hung up or stopped draining for the whole
/// grace. This runs on the binding's own pump thread, so it can never pace the
/// bake consumer or another binding.
fn send_bounded_packet(tx: &SyncSender<EncodedPacket>, mut packet: EncodedPacket) -> bool {
    let deadline = Instant::now() + PUMP_WEDGE_GRACE;
    loop {
        match tx.try_send(packet) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return false;
                }
                packet = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{
        tick_span, BindingDelivery, BindingPolicy, DeliveryLimits, RejectionReason,
        HANDOFF_LEASES, PUMP_WEDGE_GRACE,
    };
    use multiview_core::time::Rational;
    use multiview_ffmpeg::{CodecKind, EncodedPacket, NalFraming, StreamKind};
    use multiview_output::batch::RecoveryGate;
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    fn packet(size: usize) -> EncodedPacket {
        EncodedPacket::from_packet(ffmpeg_next::codec::packet::Packet::copy(&vec![0_u8; size]))
    }

    /// An Annex-B H.264 access unit whose NAL type is `nal_type` (5 = IDR slice,
    /// 1 = non-IDR slice).
    fn h264_packet(nal_type: u8) -> EncodedPacket {
        let header = 0x60 | (nal_type & 0x1f);
        EncodedPacket::from_packet(ffmpeg_next::codec::packet::Packet::copy(&[
            0, 0, 0, 1, header, 0x88,
        ]))
    }

    fn gate() -> Option<RecoveryGate> {
        Some(RecoveryGate::StrictRap {
            codec: CodecKind::H264,
            framing: NalFraming::AnnexB,
        })
    }

    fn limits(max_packets: usize) -> DeliveryLimits {
        DeliveryLimits {
            max_packets,
            max_bytes: 16 * 1024 * 1024,
            max_age: Duration::from_secs(2),
        }
    }

    fn drain(rx: &Receiver<EncodedPacket>) -> Vec<EncodedPacket> {
        let mut out = Vec::new();
        while let Ok(packet) = rx.recv() {
            out.push(packet);
        }
        out
    }

    #[test]
    fn tick_span_is_exact_for_ntsc_cadence() {
        // 1001/30000 s = 33_366_666.66… ns → floor 33_366_666 ns.
        assert_eq!(
            tick_span(Rational::new(30_000, 1001)),
            Duration::from_nanos(33_366_666)
        );
        assert_eq!(
            tick_span(Rational::new(25, 1)),
            Duration::from_millis(40)
        );
        assert_eq!(tick_span(Rational::new(0, 1)), Duration::ZERO);
    }

    #[test]
    fn one_tick_is_admitted_atomically_and_reaches_its_sink() {
        let (mut delivery, mut receivers) =
            BindingDelivery::new(vec![BindingPolicy::Bounded], DeliveryLimits::default(), None);
        let rx = receivers.remove(0);
        // A real sink drains for the whole run; the handoff is only one packet
        // deep, so the test drains on its own thread exactly like a muxer.
        let collector = std::thread::spawn(move || drain(&rx));
        let span = tick_span(Rational::new(30_000, 1001));
        delivery.stage(&[h264_packet(5), packet(4)]);
        delivery.flush_tick(Instant::now(), span);
        let status = delivery.statuses()[0];
        assert_eq!(status.admitted_batches, 1);
        assert_eq!(status.admitted_packets, 2);
        // One 6-byte Annex-B IDR AU plus a 4-byte packet.
        assert_eq!(status.admitted_bytes, 10);
        assert_eq!(status.rejected_batches, 0);
        assert_eq!(status.generation, 0);
        delivery.finish();
        assert_eq!(collector.join().expect("collector").len(), 2);
    }

    #[test]
    fn oversized_window_is_rejected_whole_and_declares_a_discontinuity() {
        let (mut delivery, receivers) =
            BindingDelivery::new(vec![BindingPolicy::Bounded], limits(4), None);
        let span = tick_span(Rational::new(30_000, 1001));
        // Five packets in one window: over the binding's 4-packet cap. The whole
        // window is rejected — never a partial burst.
        let packets: Vec<EncodedPacket> = (0..5).map(|_| packet(2)).collect();
        delivery.stage(&packets);
        delivery.flush_tick(Instant::now(), span);
        let status = delivery.statuses()[0];
        assert_eq!(status.admitted_batches, 0);
        assert_eq!(status.rejected_batches, 1);
        assert_eq!(status.rejected_packets, 5);
        assert_eq!(status.last_rejection, Some(RejectionReason::InvalidWindow));
        assert!(status.generation >= 1, "loss must bump the generation");
        delivery.finish();
        assert!(drain(&receivers[0]).is_empty());
    }

    #[test]
    fn a_wedged_binding_cannot_stall_the_producer_or_a_healthy_sibling() {
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded, BindingPolicy::Bounded],
            limits(64),
            None,
        );
        let span = tick_span(Rational::new(30_000, 1001));
        // Binding 0's sink never drains: its pump fills the one-packet handoff
        // and blocks. Binding 1 drains normally on its own thread.
        let healthy_rx = receivers.remove(1);
        let collector = std::thread::spawn(move || drain(&healthy_rx));
        let wedged_rx = receivers.remove(0);
        for _ in 0..20 {
            delivery.stage(&[packet(2)]);
            let started = Instant::now();
            delivery.flush_tick(Instant::now(), span);
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "admission must never wait on a wedged sink"
            );
        }
        // The flush loop above runs far faster than real ticks, so both queues
        // are briefly behind their sinks. With the same producer and the same
        // elapsed time the wedged binding's backlog must be strictly larger:
        // its pump is blocked on a sink that never reads, while its sibling's
        // pump keeps handing whole windows to a sink that does.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let statuses = delivery.statuses();
            let wedged = statuses[0].reserved_packets + statuses[0].queued_windows;
            let healthy = statuses[1].reserved_packets + statuses[1].queued_windows;
            if healthy < wedged || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let statuses = delivery.statuses();
        assert_eq!(statuses[1].admitted_batches, 20);
        assert_eq!(statuses[1].rejected_batches, 0);
        assert!(!statuses[1].failed);
        assert!(
            statuses[0].reserved_packets + statuses[0].queued_windows
                > statuses[1].reserved_packets + statuses[1].queued_windows,
            "wedged={:?} healthy={:?}",
            statuses[0],
            statuses[1]
        );
        // Hanging the wedged sink up fails only that binding: the healthy
        // sibling keeps being admitted afterwards.
        drop(wedged_rx);
        std::thread::sleep(Duration::from_millis(50));
        delivery.stage(&[packet(2)]);
        delivery.flush_tick(Instant::now(), span);
        let statuses = delivery.statuses();
        assert!(statuses[0].failed, "the dead binding must fail independently");
        assert_eq!(
            statuses[0].last_rejection,
            Some(RejectionReason::SinkWedged)
        );
        assert!(!statuses[1].failed);
        let healthy_before = statuses[1].admitted_batches;
        delivery.stage(&[packet(2)]);
        delivery.flush_tick(Instant::now(), span);
        assert!(
            delivery.statuses()[1].admitted_batches > healthy_before,
            "the healthy sibling must keep being admitted after its peer fails"
        );
        delivery.finish();
        assert_eq!(collector.join().expect("collector").len(), 22);
    }

    #[test]
    fn stale_queued_window_is_flushed_with_an_age_reason() {
        let (mut delivery, receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded],
            DeliveryLimits {
                max_packets: 64,
                max_bytes: 1024 * 1024,
                max_age: Duration::from_millis(10),
            },
            None,
        );
        let span = tick_span(Rational::new(30_000, 1001));
        let start = Instant::now();
        // Nobody drains this binding: the pump holds one lease and blocks, the
        // lease handoff fills, and later windows stay queued and age out.
        let mut age_loss = None;
        for step in 0..10 {
            delivery.stage(&[packet(2)]);
            delivery.flush_tick(start + Duration::from_millis(50 * step), span);
            let status = delivery.statuses()[0];
            if status.generation >= 1 {
                age_loss = Some(status);
                break;
            }
        }
        let status = age_loss.expect("the stale queued window must be flushed by age");
        assert_eq!(status.last_rejection, Some(RejectionReason::OldestAge));
        assert!(status.queued_windows <= MAX_LIVE_WINDOWS);
        drop(receivers);
        delivery.finish();
    }

    /// A generous bound used only to prove the queue stayed small in the age test.
    const MAX_LIVE_WINDOWS: usize = 64;

    #[test]
    fn finish_serves_every_admitted_tail_packet_before_closing() {
        let (mut delivery, mut receivers) =
            BindingDelivery::new(vec![BindingPolicy::Bounded], DeliveryLimits::default(), gate());
        let rx = receivers.remove(0);
        let collector = std::thread::spawn(move || drain(&rx));
        let span = tick_span(Rational::new(30_000, 1001));
        // The first window must open with a strict RAP for the configured gate.
        delivery.stage(&[h264_packet(5)]);
        delivery.flush_tick(Instant::now(), span);
        delivery.stage(&[packet(3), packet(3)]);
        delivery.flush_tick(Instant::now(), span);
        delivery.finish();
        assert_eq!(collector.join().expect("collector").len(), 3);
    }

    #[test]
    fn lease_handoff_depth_is_bounded() {
        // The consumer → pump handoff holds at most HANDOFF_LEASES in-flight
        // windows; anything more stays in the age-bounded queue.
        assert_eq!(HANDOFF_LEASES, 2);
        assert!(PUMP_WEDGE_GRACE <= Duration::from_secs(2));
    }

    #[test]
    fn a_strict_gate_requires_a_fresh_rap_before_delivery_resumes() {
        // Cap of one packet: a two-packet window is rejected whole, which is a
        // real delivery loss and closes the recovery gate.
        let (mut delivery, mut receivers) =
            BindingDelivery::new(vec![BindingPolicy::Bounded], limits(1), gate());
        let rx = receivers.remove(0);
        let collector = std::thread::spawn(move || drain(&rx));
        let span = tick_span(Rational::new(30_000, 1001));

        delivery.stage(&[h264_packet(5), packet(2)]);
        delivery.flush_tick(Instant::now(), span);
        let status = delivery.statuses()[0];
        assert_eq!(status.admitted_batches, 0);
        assert_eq!(status.rejected_batches, 1);
        assert_eq!(status.last_rejection, Some(RejectionReason::InvalidWindow));
        assert!(status.generation >= 1);

        // A non-IDR window cannot resume delivery: the bare KEY flag is not
        // trusted, the codec-specific AU must really carry an IDR.
        delivery.stage(&[h264_packet(1)]);
        delivery.flush_tick(Instant::now(), span);
        let status = delivery.statuses()[0];
        assert_eq!(status.admitted_batches, 0);
        assert_eq!(
            status.last_rejection,
            Some(RejectionReason::NeedsRecoveryPoint)
        );

        // A genuine IDR AU resumes delivery.
        delivery.stage(&[h264_packet(5)]);
        delivery.flush_tick(Instant::now(), span);
        let status = delivery.statuses()[0];
        assert_eq!(status.admitted_batches, 1);
        delivery.finish();
        assert_eq!(collector.join().expect("collector").len(), 1);
    }
}
