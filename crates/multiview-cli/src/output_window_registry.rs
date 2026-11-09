//! Effective-output subscriptions over already-encoded whole AV windows.
//!
//! The dispatcher is the sole queue owner. Control-plane subscribe/revoke
//! commands cross a bounded channel; production never locks or waits for a peer.
//! A slow subscriber loses its own entire window without affecting another
//! subscriber or a physical SRT/HLS binding. Codec metadata is supplied from
//! opened encoders, never inferred from desired output configuration.
//!
//! Every subscription retains the *actual* transport ownership: the window it
//! receives is an [`Arc<BatchLease>`] over its own bounded queue, whose packets
//! are `av_packet_ref` copies that share one coded payload allocation with the
//! encoder emission. Cloning a window for a converter adds references, never
//! media, and the last drop releases the referenced bytes.
//!
//! Cancellation has exactly one writer on each side. This registry owns
//! [`SubscriptionRevokeHandle`]; the external transport owns its own fence. Each
//! side observes the other's monotonic flag read-only — writing the peer's flag
//! would strand that fence's own waiters, so no code here ever stores into the
//! external token.
#![allow(dead_code)] // reason: published registry + tap seam; the reviewed wiring is the single `WindowDelivery` wrap at the egress construction point (output-owner change in this series).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use multiview_audio::format::AudioFormat;
use multiview_control::WhepScope;
use multiview_core::time::Rational;
use multiview_ffmpeg::{CodecKind, EncodedPacket, NalFraming, StreamCodecParameters, StreamKind};
use multiview_framestore::SwapCell;
use multiview_output::batch::{BatchLease, BoundedBatchQueue, OfferError, RecoveryGate};

use crate::binding_delivery::WindowDelivery;

/// Maximum independent subscriptions to one producer. No thread per peer.
const MAX_PEERS: usize = 16;
/// Command depth limits unprocessed attach requests from the control plane.
const COMMAND_DEPTH: usize = 32;
/// Whole admitted leases awaiting the transport (each retains its reservation).
const WINDOW_HANDOFF: usize = 2;

/// Opened video codec data for an effective rendition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VideoDescriptor {
    /// Converter-facing identifier: the RFC 6381 token when the opened
    /// parameters identify one, else the exact libav identity.
    pub(crate) codec: String,
    /// Exact opened libav codec identity — never a requested token or a guessed
    /// profile.
    pub(crate) codec_id: String,
    pub(crate) coded_width: u32,
    pub(crate) coded_height: u32,
    /// Opened encoder's exact codec configuration record (avcC/hvcC/…), if it
    /// supplied one.
    pub(crate) config: Option<Vec<u8>>,
    pub(crate) time_base: Rational,
    /// Actual output clock cadence, independent of the packet timestamp scale.
    pub(crate) cadence: Rational,
    /// Opened codec bitrate; unknown stays absent.
    pub(crate) bitrate_bps: Option<u64>,
    /// Recovery policy proven from the opened codec record. `None` means this
    /// codec has no verifiable random-access point, so the queue fails closed
    /// after a loss instead of trusting a KEY flag.
    pub(crate) recovery: Option<RecoveryGate>,
    /// Coded framing the packets of this rendition use.
    pub(crate) framing: NalFraming,
    /// Opened encoder's verified no-reorder proof. `MoQ` PTS-only mapping
    /// rejects `Unknown`; this is not a planned codec-option guess.
    pub(crate) reorder: crate::rendition_workers::ReorderBound,
}
impl VideoDescriptor {
    /// Descriptor from the **opened** encoder snapshot. The recovery gate is
    /// derived from the opened codec configuration record, never from the
    /// configured codec name, and is `None` when no framing proof exists.
    pub(crate) fn from_opened(
        params: &StreamCodecParameters,
        time_base: Rational,
        reorder: crate::rendition_workers::ReorderBound,
        cadence: Rational,
    ) -> Option<Self> {
        let (coded_width, coded_height) = params.video_dimensions()?;
        let framing = params.nal_framing();
        let recovery = match (params.codec_id(), framing) {
            (ffmpeg::codec::Id::H264, Some(framing)) => Some(RecoveryGate::StrictRap {
                codec: CodecKind::H264,
                framing,
            }),
            (ffmpeg::codec::Id::HEVC, Some(framing)) => Some(RecoveryGate::StrictRap {
                codec: CodecKind::Hevc,
                framing,
            }),
            _ => None,
        };
        let identity = format!("{:?}", params.codec_id());
        Some(Self {
            codec: params
                .hls_codec()
                .map_or_else(|| identity.clone(), str::to_owned),
            codec_id: identity,
            coded_width,
            coded_height,
            config: params.codec_config_bytes(),
            time_base,
            cadence,
            bitrate_bps: params.opened_bit_rate(),
            recovery,
            // Codecs with no NAL record emit their raw elementary stream; that is
            // the Annex-B framing the encoders produce, not a guess about a RAP.
            framing: framing.unwrap_or(NalFraming::AnnexB),
            reorder,
        })
    }
}
/// Opened audio stream data in stable stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioDescriptor {
    pub(crate) track: String,
    pub(crate) kind: StreamKind,
    pub(crate) codec: String,
    pub(crate) bitrate_bps: Option<u64>,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u32,
    pub(crate) config: Option<Vec<u8>>,
    pub(crate) time_base: Rational,
}
impl AudioDescriptor {
    /// Descriptor from the **opened** audio encoder snapshot plus the running
    /// program-bus format the encoder is fed from — never a requested config
    /// value.
    pub(crate) fn from_opened(
        track: &str,
        kind: StreamKind,
        params: &StreamCodecParameters,
        time_base: Rational,
        bus: AudioFormat,
    ) -> Self {
        Self {
            track: track.to_owned(),
            kind,
            codec: params
                .hls_codec()
                .map_or_else(|| format!("{:?}", params.codec_id()), str::to_owned),
            bitrate_bps: params.opened_bit_rate(),
            sample_rate: bus.sample_rate(),
            channels: u32::try_from(bus.channel_count()).unwrap_or(0),
            config: params.codec_config_bytes(),
            time_base,
        }
    }
}
/// Resource identity that an external authorization provider must verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DependencyKind {
    Source,
    Output,
    Program,
    MixEffect,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dependency {
    pub(crate) kind: DependencyKind,
    pub(crate) id: String,
}
/// Immutable effective binding snapshot; no pending desired fields are present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveBinding {
    pub(crate) program_id: String,
    pub(crate) bus_id: String,
    pub(crate) output_id: String,
    pub(crate) output_index: usize,
    pub(crate) config_generation: u64,
    pub(crate) video: VideoDescriptor,
    pub(crate) audio: Vec<AudioDescriptor>,
    pub(crate) dependencies: Vec<Dependency>,
}
impl EffectiveBinding {
    /// Actual managed source IDs referenced by this effective output. Program,
    /// output and mix/effect dependencies remain separately typed for auth.
    pub(crate) fn source_dependencies(&self) -> impl Iterator<Item = &str> {
        self.dependencies
            .iter()
            .filter(|d| d.kind == DependencyKind::Source)
            .map(|d| d.id.as_str())
    }
    /// The opened descriptor selected for one encoder stream kind, if any.
    pub(crate) fn audio_for(&self, kind: StreamKind) -> Option<&AudioDescriptor> {
        self.audio.iter().find(|audio| audio.kind == kind)
    }
}

/// A typed refusal instead of an implicit new encode or a guessed scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeError {
    /// The scope names a resource whose rendition is not admitted right now.
    Unavailable,
    /// Program and input previews have no independent encoded key in this
    /// registry; refusing is the documented alternative to a hidden encoder.
    UnsupportedScope,
    /// The caller resolved a different configuration generation.
    StaleGeneration,
    /// The per-producer peer cap or the attach command depth is exhausted.
    Capacity,
    /// The producer ended; no further window can arrive.
    ProducerEnded,
    /// `av_packet_ref` failed while copying a window for a transport.
    PacketReference,
}
/// One whole lease with the effective output configuration generation.
pub(crate) struct SubscriptionWindow {
    pub(crate) permit: Arc<BatchLease>,
    pub(crate) config_generation: u64,
    /// Producer-measured collection span of this window (≤ 250 ms).
    pub(crate) media_span: Duration,
}
impl SubscriptionWindow {
    /// Copy the whole admitted window into independently writable transport
    /// packets that all retain one shared permit. All-or-nothing: a failed
    /// reference releases the partial copy, never a truncated AV window.
    ///
    /// # Errors
    /// [`SubscribeError::PacketReference`] if any `av_packet_ref` fails.
    pub(crate) fn clone_window(&self) -> Result<Vec<EncodedPacket>, SubscribeError> {
        let packets = self.permit.packets();
        let mut owned = Vec::with_capacity(packets.len());
        for packet in packets {
            owned.push(
                packet
                    .clone_shared_ref()
                    .map_err(|_| SubscribeError::PacketReference)?,
            );
        }
        Ok(owned)
    }
}

/// Read-only observation of the external transport's own cancellation identity.
///
/// The transport publishes revocation through its own fence (which also wakes
/// its waiters). This registry never writes the flag: a bare store would leave
/// those waiters asleep and turn a fence into a silent, half-revoked state.
#[derive(Clone)]
pub(crate) struct ExternalCancellation {
    cancelled: Arc<AtomicBool>,
}
impl ExternalCancellation {
    /// Observe a flag the transport owns and publishes. Never written here.
    pub(crate) fn observe(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }
    /// Monotonic: once the transport has revoked, this stays true.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Cloneable synchronous revoke fence suitable for an external Send+Sync
/// demand guard, without sharing the single-consumer packet receiver.
///
/// `revoke` writes only this registry's own flag (and releases the peer slot
/// exactly once). The external token is a read-only input, so the transport's
/// own cancellation path stays the sole writer of its own fence.
#[derive(Clone)]
pub(crate) struct SubscriptionRevokeHandle {
    own_revoked: Arc<AtomicBool>,
    external: ExternalCancellation,
    released: Arc<AtomicBool>,
    slots: Arc<AtomicUsize>,
}
impl SubscriptionRevokeHandle {
    pub(crate) fn revoke(&self) {
        self.own_revoked.store(true, Ordering::Release);
        if !self.released.swap(true, Ordering::AcqRel) {
            self.slots.fetch_sub(1, Ordering::AcqRel);
        }
    }
    pub(crate) fn is_revoked(&self) -> bool {
        self.own_revoked.load(Ordering::Acquire) || self.external.is_cancelled()
    }
    /// True once this registry stopped its own producer for this subscription.
    pub(crate) fn is_own_revoked(&self) -> bool {
        self.own_revoked.load(Ordering::Acquire)
    }
}

/// Transport-facing whole-window ownership, never individual packet prefixes.
pub(crate) struct Subscription {
    pub(crate) id: u64,
    pub(crate) binding: EffectiveBinding,
    rx: Receiver<SubscriptionWindow>,
    revoke: SubscriptionRevokeHandle,
}
impl Subscription {
    /// Return a complete window without blocking the transport or producer.
    pub(crate) fn try_recv_window(&self) -> Result<Option<SubscriptionWindow>, SubscribeError> {
        match self.rx.try_recv() {
            Ok(lease) => Ok(Some(lease)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(SubscribeError::ProducerEnded),
        }
    }
    /// Synchronous fence: a producer seeing this flag cannot enqueue again.
    pub(crate) fn revoke(&self) {
        self.revoke.revoke();
    }
    pub(crate) fn revoke_handle(&self) -> SubscriptionRevokeHandle {
        self.revoke.clone()
    }
    pub(crate) fn is_revoked(&self) -> bool {
        self.revoke.is_revoked()
    }
    /// The effective output this subscription is bound to.
    pub(crate) fn output_id(&self) -> &str {
        &self.binding.output_id
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.revoke();
    }
}

struct Attach {
    binding: EffectiveBinding,
    tx: SyncSender<SubscriptionWindow>,
    own_revoked: Arc<AtomicBool>,
    external: ExternalCancellation,
}
struct Peer {
    output_index: usize,
    generation: u64,
    queue: BoundedBatchQueue,
    tx: SyncSender<SubscriptionWindow>,
    pending: Option<SubscriptionWindow>,
    own_revoked: Arc<AtomicBool>,
    external: ExternalCancellation,
    rejected: u64,
}
impl Peer {
    /// The single enqueue boundary. Both cancellation identities are re-checked
    /// here, so a revoke that raced with window assembly cannot enqueue.
    fn enqueueable(&self) -> bool {
        !self.own_revoked.load(Ordering::Acquire) && !self.external.is_cancelled()
    }
}
/// Reserve one peer slot against the global cap without an MSRV-newer atomic
/// API. Concurrent subscribers either take distinct slots or fail `Capacity`;
/// the release side is a single saturating-once decrement in the revoke handle.
fn try_reserve_slot(slots: &AtomicUsize) -> Result<(), SubscribeError> {
    let mut current = slots.load(Ordering::Acquire);
    loop {
        if current >= MAX_PEERS {
            return Err(SubscribeError::Capacity);
        }
        match slots.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

/// Off-clock handle for authenticated, effective-output lookup and attachment.
#[derive(Clone)]
pub(crate) struct SubscriptionFactory {
    effective: Arc<Vec<EffectiveBinding>>,
    commands: SyncSender<Attach>,
    next_id: Arc<AtomicU64>,
    slots: Arc<AtomicUsize>,
    generation: u64,
}
impl SubscriptionFactory {
    /// `Program` has no arbitrary first-output fallback; `Input` has no
    /// independent encoded key in this output registry.
    /// Resolve effective immutable codec/dependency metadata BEFORE authorization
    /// without allocating demand or a peer slot.
    pub(crate) fn resolve(&self, scope: &WhepScope) -> Result<EffectiveBinding, SubscribeError> {
        let WhepScope::Output(id) = scope else {
            return Err(SubscribeError::UnsupportedScope);
        };
        self.effective
            .iter()
            .find(|b| b.output_id == *id)
            .cloned()
            .ok_or(SubscribeError::Unavailable)
    }
    pub(crate) fn subscribe(
        &self,
        scope: &WhepScope,
        generation: u64,
    ) -> Result<Subscription, SubscribeError> {
        self.subscribe_with_revocation(scope, generation, Arc::new(AtomicBool::new(false)))
    }
    /// Share the exact revocation identity with the external transport fence.
    /// The supplied flag is observed READ-ONLY: the transport remains the only
    /// writer of its own fence, so its cancellation waiters are always woken by
    /// its own revoke. No second cancellation state can diverge.
    pub(crate) fn subscribe_with_revocation(
        &self,
        scope: &WhepScope,
        generation: u64,
        external_cancelled: Arc<AtomicBool>,
    ) -> Result<Subscription, SubscribeError> {
        let binding = self.resolve(scope)?;
        if binding.video.reorder != crate::rendition_workers::ReorderBound::NoReorder {
            return Err(SubscribeError::Unavailable);
        }
        if generation != binding.config_generation {
            return Err(SubscribeError::StaleGeneration);
        }
        try_reserve_slot(&self.slots)?;
        let external = ExternalCancellation::observe(external_cancelled);
        if external.is_cancelled() {
            self.slots.fetch_sub(1, Ordering::AcqRel);
            return Err(SubscribeError::Unavailable);
        }
        let (tx, rx) = mpsc::sync_channel(WINDOW_HANDOFF);
        let own_revoked = Arc::new(AtomicBool::new(false));
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        if id == u64::MAX {
            self.slots.fetch_sub(1, Ordering::AcqRel);
            return Err(SubscribeError::Capacity);
        }
        self.commands
            .try_send(Attach {
                binding: binding.clone(),
                tx,
                own_revoked: Arc::clone(&own_revoked),
                external: external.clone(),
            })
            .map_err(|err| {
                self.slots.fetch_sub(1, Ordering::AcqRel);
                match err {
                    TrySendError::Full(_) => SubscribeError::Capacity,
                    TrySendError::Disconnected(_) => SubscribeError::ProducerEnded,
                }
            })?;
        Ok(Subscription {
            id,
            binding,
            rx,
            revoke: SubscriptionRevokeHandle {
                own_revoked,
                external,
                released: Arc::new(AtomicBool::new(false)),
                slots: Arc::clone(&self.slots),
            },
        })
    }
    /// Subscriptions holding a peer slot right now (bounded by [`MAX_PEERS`]).
    pub(crate) fn slots_in_use(&self) -> usize {
        self.slots.load(Ordering::Acquire)
    }
    /// The effective-configuration generation this factory serves. A subscriber
    /// that resolved against another generation is refused `StaleGeneration`.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    /// Every admitted effective binding, in binding order — the admitted-only
    /// catalogue, as an immutable snapshot of this factory's single instance.
    /// Internal, never-subscribable outputs (the synthetic `program.ts` anchor)
    /// are absent by construction. Read-only: resolving or listing allocates no
    /// demand and takes no peer slot.
    pub(crate) fn admitted(&self) -> Arc<Vec<EffectiveBinding>> {
        Arc::clone(&self.effective)
    }
}

/// Monotonic per-process effective-configuration generation. Each egress build
/// advances it, so a subscriber (or ABR profile) that resolved against a
/// previous effective configuration is refused by the rebuilt registry rather
/// than silently attaching to a different one.
pub(crate) fn next_run_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::AcqRel)
}

/// Off-clock rendezvous between the bake consumer (publisher) and the control
/// plane (reader, e.g. the MoQ/WHEP startup). The consumer stores this run's
/// factory once, before the first window; the reader loads it lock-free and
/// never blocks the producer (invariants #1/#10).
pub(crate) type WindowSubscriptionSlot = Arc<SwapCell<Option<Arc<SubscriptionFactory>>>>;
#[must_use]
pub(crate) fn window_subscription_slot() -> WindowSubscriptionSlot {
    Arc::new(SwapCell::empty())
}
/// Producer-owned queue registry. Only the off-clock dispatcher calls its methods.
///
/// Retention is bounded globally by construction: at most [`MAX_PEERS`] peers,
/// each with one `BoundedBatchQueue` whose own packet/byte/age caps are the
/// enforced limits. Referenced bytes are charged to that peer's queue and
/// released by the [`BatchLease`] drop, so a stalled transport can only lose its
/// own windows.
pub(crate) struct OutputWindowRegistry {
    effective: Arc<Vec<EffectiveBinding>>,
    commands: Receiver<Attach>,
    peers: Vec<Peer>,
    rejected_windows: u64,
}
impl OutputWindowRegistry {
    /// # Errors
    /// [`SubscribeError::Unavailable`] for an empty or self-inconsistent
    /// effective snapshot (duplicate output id, zero geometry, empty codec).
    /// An **empty** snapshot is valid: a display-only or unconfigured run has no
    /// admitted rendition, so every scope resolves to the typed `Unavailable`
    /// instead of failing the run. Every non-empty binding must carry this
    /// registry's `generation` — a binding resolved against another effective
    /// configuration is refused rather than substituted.
    ///
    /// # Errors
    /// [`SubscribeError::Unavailable`] for a self-inconsistent snapshot
    /// (duplicate output id, zero geometry, empty codec identity, or a binding
    /// whose `config_generation` is not `generation`).
    pub(crate) fn new(
        effective: Vec<EffectiveBinding>,
        generation: u64,
    ) -> Result<(Self, SubscriptionFactory), SubscribeError> {
        let mut ids = std::collections::HashSet::new();
        if generation == 0
            || effective
                .iter()
                .any(|b| !admissible(b, generation, &mut ids))
        {
            return Err(SubscribeError::Unavailable);
        }
        Ok(Self::bare(effective, generation))
    }
    /// Build the registry pair from an already-validated snapshot. Only the
    /// validating constructor and the tap's filtering path call this; an empty
    /// snapshot is valid (a display-only run has no subscribable rendition).
    fn bare(effective: Vec<EffectiveBinding>, generation: u64) -> (Self, SubscriptionFactory) {
        let (tx, commands) = mpsc::sync_channel(COMMAND_DEPTH);
        let effective = Arc::new(effective);
        (
            Self {
                effective: Arc::clone(&effective),
                commands,
                peers: Vec::new(),
                rejected_windows: 0,
            },
            SubscriptionFactory {
                effective,
                commands: tx,
                next_id: Arc::new(AtomicU64::new(1)),
                slots: Arc::new(AtomicUsize::new(0)),
                generation,
            },
        )
    }
    /// Drain bounded commands; called once at each complete encoded window.
    fn reconcile(&mut self) {
        for _ in 0..COMMAND_DEPTH {
            let Ok(attach) = self.commands.try_recv() else {
                break;
            };
            // A binding removed or replaced by a newer effective generation is
            // never admitted; the control plane must re-resolve.
            let still_effective = self
                .effective
                .iter()
                .any(|b| b.output_id == attach.binding.output_id);
            if !still_effective
                || attach.own_revoked.load(Ordering::Acquire)
                || attach.external.is_cancelled()
            {
                continue;
            }
            debug_assert!(self.peers.len() < MAX_PEERS);
            self.peers.push(Peer {
                output_index: attach.binding.output_index,
                generation: attach.binding.config_generation,
                queue: match attach.binding.video.recovery {
                    Some(gate) => BoundedBatchQueue::new().with_recovery_gate(gate),
                    // No verifiable RAP for this codec: the queue fails closed
                    // after a loss instead of trusting a KEY flag.
                    None => BoundedBatchQueue::new(),
                },
                tx: attach.tx,
                pending: None,
                own_revoked: attach.own_revoked,
                external: attach.external,
                rejected: 0,
            });
        }
        self.peers.retain_mut(|peer| {
            if peer.enqueueable() {
                true
            } else {
                // Close the peer's queue and drop its sender: the transport's
                // next `try_recv_window` observes a definite end, and the dropped
                // lease releases exactly the bytes it still owned.
                peer.queue.close();
                false
            }
        });
    }
    /// Offer a fully proven, selected AV window BEFORE physical output admission.
    /// Packets are already shared immutable refs to one rendition allocation;
    /// only the peer's own queue may reject it. Never waits on a peer or a lock.
    ///
    /// `window` is the exact assembled window the dispatcher is about to admit
    /// to the physical binding (video key first, then the binding's selected
    /// audio in stable order). Nothing is re-encoded or re-ordered here.
    pub(crate) fn offer_complete(
        &mut self,
        output_index: usize,
        generation: u64,
        window: &[EncodedPacket],
        media_span: Duration,
        now: Instant,
    ) {
        self.reconcile();
        if !self
            .effective
            .iter()
            .any(|b| b.output_index == output_index && b.config_generation == generation)
        {
            return;
        }
        for peer in &mut self.peers {
            if peer.output_index != output_index
                || peer.generation != generation
                || !peer.enqueueable()
            {
                continue;
            }
            let mut owned = Vec::with_capacity(window.len());
            let mut failed = false;
            for packet in window {
                if let Ok(copy) = packet.clone_shared_ref() {
                    owned.push(copy);
                } else {
                    failed = true;
                    break;
                }
            }
            // Re-check at the enqueue boundary: a revoke that raced with the
            // copy above must not enqueue, and its partial copy is dropped here.
            if failed || !peer.enqueueable() {
                peer.queue.report_loss(OfferError::ProducerLoss);
                peer.rejected = peer.rejected.saturating_add(1);
                self.rejected_windows = self.rejected_windows.saturating_add(1);
                continue;
            }
            if peer.queue.offer_emitted(owned, media_span, now).is_err() {
                peer.rejected = peer.rejected.saturating_add(1);
                self.rejected_windows = self.rejected_windows.saturating_add(1);
            }
        }
        // Hand every already-admitted window to its transport. This is repeated
        // once per output tick by `pump`, so a subscriber whose channel the
        // transport has drained still receives the queued tail after the encoder
        // goes quiet — no window can sit indefinitely in `pending`/queue.
        self.pump(now);
    }

    /// Move already-admitted windows to their transports without waiting on any
    /// peer. Called after every producer window AND once per output tick (from
    /// the tap's `pump`), which is what makes a quiet producer safe: a subscriber
    /// that has drained its channel still gets the admitted tail.
    ///
    /// A full transport channel keeps at most one lease in `pending` (bounded, no
    /// blind growth); the window's own age bound then decides between delivery and
    /// an explicit whole-window loss.
    pub(crate) fn pump(&mut self, now: Instant) {
        for peer in &mut self.peers {
            if !peer.enqueueable() {
                continue;
            }
            if let Some(lease) = peer.pending.take() {
                match peer.tx.try_send(lease) {
                    Ok(()) => {}
                    Err(TrySendError::Full(lease)) => {
                        peer.pending = Some(lease);
                        continue;
                    }
                    Err(TrySendError::Disconnected(_)) => continue,
                }
            }
            if let Ok(Some(lease)) = peer.queue.pop_at(now) {
                let generation = peer.generation;
                let window = SubscriptionWindow {
                    media_span: lease.batch().media_span(),
                    permit: Arc::new(lease),
                    config_generation: generation,
                };
                if let Err(TrySendError::Full(window)) = peer.tx.try_send(window) {
                    peer.pending = Some(window);
                }
            }
        }
    }
    /// Mirror a producer-declared media loss onto the subscribers of one output
    /// so a transport cannot resume on a non-RAP after an invalid window.
    pub(crate) fn report_loss(&mut self, output_index: usize, reason: OfferError) {
        for peer in &mut self.peers {
            if peer.output_index == output_index {
                peer.queue.report_loss(reason);
                peer.rejected = peer.rejected.saturating_add(1);
                self.rejected_windows = self.rejected_windows.saturating_add(1);
            }
        }
    }
    /// Every subscription ended by the registry itself (run finished/aborted).
    pub(crate) fn close(&mut self) {
        for peer in &mut self.peers {
            peer.queue.close();
        }
        self.peers.clear();
    }
    pub(crate) fn rejected_windows(&self) -> u64 {
        self.rejected_windows
    }
    pub(crate) fn active_peers(&self) -> usize {
        self.peers.len()
    }
    /// Total packets and referenced bytes still retained by peers, including
    /// in-flight leases. Goes to zero once every window and lease is dropped.
    pub(crate) fn retained(&self) -> (usize, usize) {
        self.peers.iter().fold((0, 0), |(packets, bytes), peer| {
            let (peer_packets, peer_bytes) = peer.queue.reserved();
            (
                packets.saturating_add(peer_packets),
                bytes.saturating_add(peer_bytes),
            )
        })
    }
}
/// Whether one effective binding may be admitted into a registry that serves
/// `generation`. `ids` accumulates the ids already admitted, so a duplicate is
/// refused instead of shadowing the binding that resolved first.
fn admissible(
    binding: &EffectiveBinding,
    generation: u64,
    ids: &mut std::collections::HashSet<String>,
) -> bool {
    binding.config_generation == generation
        && binding.video.coded_width > 0
        && binding.video.coded_height > 0
        && !binding.video.codec.is_empty()
        && ids.insert(binding.output_id.clone())
}

/// Everything one egress build knows about its effective outputs, in final
/// runner order. Every field is an *opened* runtime snapshot: the encoder's own
/// codec parameters and time base, its verified reorder proof, the running
/// audio-bus format and the declared track identities.
pub(crate) struct OpenedRenditions<'a> {
    /// Configured output id per runner; `None` is an internal sink (the
    /// synthetic `program.ts` anchor) that no scope may resolve to.
    pub(crate) binding_ids: &'a [Option<String>],
    /// The opened video key each runner selects.
    pub(crate) video_keys: &'a [usize],
    /// Opened video encoder snapshots, indexed by video key.
    pub(crate) video: &'a [(StreamCodecParameters, Rational)],
    /// Actual cadence of the clock that feeds these opened encoders.
    pub(crate) cadence: Rational,
    /// Verified no-reorder proof per video key.
    pub(crate) video_reorder: &'a [crate::rendition_workers::ReorderBound],
    /// The opened program-audio encoder, if this run encodes audio.
    pub(crate) audio: Option<&'a (StreamCodecParameters, Rational)>,
    /// Opened discrete track encoders, in stable stream order.
    pub(crate) audio_tracks: &'a [(StreamCodecParameters, Rational)],
    /// The running program-bus PCM format (audio geometry, never requested config).
    pub(crate) bus: Option<AudioFormat>,
    /// Declared discrete track identities, in the same order as `audio_tracks`.
    pub(crate) declared_tracks: &'a [String],
    /// The run's program/source authorization axes.
    pub(crate) dependencies: &'a [Dependency],
    /// The effective-configuration generation of this build.
    pub(crate) generation: u64,
}
impl OpenedRenditions<'_> {
    /// The effective bindings of this build, in runner order. Bindings the
    /// registry cannot admit are omitted entirely: resolution is fail-closed, so
    /// an omitted output is `Unavailable` rather than mis-described.
    pub(crate) fn bindings(&self) -> Vec<EffectiveBinding> {
        let mut bindings = Vec::with_capacity(self.binding_ids.len());
        for (index, id) in self.binding_ids.iter().enumerate() {
            let Some(id) = id.as_ref() else {
                // Internal sink: never subscribable.
                continue;
            };
            let Some(&key) = self.video_keys.get(index) else {
                continue;
            };
            let Some((params, time_base)) = self.video.get(key) else {
                continue;
            };
            let reorder = self
                .video_reorder
                .get(key)
                .copied()
                .unwrap_or(crate::rendition_workers::ReorderBound::Unknown);
            let Some(video) =
                VideoDescriptor::from_opened(params, *time_base, reorder, self.cadence)
            else {
                continue;
            };
            let mut audio = Vec::new();
            if let (Some((params, time_base)), Some(bus)) = (self.audio, self.bus) {
                audio.push(AudioDescriptor::from_opened(
                    multiview_config::MAIN_PROGRAM,
                    StreamKind::Audio,
                    params,
                    *time_base,
                    bus,
                ));
                for (track_index, (params, time_base)) in self.audio_tracks.iter().enumerate() {
                    let kind = u16::try_from(track_index)
                        .map(StreamKind::AudioTrack)
                        .unwrap_or(StreamKind::Audio);
                    let track = self
                        .declared_tracks
                        .get(track_index)
                        .cloned()
                        .unwrap_or_else(|| format!("track-{track_index}"));
                    audio.push(AudioDescriptor::from_opened(
                        &track, kind, params, *time_base, bus,
                    ));
                }
            }
            let mut dependencies = self.dependencies.to_vec();
            dependencies.push(Dependency {
                kind: DependencyKind::Output,
                id: id.clone(),
            });
            bindings.push(EffectiveBinding {
                program_id: multiview_config::MAIN_PROGRAM.to_owned(),
                bus_id: multiview_config::MAIN_PROGRAM.to_owned(),
                output_id: id.clone(),
                output_index: index,
                config_generation: self.generation,
                video,
                audio,
                dependencies,
            });
        }
        bindings
    }
}

/// Thin [`WindowDelivery`] decorator: forward every complete window to the real
/// per-binding delivery and mirror the SAME assembled window into the off-clock
/// subscription registry. It adds no queue, thread, encoder or packet copy on
/// the physical path — a subscriber that cannot take its window loses that
/// window only.
pub(crate) struct OutputWindowTap<D: WindowDelivery> {
    delivery: D,
    registry: OutputWindowRegistry,
    config_generation: u64,
}
impl<D: WindowDelivery> OutputWindowTap<D> {
    /// Wrap the physical delivery exactly once. Every binding that is not
    /// admissible for `config_generation` (wrong generation, duplicate id, no
    /// geometry, empty codec identity) is DROPPED rather than substituted, so an
    /// inconsistent snapshot degrades to fewer subscribable outputs — never to a
    /// panic on the production path.
    pub(crate) fn new(
        delivery: D,
        effective: Vec<EffectiveBinding>,
        config_generation: u64,
    ) -> (Self, SubscriptionFactory) {
        let mut ids = std::collections::HashSet::new();
        let effective: Vec<EffectiveBinding> = effective
            .into_iter()
            .filter(|binding| admissible(binding, config_generation, &mut ids))
            .collect();
        let (registry, factory) = OutputWindowRegistry::bare(effective, config_generation);
        (
            Self {
                delivery,
                registry,
                config_generation,
            },
            factory,
        )
    }
    pub(crate) fn registry(&self) -> &OutputWindowRegistry {
        &self.registry
    }
}
impl<D: WindowDelivery> WindowDelivery for OutputWindowTap<D> {
    fn offer_window(
        &mut self,
        output_index: usize,
        generation: u64,
        packets: Vec<EncodedPacket>,
        media_span: Duration,
        now: Instant,
    ) -> Result<(), multiview_output::batch::EmissionError> {
        // Mirror first: it cannot wait, and the physical binding's own admission
        // is the authoritative outcome the dispatcher reports.
        self.registry.offer_complete(
            output_index,
            self.config_generation,
            &packets,
            media_span,
            now,
        );
        self.delivery
            .offer_window(output_index, generation, packets, media_span, now)
    }
    fn report_loss(&mut self, output_index: usize, reason: OfferError) {
        self.registry.report_loss(output_index, reason);
        self.delivery.report_loss(output_index, reason);
    }
    fn take_discontinuity(
        &mut self,
        output_index: usize,
    ) -> Option<multiview_output::batch::Discontinuity> {
        self.delivery.take_discontinuity(output_index)
    }
    fn generation(&self, output_index: usize) -> u64 {
        self.delivery.generation(output_index)
    }
    fn statuses(&self) -> Vec<crate::binding_delivery::BindingStatus> {
        self.delivery.statuses()
    }
    fn pump(&mut self, now: Instant) {
        // One output tick has passed: hand any admitted tail to its transport even
        // though no new window was offered (a quiet encoder must not strand it).
        self.registry.pump(now);
    }
    fn finish(&mut self) {
        self.registry.close();
        self.delivery.finish();
    }
    fn abort(&mut self) {
        self.registry.close();
        self.delivery.abort();
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use ffmpeg_next::codec::packet::{Flags, Packet};
    use multiview_ffmpeg::CodecKind;

    /// The effective-configuration generation these fixtures belong to.
    const GENERATION: u64 = 7;

    /// A registry that already carries the fixture bindings' generation, so a
    /// test never has to restate it.
    fn registry(
        effective: Vec<EffectiveBinding>,
    ) -> Result<(OutputWindowRegistry, SubscriptionFactory), SubscribeError> {
        OutputWindowRegistry::new(effective, GENERATION)
    }

    #[test]
    fn audio_bitrate_is_only_the_actual_opened_value() {
        let config = multiview_output::AudioEncodeConfig::aac(48_000, 2, 128_000);
        let encoder = multiview_output::AudioRenditionEncoder::new(&config).unwrap();
        let descriptor = AudioDescriptor::from_opened(
            "program",
            StreamKind::Audio,
            encoder.codec_params(),
            encoder.time_base(),
            AudioFormat::new(48_000, multiview_audio::format::ChannelLayout::Stereo),
        );
        assert_eq!(
            descriptor.bitrate_bps,
            encoder.codec_params().opened_bit_rate()
        );
        assert_eq!(descriptor.bitrate_bps, Some(128_000));
        assert_eq!(binding("unknown", 0).audio[0].bitrate_bps, None);
    }

    #[test]
    fn opened_descriptor_preserves_cadence_independent_of_timestamp_scale() {
        let encoder = multiview_ffmpeg::VideoEncoder::new(&multiview_ffmpeg::VideoEncodeTarget {
            codec_name: "mpeg2video".into(),
            width: 64,
            height: 48,
            format: ffmpeg::format::Pixel::YUV420P,
            time_base: Rational::new(1, 25),
            bit_rate: 500_000,
            gop: 12,
            cuda_device: None,
        })
        .expect("opened encoder");
        let params = StreamCodecParameters::from_encoder(&encoder);
        let descriptor = VideoDescriptor::from_opened(
            &params,
            Rational::new(1, 90_000),
            crate::rendition_workers::ReorderBound::NoReorder,
            Rational::new(30_000, 1_001),
        )
        .expect("video descriptor");
        assert_eq!(descriptor.time_base, Rational::new(1, 90_000));
        assert_eq!(descriptor.cadence, Rational::new(30_000, 1_001));
        assert_eq!(descriptor.bitrate_bps, params.opened_bit_rate());
        assert_eq!(descriptor.bitrate_bps, Some(500_000));
    }

    fn binding(id: &str, index: usize) -> EffectiveBinding {
        EffectiveBinding {
            program_id: "program".into(),
            bus_id: "program".into(),
            output_id: id.into(),
            output_index: index,
            config_generation: GENERATION,
            video: VideoDescriptor {
                codec: "avc1.640028".into(),
                codec_id: "H264".into(),
                coded_width: 1920,
                coded_height: 1080,
                config: Some(vec![1, 2, 3]),
                time_base: Rational::new(1, 25),
                cadence: Rational::new(25, 1),
                bitrate_bps: Some(2_000_000),
                framing: NalFraming::AnnexB,
                recovery: Some(RecoveryGate::StrictRap {
                    codec: CodecKind::H264,
                    framing: NalFraming::AnnexB,
                }),
                reorder: crate::rendition_workers::ReorderBound::NoReorder,
            },
            audio: vec![AudioDescriptor {
                track: "program".into(),
                kind: StreamKind::Audio,
                codec: "mp4a.40.2".into(),
                bitrate_bps: None,
                sample_rate: 48_000,
                channels: 2,
                config: Some(vec![0x12, 0x10]),
                time_base: Rational::new(1, 48_000),
            }],
            dependencies: vec![
                Dependency {
                    kind: DependencyKind::Source,
                    id: "camera-a".into(),
                },
                Dependency {
                    kind: DependencyKind::Program,
                    id: "program".into(),
                },
            ],
        }
    }
    /// One real encoder emission: an independent payload allocation, exactly
    /// what `encode-once-mux-many` fans out. `payload()` pointers prove sharing.
    fn encoded(pts: i64, key: bool) -> Arc<EncodedPacket> {
        let mut raw = Packet::copy(&[0, 0, 0, 1, 0x65, 0x88, 0x11, 0x22, 0x33]);
        if key {
            raw.set_flags(Flags::KEY);
        }
        raw.set_pts(Some(pts));
        raw.set_dts(Some(pts));
        Arc::new(EncodedPacket::from_packet(raw))
    }
    fn audio_packet(pts: i64) -> Arc<EncodedPacket> {
        let mut raw = Packet::copy(&[0x21, 0x10, 0x55]);
        raw.set_pts(Some(pts));
        raw.set_dts(Some(pts));
        Arc::new(EncodedPacket::with_kind(raw, StreamKind::Audio))
    }
    fn window_of(packets: &[Arc<EncodedPacket>]) -> Vec<EncodedPacket> {
        packets
            .iter()
            .map(|packet| packet.clone_shared_ref().unwrap())
            .collect()
    }
    fn pointer(packet: &EncodedPacket) -> usize {
        packet.payload().unwrap().as_ptr() as usize
    }

    #[test]
    fn resolve_uses_opened_descriptors_and_refuses_unencoded_scopes() {
        let (registry, factory) = registry(vec![binding("out", 0)]).unwrap();
        let snapshot = factory.resolve(&WhepScope::Output("out".into())).unwrap();
        assert_eq!(
            snapshot.source_dependencies().collect::<Vec<_>>(),
            vec!["camera-a"]
        );
        // Effective == opened: the descriptor is the encoder's own snapshot, so
        // there is no pending desired configuration to substitute.
        assert_eq!(snapshot.video.codec, "avc1.640028");
        assert_eq!(snapshot.video.config.as_deref(), Some([1, 2, 3].as_slice()));
        assert_eq!(snapshot.video.time_base, Rational::new(1, 25));
        assert_eq!(
            snapshot
                .audio_for(StreamKind::Audio)
                .map(|a| a.track.as_str()),
            Some("program")
        );
        assert_eq!(snapshot.config_generation, 7);
        // Resolution allocates no demand and no peer slot.
        assert_eq!(factory.slots_in_use(), 0);
        // Program/input previews have no independent encoded key: typed refusal,
        // never a hidden encoder.
        for scope in [WhepScope::Program, WhepScope::Input("camera-a".into())] {
            assert!(matches!(
                factory.subscribe(&scope, 7),
                Err(SubscribeError::UnsupportedScope)
            ));
            assert!(matches!(
                factory.resolve(&scope),
                Err(SubscribeError::UnsupportedScope)
            ));
        }
        assert!(matches!(
            factory.resolve(&WhepScope::Output("absent".into())),
            Err(SubscribeError::Unavailable)
        ));
        assert_eq!(factory.slots_in_use(), 0);
        assert_eq!(registry.active_peers(), 0);
    }

    #[test]
    fn three_outputs_two_encodes_share_payload_and_slow_peer_cannot_backpressure() {
        // Two opened video encodes fan out to three outputs: 0 and 1 select the
        // first encode, 2 selects the second. Those are the only media
        // allocations the whole path may ever reference.
        let source_a = encoded(0, true);
        let source_audio = audio_packet(0);
        let source_b = encoded(0, true);
        let mut bindings: Vec<_> = (0..3).map(|i| binding(["a", "b", "c"][i], i)).collect();
        bindings[2].video.codec = "avc1.640029".into();
        let (mut registry, factory) = registry(bindings).unwrap();
        let subs: Vec<_> = (0..3)
            .map(|i| {
                factory
                    .subscribe(&WhepScope::Output(["a", "b", "c"][i].into()), 7)
                    .unwrap()
            })
            .collect();
        let now = Instant::now();
        let span = Duration::from_millis(40);
        for index in 0..3 {
            let first = if index == 2 { &source_b } else { &source_a };
            registry.offer_complete(
                index,
                7,
                &window_of(&[Arc::clone(first), Arc::clone(&source_audio)]),
                span,
                now,
            );
        }
        let expected = [pointer(&source_a), pointer(&source_a), pointer(&source_b)];
        let mut delivered = std::collections::BTreeSet::new();
        for (index, sub) in subs.iter().enumerate() {
            let window = sub.try_recv_window().unwrap().unwrap();
            assert_eq!(window.config_generation, 7);
            assert_eq!(window.media_span, span);
            let packets = window.clone_window().unwrap();
            assert_eq!(packets.len(), 2);
            assert_eq!(pointer(&packets[0]), expected[index]);
            assert_eq!(pointer(&packets[1]), pointer(&source_audio));
            delivered.insert(pointer(&packets[0]));
            // The clone retains real transport ownership: the lease is still
            // charged while the copies live.
            assert!(registry.retained().1 > 0);
            drop(packets);
        }
        // Three outputs fanned the SAME two encoder allocations: exactly one
        // media buffer per encode, never one per viewer.
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered, [pointer(&source_a), pointer(&source_b)].into());
        // The slow peer (index 2) stops draining. Its own handoff and queue fill;
        // the producer must still return promptly and its siblings keep serving.
        let mut siblings = 0;
        let mut peak_retained = 0;
        // Offer well past the 2 s admission age: the stalled peer's own queued
        // windows expire and are flushed as whole-window losses, while its
        // siblings keep consuming every window at cadence.
        for tick in 1..60 {
            for index in 0..3 {
                let first = if index == 2 { &source_b } else { &source_a };
                registry.offer_complete(
                    index,
                    7,
                    &window_of(&[Arc::clone(first), Arc::clone(&source_audio)]),
                    span,
                    now + Duration::from_millis(40 * tick),
                );
            }
            sibling_drain(&subs[0], &mut siblings);
            sibling_drain(&subs[1], &mut siblings);
            // Bounded memory: retention is a function of the per-peer queue caps,
            // never of how many windows the producer has offered.
            peak_retained = peak_retained.max(registry.retained().1);
            assert!(peak_retained <= MAX_PEERS * multiview_output::batch::MAX_QUEUE_BYTES);
        }
        assert!(peak_retained > 0, "retention must be live, not vacuous");
        assert!(
            registry.rejected_windows() > 0,
            "stalled peer must lose its own whole windows once they expire"
        );
        assert!(siblings >= 2, "sibling transports must keep receiving");
        assert_eq!(
            registry.active_peers(),
            3,
            "the stalled peer is not global failure"
        );
        // The stalled peer still holds a complete, bounded window it never read.
        assert!(subs[2].try_recv_window().unwrap().is_some());
    }

    fn drain(sub: &Subscription) -> usize {
        let mut delivered = 0;
        while let Ok(Some(window)) = sub.try_recv_window() {
            delivered += 1;
            drop(window);
        }
        delivered
    }

    #[test]
    fn quiet_producer_tail_is_pumped_once_the_transport_drains() {
        // Deterministic repro of the quiet-tail stall: windows are admitted while
        // the transport reads none (its 2-deep handoff fills, the rest stay
        // queued), then the producer goes QUIET. Every admitted window must still
        // reach the transport on later output ticks — no indefinite `pending`.
        let (mut registry, factory) = registry(vec![binding("out", 0)]).unwrap();
        let sub = factory
            .subscribe(&WhepScope::Output("out".into()), GENERATION)
            .unwrap();
        let source = encoded(0, true);
        let span = Duration::from_millis(40);
        let now = Instant::now();
        for tick in 0..4 {
            registry.offer_complete(
                0,
                GENERATION,
                &window_of(&[Arc::clone(&source)]),
                span,
                now + Duration::from_millis(40 * tick),
            );
        }
        let mut delivered = 0;
        for tick in 4..12 {
            registry.pump(now + Duration::from_millis(40 * tick));
            delivered += drain(&sub);
        }
        assert_eq!(
            delivered, 4,
            "every admitted window must reach the transport after the producer goes quiet"
        );
        assert_eq!(registry.rejected_windows(), 0);
        assert_eq!(registry.retained(), (0, 0));
    }

    #[test]
    fn recoverable_loss_is_not_a_terminal_revocation() {
        let (mut registry, factory) = registry(vec![binding("out", 0)]).unwrap();
        let sub = factory
            .subscribe(&WhepScope::Output("out".into()), GENERATION)
            .unwrap();
        let source = encoded(0, true);
        let span = Duration::from_millis(40);
        let now = Instant::now();
        // The transport never drains: once its admitted windows exceed the age
        // bound they are flushed as ITS OWN whole-window loss.
        for tick in 0..80 {
            registry.offer_complete(
                0,
                GENERATION,
                &window_of(&[Arc::clone(&source)]),
                span,
                now + Duration::from_millis(40 * tick),
            );
        }
        let later = now + Duration::from_millis(40 * 80);
        registry.pump(later);
        assert!(
            registry.rejected_windows() > 0,
            "aged-out queued windows must be an explicit loss"
        );
        assert!(
            !sub.is_revoked(),
            "a recoverable loss must not revoke the subscription"
        );
        assert_eq!(factory.slots_in_use(), 1);
        assert!(drain(&sub) >= 1, "in-flight leases stay readable");
        // Empty the handoff completely (a stale in-flight lease may still be
        // pending) before asserting on the fresh window alone.
        registry.pump(later);
        let _ = drain(&sub);
        // A later, fresh RAP window is still admitted and delivered: the peer was
        // never terminally revoked by its own loss.
        registry.offer_complete(
            0,
            GENERATION,
            &window_of(&[Arc::clone(&source)]),
            span,
            later,
        );
        registry.pump(later);
        assert!(
            drain(&sub) >= 1,
            "delivery resumes after a recoverable loss"
        );
        assert_eq!(registry.active_peers(), 1);
    }

    fn sibling_drain(sub: &Subscription, delivered: &mut usize) {
        while let Ok(Some(window)) = sub.try_recv_window() {
            *delivered += 1;
            drop(window);
        }
    }

    #[test]
    fn attach_and_detach_reuse_the_same_encodes_and_release_ownership() {
        let source = encoded(0, true);
        let source_audio = audio_packet(0);
        let (mut registry, factory) = registry(vec![binding("a", 0), binding("b", 1)]).unwrap();
        let a = factory
            .subscribe(&WhepScope::Output("a".into()), 7)
            .unwrap();
        let b = factory
            .subscribe(&WhepScope::Output("b".into()), 7)
            .unwrap();
        let now = Instant::now();
        let span = Duration::from_millis(40);
        let window = window_of(&[Arc::clone(&source), Arc::clone(&source_audio)]);
        registry.offer_complete(0, 7, &window, span, now);
        registry.offer_complete(1, 7, &window, span, now);
        let first = a
            .try_recv_window()
            .unwrap()
            .unwrap()
            .clone_window()
            .unwrap();
        assert_eq!(pointer(&first[0]), pointer(&source));
        drop(first);
        // Detach one peer: RAII revoke + reconcile drops its peer and closes the
        // queue; the byte reservation returns to zero for that peer.
        b.revoke();
        assert_eq!(factory.slots_in_use(), 1);
        drop(b);
        registry.reconcile();
        assert_eq!(registry.active_peers(), 1);
        let window = window_of(&[Arc::clone(&source), Arc::clone(&source_audio)]);
        registry.offer_complete(0, 7, &window, span, now);
        registry.offer_complete(1, 7, &window, span, now);
        let second = a
            .try_recv_window()
            .unwrap()
            .unwrap()
            .clone_window()
            .unwrap();
        // Same two media allocations; no new encode, no new payload buffer.
        assert_eq!(pointer(&second[0]), pointer(&source));
        assert_eq!(pointer(&second[1]), pointer(&source_audio));
        drop(second);
        while a.try_recv_window().unwrap().is_some() {}
        assert_eq!(registry.retained(), (0, 0));
        // A fresh attach reuses the same opened rendition.
        let c = factory
            .subscribe(&WhepScope::Output("b".into()), 7)
            .unwrap();
        registry.reconcile();
        assert_eq!(registry.active_peers(), 2);
        registry.offer_complete(1, 7, &window, span, now);
        let third = c
            .try_recv_window()
            .unwrap()
            .unwrap()
            .clone_window()
            .unwrap();
        assert_eq!(pointer(&third[0]), pointer(&source));
        drop(third);
        drop(a);
        drop(c);
        assert_eq!(factory.slots_in_use(), 0);
    }

    #[test]
    fn cap_and_raii_release_are_per_peer_and_do_not_touch_effective_outputs() {
        let (mut registry, factory) = registry(vec![binding("a", 0), binding("b", 1)]).unwrap();
        let mut peers = Vec::new();
        for _ in 0..MAX_PEERS {
            peers.push(
                factory
                    .subscribe(&WhepScope::Output("a".into()), 7)
                    .unwrap(),
            );
        }
        assert!(matches!(
            factory.subscribe(&WhepScope::Output("a".into()), 7),
            Err(SubscribeError::Capacity)
        ));
        assert_eq!(factory.slots_in_use(), MAX_PEERS);
        let released = peers.pop().unwrap();
        // Dropping the subscription is the RAII release: exactly one slot.
        drop(released);
        assert_eq!(factory.slots_in_use(), MAX_PEERS - 1);
        let new_peer = factory
            .subscribe(&WhepScope::Output("b".into()), 7)
            .unwrap();
        registry.reconcile();
        assert_eq!(registry.active_peers(), MAX_PEERS);
        assert_eq!(new_peer.binding.output_id, "b");
        assert_eq!(registry.effective.len(), 2);
    }

    #[test]
    fn config_generation_mismatch_is_refused_without_enqueue() {
        let source = encoded(0, true);
        let (mut registry, factory) = registry(vec![binding("out", 0)]).unwrap();
        assert!(matches!(
            factory.subscribe(&WhepScope::Output("out".into()), 8),
            Err(SubscribeError::StaleGeneration)
        ));
        assert_eq!(factory.slots_in_use(), 0);
        let sub = factory
            .subscribe(&WhepScope::Output("out".into()), 7)
            .unwrap();
        let now = Instant::now();
        let span = Duration::from_millis(40);
        // A newer generation's window is not the admitted one: nothing enqueues.
        registry.offer_complete(0, 8, &window_of(&[Arc::clone(&source)]), span, now);
        assert!(sub.try_recv_window().unwrap().is_none());
        assert_eq!(registry.rejected_windows(), 0);
        assert_eq!(registry.retained(), (0, 0));
        registry.offer_complete(0, 7, &window_of(&[Arc::clone(&source)]), span, now);
        assert!(sub.try_recv_window().unwrap().is_some());
    }

    #[test]
    fn external_fence_is_observed_read_only_and_own_revoke_never_writes_it() {
        let (mut registry, factory) =
            registry(vec![binding("a", 0), binding("b", 1), binding("c", 2)]).unwrap();
        // The transport owns each token; only its own revoke path may publish it.
        let fence = Arc::new(AtomicBool::new(false));
        let own_token = Arc::new(AtomicBool::new(false));
        let a = factory
            .subscribe_with_revocation(&WhepScope::Output("a".into()), 7, Arc::clone(&fence))
            .unwrap();
        let b = factory
            .subscribe(&WhepScope::Output("b".into()), 7)
            .unwrap();
        let c = factory
            .subscribe_with_revocation(&WhepScope::Output("c".into()), 7, Arc::clone(&own_token))
            .unwrap();
        assert_eq!(factory.slots_in_use(), 3);
        let source = encoded(0, true);
        let now = Instant::now();
        let span = Duration::from_millis(40);
        for index in 0..3 {
            registry.offer_complete(index, 7, &window_of(&[Arc::clone(&source)]), span, now);
        }
        let held = a.try_recv_window().unwrap().unwrap();
        assert!(held.permit.is_current());
        assert!(c.try_recv_window().unwrap().is_some());
        assert!(b.try_recv_window().unwrap().is_some());
        // Registry-side revoke: stops this peer and releases exactly its slot,
        // but never writes the transport's token — a bare store would strand the
        // transport fence's own cancellation waiters.
        let c_handle = c.revoke_handle();
        c_handle.revoke();
        assert!(c_handle.is_own_revoked());
        assert!(c_handle.is_revoked());
        assert!(
            !own_token.load(Ordering::Acquire),
            "the registry must never write the transport fence"
        );
        assert_eq!(factory.slots_in_use(), 2);
        registry.reconcile();
        assert_eq!(registry.active_peers(), 2);
        registry.offer_complete(2, 7, &window_of(&[Arc::clone(&source)]), span, now);
        assert!(matches!(
            c.try_recv_window(),
            Err(SubscribeError::ProducerEnded)
        ));
        // The transport publishes its own cancellation (its own path wakes its
        // waiters): only its peer stops enqueueing, the untouched peer continues.
        fence.store(true, Ordering::Release);
        assert!(a.is_revoked());
        registry.reconcile();
        assert_eq!(registry.active_peers(), 1);
        registry.offer_complete(0, 7, &window_of(&[Arc::clone(&source)]), span, now);
        assert!(matches!(
            a.try_recv_window(),
            Err(SubscribeError::ProducerEnded)
        ));
        registry.offer_complete(1, 7, &window_of(&[Arc::clone(&source)]), span, now);
        assert!(b.try_recv_window().unwrap().is_some());
        // External cancellation does not steal the transport's slot; the RAII
        // drop of the subscription releases it.
        assert_eq!(factory.slots_in_use(), 2);
        drop(a);
        assert_eq!(factory.slots_in_use(), 1);
        // The in-flight window still owns its payload through the clone path.
        let cloned = held.clone_window().unwrap();
        assert_eq!(pointer(&cloned[0]), pointer(&source));
        drop(cloned);
        drop(held);
        drop(b);
        drop(c);
        assert_eq!(factory.slots_in_use(), 0);
        registry.reconcile();
        assert_eq!(registry.active_peers(), 0);
        assert_eq!(registry.retained(), (0, 0));
    }

    #[test]
    fn tap_forwards_the_same_window_and_ends_subscriptions_on_finish() {
        struct Spy {
            offers: Vec<(usize, usize, usize)>,
            finished: bool,
        }
        impl WindowDelivery for Spy {
            fn offer_window(
                &mut self,
                output_index: usize,
                _generation: u64,
                packets: Vec<EncodedPacket>,
                _media_span: Duration,
                _now: Instant,
            ) -> Result<(), multiview_output::batch::EmissionError> {
                self.offers.push((
                    output_index,
                    packets.len(),
                    packets.first().map_or(0, pointer),
                ));
                Ok(())
            }
            fn report_loss(&mut self, _output_index: usize, _reason: OfferError) {}
            fn take_discontinuity(
                &mut self,
                _output_index: usize,
            ) -> Option<multiview_output::batch::Discontinuity> {
                None
            }
            fn generation(&self, _output_index: usize) -> u64 {
                0
            }
            fn statuses(&self) -> Vec<crate::binding_delivery::BindingStatus> {
                Vec::new()
            }
            fn finish(&mut self) {
                self.finished = true;
            }
            fn abort(&mut self) {
                self.finished = true;
            }
        }
        let source = encoded(0, true);
        let (mut tap, factory) = OutputWindowTap::new(
            Spy {
                offers: Vec::new(),
                finished: false,
            },
            vec![binding("out", 0)],
            GENERATION,
        );
        let sub = factory
            .subscribe(&WhepScope::Output("out".into()), 7)
            .unwrap();
        let assembled = window_of(&[Arc::clone(&source)]);
        tap.offer_window(0, 1, assembled, Duration::from_millis(40), Instant::now())
            .unwrap();
        // The physical path saw the same payload allocation the subscriber got.
        assert_eq!(tap.delivery.offers.len(), 1);
        assert_eq!(tap.delivery.offers[0].2, pointer(&source));
        let mirrored = sub.try_recv_window().unwrap().unwrap();
        assert_eq!(pointer(&mirrored.permit.packets()[0]), pointer(&source));
        drop(mirrored);
        tap.finish();
        assert!(tap.delivery.finished);
        assert_eq!(tap.registry().active_peers(), 0);
        assert!(matches!(
            sub.try_recv_window(),
            Err(SubscribeError::ProducerEnded)
        ));
    }
}
