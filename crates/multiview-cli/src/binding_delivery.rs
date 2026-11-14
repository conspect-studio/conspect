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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use multiview_core::time::Rational;
use multiview_ffmpeg::{EncodedPacket, StreamKind};
use multiview_output::batch::{
    BatchLease, BoundedBatchQueue, Discontinuity, EmissionError, OfferError, MAX_QUEUE_AGE,
    MAX_QUEUE_BYTES, MAX_QUEUE_PACKETS,
};
use multiview_output::push_link::PushLinkStatus;

/// Grace a binding pump gives a sink that has stopped draining before it
/// abandons that binding. Bounded: a wedged sink can never pin a pump thread or
/// its in-flight lease forever.
pub(crate) const PUMP_WEDGE_GRACE: Duration = Duration::from_secs(2);

/// Depth (in packets) of the pump → sink handoff. One packet: the pump blocks
/// on the sink instead of buffering a second copy of a potentially huge IDR.
pub(crate) const HANDOFF_PACKETS: usize = 1;

/// Depth (in leases) of the consumer → pump handoff. A lease owns its batch's
/// reservation until the mux drops it, so this is a bounded in-flight window,
/// not a second unbounded queue.
pub(crate) const HANDOFF_LEASES: usize = 2;

/// Total trailer deadline for draining already-admitted windows at
/// [`BindingDelivery::finish`]. Bounded, per the contract's 2 s sink
/// lifecycle/flush limit.
pub(crate) const FINISH_DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// How a binding's delivery treats a full or stale batch queue.
#[derive(Clone)]
pub(crate) enum BindingPolicy {
    /// A local artifact sink (file/HLS): a lost batch fails this binding
    /// visibly; it is never silently dropped and never delays another binding.
    Bounded,
    /// A resilient live push sink: losses are counted on its existing link
    /// status, preserving the `/status` counters the UI already reads.
    Push(Arc<PushLinkStatus>),
    /// A sink that may stall and come back (the cast packager's disk writes):
    /// every packet it does not receive is added to the counter, and a
    /// stalled sink is never abandoned. Its pump waits one
    /// [`PUMP_WEDGE_GRACE`] for a stalled sink, then drops (and counts)
    /// without waiting until the sink takes a packet again. The sink reads
    /// the counter and declares the gap in its own stream.
    DropCounted(Arc<AtomicU64>),
}

/// Why a batch was not admitted, for status/telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectionReason {
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
            OfferError::NeedsRecoveryPoint
            | OfferError::Closed
            | OfferError::ProducerLoss
            | OfferError::DecoderConfigChanged => Self::NeedsRecoveryPoint,
            OfferError::WrongProgramShape => Self::WrongProgramShape,
        }
    }

    /// A short, stable label for logs and status payloads.
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
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
pub(crate) struct BindingStatus {
    /// Windows admitted atomically.
    pub(crate) admitted_batches: u64,
    /// Packets inside admitted windows.
    pub(crate) admitted_packets: u64,
    /// Referenced coded bytes inside admitted windows.
    pub(crate) admitted_bytes: u64,
    /// Windows rejected atomically (whole-window loss).
    pub(crate) rejected_batches: u64,
    /// Packets lost with those rejected windows.
    pub(crate) rejected_packets: u64,
    /// Bytes lost with those rejected windows.
    pub(crate) rejected_bytes: u64,
    /// The last rejection reason.
    pub(crate) last_rejection: Option<RejectionReason>,
    /// Carrier discontinuity generation (bumped on every loss).
    pub(crate) generation: u64,
    /// Windows still queued (not in-flight leases).
    pub(crate) queued_windows: usize,
    /// Packet reservations owned by the queue **including** in-flight leases.
    pub(crate) reserved_packets: usize,
    /// Byte reservations owned by the queue **including** in-flight leases.
    pub(crate) reserved_bytes: usize,
    /// The sink wedged past the grace (or hung up) and this binding was
    /// abandoned; the run's other bindings continue.
    pub(crate) failed: bool,
}

/// Admission limits. Defaults are the carrier's broadcast ceilings; tests and
/// capacity measurement may tighten them, never to paper over overload.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)] // Public limit names mirror the carrier's max_packets/max_bytes/max_age contract.
pub(crate) struct DeliveryLimits {
    /// Maximum packets owned by one binding, including in-flight leases.
    pub(crate) max_packets: usize,
    /// Maximum referenced coded bytes owned by one binding.
    pub(crate) max_bytes: usize,
    /// Maximum time since admission for a queued, unconsumed window.
    pub(crate) max_age: Duration,
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
pub(crate) fn tick_span(cadence: Rational) -> Duration {
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

/// One assembled, immutable A/V window for exactly one binding.
///
/// The window assembler (DTS watermarking + which packets belong to which
/// encoded interval) owns this; delivery owns the queue it is admitted into.
pub(crate) struct DeliveryWindow {
    /// Index into the configured output order.
    pub(crate) output_index: usize,
    /// Video-key packets first, then this binding's selected audio, in stable
    /// stream order. Built with `clone_shared_ref`: independent headers, one
    /// coded payload allocation.
    pub(crate) packets: Vec<EncodedPacket>,
    /// The assembler's measured collection span for this window (≤ 250 ms).
    pub(crate) media_span: Duration,
}

/// Receipt for an atomically admitted window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admission {
    /// The binding the window was admitted to.
    pub(crate) output_index: usize,
    /// The queue generation it was admitted into.
    pub(crate) generation: u64,
}

/// A window rejected **whole**: none of its packets were queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowRejection {
    /// The binding that rejected the window.
    pub(crate) output_index: usize,
    /// The post-loss queue generation (also the signal to reset mux continuity).
    pub(crate) generation: u64,
    /// Stable rejection reason for status.
    pub(crate) reason: RejectionReason,
    /// Packets lost with the rejected window.
    pub(crate) packets: usize,
    /// Referenced coded bytes lost with the rejected window.
    pub(crate) bytes: usize,
}

/// The only crossing point between a window assembler and delivery.
///
/// The assembler never owns a queue, generation or lease: it decides *what*
/// belongs in a window and calls [`Self::offer_window`]; delivery owns the one
/// bounded queue per binding, the atomic admission, the generation/loss truth
/// and the mux lease lifetime.
#[allow(dead_code)] // reason: the published assembler seam; `offer_window` is used by the pipeline's direct path and the rest is called by `rendition_dispatch`.
pub(crate) trait WindowDelivery {
    /// Atomically admit one complete window for one binding. Never waits, never
    /// partially admits. `generation` is the assembler's view; the queue's own
    /// generation is authoritative and is what the receipt/loss reports.
    ///
    /// # Errors
    /// Returns the whole-window rejection (nothing was delivered).
    fn offer_window(
        &mut self,
        output_index: usize,
        generation: u64,
        packets: Vec<EncodedPacket>,
        media_span: Duration,
        now: Instant,
    ) -> Result<(), EmissionError>;

    /// Declare real encoded media lost before admission (an invalid window the
    /// assembler discarded), so the binding requires a fresh RAP.
    fn report_loss(&mut self, output_index: usize, reason: OfferError);

    /// Declare a mux discontinuity that is NOT a loss (the next offered window
    /// opens at a strict RAP with a different decoder configuration): bumps
    /// the binding's generation without flushing or arming the RAP gate, so
    /// the window itself is admitted. Not counted as a rejection.
    fn mark_discontinuity(&mut self, output_index: usize, reason: OfferError);

    /// [`Self::mark_discontinuity`] with `DecoderConfigChanged` carrying the
    /// new configuration's video stream parameters (#342 B3 review V2), so
    /// the output's muxer can be rebuilt from them. The default drops them.
    fn mark_config_change(
        &mut self,
        output_index: usize,
        params: Option<multiview_ffmpeg::StreamCodecParameters>,
    ) {
        let _ = params;
        self.mark_discontinuity(output_index, OfferError::DecoderConfigChanged);
    }

    /// Consume this binding's loss event (generation + flushed counters) so the
    /// mux can reset continuity. Exactly one consumer.
    fn take_discontinuity(&mut self, output_index: usize) -> Option<Discontinuity>;

    /// The binding's current queue generation.
    fn generation(&self, output_index: usize) -> u64;

    /// Point-in-time per-binding truth.
    fn statuses(&self) -> Vec<BindingStatus>;

    /// Off-clock tail pump: the producer calls this once per output tick (and
    /// after every offered window) so a subscriber whose handoff the transport
    /// has already drained still receives windows admitted from a now-quiet
    /// producer. Never waits on a peer; the default does no periodic work.
    fn pump(&mut self, _now: Instant) {}

    /// Stop after serving every admitted window.
    fn finish(&mut self);

    /// Fatal assembler loss: invalidate queued media and stop future offers.
    /// Unlike `finish`, this does not claim an admitted tail was delivered.
    fn abort(&mut self);
}

impl WindowDelivery for BindingDelivery {
    fn offer_window(
        &mut self,
        output_index: usize,
        generation: u64,
        packets: Vec<EncodedPacket>,
        media_span: Duration,
        now: Instant,
    ) -> Result<(), EmissionError> {
        if packets.is_empty() {
            return Ok(());
        }
        if !self.bindings.iter().any(|_| true) || self.bindings.get(output_index).is_none() {
            return Err(EmissionError::Admission(OfferError::WrongProgramShape));
        }
        // The queue's generation is authoritative. A stale assembler generation
        // is not fatal: its own `take_discontinuity` call realigns it, and the
        // window still has to pass the strict-RAP gate.
        let _ = generation;
        match self.try_offer(
            DeliveryWindow {
                output_index,
                packets,
                media_span,
            },
            now,
        ) {
            Ok(_) => Ok(()),
            Err(rejection) => Err(EmissionError::Admission(match rejection.reason {
                RejectionReason::InvalidWindow => OfferError::Oversized,
                RejectionReason::Occupied | RejectionReason::SinkWedged => OfferError::Occupied,
                RejectionReason::OldestAge => OfferError::OldestAge,
                RejectionReason::ClockReversed => OfferError::ClockReversed,
                RejectionReason::NeedsRecoveryPoint => OfferError::NeedsRecoveryPoint,
                RejectionReason::WrongProgramShape => OfferError::WrongProgramShape,
            })),
        }
    }

    fn report_loss(&mut self, output_index: usize, reason: OfferError) {
        if let Some(binding) = self.bindings.get_mut(output_index) {
            binding.queue.report_loss(reason);
            binding.rejected_batches = binding.rejected_batches.saturating_add(1);
            binding.last_rejection = Some(RejectionReason::from_offer(reason));
            sync_drop_counted_flushed(binding);
        }
    }

    fn mark_discontinuity(&mut self, output_index: usize, reason: OfferError) {
        if let Some(binding) = self.bindings.get_mut(output_index) {
            binding.queue.mark_discontinuity(reason);
        }
    }

    fn mark_config_change(
        &mut self,
        output_index: usize,
        params: Option<multiview_ffmpeg::StreamCodecParameters>,
    ) {
        if let Some(binding) = self.bindings.get_mut(output_index) {
            binding.queue.mark_config_change(params);
        }
    }

    fn take_discontinuity(&mut self, output_index: usize) -> Option<Discontinuity> {
        self.bindings
            .get_mut(output_index)
            .and_then(|binding| binding.queue.take_discontinuity())
    }

    fn generation(&self, output_index: usize) -> u64 {
        self.bindings
            .get(output_index)
            .map_or(0, |binding| binding.queue.generation())
    }

    fn statuses(&self) -> Vec<BindingStatus> {
        BindingDelivery::statuses(self)
    }

    fn finish(&mut self) {
        BindingDelivery::finish(self);
    }
    fn abort(&mut self) {
        for binding in &mut self.bindings {
            #[cfg(test)]
            binding.staged.clear();
            binding.queue.report_loss(OfferError::ProducerLoss);
            binding.queue.close();
            sync_drop_counted_flushed(binding);
            binding.failed = true;
            binding.last_rejection = Some(RejectionReason::NeedsRecoveryPoint);
        }
    }
}

/// One packet and the whole-window admission permit. The permit is retained
/// through the actual sink write, including the last packet in a window.
/// `av_interleaved_write_frame` can move the packet into libav's internal
/// interleave buffer: its `AVBufferRef` keeps coded bytes alive after the next
/// pull releases this permit. That separate mux-internal retention must have
/// its own interleave bound; the queue reservation covers the sink-write ack.
pub(crate) struct LeasedPacket {
    pub(crate) packet: EncodedPacket,
    pub(crate) permit: Arc<BatchLease>,
}

struct Binding {
    policy: BindingPolicy,
    queue: BoundedBatchQueue,
    lease_tx: SyncSender<BatchLease>,
    /// A lease popped but not yet handed to the pump (at most one).
    pending: Option<BatchLease>,
    #[cfg(test)]
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
    /// Last value of `queue.flushed_packets_total()` already folded into this
    /// binding's `DropCounted` counter (INV-1, review-lens3): a monotonic
    /// high-water mark so every queue-flushed packet is added exactly once,
    /// independent of how many separate loss events produced it.
    drop_counted_flushed_seen: u64,
}

/// For a `DropCounted` binding, add every packet the carrier's monotonic
/// `flushed_packets_total()` has newly flushed from the queued backlog since
/// the last call (admission `OldestAge`/`Occupied`, a stale `pop_at`,
/// `report_loss`, or `close`) to the external counter, exactly once. A no-op
/// for `Bounded`/`Push` bindings, whose paths are unchanged.
fn sync_drop_counted_flushed(binding: &mut Binding) {
    if let BindingPolicy::DropCounted(counter) = &binding.policy {
        let total = binding.queue.flushed_packets_total();
        let delta = total.saturating_sub(binding.drop_counted_flushed_seen);
        if delta > 0 {
            counter.fetch_add(delta, Ordering::AcqRel);
        }
        binding.drop_counted_flushed_seen = total;
    }
}

/// A whole lease discarded at a real ownership boundary WITHOUT any of its
/// packets having been handed to the sink: count it exactly once for a
/// `DropCounted` binding. Call this only at an actual discard point (a
/// popped-but-never-pumped lease, a lease still owned by a failed/abandoned
/// pump) — never infer a loss from reservation occupancy (INV-1,
/// review-lens3 round 2: a reservation also covers delivered packets a sink
/// is still retaining, and inferring loss from it both misses real losses
/// and double-counts ones already counted elsewhere).
fn count_discarded_lease(policy: &BindingPolicy, lease: &BatchLease) {
    if let BindingPolicy::DropCounted(counter) = policy {
        let lost = u64::try_from(lease.packets().len()).unwrap_or(u64::MAX);
        counter.fetch_add(lost, Ordering::AcqRel);
    }
}

/// Atomic delivery for every binding of one program.
pub(crate) struct BindingDelivery {
    bindings: Vec<Binding>,
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
    pub(crate) fn new(
        policies: Vec<BindingPolicy>,
        limits: DeliveryLimits,
        gates: Vec<Option<multiview_output::batch::RecoveryGate>>,
    ) -> (Self, Vec<Receiver<LeasedPacket>>) {
        assert_eq!(
            policies.len(),
            gates.len(),
            "each output must declare a recovery gate"
        );
        let mut bindings = Vec::with_capacity(policies.len());
        let mut receivers = Vec::with_capacity(policies.len());
        for (policy, gate) in policies.into_iter().zip(gates) {
            let (packet_tx, packet_rx) = mpsc::sync_channel::<LeasedPacket>(HANDOFF_PACKETS);
            let (lease_tx, lease_rx) = mpsc::sync_channel::<BatchLease>(HANDOFF_LEASES);
            let wedged = Arc::new(AtomicBool::new(false));
            let pump_wedged = Arc::clone(&wedged);
            let drop_counter = match &policy {
                BindingPolicy::DropCounted(counter) => Some(Arc::clone(counter)),
                BindingPolicy::Bounded | BindingPolicy::Push(_) => None,
            };
            let pump = thread::Builder::new()
                .name("multiview-binding-pump".to_owned())
                .spawn(move || {
                    // Only a `DropCounted` binding survives a stalled sink.
                    let mut stalled = false;
                    while let Ok(lease) = lease_rx.recv() {
                        let step = pump_lease(
                            &packet_tx,
                            &Arc::new(lease),
                            drop_counter.as_deref(),
                            &mut stalled,
                        );
                        if step == PumpStep::Abandon {
                            // The sink hung up or (for a sink that must not
                            // lose a packet) stopped draining for the whole
                            // grace: this binding is finished. Mark it wedged
                            // immediately so every subsequent `admit`/
                            // `pump_binding` call observes `failed` and stops
                            // handing this pump any further lease (see
                            // `pump_binding`'s `binding.failed` branch).
                            pump_wedged.store(true, Ordering::Release);
                            if let Some(counter) = drop_counter.as_deref() {
                                // INV-1-R3 (review3-lens3-invariants.md): a
                                // lease may already have been accepted into
                                // the bounded `lease_tx`/`lease_rx` channel
                                // (by a racing `admit` call that read
                                // `wedged` as false just before the store
                                // above) before this abandon is visible to
                                // any caller. Simply returning here would
                                // drop `lease_rx`, and every such
                                // already-queued (and any still-racing)
                                // lease would be destroyed — releasing its
                                // reservation — without ever being counted:
                                // a silent, uncounted loss of whole
                                // `BatchLease`s this carrier had already
                                // admitted.
                                //
                                // For `DropCounted` only, keep draining
                                // `lease_rx` (this pump thread alone; no
                                // other binding, no producer, is blocked by
                                // this) until the sender side closes. The
                                // sender closes in two cases: `finish()`
                                // replaces `lease_tx` with a closed channel
                                // and drops the real one once every lease it
                                // already owns has been forwarded, and
                                // `pump_binding` stops calling `try_send` on
                                // this binding once `wedged` (stored above)
                                // makes a later `admit`/`pump_binding` call
                                // observe `binding.failed` — so no new lease
                                // is offered into this channel after that
                                // point. Every packet of every lease
                                // received here is counted as dropped and
                                // the lease is dropped at once, releasing
                                // its reservation, so no lease that ever
                                // entered this channel is lost uncounted.
                                // The sink is already gone (or permanently
                                // wedged for a `DropCounted` binding, which
                                // never otherwise reaches `Abandon`), so
                                // none of these packets could have been
                                // delivered anyway: counting them here
                                // cannot race a successful send.
                                while let Ok(lease) = lease_rx.recv() {
                                    let lost =
                                        u64::try_from(lease.packets().len()).unwrap_or(u64::MAX);
                                    counter.fetch_add(lost, Ordering::AcqRel);
                                    drop(lease);
                                }
                            }
                            // Bounded/Push: unchanged. Their failure is
                            // already surfaced via `binding.failed`
                            // (Bounded) or the link status's own counters
                            // (Push), and nothing in this file infers a
                            // `DropCounted`-style packet-loss count for
                            // them, so draining further would not add any
                            // accounting value; dropping `lease_rx` now
                            // matches their existing, already-reviewed
                            // semantics.
                            return;
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
                #[cfg(test)]
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
                drop_counted_flushed_seen: 0,
            });
            receivers.push(packet_rx);
        }
        (Self { bindings }, receivers)
    }

    #[cfg(test)]
    /// Test-only legacy staging entry. Production admits only fully assembled
    /// windows through [`WindowDelivery::offer_window`].
    /// Stage one emitted packet group (a tick's video, program audio or discrete
    /// tracks) for every live binding. Cloning an [`EncodedPacket`] is a
    /// ref-counted copy of the coded payload, exactly as the previous fan-out.
    ///
    /// A push binding whose producer-side gate is closed until a post-loss
    /// keyframe simply does not stage the packet — its link status already
    /// records that gate state.
    pub(crate) fn stage(&mut self, packets: &[EncodedPacket]) {
        if packets.is_empty() {
            return;
        }
        for binding in &mut self.bindings {
            if binding.failed {
                continue;
            }
            for packet in packets {
                let video_keyframe =
                    matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe();
                if let BindingPolicy::Push(link) = &binding.policy {
                    if !link.admit_upstream(video_keyframe) {
                        continue;
                    }
                }
                // Shared-ref copy: independent AVPacket metadata, ONE coded payload.
                if let Ok(shared) = packet.clone_shared_ref() {
                    binding.staged.push(shared);
                } else {
                    binding.staged.clear();
                    binding.queue.report_loss(OfferError::ProducerLoss);
                    binding.failed = true;
                    binding.last_rejection = Some(RejectionReason::InvalidWindow);
                    break;
                }
            }
        }
    }

    #[cfg(test)]
    /// Test-only direct tick flush. Production closes complete DTS-watermarked
    /// windows through the dispatcher; this preserves the original queue tests.
    /// Atomically admit the staged batch of the **previous** tick to every
    /// binding. Never waits: the only outcome of a full or stale binding is a
    /// whole-batch loss recorded against that binding.
    ///
    /// This test-only path still calls [`WindowDelivery::offer_window`], so the
    /// queue and generation semantics remain identical to production.
    pub(crate) fn flush_tick(&mut self, now: Instant, media_span: Duration) {
        for index in 0..self.bindings.len() {
            if self.bindings[index].failed {
                self.bindings[index].staged.clear();
                continue;
            }
            if self.bindings[index].staged.is_empty() {
                continue;
            }
            let packets = std::mem::take(&mut self.bindings[index].staged);
            // Same single admission path an external assembler uses; the
            // generation argument is advisory (the queue is authoritative).
            let _ = self.offer_window(index, 0, packets, media_span, now);
        }
    }

    /// Atomically admit one assembled window (the assembler-facing entry
    /// point; [`WindowDelivery::offer_window`] is the same path).
    ///
    /// # Errors
    /// Returns the whole-window rejection: none of its packets were queued.
    pub(crate) fn try_offer(
        &mut self,
        window: DeliveryWindow,
        now: Instant,
    ) -> Result<Admission, WindowRejection> {
        let DeliveryWindow {
            output_index,
            packets,
            media_span,
        } = window;
        if packets.is_empty() {
            return Ok(Admission {
                output_index,
                generation: self.generation(output_index),
            });
        }
        if self
            .bindings
            .get(output_index)
            .is_some_and(|binding| binding.failed)
        {
            return Err(WindowRejection {
                output_index,
                generation: self.generation(output_index),
                reason: RejectionReason::SinkWedged,
                packets: packets.len(),
                bytes: packets.iter().map(EncodedPacket::len).sum(),
            });
        }
        self.admit(output_index, packets, media_span, now)
    }

    /// The single admission path for one binding's complete window.
    ///
    /// Whole-window atomic: the carrier validates and admits all streams
    /// together, or rejects the entire window. Never waits.
    fn admit(
        &mut self,
        index: usize,
        staged: Vec<EncodedPacket>,
        media_span: Duration,
        now: Instant,
    ) -> Result<Admission, WindowRejection> {
        let packets = staged.len();
        let bytes = staged.iter().map(EncodedPacket::len).sum::<usize>();
        let keyframes: Vec<bool> = staged
            .iter()
            .map(|packet| matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe())
            .collect();
        let Some(binding) = self.bindings.get_mut(index) else {
            return Err(WindowRejection {
                output_index: index,
                generation: 0,
                reason: RejectionReason::WrongProgramShape,
                packets,
                bytes,
            });
        };
        match binding.queue.offer_emitted(staged, media_span, now) {
            Ok(()) => {
                binding.admitted_batches = binding.admitted_batches.saturating_add(1);
                binding.admitted_packets = binding
                    .admitted_packets
                    .saturating_add(u64::try_from(packets).unwrap_or(u64::MAX));
                binding.admitted_bytes = binding
                    .admitted_bytes
                    .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
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
                binding.rejected_packets = binding
                    .rejected_packets
                    .saturating_add(u64::try_from(packets).unwrap_or(u64::MAX));
                binding.rejected_bytes = binding
                    .rejected_bytes
                    .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
                binding.last_rejection = Some(reason);
                match &binding.policy {
                    BindingPolicy::Push(link) => {
                        for _ in &keyframes {
                            link.shed_upstream();
                        }
                    }
                    BindingPolicy::DropCounted(counter) => {
                        // The rejected incoming window's own packets never
                        // entered the queue, so `flushed_packets_total` does
                        // not (and must not) include them: count them here.
                        counter.fetch_add(
                            u64::try_from(packets).unwrap_or(u64::MAX),
                            Ordering::AcqRel,
                        );
                        // This same `offer_emitted` call may ALSO have
                        // invalidated (flushed) an already-admitted queued
                        // backlog (OldestAge/Occupied): add that delta too,
                        // exactly once (INV-1, review-lens3).
                    }
                    BindingPolicy::Bounded => {}
                }
                sync_drop_counted_flushed(binding);
                // A wedged pump means this binding is over: report it and stop
                // offering (its queue keeps draining until the lease expires).
                if binding.wedged.load(Ordering::Acquire) {
                    binding.failed = true;
                    binding.last_rejection = Some(RejectionReason::SinkWedged);
                }
                let generation = binding.queue.generation();
                Self::pump_binding(binding, now);
                return Err(WindowRejection {
                    output_index: index,
                    generation,
                    reason,
                    packets,
                    bytes,
                });
            }
        }
        if binding.wedged.load(Ordering::Acquire) {
            binding.failed = true;
            binding.last_rejection = Some(RejectionReason::SinkWedged);
        }
        let generation = binding.queue.generation();
        Self::pump_binding(binding, now);
        Ok(Admission {
            output_index: index,
            generation,
        })
    }

    /// Hand at most one pending lease and then every immediately available
    /// queued window to the binding's pump without waiting. A full lease
    /// handoff leaves the window queued (and age-bounded) rather than blocking.
    fn pump_binding(binding: &mut Binding, now: Instant) {
        if binding.failed {
            // The pump is gone (abandoned or disconnected): every window
            // popped here is discarded without ever reaching the sink. Count
            // each one exactly once at this real discard boundary.
            while let Some(lease) = binding.queue.pop_at(now).ok().flatten() {
                count_discarded_lease(&binding.policy, &lease);
            }
            sync_drop_counted_flushed(binding);
            return;
        }
        if let Some(lease) = binding.pending.take() {
            match binding.lease_tx.try_send(lease) {
                Ok(()) => {}
                Err(TrySendError::Full(lease)) => {
                    binding.pending = Some(lease);
                    return;
                }
                Err(TrySendError::Disconnected(lease)) => {
                    // The pump thread exited without this lease: it never
                    // reached the sink, so it is a real, uncounted loss.
                    count_discarded_lease(&binding.policy, &lease);
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
                    Err(TrySendError::Disconnected(lease)) => {
                        count_discarded_lease(&binding.policy, &lease);
                        binding.failed = true;
                        return;
                    }
                },
                Ok(None) => return,
                Err(error) => {
                    // A stale queued window invalidated the backlog: media
                    // this binding had admitted is lost. `pop_at`'s
                    // `invalidate` call already flushed the exact packet
                    // count into `flushed_packets_total`; fold the delta in
                    // (INV-1, review-lens3), not a fixed per-event count of 1.
                    binding.last_rejection = Some(RejectionReason::from_offer(error));
                    sync_drop_counted_flushed(binding);
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
    pub(crate) fn finish(&mut self) {
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
        for (index, binding) in self.bindings.iter_mut().enumerate() {
            // Anything still queued here was never handed to the pump within
            // the trailer deadline (or the binding had already failed and
            // `pump_binding`'s failed branch already drained it, so this is
            // a no-op). It will never reach the sink: count it now, at this
            // real discard boundary, before it is dropped and its
            // reservation silently released.
            while let Some(lease) = binding.queue.pop_at(Instant::now()).ok().flatten() {
                count_discarded_lease(&binding.policy, &lease);
            }
            // Fold in any flush `invalidate` performed above (a stale
            // `pop_at` during this drain) exactly once.
            sync_drop_counted_flushed(binding);
            // A lease already popped but not yet handed to the pump (its
            // handoff stayed full past the trailer deadline) also never
            // reached the sink.
            if let Some(lease) = binding.pending.take() {
                count_discarded_lease(&binding.policy, &lease);
                binding.rejected_batches = binding.rejected_batches.saturating_add(1);
                binding.last_rejection = Some(RejectionReason::Occupied);
                tracing::warn!(
                    binding = index,
                    "a pending lease never reached the sink before teardown (counted as a loss)"
                );
            }
            // Replace the sender with a closed one so the pump's `recv()` ends
            // once it has forwarded every lease it already owns.
            let (closed, _) = mpsc::sync_channel::<BatchLease>(0);
            let sender = std::mem::replace(&mut binding.lease_tx, closed);
            drop(sender);
            if let Some(pump) = binding.pump.take() {
                let _ = pump.join();
            }
        }
        // Truthful teardown: a residual reservation here is never a loss by
        // itself — it covers a whole lease that a delivered `LeasedPacket`
        // the sink still holds keeps alive (INV-1, review-lens3 round 2).
        // Never infer a drop from occupancy; only log it for diagnosis.
        for (index, binding) in self.bindings.iter().enumerate() {
            if !binding.queue.is_drained() {
                let (packets, bytes) = binding.queue.reserved();
                tracing::warn!(
                    binding = index,
                    packets,
                    bytes,
                    "binding still holds a reservation at close (the sink may still be \
                     retaining delivered packets; not counted as a loss)"
                );
            }
        }
    }

    /// A status snapshot per binding, in configured output order.
    #[must_use]
    pub(crate) fn statuses(&self) -> Vec<BindingStatus> {
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
}

/// What a binding pump does after handing one lease to its sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpStep {
    /// Take the next lease.
    Continue,
    /// The binding is over (see the caller).
    Abandon,
}

/// Hand every packet of one lease to the sink. With a `drop_counter` (a
/// [`BindingPolicy::DropCounted`] binding) a sink that does not take a packet
/// within the grace loses the rest of the lease, counted, and while it stays
/// stalled (`stalled`) later packets are dropped without waiting at all.
fn pump_lease(
    tx: &SyncSender<LeasedPacket>,
    permit: &Arc<BatchLease>,
    drop_counter: Option<&AtomicU64>,
    stalled: &mut bool,
) -> PumpStep {
    let packets = permit.packets();
    for (index, packet) in packets.iter().enumerate() {
        let Ok(shared) = packet.clone_shared_ref() else {
            // The remainder of this lease, starting at this packet, never
            // reaches the sink (real discard boundary — count it, never the
            // reservation it still holds).
            if let Some(counter) = drop_counter {
                let lost = packets.len().saturating_sub(index);
                counter.fetch_add(u64::try_from(lost).unwrap_or(u64::MAX), Ordering::AcqRel);
            }
            return PumpStep::Abandon;
        };
        let leased = LeasedPacket {
            packet: shared,
            permit: Arc::clone(permit),
        };
        let Some(counter) = drop_counter else {
            if !send_bounded_packet(tx, leased) {
                return PumpStep::Abandon;
            }
            continue;
        };
        let grace = if *stalled {
            Duration::ZERO
        } else {
            PUMP_WEDGE_GRACE
        };
        match send_within(tx, leased, grace) {
            Handoff::Sent => *stalled = false,
            Handoff::Gone => {
                // The sink hung up with this packet (and every later one in
                // the lease) never handed off: count the real remainder.
                let lost = packets.len().saturating_sub(index);
                counter.fetch_add(u64::try_from(lost).unwrap_or(u64::MAX), Ordering::AcqRel);
                return PumpStep::Abandon;
            }
            Handoff::Full => {
                *stalled = true;
                let lost = packets.len().saturating_sub(index);
                counter.fetch_add(u64::try_from(lost).unwrap_or(u64::MAX), Ordering::AcqRel);
                return PumpStep::Continue;
            }
        }
    }
    PumpStep::Continue
}

/// The outcome of offering one packet to a sink's handoff.
enum Handoff {
    /// The sink has (or will take) the packet.
    Sent,
    /// The handoff stayed full for the whole grace.
    Full,
    /// The sink hung up.
    Gone,
}

/// Offer one packet to a sink, waiting at most `grace` for a slot.
fn send_within(
    tx: &SyncSender<LeasedPacket>,
    mut packet: LeasedPacket,
    grace: Duration,
) -> Handoff {
    let deadline = Instant::now() + grace;
    loop {
        match tx.try_send(packet) {
            Ok(()) => return Handoff::Sent,
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return Handoff::Full;
                }
                packet = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return Handoff::Gone,
        }
    }
}

/// Send one packet to a sink, waiting at most [`PUMP_WEDGE_GRACE`] for a slot.
/// Returns `false` when the sink hung up or stopped draining for the whole
/// grace. This runs on the binding's own pump thread, so it can never pace the
/// bake consumer or another binding.
fn send_bounded_packet(tx: &SyncSender<LeasedPacket>, packet: LeasedPacket) -> bool {
    matches!(send_within(tx, packet, PUMP_WEDGE_GRACE), Handoff::Sent)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{
        tick_span, BindingDelivery, BindingPolicy, DeliveryLimits, DeliveryWindow, RejectionReason,
        WindowDelivery, HANDOFF_LEASES, PUMP_WEDGE_GRACE,
    };
    use multiview_core::time::Rational;
    use multiview_ffmpeg::{CodecKind, EncodedPacket, NalFraming};
    use multiview_output::batch::{OfferError, RecoveryGate};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::thread;
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

    fn gate() -> RecoveryGate {
        RecoveryGate::StrictRap {
            codec: CodecKind::H264,
            framing: NalFraming::AnnexB,
        }
    }

    fn limits(max_packets: usize) -> DeliveryLimits {
        DeliveryLimits {
            max_packets,
            max_bytes: 16 * 1024 * 1024,
            max_age: Duration::from_secs(2),
        }
    }

    fn drain(rx: &Receiver<super::LeasedPacket>) -> Vec<EncodedPacket> {
        let mut out = Vec::new();
        while let Ok(packet) = rx.recv() {
            out.push(packet.packet);
            drop(packet.permit);
        }
        out
    }

    /// ADR-0111 (invariants #1/#10): a `DropCounted` binding whose sink
    /// stalls is never abandoned and never grows. Every packet the sink
    /// did not receive is counted; the retained packets never exceed the
    /// queue cap; when the sink takes packets again, delivery resumes at a
    /// keyframe.
    #[test]
    fn a_drop_counted_binding_survives_a_stalled_sink_and_stays_at_its_cap() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        const CAP: usize = 8;
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            limits(CAP),
            vec![Some(gate())],
        );
        let rx = receivers.pop().unwrap();
        let start = Instant::now();
        // The sink takes nothing for 3 s (longer than the pump's grace)
        // while one window per 40 ms is offered, an IDR every 25th.
        let mut offered = 0_u64;
        let mut peak = 0;
        let offer = |delivery: &mut BindingDelivery, index: u64, now: Instant| {
            let nal = if index % 25 == 0 { 5 } else { 1 };
            delivery.stage(&[h264_packet(nal)]);
            delivery.flush_tick(now, Duration::from_millis(40));
        };
        while start.elapsed() < Duration::from_secs(3) {
            let began = Instant::now();
            offer(&mut delivery, offered, began);
            offered += 1;
            // The consumer side never waits on the stalled sink.
            assert!(
                began.elapsed() < Duration::from_millis(100),
                "{:?}",
                began.elapsed()
            );
            let status = delivery.statuses()[0];
            peak = peak.max(status.reserved_packets);
            assert!(status.reserved_packets <= CAP, "{status:?}");
            assert!(!status.failed, "a stalled cast sink is never abandoned");
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(peak > 0 && peak <= CAP, "{peak}");
        let lost = dropped.load(Ordering::Acquire);
        assert!(lost > 0, "the stalled sink's packets are counted");
        // The sink comes back: it drains what was handed off, and new
        // windows flow again from the next IDR.
        let mut received = 0_u64;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut idr_after_stall = false;
        while Instant::now() < deadline && !idr_after_stall {
            offer(&mut delivery, offered, Instant::now());
            offered += 1;
            while let Ok(leased) = rx.recv_timeout(Duration::from_millis(40)) {
                received += 1;
                let payload = leased.packet.payload().unwrap();
                if received > 2 && payload[4] & 0x1F == 5 {
                    idr_after_stall = true;
                }
            }
        }
        assert!(
            idr_after_stall,
            "delivery resumed at a keyframe after the stall"
        );
        let status = delivery.statuses()[0];
        assert!(
            !status.failed && status.reserved_packets <= CAP,
            "{status:?}"
        );
        // Drain whatever remains while `finish` hands off the rest of the
        // backlog, then STRENGTHENED (review-lens3 INV-1; was a `+ CAP`
        // allowance that could hide a real missing packet inside the cap):
        // every offered packet is received, counted as dropped, or both
        // never happens — received + dropped must equal offered exactly.
        let collector = thread::spawn(move || {
            let mut tail = 0_u64;
            while rx.recv().is_ok() {
                tail += 1;
            }
            tail
        });
        delivery.finish();
        received += collector.join().expect("collector");
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(
            received + lost,
            offered,
            "received {received} + dropped {lost} must equal offered {offered} exactly"
        );
        assert!(received < offered);
    }

    #[test]
    fn last_packet_waiting_in_receiver_keeps_batch_reservation_until_mux_ack() {
        let now = Instant::now();
        let (mut delivery, receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded],
            DeliveryLimits::default(),
            vec![Some(gate())],
        );
        let mut receivers = receivers;
        let rx = receivers.pop().unwrap();
        delivery.stage(&[packet(32)]);
        delivery.flush_tick(now, Duration::from_millis(33));
        let packet = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        // This packet is in the actual sink's hands but no mux write has yet
        // completed. Its reservation must remain charged until the sink acks.
        assert_eq!(delivery.statuses()[0].reserved_packets, 1);
        drop(packet);
        assert_eq!(delivery.statuses()[0].reserved_packets, 0);
    }

    #[test]
    fn blocked_last_mux_write_keeps_its_budget_without_stalling_healthy_peer() {
        let now = Instant::now();
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded, BindingPolicy::Bounded],
            DeliveryLimits::default(),
            vec![Some(gate()), Some(gate())],
        );
        let healthy = receivers.pop().unwrap();
        let slow = receivers.pop().unwrap();
        delivery.stage(&[h264_packet(5)]);
        delivery.flush_tick(now, Duration::from_millis(33));
        let held = slow.recv_timeout(Duration::from_secs(1)).unwrap();
        let ready = healthy.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(delivery.statuses()[0].reserved_packets, 1);
        assert_eq!(delivery.statuses()[1].reserved_packets, 1);
        drop(ready); // healthy mux write acknowledged
                     // The pump may still hold its local Arc briefly after sending. Wait
                     // only for that finite handoff to finish; do not assert scheduler speed.
        let deadline = Instant::now() + Duration::from_secs(1);
        while delivery.statuses()[1].reserved_packets != 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(delivery.statuses()[1].reserved_packets, 0);
        assert_eq!(delivery.statuses()[0].reserved_packets, 1);
        delivery.stage(&[packet(4)]);
        delivery.flush_tick(now, Duration::from_millis(33));
        assert_eq!(delivery.statuses()[1].admitted_batches, 2);
        assert_eq!(delivery.statuses()[0].reserved_packets, 2);
        drop(held); // slow mux completes; later packet remains outstanding
        assert_eq!(delivery.statuses()[0].reserved_packets, 1);
        drop(healthy);
        drop(slow);
        delivery.finish();
    }

    #[test]
    fn tick_span_is_exact_for_ntsc_cadence() {
        // 1001/30000 s = 33_366_666.66… ns → floor 33_366_666 ns.
        assert_eq!(
            tick_span(Rational::new(30_000, 1001)),
            Duration::from_nanos(33_366_666)
        );
        assert_eq!(tick_span(Rational::new(25, 1)), Duration::from_millis(40));
        assert_eq!(tick_span(Rational::new(0, 1)), Duration::ZERO);
    }

    #[test]
    fn one_tick_is_admitted_atomically_and_reaches_its_sink() {
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded],
            DeliveryLimits::default(),
            vec![None],
        );
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
            BindingDelivery::new(vec![BindingPolicy::Bounded], limits(4), vec![None]);
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
            vec![None, None],
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
        assert!(
            statuses[0].failed,
            "the dead binding must fail independently"
        );
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
            vec![None],
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
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded],
            DeliveryLimits::default(),
            vec![Some(gate())],
        );
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
            BindingDelivery::new(vec![BindingPolicy::Bounded], limits(1), vec![Some(gate())]);
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

    /// Offer multi-packet windows to a `DropCounted` binding whose sink never
    /// drains, long enough for a real queued backlog to exist (the pump's
    /// in-flight pipeline — one held lease plus the bounded lease/packet
    /// handoffs — fills up and further admitted windows pile up in the
    /// queue itself), then return however many packets were offered.
    fn offer_until_backlog_builds(
        delivery: &mut BindingDelivery,
        deadline: Instant,
        packets_per_window: usize,
    ) -> u64 {
        let span = Duration::from_millis(5);
        let mut offered = 0_u64;
        let mut index = 0_u64;
        while Instant::now() < deadline {
            let mut packets: Vec<EncodedPacket> = Vec::with_capacity(packets_per_window);
            for slot in 0..packets_per_window {
                // An IDR opens every window so a strict-RAP gate (armed by a
                // prior loss) never blocks the next admission.
                packets.push(if slot == 0 { h264_packet(5) } else { packet(2) });
            }
            let now = Instant::now();
            let _ = delivery.try_offer(
                DeliveryWindow {
                    output_index: 0,
                    packets,
                    media_span: span,
                },
                now,
            );
            offered += u64::try_from(packets_per_window).unwrap_or(u64::MAX);
            index += 1;
            if index % 8 == 0 {
                thread::sleep(Duration::from_millis(5));
            }
        }
        offered
    }

    /// INV-1 (review-lens3): offer multi-packet windows to a `DropCounted`
    /// binding with a tiny packet cap and a sink that never drains, so a
    /// real backlog of already-admitted queued windows piles up and later
    /// `Occupied` admission failures flush it. Drain the sink and `finish`,
    /// then assert EXACT conservation: every offered packet was received by
    /// the sink, counted as dropped, or both never happens.
    #[test]
    fn drop_counted_occupied_with_backlog_conserves_every_packet_exactly() {
        const CAP: usize = 6;
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            limits(CAP),
            vec![Some(gate())],
        );
        let rx = receivers.pop().unwrap();
        let offered = offer_until_backlog_builds(
            &mut delivery,
            Instant::now() + Duration::from_millis(600),
            3,
        );
        assert!(
            delivery.statuses()[0].queued_windows > 0
                || dropped.load(Ordering::Acquire) > 0
                || delivery.statuses()[0].reserved_packets > 0,
            "the never-draining sink must have produced backlog, drops, or both"
        );
        let collector = thread::spawn(move || {
            let mut received = 0_u64;
            while rx.recv().is_ok() {
                received += 1;
            }
            received
        });
        delivery.finish();
        let received = collector.join().expect("collector");
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(
            received + lost,
            offered,
            "received {received} + dropped {lost} must equal offered {offered} exactly"
        );
    }

    /// INV-1 (review-lens3): a tiny `max_age` with a never-draining sink ages
    /// out queued backlog windows (both when the next offer discovers the
    /// stale entry, and when `finish`'s own drain loop calls `pop_at` at a
    /// later `now` with no new offer in between). Drain and `finish`, then
    /// assert EXACT conservation.
    #[test]
    fn drop_counted_oldest_age_with_backlog_conserves_every_packet_exactly() {
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            DeliveryLimits {
                max_packets: 64,
                max_bytes: 1024 * 1024,
                max_age: Duration::from_millis(15),
            },
            vec![Some(gate())],
        );
        let rx = receivers.pop().unwrap();
        let offered = offer_until_backlog_builds(
            &mut delivery,
            Instant::now() + Duration::from_millis(600),
            2,
        );
        let collector = thread::spawn(move || {
            let mut received = 0_u64;
            while rx.recv().is_ok() {
                received += 1;
            }
            received
        });
        delivery.finish();
        let received = collector.join().expect("collector");
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(
            received + lost,
            offered,
            "received {received} + dropped {lost} must equal offered {offered} exactly"
        );
    }

    /// INV-1 (review-lens3): build a real queued backlog on a `DropCounted`
    /// binding with a never-draining sink, then declare an explicit
    /// `report_loss` (the assembler's "I discarded real media before
    /// admission" path), which must flush and count that backlog. Drain and
    /// `finish`, then assert EXACT conservation.
    #[test]
    fn drop_counted_report_loss_with_backlog_conserves_every_packet_exactly() {
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            limits(64),
            vec![Some(gate())],
        );
        let rx = receivers.pop().unwrap();
        let mut offered = offer_until_backlog_builds(
            &mut delivery,
            Instant::now() + Duration::from_millis(400),
            2,
        );
        // The assembler now discards a real emission before admission: it
        // must declare the loss explicitly. This flushes and must count the
        // entire queued backlog that existed at that moment, exactly.
        WindowDelivery::report_loss(&mut delivery, 0, OfferError::ProducerLoss);
        // Keep offering a little more (now past the loss, requiring a fresh
        // RAP, which `offer_until_backlog_builds` always supplies) so the
        // conservation check also covers packets offered after the loss.
        offered += offer_until_backlog_builds(
            &mut delivery,
            Instant::now() + Duration::from_millis(200),
            2,
        );
        let collector = thread::spawn(move || {
            let mut received = 0_u64;
            while rx.recv().is_ok() {
                received += 1;
            }
            received
        });
        delivery.finish();
        let received = collector.join().expect("collector");
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(
            received + lost,
            offered,
            "received {received} + dropped {lost} must equal offered {offered} exactly"
        );
    }

    /// INV-1 (review-lens3, round 2): a `LeasedPacket` the sink has received
    /// AND retains across `finish()` still holds the whole lease's
    /// reservation (`permit: Arc<BatchLease>`). `finish` must never infer a
    /// drop from that reservation: a delivered packet is never a loss, no
    /// matter how long the sink keeps its `Arc` alive.
    #[test]
    fn a_retained_delivered_packet_is_never_double_counted_as_dropped_at_finish() {
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            limits(64),
            vec![Some(gate())],
        );
        let rx = receivers.pop().unwrap();
        delivery.stage(&[h264_packet(5)]);
        delivery.flush_tick(Instant::now(), Duration::from_millis(33));
        // The sink receives the one packet and keeps the `LeasedPacket`
        // (and its reservation-holding permit) alive across `finish()`,
        // exactly like a real mux write still in flight.
        let retained = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        delivery.finish();
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(lost, 0, "a delivered, still-retained packet is not a drop");
        drop(retained);
        assert_eq!(
            dropped.load(Ordering::Acquire),
            0,
            "dropping the retained packet after finish must not count it either"
        );
    }

    /// INV-1 (review-lens3, round 2): a three-packet lease where the first
    /// packet is received and retained by the sink, the second occupies the
    /// one-slot handoff (so it is also delivered once the sink drains it),
    /// and the third times out past the pump's wedge grace (counted once by
    /// `pump_lease`). After `finish()`, `received + dropped` must equal
    /// `offered` exactly: no double count from the lease's residual
    /// reservation (the first packet's `Arc` keeps it alive) and no missed
    /// count either.
    #[test]
    fn a_partially_delivered_lease_conserves_packets_exactly_at_finish() {
        const CAP: usize = 64;
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            limits(CAP),
            vec![Some(gate())],
        );
        let rx = receivers.pop().unwrap();
        // One window carrying three packets, keyed so the strict RAP gate
        // admits it. The handoff is one packet deep (`HANDOFF_PACKETS`), so
        // the pump sends packet 1 immediately, then blocks offering packet 2
        // until packet 1 is taken.
        delivery.stage(&[h264_packet(5), packet(2), packet(2)]);
        delivery.flush_tick(Instant::now(), Duration::from_millis(33));
        let offered = 3_u64;
        // Receive and retain the first packet. This frees the handoff slot,
        // so the pump immediately sends packet 2 into it — buffered there,
        // not yet taken by anything.
        let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        // The pump now offers packet 3 into the still-full handoff (packet 2
        // sits there unconsumed) and times out after `PUMP_WEDGE_GRACE`,
        // counting exactly packet 3 as lost.
        let deadline = Instant::now() + PUMP_WEDGE_GRACE + Duration::from_secs(1);
        while dropped.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            dropped.load(Ordering::Acquire),
            1,
            "only the un-handed-off third packet is counted as lost"
        );
        // Now take the second packet out of the handoff: it was buffered,
        // not lost, and retain it too.
        let second = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        delivery.finish();
        // Both delivered packets stay retained by "the sink" through finish
        // and beyond: their reservations must never be read as drops.
        let received = 2_u64;
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(
            received + lost,
            offered,
            "received {received} + dropped {lost} must equal offered {offered} exactly"
        );
        drop(first);
        drop(second);
        assert_eq!(
            dropped.load(Ordering::Acquire),
            lost,
            "releasing retained packets after finish must not add further drops"
        );
    }

    /// INV-1-R3 (review3-lens3-invariants.md): lease A's handoff to the sink
    /// fails (the sink has already disconnected) while lease B sits fully
    /// admitted in the bounded `lease_tx`/`lease_rx` channel (`HANDOFF_LEASES`
    /// = 2, so both A and B fit there at once, never touched by the pump
    /// until it is ready for each in turn). `pump_lease` returns `Abandon`
    /// for A via `Handoff::Gone`, counting A's own packet. Before this fix,
    /// the pump's caller then returned immediately, dropping `lease_rx` and
    /// destroying lease B outright (releasing its reservation) without ever
    /// counting its packets — a silent loss of a whole lease already
    /// accepted into the channel. After the fix, the pump keeps draining
    /// `lease_rx` until `finish()` closes the sender, counting every packet
    /// of lease B as dropped too.
    ///
    /// The sink's receiver is dropped *before* either lease is admitted, so
    /// every `try_send`/`send_within` call the pump makes is deterministically
    /// `Disconnected` from the first attempt: nothing is ever buffered
    /// ambiguously in the one-packet handoff, so this test needs no timing
    /// window or retry/backoff loop to be exact.
    ///
    /// After `finish()`, `received + dropped` must equal `offered` exactly.
    #[test]
    fn a_lease_queued_behind_an_abandoned_lease_is_counted_not_lost() {
        let dropped = Arc::new(AtomicU64::new(0));
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::DropCounted(Arc::clone(&dropped))],
            limits(64),
            vec![Some(gate())],
        );
        // Drop the sink's packet receiver immediately: every later handoff
        // attempt from the pump is `Disconnected` from its very first
        // `try_send`, so nothing can ever land in the capacity-1 handoff
        // buffer and this test has no race window at all.
        drop(receivers.pop().unwrap());
        let span = Duration::from_millis(33);

        // Lease A: a single-packet window, admitted first.
        delivery
            .try_offer(
                DeliveryWindow {
                    output_index: 0,
                    packets: vec![h264_packet(5)],
                    media_span: span,
                },
                Instant::now(),
            )
            .expect("lease A is admitted");
        // Lease B: a two-packet window, admitted second. `admit`'s own
        // `pump_binding` call hands each lease straight into `lease_tx`
        // (capacity `HANDOFF_LEASES` = 2) rather than leaving it queued, so
        // both A and B are owned by the lease channel before the pump
        // thread has necessarily processed either.
        delivery
            .try_offer(
                DeliveryWindow {
                    output_index: 0,
                    packets: vec![packet(2), packet(2)],
                    media_span: span,
                },
                Instant::now(),
            )
            .expect("lease B is admitted");
        let offered = 3_u64; // 1 (lease A) + 2 (lease B)

        // `finish()` replaces `lease_tx` with a closed sender and joins the
        // pump thread, which blocks until the pump has drained `lease_rx`
        // down to disconnection — i.e. until it has processed both A
        // (Abandon via `Handoff::Gone`) and, with the fix, B as well. No
        // polling is needed: by the time `finish()` returns, the pump
        // thread has necessarily finished.
        delivery.finish();
        let received = 0_u64; // the sink's receiver was never alive to take anything
        let lost = dropped.load(Ordering::Acquire);
        assert_eq!(
            received + lost,
            offered,
            "received {received} + dropped {lost} must equal offered {offered} exactly \
             (lease B must not vanish uncounted behind abandoned lease A)"
        );
        assert_eq!(
            lost, offered,
            "every packet of both leases must be counted as dropped"
        );
    }
}
