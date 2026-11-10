//! Additive optional (preview) rendition registration for the shared window
//! registry.
//!
//! A preview rendition is an **ordinary `EffectiveBinding` with a distinct,
//! stable `output_id`**, published into the same `SwapCell` slot the production
//! run already publishes through. That is deliberate: the registry's existing
//! `resolve`/`subscribe` path, its duplicate-id rule (`admissible` keys on
//! `output_id`), its generation check and its per-peer revocation then apply
//! unchanged, so:
//!
//! * production bindings, their peers and their queues are never touched — the
//!   composed snapshot contains the production bindings byte-for-byte;
//! * the composed snapshot keeps the **production generation**, so a production
//!   subscriber that resolved before the preview appeared is not refused with
//!   `StaleGeneration` and unrelated peers are never invalidated;
//! * only the preview `output_id`s are removable, and removing them makes a
//!   later `resolve` of those scopes return the typed `Unavailable`.
//!
//! The preview's encoder is opened by its owner (the preview runtime), never
//! here: this module receives an already-opened [`VideoDescriptor`] and refuses
//! anything that looks like a request token or a guess. Nothing in this file
//! edits the pipeline or opens an encoder.
// The typed bridge below is consumed by the preview runtime (preview_runtime.rs)
// which lands separately; until it is wired, only this module's own tests
// construct these types. Remove this allow when the consumer is committed.
#![allow(dead_code)]

use crate::output_window_registry::{
    Dependency, EffectiveBinding, EffectiveSnapshotState, OptionalWindowProducer,
    SubscriptionFactory, VideoDescriptor,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The most optional renditions one registration may publish. Bounded so a
/// single call cannot grow the effective snapshot without limit; the registry's
/// own peer cap (`MAX_PEERS`) still bounds live subscribers independently.
pub(crate) const MAX_OPTIONAL_RENDITIONS: usize = 8;

/// The GLOBAL bound on live optional bindings across every registration. A
/// per-call cap alone lets repeated calls grow the set without limit, so the
/// serialized owner reserves against this total before publishing anything.
pub(crate) const MAX_OPTIONAL_TOTAL: usize = 8;

/// Stable identity of one optional rung.
///
/// `profile_id` is what keeps simultaneous rungs of the same output apart;
/// `output_id` is the stable id the catalogue maps and the id a subscriber
/// resolves, so it must be unique across the whole effective snapshot (the
/// registry's duplicate rule enforces that at publication).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewScope {
    pub(crate) profile_id: String,
    pub(crate) output_id: String,
    pub(crate) output_index: usize,
    pub(crate) program_id: String,
    pub(crate) bus_id: String,
}

impl PreviewScope {
    /// The scope key used for duplicate detection inside one registration.
    #[must_use]
    pub(crate) fn key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.profile_id, self.output_id, self.output_index, self.program_id, self.bus_id
        )
    }
}

/// A resource owner retained while the registration is active, dropped at
/// retirement. In-flight packets retain their payload through `av_packet_ref`,
/// not through this lease. The bridge must stop encoder work off-clock before
/// releasing its last encoder owner; this lease does not extend worker lifetime.
/// Type-erased so this module need not know the encoder type.
pub(crate) struct ResourceLease(#[allow(dead_code)] Arc<dyn std::any::Any + Send + Sync>);

impl ResourceLease {
    #[must_use]
    pub(crate) fn new<T: std::any::Any + Send + Sync>(owner: Arc<T>) -> Self {
        Self(owner)
    }
}

/// The off-clock producer that will offer this rendition's windows. The registry
/// itself never uses it; it is carried so the caller's lifetime travels with the
/// registration.
pub(crate) type PreviewProducer = OptionalWindowProducer;

/// One optional rendition to publish: the **already opened** descriptor, the
/// scope it attaches to, its auth dependencies, and owners retained while active.
pub(crate) struct OptionalRendition {
    pub(crate) scope: PreviewScope,
    pub(crate) video: VideoDescriptor,
    pub(crate) audio: Vec<crate::output_window_registry::AudioDescriptor>,
    pub(crate) dependencies: Vec<Dependency>,
    /// `None` for a metadata-only registration (tests, catalogue-free callers);
    /// the producer-side wiring attaches the real one.
    pub(crate) producer: Option<PreviewProducer>,
    pub(crate) resources: Vec<ResourceLease>,
}

impl OptionalRendition {
    /// A rendition with no producer attached; the caller keeps its own producer
    /// handle. Validation still applies at registration.
    #[must_use]
    pub(crate) fn new(
        scope: PreviewScope,
        video: VideoDescriptor,
        dependencies: Vec<Dependency>,
        resources: Vec<ResourceLease>,
    ) -> Self {
        Self {
            scope,
            video,
            audio: Vec::new(),
            dependencies,
            producer: None,
            resources,
        }
    }

    /// Attach the bounded off-clock producer.
    #[must_use]
    pub(crate) fn with_producer(mut self, producer: PreviewProducer) -> Self {
        self.producer = Some(producer);
        self
    }

    fn binding(&self, generation: u64) -> EffectiveBinding {
        EffectiveBinding {
            program_id: self.scope.program_id.clone(),
            bus_id: self.scope.bus_id.clone(),
            output_id: self.scope.output_id.clone(),
            output_index: self.scope.output_index,
            config_generation: generation,
            video: self.video.clone(),
            audio: self.audio.clone(),
            dependencies: self.dependencies.clone(),
        }
    }

    /// A descriptor is usable only when it came from an opened encoder: real
    /// geometry, a real codec identity, and a proven random-access gate. A
    /// requested token or a guessed profile is refused here rather than
    /// published as if it had been opened.
    fn validate(&self) -> Result<(), OptionalRegisterError> {
        if self.scope.profile_id.is_empty() || self.scope.output_id.is_empty() {
            return Err(OptionalRegisterError::InvalidScope(
                "profile_id and output_id must be non-empty".to_owned(),
            ));
        }
        if self.video.codec.is_empty() || self.video.codec_id.is_empty() {
            return Err(OptionalRegisterError::InvalidDescriptor(
                "opened codec identity is empty".to_owned(),
            ));
        }
        if self.video.coded_width == 0 || self.video.coded_height == 0 {
            return Err(OptionalRegisterError::InvalidDescriptor(
                "opened geometry is zero".to_owned(),
            ));
        }
        if self.video.recovery.is_none() {
            return Err(OptionalRegisterError::InvalidDescriptor(
                "opened codec record proves no random-access point".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A typed refusal. Additive: distinct from [`SubscribeError`] so the
/// registration path can name its own failures without widening the subscriber
/// enum the rest of the registry matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OptionalRegisterError {
    /// More than [`MAX_OPTIONAL_RENDITIONS`] in one call.
    Capacity,
    /// Two renditions in one call share a scope key, or an `output_id` already
    /// present in the effective snapshot.
    DuplicateScope(String),
    /// The scope identity is empty or self-inconsistent.
    InvalidScope(String),
    /// The descriptor was not produced by an opened encoder.
    InvalidDescriptor(String),
    /// The registration was already retired.
    NotRegistered,
    /// A protected/ABR registration carried no producer: advertising a rung
    /// whose windows nothing will ever offer would be a dead entry in the
    /// catalogue, so it is refused here instead.
    MissingProducer,
    /// This factory lifecycle already has an optional publisher.
    AlreadyOwned,
}

/// RAII owner of one published optional generation.
///
/// Dropping it retires **only the `output_id`s this call published**: the
/// optional set is held by one serialized owner (the publisher), so a
/// concurrent rung registered later survives this retirement, and production is
/// never touched. The serialized owner holds the [`ResourceLease`]s only while
/// active and drops them at retirement. In-flight packet payloads have their own
/// `FFmpeg` references; they do not retain the encoder.
pub(crate) struct OptionalRegistration {
    state: Arc<Mutex<OptionalActive>>,
    owner_id: u64,
    keys: Vec<String>,
    generation: u64,
    released: AtomicBool,
}

impl OptionalRegistration {
    /// The generation the composed snapshot was published at — the production
    /// generation, deliberately unchanged.
    #[must_use]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The `output_id`s this registration owns; nothing outside this set is
    /// ever retired.
    #[must_use]
    pub(crate) fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Retire explicitly. Idempotent; `Drop` runs the same path. Only this
    /// registration's own bindings are removed, and the remaining active rungs
    /// plus production are republished together.
    pub(crate) fn retire(&self) -> Result<(), OptionalRegisterError> {
        if self.released.swap(true, Ordering::AcqRel) {
            return Err(OptionalRegisterError::NotRegistered);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| OptionalRegisterError::NotRegistered)?;
        // Release this registration's GLOBAL reservation with its bindings: a
        // retirement that kept the reservation would shrink the optional budget
        // permanently, so a later register would be refused with `Capacity`
        // even though nothing optional was live.
        let Some(removed) = state.active.remove(&self.owner_id) else {
            return Err(OptionalRegisterError::NotRegistered);
        };
        state.reserved = state.reserved.saturating_sub(removed.bindings.len());
        state.republish();
        Ok(())
    }
}

impl Drop for OptionalRegistration {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

impl std::fmt::Debug for OptionalRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptionalRegistration")
            .field("keys", &self.keys)
            .field("generation", &self.generation)
            .field("retired", &self.released.load(Ordering::Acquire))
            .field("owner_id", &self.owner_id)
            .finish_non_exhaustive()
    }
}

/// One live optional registration inside the serialized optional set.
struct ActiveRegistration {
    bindings: Vec<EffectiveBinding>,
    #[allow(dead_code)]
    producers: Vec<PreviewProducer>,
    #[allow(dead_code)]
    resources: Vec<ResourceLease>,
}

/// The serialized owner of the whole optional set plus the atomically published
/// snapshot. One instance per registry lifecycle: `register_optional` and every
/// `retire` go through it, so two concurrent `Drop`s cannot race a
/// read-modify-write and erase each other's rungs.
struct OptionalActive {
    active: std::collections::BTreeMap<u64, ActiveRegistration>,
    /// Total bindings reserved across every live registration (the GLOBAL cap).
    reserved: usize,
    production: Arc<SubscriptionFactory>,
    baseline: Arc<EffectiveSnapshotState>,
    next_index: usize,
}

impl OptionalActive {
    /// Publish production + every live optional binding as ONE snapshot.
    fn republish(&self) {
        let generation = self.baseline.generation;
        let mut composed = self.baseline.effective.as_ref().clone();
        for registration in self.active.values() {
            composed.extend(registration.bindings.iter().cloned());
        }
        // Publish into the ONE shared snapshot cell the producer already reads,
        // and treat the returned generation as the producer-side ack: after this
        // returns, offer_complete for a binding in it can route.
        let _ack = self.production.publish_effective(composed, generation);
    }
}

/// Publishes optional renditions over an existing production factory.
///
/// This is the serialized owner: it holds the live optional set and the only
/// path that mutates the published snapshot, so concurrent registrations merge
/// and a retirement removes exactly one registration's `output_id`s.
pub(crate) struct OptionalRegistryPublisher {
    state: Arc<Mutex<OptionalActive>>,
    next_owner: AtomicU64,
}

impl OptionalRegistryPublisher {
    /// Claim the single optional owner for this factory lifecycle.
    ///
    /// # Errors
    /// [`OptionalRegisterError::AlreadyOwned`] if any handle already claimed it,
    /// even if that publisher was dropped. Share the publisher, not a new owner.
    pub(crate) fn new(production: Arc<SubscriptionFactory>) -> Result<Self, OptionalRegisterError> {
        if !production.claim_optional_publisher() {
            return Err(OptionalRegisterError::AlreadyOwned);
        }
        Ok(Self {
            state: Arc::new(Mutex::new(OptionalActive {
                active: std::collections::BTreeMap::new(),
                reserved: 0,
                next_index: production
                    .effective()
                    .iter()
                    .map(|b| b.output_index)
                    .max()
                    .map_or(0, |i| i.saturating_add(1)),
                baseline: production.snapshot(),
                production,
            })),
            next_owner: AtomicU64::new(1),
        })
    }

    /// Publish `optional` on top of the current production snapshot and every
    /// live optional registration, atomically, all-or-nothing.
    ///
    /// # Errors
    /// [`OptionalRegisterError::Capacity`] when the batch exceeds
    /// [`MAX_OPTIONAL_RENDITIONS`] **or the global optional total**
    /// [`MAX_OPTIONAL_TOTAL`] would be exceeded — the reservation is checked
    /// before anything is published; [`OptionalRegisterError::DuplicateScope`]
    /// for a repeated scope key or an `output_id` already in the snapshot (an
    /// active optional rung included); [`OptionalRegisterError::InvalidScope`] /
    /// `InvalidDescriptor` for a scope or descriptor that cannot come from an
    /// opened encoder.
    pub(crate) fn register_optional(
        &self,
        optional: Vec<OptionalRendition>,
    ) -> Result<OptionalRegistration, OptionalRegisterError> {
        self.register(optional, true)
    }

    /// Register without requiring a producer: catalogue/UI pre-registration and
    /// tests. Named distinctly so a protected caller cannot reach it by accident
    /// — the real path is [`Self::register_optional`], which refuses a rung whose
    /// windows nothing would offer.
    ///
    /// # Errors
    /// As [`Self::register_optional`], minus `MissingProducer`.
    #[cfg(test)]
    pub(crate) fn register_optional_metadata(
        &self,
        optional: Vec<OptionalRendition>,
    ) -> Result<OptionalRegistration, OptionalRegisterError> {
        self.register(optional, false)
    }

    fn register(
        &self,
        optional: Vec<OptionalRendition>,
        require_producer: bool,
    ) -> Result<OptionalRegistration, OptionalRegisterError> {
        if optional.is_empty() || optional.len() > MAX_OPTIONAL_RENDITIONS {
            return Err(OptionalRegisterError::Capacity);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| OptionalRegisterError::Capacity)?;
        // GLOBAL reservation, checked before anything is published.
        if state.reserved + optional.len() > MAX_OPTIONAL_TOTAL {
            return Err(OptionalRegisterError::Capacity);
        }
        // Validate the WHOLE batch before publishing anything (all-or-nothing),
        // against production AND against the rungs already active.
        let production_ids = &state.baseline.effective;
        let mut indexes = std::collections::HashSet::new();
        let mut keys = Vec::with_capacity(optional.len());
        let mut ids: Vec<String> = production_ids.iter().map(|b| b.output_id.clone()).collect();
        for registration in state.active.values() {
            ids.extend(registration.bindings.iter().map(|b| b.output_id.clone()));
        }
        for rendition in &optional {
            rendition.validate()?;
            if require_producer && rendition.producer.is_none() {
                return Err(OptionalRegisterError::MissingProducer);
            }
            if let Some(producer) = &rendition.producer {
                if !producer.matches(
                    &state.production,
                    rendition.scope.output_index,
                    state.baseline.generation,
                ) {
                    return Err(OptionalRegisterError::InvalidScope(
                        "producer belongs to another registry or routing key".into(),
                    ));
                }
            }
            let key = rendition.scope.key();
            if keys.contains(&key) {
                return Err(OptionalRegisterError::DuplicateScope(key));
            }
            if ids.contains(&rendition.scope.output_id) {
                return Err(OptionalRegisterError::DuplicateScope(
                    rendition.scope.output_id.clone(),
                ));
            }
            if rendition.scope.output_index < state.next_index
                || rendition.scope.output_index == usize::MAX
                || indexes.contains(&rendition.scope.output_index)
            {
                return Err(OptionalRegisterError::InvalidScope(
                    "output_index is already routed".into(),
                ));
            }
            indexes.insert(rendition.scope.output_index);
            ids.push(rendition.scope.output_id.clone());
            keys.push(key);
        }

        let generation = state.production.generation();
        let owner_id = self.next_owner.fetch_add(1, Ordering::AcqRel);
        let bindings: Vec<EffectiveBinding> = optional
            .iter()
            .map(|rendition| rendition.binding(generation))
            .collect();
        let mut producers = Vec::new();
        let mut resources = Vec::new();
        for rendition in optional {
            if let Some(producer) = rendition.producer {
                producers.push(producer);
            }
            resources.extend(rendition.resources);
        }
        state.next_index = indexes.into_iter().max().unwrap_or(state.next_index) + 1;
        state.reserved += bindings.len();
        state.active.insert(
            owner_id,
            ActiveRegistration {
                bindings,
                producers,
                resources,
            },
        );
        state.republish();

        let state_handle = Arc::clone(&self.state);
        Ok(OptionalRegistration {
            state: state_handle,
            owner_id,
            keys,
            generation,
            released: AtomicBool::new(false),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod optional_registration_tests {
    use super::*;
    use crate::output_window_registry::{
        window_subscription_slot, DependencyKind, OutputWindowRegistry, SubscribeError,
        WindowSubscriptionSlot,
    };
    use multiview_control::WhepScope;
    use multiview_core::time::Rational;
    use multiview_ffmpeg::{CodecKind, NalFraming};
    use multiview_output::batch::RecoveryGate;

    const GENERATION: u64 = 7;

    fn descriptor() -> VideoDescriptor {
        VideoDescriptor {
            codec: "avc1.640028".to_owned(),
            codec_id: "h264".to_owned(),
            coded_width: 1920,
            coded_height: 1080,
            config: None,
            time_base: Rational::FPS_25,
            cadence: Rational::FPS_25,
            bitrate_bps: None,
            recovery: Some(RecoveryGate::StrictRap {
                codec: CodecKind::H264,
                framing: NalFraming::AnnexB,
            }),
            framing: NalFraming::AnnexB,
            reorder: crate::rendition_workers::ReorderBound::NoReorder,
        }
    }

    fn production_binding(output_id: &str, index: usize) -> EffectiveBinding {
        EffectiveBinding {
            program_id: "prog".to_owned(),
            bus_id: "bus".to_owned(),
            output_id: output_id.to_owned(),
            output_index: index,
            config_generation: GENERATION,
            video: descriptor(),
            audio: Vec::new(),
            dependencies: vec![Dependency {
                kind: DependencyKind::Source,
                id: format!("src-{output_id}"),
            }],
        }
    }

    fn scope(profile: &str, output: &str) -> PreviewScope {
        PreviewScope {
            profile_id: profile.to_owned(),
            output_id: output.to_owned(),
            output_index: {
                static INDEX: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(1);
                INDEX.fetch_add(1, Ordering::Relaxed)
            },
            program_id: "prog".to_owned(),
            bus_id: "bus".to_owned(),
        }
    }

    /// A production run: one admitted output published into the slot.
    fn fixture() -> (
        WindowSubscriptionSlot,
        Arc<SubscriptionFactory>,
        OutputWindowRegistry,
    ) {
        let slot = window_subscription_slot();
        let (registry, production) =
            OutputWindowRegistry::bare(vec![production_binding("out-main", 0)], GENERATION);
        let production = Arc::new(production);
        slot.store(Some(Arc::clone(&production)));
        // The registry owns the Attach receiver; it must outlive the test or the
        // shared channel closes and no peer can attach.
        (slot, production, registry)
    }

    fn current(slot: &WindowSubscriptionSlot) -> Arc<SubscriptionFactory> {
        slot.load().expect("a factory is always published")
    }

    // Retained regression from 5d38016a, now exercises the actual bounded
    // optional adapter ingress rather than only a direct registry offer.
    #[test]
    fn optional_subscriber_receives_real_video_window_from_shared_producer() {
        use ffmpeg_next::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::EncodedPacket;
        use std::time::{Duration, Instant};
        let (slot, production, mut registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let main = production
            .subscribe(&WhepScope::Output("out-main".into()), GENERATION)
            .unwrap();
        let optional = scope("low", "preview-low");
        let index = optional.output_index;
        let producer = production.optional_producer(index, GENERATION);
        let registration = publisher
            .register_optional(vec![OptionalRendition::new(
                optional,
                descriptor(),
                Vec::new(),
                Vec::new(),
            )
            .with_producer(producer.clone())])
            .unwrap();
        let sub = current(&slot)
            .subscribe(
                &WhepScope::Output("preview-low".into()),
                registration.generation(),
            )
            .unwrap();
        let packet = || {
            let mut coded = Packet::copy(&[0, 0, 0, 1, 0x65, 0x88, 0x11]);
            coded.set_flags(Flags::KEY);
            coded.set_pts(Some(0));
            coded.set_dts(Some(0));
            EncodedPacket::from_packet(coded)
        };
        let now = Instant::now();
        let span = Duration::from_millis(40);
        // Registration returns ready without requiring a clock tick or drain ACK.
        producer.offer_complete(vec![packet()], span, now).unwrap();
        registry.pump(now);
        let received = sub
            .try_recv_window()
            .expect("bounded nonblocking poll")
            .expect("registered optional subscriber must receive actual encoded window");
        assert_eq!(received.permit.packets().len(), 1);
        registry.offer_complete(0, GENERATION, &[packet()], span, now);
        assert!(main.try_recv_window().unwrap().is_some());
        registration.retire().unwrap();
        assert_eq!(
            producer.offer_complete(vec![packet()], span, now),
            Err(SubscribeError::Unavailable)
        );
        registry.pump(now);
        assert!(matches!(
            sub.try_recv_window(),
            Err(SubscribeError::ProducerEnded)
        ));
        registry.offer_complete(0, GENERATION, &[packet()], span, now);
        assert!(
            main.try_recv_window().unwrap().is_some(),
            "unrelated production peer survives retirement"
        );
    }

    #[test]
    fn stalled_optional_ingress_is_bounded_and_never_blocks_registration_or_production() {
        use ffmpeg_next::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::EncodedPacket;
        use std::time::{Duration, Instant};
        let (_, factory, mut registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&factory)).unwrap();
        let rung =
            OptionalRendition::new(scope("p", "bounded"), descriptor(), Vec::new(), Vec::new());
        let producer = factory.optional_producer(rung.scope.output_index, GENERATION);
        let _guard = publisher
            .register_optional(vec![rung.with_producer(producer.clone())])
            .unwrap();
        let main = factory
            .subscribe(&WhepScope::Output("out-main".into()), GENERATION)
            .unwrap();
        let packet = || {
            let mut p = Packet::copy(&[0, 0, 0, 1, 0x65, 0x88, 0x11]);
            p.set_flags(Flags::KEY);
            EncodedPacket::from_packet(p)
        };
        let now = Instant::now();
        let span = Duration::from_millis(40);
        for _ in 0..8 {
            producer.offer_complete(vec![packet()], span, now).unwrap();
        }
        for _ in 0..1000 {
            assert_eq!(
                producer.offer_complete(vec![packet()], span, now),
                Err(SubscribeError::Capacity)
            );
        }
        registry.offer_complete(0, GENERATION, &[packet()], span, now);
        assert!(main.try_recv_window().unwrap().is_some());
        registry.pump(now);
        producer.offer_complete(vec![packet()], span, now).unwrap();
    }

    #[test]
    fn registration_requires_matching_actual_producer() {
        let (_, factory, _registry) = fixture();
        let (_, other, _other_registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&factory)).unwrap();
        let make =
            || OptionalRendition::new(scope("p", "optional"), descriptor(), Vec::new(), Vec::new());
        assert_eq!(
            publisher.register_optional(vec![make()]).unwrap_err(),
            OptionalRegisterError::MissingProducer
        );
        let rung = make();
        let wrong = other.optional_producer(rung.scope.output_index, GENERATION);
        assert!(matches!(
            publisher.register_optional(vec![rung.with_producer(wrong)]),
            Err(OptionalRegisterError::InvalidScope(_))
        ));
        let rung = make();
        let stale = factory.optional_producer(rung.scope.output_index, GENERATION + 1);
        assert!(matches!(
            publisher.register_optional(vec![rung.with_producer(stale)]),
            Err(OptionalRegisterError::InvalidScope(_))
        ));
        assert_eq!(factory.effective().len(), 1);
    }

    #[test]
    fn register_publishes_preview_beside_untouched_production() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let registration = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("p720", "preview-720"),
                descriptor(),
                Vec::new(),
                Vec::new(),
            )])
            .expect("a valid preview registers");
        assert_eq!(registration.generation(), GENERATION);
        assert_eq!(registration.keys().len(), 1);

        let factory = current(&slot);
        // The preview resolves at the unchanged production generation.
        assert!(factory
            .resolve(&WhepScope::Output("preview-720".into()))
            .is_ok());
        // Production is untouched, dependencies included.
        let main = factory
            .resolve(&WhepScope::Output("out-main".into()))
            .expect("production still resolves");
        assert_eq!(main.output_index, 0);
        assert_eq!(
            main.source_dependencies().collect::<Vec<_>>(),
            vec!["src-out-main"]
        );
        assert_eq!(factory.generation(), GENERATION);
    }

    #[test]
    fn register_is_all_or_nothing_and_rejects_duplicates() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let ids = |f: &SubscriptionFactory| -> Vec<String> {
            f.effective().iter().map(|x| x.output_id.clone()).collect()
        };
        let before = ids(&current(&slot));

        // Two renditions with the same output_id: the whole call is refused.
        let err = publisher
            .register_optional_metadata(vec![
                OptionalRendition::new(
                    scope("a", "preview-dup"),
                    descriptor(),
                    Vec::new(),
                    Vec::new(),
                ),
                OptionalRendition::new(
                    scope("b", "preview-dup"),
                    descriptor(),
                    Vec::new(),
                    Vec::new(),
                ),
            ])
            .unwrap_err();
        assert_eq!(
            err,
            OptionalRegisterError::DuplicateScope("preview-dup".to_owned())
        );
        assert_eq!(
            ids(&current(&slot)),
            before,
            "a refused batch publishes nothing"
        );

        // Same scope key twice (same profile and id), and an id that collides
        // with production, are both refused.
        assert!(matches!(
            publisher.register_optional_metadata(vec![
                OptionalRendition::new(scope("p", "s1"), descriptor(), Vec::new(), Vec::new()),
                OptionalRendition::new(scope("p", "s1"), descriptor(), Vec::new(), Vec::new()),
            ]),
            Err(OptionalRegisterError::DuplicateScope(_))
        ));
        assert!(matches!(
            publisher.register_optional_metadata(vec![OptionalRendition::new(
                scope("p", "out-main"),
                descriptor(),
                Vec::new(),
                Vec::new()
            )]),
            Err(OptionalRegisterError::DuplicateScope(_))
        ));
        // Production survived every refusal.
        assert!(current(&slot)
            .resolve(&WhepScope::Output("out-main".into()))
            .is_ok());
    }

    #[test]
    fn register_refuses_a_descriptor_that_was_not_opened() {
        let (_slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let mut unopened = descriptor();
        unopened.recovery = None; // no verifiable random-access point
        assert!(matches!(
            publisher.register_optional_metadata(vec![OptionalRendition::new(
                scope("p", "preview-x"),
                unopened,
                Vec::new(),
                Vec::new()
            )]),
            Err(OptionalRegisterError::InvalidDescriptor(_))
        ));
        let mut empty_codec = descriptor();
        empty_codec.codec.clear();
        assert!(matches!(
            publisher.register_optional_metadata(vec![OptionalRendition::new(
                scope("p", "preview-y"),
                empty_codec,
                Vec::new(),
                Vec::new()
            )]),
            Err(OptionalRegisterError::InvalidDescriptor(_))
        ));
        assert!(matches!(
            publisher.register_optional_metadata(vec![OptionalRendition::new(
                PreviewScope {
                    output_id: String::new(),
                    ..scope("p", "z")
                },
                descriptor(),
                Vec::new(),
                Vec::new()
            )]),
            Err(OptionalRegisterError::InvalidScope(_))
        ));
    }

    #[test]
    fn retire_removes_only_the_optional_bindings() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let registration = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("p1440", "preview-1440"),
                descriptor(),
                Vec::new(),
                Vec::new(),
            )])
            .expect("registers");
        assert!(current(&slot)
            .resolve(&WhepScope::Output("preview-1440".into()))
            .is_ok());

        registration.retire().expect("first retire wins");
        let after = current(&slot);
        assert_eq!(
            after.resolve(&WhepScope::Output("preview-1440".into())),
            Err(SubscribeError::Unavailable),
            "the preview is gone, typed"
        );
        assert!(
            after.resolve(&WhepScope::Output("out-main".into())).is_ok(),
            "production was never retired"
        );
        // Idempotent: an explicit retire then Drop does not double-restore.
        assert_eq!(
            registration.retire(),
            Err(OptionalRegisterError::NotRegistered)
        );
    }

    #[test]
    fn retirement_drops_encoder_owner_but_in_flight_packet_keeps_payload() {
        use ffmpeg_next::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::EncodedPacket;
        use std::time::{Duration, Instant};

        let (slot, production, mut registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let owner = Arc::new(AtomicBool::new(false));
        let weak = Arc::downgrade(&owner);
        let optional = scope("p", "preview-lease");
        let producer = production.optional_producer(optional.output_index, GENERATION);
        let registration = publisher
            .register_optional(vec![OptionalRendition::new(
                optional,
                descriptor(),
                Vec::new(),
                vec![ResourceLease::new(owner)],
            )
            .with_producer(producer.clone())])
            .unwrap();
        // No extra strong Arc: only the active registration owns the encoder.
        assert_eq!(weak.strong_count(), 1);
        let sub = current(&slot)
            .subscribe(&WhepScope::Output("preview-lease".into()), GENERATION)
            .unwrap();
        let payload = [0, 0, 0, 1, 0x65, 0x88, 0x11];
        let mut raw = Packet::copy(&payload);
        raw.set_flags(Flags::KEY);
        let now = Instant::now();
        producer
            .offer_complete(
                vec![EncodedPacket::from_packet(raw)],
                Duration::from_millis(40),
                now,
            )
            .unwrap();
        registry.pump(now);
        let received = sub.try_recv_window().unwrap().unwrap();
        let packets = received.clone_window().unwrap();
        drop(received);
        drop(registration);
        assert!(
            weak.upgrade().is_none(),
            "retire releases the actual encoder owner"
        );
        assert_eq!(packets[0].payload().unwrap(), payload);
        assert_eq!(
            current(&slot).resolve(&WhepScope::Output("preview-lease".into())),
            Err(SubscribeError::Unavailable)
        );
    }

    #[test]
    fn factory_clones_cannot_create_a_second_publisher_even_after_drop() {
        let (_slot, production, _registry) = fixture();
        let other_handle = Arc::new(production.as_ref().clone());
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let registration = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("p", "preview-owner"),
                descriptor(),
                Vec::new(),
                Vec::new(),
            )])
            .unwrap();
        assert!(matches!(
            OptionalRegistryPublisher::new(Arc::clone(&other_handle)),
            Err(OptionalRegisterError::AlreadyOwned)
        ));
        assert!(other_handle
            .resolve(&WhepScope::Output("preview-owner".into()))
            .is_ok());
        drop(registration);
        drop(publisher);
        assert!(matches!(
            OptionalRegistryPublisher::new(other_handle),
            Err(OptionalRegisterError::AlreadyOwned)
        ));
    }

    #[test]
    fn retired_index_cannot_be_reused_and_invalid_batches_do_not_publish() {
        let (_slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let rendition = |id: &str, index| {
            let mut scoped = scope("p", id);
            scoped.output_index = index;
            OptionalRendition::new(scoped, descriptor(), Vec::new(), Vec::new())
        };
        let registration = publisher
            .register_optional_metadata(vec![rendition("first", 10)])
            .unwrap();
        registration.retire().unwrap();
        for batch in [
            vec![rendition("reuse", 10)],
            vec![rendition("duplicate-a", 11), rendition("duplicate-b", 11)],
            vec![rendition("overflow", usize::MAX)],
        ] {
            let before = production.snapshot();
            assert_eq!(
                publisher.register_optional_metadata(batch).unwrap_err(),
                OptionalRegisterError::InvalidScope("output_index is already routed".into())
            );
            assert!(
                Arc::ptr_eq(&before, &production.snapshot()),
                "no publication on refusal"
            );
        }
        // Refused batches neither advance the watermark nor consume capacity.
        let _next = publisher
            .register_optional_metadata(vec![rendition("next", 11)])
            .unwrap();
    }

    #[test]
    fn retiring_one_registration_keeps_other_rungs_and_production() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let first = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("r1", "preview-r1"),
                descriptor(),
                Vec::new(),
                Vec::new(),
            )])
            .expect("first rung");
        let second = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("r2", "preview-r2"),
                descriptor(),
                Vec::new(),
                Vec::new(),
            )])
            .expect("second rung");
        assert!(current(&slot)
            .resolve(&WhepScope::Output("preview-r2".into()))
            .is_ok());

        // Retiring the FIRST must not erase the second: the optional set has one
        // serialized owner and each registration removes only its own ids.
        drop(first);
        let after = current(&slot);
        assert_eq!(
            after.resolve(&WhepScope::Output("preview-r1".into())),
            Err(SubscribeError::Unavailable),
            "the retired rung is gone"
        );
        assert!(
            after
                .resolve(&WhepScope::Output("preview-r2".into()))
                .is_ok(),
            "the concurrent rung survives the other's retirement"
        );
        assert!(
            after.resolve(&WhepScope::Output("out-main".into())).is_ok(),
            "production survives"
        );
        drop(second);
        assert!(current(&slot)
            .resolve(&WhepScope::Output("out-main".into()))
            .is_ok());
        assert_eq!(current(&slot).effective().len(), 1);
    }

    #[test]
    fn the_global_optional_total_is_enforced_across_calls() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let mut guards = Vec::new();
        for i in 0..MAX_OPTIONAL_TOTAL {
            guards.push(
                publisher
                    .register_optional_metadata(vec![OptionalRendition::new(
                        scope(&format!("g{i}"), &format!("preview-g{i}")),
                        descriptor(),
                        Vec::new(),
                        Vec::new(),
                    )])
                    .unwrap_or_else(|e| panic!("rung {i}: {e:?}")),
            );
        }
        assert_eq!(current(&slot).effective().len(), 1 + MAX_OPTIONAL_TOTAL);
        // The ninth ACROSS CALLS is refused: the cap is global, not per call.
        assert_eq!(
            publisher
                .register_optional_metadata(vec![OptionalRendition::new(
                    scope("g9", "preview-g9"),
                    descriptor(),
                    Vec::new(),
                    Vec::new()
                )])
                .unwrap_err(),
            OptionalRegisterError::Capacity
        );
        assert_eq!(
            current(&slot).effective().len(),
            1 + MAX_OPTIONAL_TOTAL,
            "a refused call publishes nothing"
        );
        // Retiring one frees exactly one reservation.
        drop(guards.pop());
        assert!(publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("g9", "preview-g9"),
                descriptor(),
                Vec::new(),
                Vec::new()
            )])
            .is_ok());
    }

    #[test]
    fn a_capacity_overflow_is_refused_without_publishing() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let ids = |f: &SubscriptionFactory| -> Vec<String> {
            f.effective().iter().map(|x| x.output_id.clone()).collect()
        };
        let before = ids(&current(&slot));
        let too_many: Vec<_> = (0..=MAX_OPTIONAL_RENDITIONS)
            .map(|i| {
                OptionalRendition::new(
                    scope(&format!("p{i}"), &format!("preview-{i}")),
                    descriptor(),
                    Vec::new(),
                    Vec::new(),
                )
            })
            .collect();
        assert_eq!(
            publisher.register_optional_metadata(too_many).unwrap_err(),
            OptionalRegisterError::Capacity
        );
        assert!(publisher.register_optional_metadata(Vec::new()).is_err());
        assert_eq!(ids(&current(&slot)), before);
    }

    #[test]
    fn a_stale_generation_is_still_refused_by_the_published_factory() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let _registration = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("p", "preview-gen"),
                descriptor(),
                Vec::new(),
                Vec::new(),
            )])
            .expect("registers");
        let factory = current(&slot);
        // Present generation resolves; a caller that resolved against another
        // effective configuration is refused, exactly as for production.
        assert!(factory
            .subscribe(&WhepScope::Output("preview-gen".into()), GENERATION)
            .is_ok());
        assert_eq!(
            factory
                .subscribe(&WhepScope::Output("preview-gen".into()), GENERATION + 1)
                .map(|_| ())
                .unwrap_err(),
            SubscribeError::StaleGeneration
        );
    }

    #[test]
    fn registering_does_not_change_production_bindings() {
        let (slot, production, _registry) = fixture();
        let publisher = OptionalRegistryPublisher::new(Arc::clone(&production)).unwrap();
        let before = production.effective().as_ref().clone();
        let registration = publisher
            .register_optional_metadata(vec![OptionalRendition::new(
                scope("p", "preview-auth"),
                descriptor(),
                vec![Dependency {
                    kind: DependencyKind::Source,
                    id: "src-preview".to_owned(),
                }],
                Vec::new(),
            )])
            .expect("registers");
        let composed = current(&slot).effective();
        // The production slice is byte-for-byte the same prefix: same bindings,
        // same dependencies, same generation.
        assert_eq!(composed[..before.len()], before[..]);
        assert_eq!(composed.len(), before.len() + 1);
        assert_eq!(composed[before.len()].output_id, "preview-auth");
        // The preview carries only its own declared dependency.
        assert_eq!(
            composed[before.len()]
                .source_dependencies()
                .collect::<Vec<_>>(),
            vec!["src-preview"]
        );
        registration.retire().expect("retire");
        assert_eq!(current(&slot).effective().as_ref(), &before);
    }
}
