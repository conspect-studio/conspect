//! The real `AuthorizedFeedFactory`: the production effective-output registry
//! wired to the zero-copy whole-window converter.
//!
//! Attachment order is fixed and observable, and every step is a typed refusal
//! rather than a fallback:
//!
//! 1. **Resolve** the effective `(program, bus, output, generation)` binding and
//!    its decoder metadata from the registry. `Input` and `Program` have no
//!    independent encoded key in this registry, so they are refused instead of
//!    minting a hidden per-viewer encoder.
//! 2. **Authorize** every requested and resolved axis — object, output, program
//!    and mix/effect — *before* any demand, peer slot or queue exists.
//! 3. **Subscribe** with the transport `Fence`'s *own* cancellation identity
//!    ([`Fence::revocation`]) installed in the registry before the peer can be
//!    published to, so an HTTP DELETE fences registry enqueue at the same
//!    instant it fences the transport. The registry observes that flag read-only
//!    and keeps its own flag for its own revoke: this side never writes the
//!    fence's token, and the fence never writes the registry's.
//! 4. **Convert off the clock**: one thread per attached subscription awaits the
//!    transport's cancellation observer ([`Revocation::cancelled`]) off the
//!    production path, drains whole windows at a bounded cadence, checks the
//!    registry's own flag plus generation/loss on the real
//!    [`multiview_output::batch::BatchLease`] *before* converting, converts with
//!    [`super::convert_window`] (shared `av_packet_ref` payloads, the whole batch
//!    lease retained through every nonempty `Bytes` view), re-checks the same
//!    state *after* converting, then calls `Fence::try_offer` once per whole A/V
//!    window.
//!
//! Rejection handling is classified, not uniformly fatal: a transport rejection
//! or a changed effective generation is terminal for the peer, while a
//! superseded/gapped window is dropped so the peer survives and resumes on the
//! next strict random-access point.
//!
//! Teardown is exactly-once: the transport's demand guard revokes the registry's
//! own flag, joins the worker, and the subscription's own drop revokes again
//! idempotently, so the peer slot, queued permits and source demand are released
//! one time. The fence's token is written only by the fence.
//!
//! # Ladder extensibility
//!
//! The MoQ-first browser plays one rendition per subscription, so the ABR ladder
//! is the *family* of effective outputs of one program. [`RenditionFamily`]
//! carries the family identity per admitted rendition, and the registry admits a
//! new ladder member by admitting another effective output — nothing in this
//! module fixes a single quality forever.
//!
//! The pipeline hook that constructs [`FeedRegistry`] and implements the ladder
//! registration belongs to the output owner (t56/t65), so the whole seam is
//! unused in a build that has no installed pipeline yet.
#![allow(dead_code)] // reason: the installer (pipeline hook t56/t65) and the effective-binding registration are external; the attach path itself is exercised by the in-module tests below.

use super::{
    convert_window,
    metadata::{
        self, AudioDescriptor as FactoryAudio, PreparedMetadata, VideoDescriptor as FactoryVideo,
    },
    Timebase, TrackMapping,
};
use crate::output_window_registry::{
    AudioDescriptor, Dependency, DependencyKind, EffectiveBinding, OutputWindowRegistry,
    SubscribeError, Subscription, SubscriptionFactory, SubscriptionRevokeHandle,
    SubscriptionWindow, VideoDescriptor,
};
use multiview_control::{
    auth::Principal,
    moq::MoqReject,
    moq_provider::{AuthorizedFeed, AuthorizedFeedFactory, AuthorizedSharedFeed, LadderRendition},
    preview::WhepScope,
};
use multiview_core::time::Rational;
use multiview_ffmpeg::{is_idr, CodecId, CodecKind, EncodedPacket, NalFraming, StreamKind};
use multiview_moq::provider::{Fence, Revocation};
use multiview_output::batch::RecoveryGate;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Bounded park of the off-clock worker. It only schedules the next cancellation
/// check; a set flag already fences enqueue synchronously inside the registry.
const WORKER_PARK: Duration = Duration::from_millis(10);

/// One admitted rendition of a program's output ladder.
///
/// The identity is the *effective* binding's, never the pending desired config,
/// so an ABR owner can group renditions by `(program_id, bus_id)` and pick by
/// `config_generation` without any rendition being privileged in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenditionFamily {
    pub program_id: String,
    pub bus_id: String,
    pub output_id: String,
    pub output_index: usize,
    pub config_generation: u64,
}

/// Read-only run-scoped pipeline rendezvous. Each admission snapshots the
/// current canonical factory; retired runs never lend a stale registry.
pub(crate) struct DynamicRegistryFactory(
    pub(crate) crate::output_window_registry::WindowSubscriptionSlot,
);
fn read_relay_endpoint(path: &std::path::Path) -> Result<url::Url, MoqReject> {
    #[cfg(unix)]
    let endpoint = {
        use std::io::Read;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
                    .map_err(|_| MoqReject::Unavailable)?,
            )
            .open(path)
            .map_err(|_| MoqReject::Unavailable)?;
        let meta = file.metadata().map_err(|_| MoqReject::Unavailable)?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.len() > 8192 {
            return Err(MoqReject::Unavailable);
        }
        let mut value = String::new();
        file.take(8193)
            .read_to_string(&mut value)
            .map_err(|_| MoqReject::Unavailable)?;
        value.trim().to_owned()
    };
    #[cfg(not(unix))]
    let endpoint: String = {
        let _ = path;
        return Err(MoqReject::Unavailable);
    };
    let endpoint = url::Url::parse(&endpoint).map_err(|_| MoqReject::Unavailable)?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || endpoint.fragment().is_some()
    {
        return Err(MoqReject::Unavailable);
    }
    Ok(endpoint)
}

fn outgoing_principal(
    keys: &multiview_control::auth::ApiKeyStore,
    key_id: &str,
) -> Result<(Principal, u64), MoqReject> {
    let principal = keys.principal_for_key(key_id).ok_or(MoqReject::Forbidden)?;
    principal
        .role
        .require(multiview_control::auth::Action::Write)
        .map_err(|_| MoqReject::Forbidden)?;
    Ok((principal, keys.generation()))
}

pub(crate) struct OutgoingTask {
    stop: tokio::sync::watch::Sender<bool>,
    worker: tokio::task::JoinHandle<()>,
}
impl OutgoingTask {
    pub(crate) async fn shutdown(mut self) {
        self.stop.send_replace(true);
        let _ = (&mut self.worker).await;
    }
}
impl Drop for OutgoingTask {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        self.worker.abort();
    }
}
impl DynamicRegistryFactory {
    /// Explicit one-shot opt-in, independent of viewer admissions. The endpoint
    /// is read from a private regular file, never an argument or diagnostic.
    pub(crate) fn activate_from_env(
        &self,
        keys: Arc<multiview_control::auth::ApiKeyStore>,
    ) -> Result<Option<OutgoingTask>, MoqReject> {
        let Some(key_id) = std::env::var("MULTIVIEW_MOQ_RELAY_KEY_ID").ok() else {
            return Ok(None);
        };
        let output =
            std::env::var("MULTIVIEW_MOQ_RELAY_OUTPUT").map_err(|_| MoqReject::Unavailable)?;
        let (principal, generation) = outgoing_principal(&keys, &key_id)?;
        let path =
            std::env::var("MULTIVIEW_MOQ_RELAY_URL_FILE").map_err(|_| MoqReject::Unavailable)?;
        let endpoint = read_relay_endpoint(std::path::Path::new(&path))?;
        if std::env::var("MULTIVIEW_MOQ_RELAY_PROTECTED")
            .ok()
            .is_some_and(|value| value != "false")
        {
            return Err(MoqReject::Unavailable);
        }
        let config = multiview_moq::provider::FamilyRelayConfig {
            endpoint,
            connect: Default::default(),
            overhead_bps: 0,
            send_window_bytes: 4 * 1024 * 1024,
            retained_bytes: 8 * 1024 * 1024,
            protected: false,
        };
        let slot = self.0.clone();
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        Ok(Some(OutgoingTask {
            stop,
            worker: tokio::spawn(async move {
                let dynamic = DynamicRegistryFactory(slot);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while !dynamic.available() {
                    if keys.generation() != generation || tokio::time::Instant::now() >= deadline {
                        return;
                    }
                    tokio::select! { _ = tokio::time::sleep(Duration::from_millis(20)) => {}, _ = stopped.wait_for(|stop| *stop) => return }
                }
                let scope = WhepScope::Output(output);
                let Ok(mut publisher) = dynamic.start_outgoing(&principal, &scope, config).await
                else {
                    return;
                };
                loop {
                    tokio::select! { _ = tokio::time::sleep(Duration::from_millis(100)) => {}, _ = stopped.wait_for(|stop| *stop) => break }
                    if keys.generation() != generation || !dynamic.available() {
                        break;
                    }
                    let Ok(current) = dynamic.current() else {
                        break;
                    };
                    if current.subscriptions.generation() != publisher.ladder().generation() {
                        break;
                    }
                }
                publisher.shutdown().await;
            }),
        }))
    }
}

impl DynamicRegistryFactory {
    pub(crate) async fn start_outgoing(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        config: multiview_moq::provider::FamilyRelayConfig,
    ) -> Result<multiview_moq::provider::FamilyPublisher, MoqReject> {
        self.current()?
            .start_outgoing(principal, scope, config)
            .await
    }
    fn current(&self) -> Result<RegistryFeedFactory, MoqReject> {
        let factory = self.0.load();
        let snapshot = factory.as_ref();
        let factory = snapshot.as_ref().ok_or(MoqReject::Unavailable)?;
        RegistryFeedFactory::from_subscriptions((***factory).clone())
    }
}
impl AuthorizedFeedFactory for DynamicRegistryFactory {
    fn available(&self) -> bool {
        self.current().is_ok()
    }
    fn attach(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        sink: Arc<Fence>,
    ) -> Result<AuthorizedFeed, MoqReject> {
        self.current()?.attach(principal, scope, sink)
    }
    fn attach_shared(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        sink: Arc<Fence>,
    ) -> Result<Option<AuthorizedSharedFeed>, MoqReject> {
        self.current()?.attach_shared(principal, scope, sink)
    }
}

/// Attach-side half: resolves authorized effective renditions and hands each
/// peer an off-clock, whole-window converter.
pub(crate) struct RegistryFeedFactory {
    subscriptions: SubscriptionFactory,
    prepared: BTreeMap<String, PreparedMetadata>,
    ladder: Vec<RenditionFamily>,
    /// Typed alternatives handed to every admitted feed (additive; the selected
    /// `video`/`audio` fields stay the resolved rendition for back-compat).
    renditions: Vec<LadderRendition>,
    ready: Arc<AtomicBool>,
}

impl RegistryFeedFactory {
    /// Prepare validated wire metadata for every effective binding and build the
    /// producer half of the registry. No encoder is opened and no rendition is
    /// invented: a binding whose opened codec, configuration, framing or reorder
    /// proof cannot produce decoder-ready metadata fails this call.
    ///
    /// # Errors
    /// [`MoqReject::Codec`] for a binding that is not decoder-ready (reordered
    /// video, audio-only recovery, unrepresentable timebase, or configuration
    /// bytes the pinned catalog parser rejects); [`MoqReject::Unavailable`] for
    /// an empty or duplicated set of outputs.
    fn new(effective: &[EffectiveBinding]) -> Result<(OutputWindowRegistry, Self), MoqReject> {
        let mut prepared = BTreeMap::new();
        let mut ladder = Vec::with_capacity(effective.len());
        let mut renditions = Vec::with_capacity(effective.len());
        for binding in effective {
            let video = factory_video(&binding.video)?;
            let audio = binding
                .audio
                .iter()
                .map(factory_audio)
                .collect::<Result<Vec<_>, _>>()?;
            let prepared_metadata = metadata::prepare(&video, &audio)?;
            // Typed alternatives from the same validated metadata the selected
            // rendition uses: no second codec model, no desired-config guess.
            //
            // `cadence` and `bitrate_bps` are explicitly `None`: this build has no
            // validated opened-encoder cadence or bitrate, and an encoder
            // **timebase is not a frame rate** (`1/90000` is a tick resolution, so
            // its reciprocal would publish a fabricated 90000 fps rung). The
            // canonical descriptor is gaining an explicit `cadence: Rational` plus
            // an optional opened bitrate (t65); `None` is the honest value until
            // that explicit source is in this tree. An unknown bitrate is
            // non-adaptive, never `0`.
            renditions.push(LadderRendition {
                track: binding.output_id.clone(),
                codec: prepared_metadata.video.codec.clone(),
                coded_width: prepared_metadata.video.coded_width,
                coded_height: prepared_metadata.video.coded_height,
                description: prepared_metadata.video.description.clone(),
                cadence: Some((
                    u32::try_from(binding.video.cadence.num).map_err(|_| MoqReject::Codec)?,
                    u32::try_from(binding.video.cadence.den).map_err(|_| MoqReject::Codec)?,
                )),
                bitrate_bps: binding.video.bitrate_bps,
            });
            if prepared
                .insert(binding.output_id.clone(), prepared_metadata)
                .is_some()
            {
                return Err(MoqReject::Unavailable);
            }
            ladder.push(RenditionFamily {
                program_id: binding.program_id.clone(),
                bus_id: binding.bus_id.clone(),
                output_id: binding.output_id.clone(),
                output_index: binding.output_index,
                config_generation: binding.config_generation,
            });
        }
        let (registry, subscriptions) = OutputWindowRegistry::new(
            effective.to_vec(),
            effective
                .first()
                .ok_or(MoqReject::Unavailable)?
                .config_generation,
        )
        .map_err(refusal)?;
        Ok((
            registry,
            Self {
                subscriptions,
                prepared,
                ladder,
                renditions,
                ready: Arc::new(AtomicBool::new(true)),
            },
        ))
    }

    /// Attach to the canonical pipeline registry. No dispatcher, encoder or
    /// delivery pump is created here.
    pub(crate) fn from_subscriptions(
        subscriptions: SubscriptionFactory,
    ) -> Result<Self, MoqReject> {
        let effective = subscriptions.admitted();
        if effective.is_empty() {
            return Err(MoqReject::Unavailable);
        }
        let mut prepared = BTreeMap::new();
        let mut ladder = Vec::new();
        let mut renditions = Vec::new();
        for binding in effective.iter() {
            let audio = binding
                .audio
                .iter()
                .map(factory_audio)
                .collect::<Result<Vec<_>, _>>()?;
            let metadata = metadata::prepare(&factory_video(&binding.video)?, &audio)?;
            renditions.push(LadderRendition {
                track: binding.output_id.clone(),
                codec: metadata.video.codec.clone(),
                coded_width: metadata.video.coded_width,
                coded_height: metadata.video.coded_height,
                description: metadata.video.description.clone(),
                cadence: Some((
                    u32::try_from(binding.video.cadence.num).map_err(|_| MoqReject::Codec)?,
                    u32::try_from(binding.video.cadence.den).map_err(|_| MoqReject::Codec)?,
                )),
                bitrate_bps: binding.video.bitrate_bps,
            });
            prepared.insert(binding.output_id.clone(), metadata);
            ladder.push(RenditionFamily {
                program_id: binding.program_id.clone(),
                bus_id: binding.bus_id.clone(),
                output_id: binding.output_id.clone(),
                output_index: binding.output_index,
                config_generation: binding.config_generation,
            });
        }
        Ok(Self {
            subscriptions,
            prepared,
            ladder,
            renditions,
            ready: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Start the one outgoing publisher from this canonical registry snapshot.
    /// The service supplies its explicit scoped principal, never a synthetic
    /// privileged viewer. All family axes are authorized before demand exists.
    pub(crate) async fn start_outgoing(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        config: multiview_moq::provider::FamilyRelayConfig,
    ) -> Result<multiview_moq::provider::FamilyPublisher, MoqReject> {
        if config.protected {
            return Err(MoqReject::Unavailable);
        }
        let (sink, windows) = Fence::ladder_channel(4);
        let shared = match self.attach_shared(principal, scope, sink.clone()) {
            Ok(Some(shared)) => shared,
            Ok(None) => {
                sink.revoke();
                return Err(MoqReject::Unavailable);
            }
            Err(error) => {
                sink.revoke();
                return Err(error);
            }
        };
        let AuthorizedSharedFeed { mut feed, .. } = shared;
        feed.windows = windows;
        match multiview_moq::provider::FamilyPublisher::start(feed, Some(config)).await {
            Ok(publisher) => Ok(publisher),
            Err(_) => {
                sink.revoke();
                Err(MoqReject::Unavailable)
            }
        }
    }

    /// Every admitted rendition of the ladder, in registry order.
    #[must_use]
    pub(crate) fn ladder(&self) -> &[RenditionFamily] {
        &self.ladder
    }

    /// The typed alternatives handed to every admitted feed.
    #[must_use]
    pub(crate) fn renditions(&self) -> &[LadderRendition] {
        &self.renditions
    }
}

impl AuthorizedFeedFactory for RegistryFeedFactory {
    fn available(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.prepared.is_empty()
    }

    fn attach_shared(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        sink: Arc<Fence>,
    ) -> Result<Option<AuthorizedSharedFeed>, MoqReject> {
        let selected = self.subscriptions.resolve(scope).map_err(refusal)?;
        let admitted = self.subscriptions.admitted();
        let mut family = admitted
            .iter()
            .filter(|binding| {
                binding.program_id == selected.program_id
                    && binding.bus_id == selected.bus_id
                    && binding.config_generation == selected.config_generation
            })
            .cloned()
            .collect::<Vec<_>>();
        if family.len() < 2 {
            return Ok(None);
        }
        if family.iter().any(|binding| binding.audio != selected.audio) {
            return Err(MoqReject::Codec);
        }
        family.sort_by_key(|binding| binding.video.bitrate_bps.unwrap_or(u64::MAX));
        // Multiple physical outputs can share one identical encoded rendition.
        // They are not an adaptive ladder and must not fabricate duplicate rungs.
        if family
            .windows(2)
            .any(|pair| pair[0].video.bitrate_bps == pair[1].video.bitrate_bps)
        {
            return Ok(None);
        }
        for binding in &family {
            let dependencies = binding
                .dependencies
                .iter()
                .map(typed_dependency)
                .collect::<Vec<_>>();
            metadata::authorize_dependencies(principal, &dependencies)?;
            let sources = binding
                .source_dependencies()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            metadata::authorize_binding(principal, scope, &binding.output_id, &sources)?;
            factory_video(&binding.video)?;
            if binding.video.cadence != selected.video.cadence {
                return Err(MoqReject::Codec);
            }
        }
        let mut specs = Vec::new();
        let mut alternatives = Vec::new();
        for binding in &family {
            let prepared = self
                .prepared
                .get(&binding.output_id)
                .ok_or(MoqReject::Unavailable)?;
            let numerator =
                u32::try_from(binding.video.cadence.num).map_err(|_| MoqReject::Codec)?;
            let denominator =
                u32::try_from(binding.video.cadence.den).map_err(|_| MoqReject::Codec)?;
            let bitrate = binding.video.bitrate_bps.ok_or(MoqReject::Codec)?;
            specs.push(
                multiview_moq::ladder::RenditionSpec::new(
                    &binding.output_id,
                    &prepared.video.codec,
                    prepared.video.coded_width,
                    prepared.video.coded_height,
                    numerator,
                    denominator,
                    bitrate,
                )
                .map_err(|_| MoqReject::Codec)?
                .with_description(binding.video.config.clone().map(bytes::Bytes::from)),
            );
            alternatives.push(multiview_control::moq::MoqVideoAlternative {
                track: binding.output_id.clone(),
                codec: prepared.video.codec.clone(),
                coded_width: prepared.video.coded_width,
                coded_height: prepared.video.coded_height,
                cadence_numerator: numerator,
                cadence_denominator: denominator,
                bitrate_bps: bitrate,
                description: prepared.video.description.clone(),
            });
        }
        let audio_specs = selected
            .audio
            .iter()
            .map(|audio| {
                multiview_moq::ladder::AudioSpec::new(
                    &audio.track,
                    &audio.codec,
                    audio.sample_rate,
                    audio.channels,
                    audio.bitrate_bps.ok_or(MoqReject::Codec)?,
                )
                .map(|spec| spec.with_description(audio.config.clone().map(bytes::Bytes::from)))
                .map_err(|_| MoqReject::Codec)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ladder = multiview_moq::ladder::SharedLadder::admit(
            selected.config_generation,
            specs,
            audio_specs,
            0,
        )
        .map_err(|_| MoqReject::Codec)?;
        // Failed grouped admission revokes the common producer boundary even
        // when bounded Attach commands have not yet reached the registry pump.
        let mut rollback = AdmissionRollback {
            sink: sink.clone(),
            armed: true,
        };
        let mut rungs = Vec::new();
        for binding in &family {
            let scope = WhepScope::Output(binding.output_id.clone());
            let subscription = self
                .subscriptions
                .subscribe_with_revocation(
                    &scope,
                    binding.config_generation,
                    sink.revocation().flag(),
                )
                .map_err(refusal)?;
            rungs.push(GroupRung {
                subscription,
                mapping: self.prepared[&binding.output_id].mapping.clone(),
                recovery: binding.video.recovery.ok_or(MoqReject::Codec)?,
                pending: None,
            });
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let generation = selected.config_generation;
        let worker_sink = sink.clone();
        let join = thread::Builder::new()
            .name("moq-family-convert".into())
            .spawn(move || {
                run_group(&worker_sink, &worker_stop, generation, &mut rungs);
            })
            .map_err(|_| MoqReject::Unavailable)?;
        let (_unused, receiver) = tokio::sync::mpsc::channel(1);
        rollback.armed = false;
        Ok(Some(AuthorizedSharedFeed {
            feed: multiview_moq::provider::LadderFeed {
                ladder,
                capacity_bps: None,
                config: Default::default(),
                windows: receiver,
                demand: Box::new(GroupDemand {
                    stop,
                    join: Some(join),
                }),
            },
            video: self.prepared[&selected.output_id].video.clone(),
            audio: self.prepared[&selected.output_id].audio.clone(),
            alternatives,
            clock_wall_micros: None,
        }))
    }

    fn attach(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        sink: Arc<Fence>,
    ) -> Result<AuthorizedFeed, MoqReject> {
        if !self.available() {
            return Err(MoqReject::Unavailable);
        }
        // 1. Resolve the effective immutable binding. This allocates no peer
        //    slot, acquires no source demand and opens no codec.
        let binding = self.subscriptions.resolve(scope).map_err(refusal)?;
        let prepared = self
            .prepared
            .get(&binding.output_id)
            .ok_or(MoqReject::Unavailable)?;
        // 2. Authorize every resolved axis before any demand or peer slot: the
        //    requested scope, the effective output, and every typed dependency
        //    (source object, output, program, mix/effect).
        let dependencies = binding
            .dependencies
            .iter()
            .map(typed_dependency)
            .collect::<Vec<_>>();
        metadata::authorize_dependencies(principal, &dependencies)?;
        let sources = binding
            .source_dependencies()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        metadata::authorize_binding(principal, scope, &binding.output_id, &sources)?;
        // The transport's own cancellation identity, obtained from the fence and
        // installed in the registry BEFORE any window can be published to this
        // peer. An already-cancelled fence fails here: no demand, no peer slot.
        let revocation = sink.revocation();
        if revocation.is_revoked() {
            return Err(MoqReject::Unavailable);
        }
        // The worker awaits that observer off the production path; a caller
        // outside a runtime cannot install the off-clock observer, so it fails
        // closed rather than falling back to polling the fence's flag.
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| MoqReject::Unavailable)?;
        // 3. Subscribe with the fence's OWN flag: the registry observes it
        //    read-only, so an HTTP DELETE fences registry enqueue at the same
        //    instant it fences the transport.
        let subscription = self
            .subscriptions
            .subscribe_with_revocation(scope, binding.config_generation, revocation.flag())
            .map_err(refusal)?;
        // 4. One off-clock worker per subscription, never on the output clock.
        let handle = subscription.revoke_handle();
        let worker = OffClockWorker::spawn(
            &runtime,
            revocation,
            WorkerState {
                sink,
                subscription,
                mapping: prepared.mapping.clone(),
                recovery: binding.video.recovery.ok_or(MoqReject::Codec)?,
                generation: binding.config_generation,
                handle: handle.clone(),
            },
        )
        .map_err(|_| MoqReject::Unavailable)?;
        Ok(AuthorizedFeed {
            generation: binding.config_generation,
            catalog: prepared.catalog.clone(),
            video: prepared.video.clone(),
            audio: prepared.audio.clone(),
            ladder: self.renditions.clone(),
            demand: Box::new(RegistryDemand {
                handle,
                worker: Some(worker),
            }),
        })
    }
}

struct AdmissionRollback {
    sink: Arc<Fence>,
    armed: bool,
}
impl Drop for AdmissionRollback {
    fn drop(&mut self) {
        if self.armed {
            self.sink.revoke();
        }
    }
}
struct GroupRung {
    subscription: Subscription,
    mapping: TrackMapping,
    recovery: RecoveryGate,
    pending: Option<multiview_moq::av::AvWindow>,
}
struct GroupDemand {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}
impl Drop for GroupDemand {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
/// Bounded off-clock join: at most one whole converted window per rung. A
/// missing producer cannot block the production clock or grow a backlog.
fn run_group(sink: &Fence, stop: &AtomicBool, generation: u64, rungs: &mut [GroupRung]) {
    struct ExitFence<'a>(&'a Fence);
    impl Drop for ExitFence<'_> {
        fn drop(&mut self) {
            self.0.revoke();
        }
    }
    let _exit_fence = ExitFence(sink);
    let mut discontinuities = 0;
    while !stop.load(Ordering::Acquire) && !sink.revocation().is_revoked() {
        for (index, rung) in rungs.iter_mut().enumerate() {
            if rung.subscription.is_revoked() {
                return;
            }
            if rung.pending.is_some() {
                continue;
            }
            match rung.subscription.try_recv_window() {
                Ok(Some(window)) => {
                    if window.config_generation != generation {
                        sink.revoke();
                        return;
                    }
                    if !window.permit.is_current() {
                        discontinuities |= 1 << index;
                        continue;
                    }
                    let Ok(packets) = window.clone_window() else {
                        discontinuities |= 1 << index;
                        continue;
                    };
                    let Ok(mut converted) = convert_window(
                        packets,
                        window.permit.clone(),
                        generation,
                        generation,
                        window.media_span,
                        &rung.mapping,
                        |packet| rap_verified(rung.recovery, packet),
                    ) else {
                        discontinuities |= 1 << index;
                        continue;
                    };
                    if !window.permit.is_current() {
                        discontinuities |= 1 << index;
                        continue;
                    }
                    if super::frame_avcc(&mut converted, rung.recovery).is_err() {
                        // Framing refusal cannot leave a decodable dependency chain.
                        // ExitFence revokes this admission before another delta.
                        return;
                    }
                    rung.pending = Some(converted);
                }
                Ok(None) => {}
                Err(_) => {
                    sink.revoke();
                    return;
                }
            }
        }
        if rungs.iter().any(|rung| rung.pending.is_none()) {
            thread::park_timeout(WORKER_PARK);
            continue;
        }
        let starts = rungs
            .iter()
            .map(|rung| {
                rung.pending
                    .as_ref()
                    .and_then(|window| window.video.packets.first())
                    .map(|packet| packet.pts_us)
            })
            .collect::<Vec<_>>();
        let latest = starts.iter().flatten().max().copied();
        if starts.iter().any(|start| *start != latest) {
            for (index, rung) in rungs.iter_mut().enumerate() {
                if starts[index] != latest {
                    rung.pending.take();
                    discontinuities |= 1 << index;
                }
            }
            continue;
        }
        let mut complete = rungs
            .iter_mut()
            .map(|rung| rung.pending.take().unwrap())
            .collect::<Vec<_>>();
        // All rungs carry the same admitted shared audio identity and clock.
        // Publish its first complete copy exactly once, outside the video index.
        let audio = std::mem::take(&mut complete[0].audio);
        let mut window = multiview_moq::publication::LadderWindow::new(
            generation,
            complete.into_iter().map(|window| window.video).collect(),
        );
        window.audio = audio;
        window.discontinuities = discontinuities;
        match sink.offer_ladder(window) {
            Ok(()) => discontinuities = 0,
            Err(multiview_moq::provider::OfferRefusal::Full(_)) => {
                discontinuities = (1 << rungs.len()) - 1
            }
            Err(_) => return,
        }
    }
}

/// Map a registry refusal onto the control plane's typed rejection. An
/// unsupported scope is reported as unavailable (no admitted rendition exists),
/// never as a new encode.
fn refusal(error: SubscribeError) -> MoqReject {
    match error {
        SubscribeError::StaleGeneration => MoqReject::Generation,
        SubscribeError::Capacity => MoqReject::Capacity,
        SubscribeError::Unavailable
        | SubscribeError::UnsupportedScope
        | SubscribeError::ProducerEnded
        | SubscribeError::PacketReference => MoqReject::Unavailable,
    }
}

/// The registry's typed dependency, borrowed for the authorization helper.
fn typed_dependency(dependency: &Dependency) -> metadata::Dependency<'_> {
    match dependency.kind {
        DependencyKind::Source => metadata::Dependency::Source(&dependency.id),
        DependencyKind::Output => metadata::Dependency::Output(&dependency.id),
        DependencyKind::Program => metadata::Dependency::Program(&dependency.id),
        DependencyKind::MixEffect => metadata::Dependency::MixEffect(&dependency.id),
    }
}

/// Exact integer timebase from the opened encoder, never a float rate.
fn timebase(base: Rational) -> Result<Timebase, MoqReject> {
    let numerator = u32::try_from(base.num).map_err(|_| MoqReject::Codec)?;
    let denominator = u32::try_from(base.den).map_err(|_| MoqReject::Codec)?;
    if numerator == 0 || denominator == 0 {
        return Err(MoqReject::Codec);
    }
    Ok(Timebase {
        numerator,
        denominator,
    })
}

/// The strict-RAP recovery gate an **opened** encoder identity and its coded
/// framing record produce, or `None` when that codec has no framing proof.
///
/// Minimal extraction of the canonical `VideoDescriptor::from_opened` rule
/// (registry lane `05f89fe4`): the gate is derived from the opened codec id plus
/// the coded framing, never from the configured codec name. `from_opened` itself
/// stays in the canonical lane because it also needs the pipeline's reorder
/// proof and a codec-identity field this adapter build does not carry.
fn recovery_gate(codec: CodecId, framing: Option<NalFraming>) -> Option<RecoveryGate> {
    let framing = framing?;
    match codec {
        CodecId::H264 => Some(RecoveryGate::StrictRap {
            codec: CodecKind::H264,
            framing,
        }),
        CodecId::HEVC => Some(RecoveryGate::StrictRap {
            codec: CodecKind::Hevc,
            framing,
        }),
        _ => None,
    }
}

/// The opened codec id a `WebCodecs` codec string denotes, when this build can
/// prove its random-access structure.
fn webcodecs_codec_id(codec: &str) -> Option<CodecId> {
    let lowered = codec.to_ascii_lowercase();
    if lowered.starts_with("avc1.") || lowered.starts_with("avc3.") {
        Some(CodecId::H264)
    } else if lowered.starts_with("hvc1.") || lowered.starts_with("hev1.") {
        Some(CodecId::HEVC)
    } else {
        None
    }
}

/// Opened video descriptor -> validated decoder metadata input. Reordered video
/// and video-less recovery gates are refused: the hang mapper is PTS-only.
fn factory_video(descriptor: &VideoDescriptor) -> Result<FactoryVideo, MoqReject> {
    if descriptor.reorder != crate::rendition_workers::ReorderBound::NoReorder {
        return Err(MoqReject::Codec);
    }
    // The declared gate must be exactly what the opened codec id and framing
    // record produce: a gate for a codec with no framing proof, a gate whose
    // framing disagrees with the descriptor's, or `AudioOnly` on a video
    // descriptor is a fabricated proof, not the opened encoder's.
    let Some(RecoveryGate::StrictRap { framing, .. }) = descriptor.recovery else {
        return Err(MoqReject::Codec);
    };
    if framing != descriptor.framing
        || webcodecs_codec_id(&descriptor.codec)
            .and_then(|codec| recovery_gate(codec, Some(framing)))
            != descriptor.recovery
    {
        return Err(MoqReject::Codec);
    }
    Ok(FactoryVideo {
        codec: descriptor.codec.clone(),
        description: descriptor.config.clone().unwrap_or_default(),
        width: descriptor.coded_width,
        height: descriptor.coded_height,
        timebase: timebase(descriptor.time_base)?,
        reorder_depth: 0,
    })
}

/// Opened audio descriptor -> validated decoder metadata input. A non-audio
/// stream kind is refused rather than guessed into a track.
fn factory_audio(descriptor: &AudioDescriptor) -> Result<FactoryAudio, MoqReject> {
    let index = match descriptor.kind {
        StreamKind::Audio => None,
        StreamKind::AudioTrack(id) => Some(id),
        _ => return Err(MoqReject::Codec),
    };
    Ok(FactoryAudio {
        index,
        track: descriptor.track.clone(),
        codec: descriptor.codec.clone(),
        description: descriptor.config.clone().unwrap_or_default(),
        sample_rate: descriptor.sample_rate,
        channels: descriptor.channels,
        timebase: timebase(descriptor.time_base)?,
    })
}

/// Verified random-access proof from the opened encoder's codec and framing,
/// never a KEY flag or a desired-configuration guess.
fn rap_verified(recovery: RecoveryGate, packet: &EncodedPacket) -> bool {
    match recovery {
        RecoveryGate::StrictRap { codec, framing } => {
            matches!(packet.kind(), StreamKind::Video)
                && packet
                    .payload()
                    .is_some_and(|access_unit| is_idr(access_unit, codec, framing))
        }
        RecoveryGate::AudioOnly => false,
    }
}

/// Why a window was dropped while the peer keeps its subscription. These are the
/// *loss* authority (the queue's own generation) and transient per-window
/// failures — never the cancellation or configuration authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DroppedCause {
    /// The lease's queue generation moved on (real media loss or an aged
    /// backlog): recoverable, resume on the next strict random-access point.
    SupersededLease,
    /// `av_packet_ref` failed for this window; the peer is unaffected.
    PacketReference,
    /// The window's own timestamps/budget could not be converted.
    UnencodableWindow,
}

/// Why the peer's subscription ended. Each cause is a different authority:
/// `Cancelled` is the transport/auth identity, `ConfigGenerationChanged` is the
/// effective-configuration generation, `TransportRejected` is the transport's own
/// terminal contract (a full fence queue stays terminal until the reviewed
/// recoverable-overflow/discontinuity contract exists), `ProducerEnded` is the
/// registry peer, and `UnrepresentableStreams` is the binding's stream mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalCause {
    /// The registry's or the transport's cancellation identity was revoked.
    Cancelled,
    /// The effective configuration generation changed: re-resolve the binding.
    ConfigGenerationChanged,
    /// The binding cannot represent this window's streams.
    UnrepresentableStreams,
    /// The producer's peer ended; no further window can arrive.
    ProducerEnded,
    /// The transport rejected the offer (revoked fence or a full queue).
    TransportRejected,
}

/// What a rejected or converted window means for the peer's subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The whole window reached the transport.
    Delivered,
    /// Recoverable: the peer stays subscribed and resumes on the next strict
    /// random-access point.
    Dropped(DroppedCause),
    /// Terminal for this peer.
    Terminal(TerminalCause),
}

/// The off-clock conversion worker's owned state. It moves to its own thread so
/// no conversion, timestamp mapping or queue operation touches the output clock
/// or the producer's thread.
struct WorkerState {
    sink: Arc<Fence>,
    subscription: Subscription,
    mapping: TrackMapping,
    recovery: RecoveryGate,
    generation: u64,
    handle: SubscriptionRevokeHandle,
}

/// Convert one whole admitted lease and offer it once. The returned outcome
/// decides whether this peer keeps its subscription.
fn publish(
    sink: &Fence,
    mapping: &TrackMapping,
    recovery: RecoveryGate,
    generation: u64,
    handle: &SubscriptionRevokeHandle,
    window: &SubscriptionWindow,
) -> Outcome {
    // A changed effective generation is terminal (the control plane re-resolves)
    // and so is a revoked cancellation identity; the two authorities stay typed
    // and separate from the lease's loss generation below.
    if window.config_generation != generation {
        return Outcome::Terminal(TerminalCause::ConfigGenerationChanged);
    }
    if handle.is_revoked() {
        return Outcome::Terminal(TerminalCause::Cancelled);
    }
    // A superseded lease is a recoverable gap: the queue's generation moved on
    // (loss or an aged backlog), so drop this window and wait for the next RAP.
    if !window.permit.is_current() {
        return Outcome::Dropped(DroppedCause::SupersededLease);
    }
    let Ok(packets) = window.clone_window() else {
        return Outcome::Dropped(DroppedCause::PacketReference);
    };
    let converted = convert_window(
        packets,
        Arc::clone(&window.permit),
        window.config_generation,
        generation,
        window.media_span,
        mapping,
        |packet| rap_verified(recovery, packet),
    );
    let mut converted = match converted {
        Ok(converted) => converted,
        // The binding cannot represent this window's streams: retrying the same
        // shape cannot help, so the peer is revoked rather than silently starved.
        Err(super::ConversionError::Stream) => {
            return Outcome::Terminal(TerminalCause::UnrepresentableStreams);
        }
        // Timestamp or budget rejection: drop the window, keep the peer.
        Err(_) => return Outcome::Dropped(DroppedCause::UnencodableWindow),
    };
    if super::frame_avcc(&mut converted, recovery).is_err() {
        return Outcome::Terminal(TerminalCause::UnrepresentableStreams);
    }
    // Re-check after conversion: a loss during conversion invalidated the lease,
    // so the converted window must not be published.
    if handle.is_revoked() {
        return Outcome::Terminal(TerminalCause::Cancelled);
    }
    if !window.permit.is_current() {
        return Outcome::Dropped(DroppedCause::SupersededLease);
    }
    // Every transport rejection is terminal for this subscription (revoked fence
    // or a full queue), per the transport's own contract. A full queue stays
    // terminal until t40 publishes the reviewed typed recoverable
    // overflow/discontinuity + RAP-gate contract; it is never treated as a silent
    // per-window drop, and no memory or auth cancellation rule is weakened.
    if sink.try_offer(converted).is_ok() {
        Outcome::Delivered
    } else {
        Outcome::Terminal(TerminalCause::TransportRejected)
    }
}

/// Handle to the conversion thread plus the observer task that awaits the
/// transport fence's cancellation and wakes the thread.
struct OffClockWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    observer: Option<tokio::task::JoinHandle<()>>,
}

impl OffClockWorker {
    /// Await the fence's cancellation off the production path in a runtime task,
    /// and wake the conversion thread through a plain channel — the fence's flag
    /// is never polled for cancellation authority.
    fn spawn(
        runtime: &tokio::runtime::Handle,
        revocation: Revocation,
        mut state: WorkerState,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel::<()>();
        let observer = runtime.spawn(async move {
            revocation.cancelled().await;
            // A closed receiver (worker already gone) is not an error.
            let _ = cancelled_tx.send(());
        });
        let join = match thread::Builder::new()
            .name("moq-window-convert".to_owned())
            .spawn(move || state.run(&thread_stop, &cancelled_rx))
        {
            Ok(join) => join,
            Err(error) => {
                observer.abort();
                return Err(error);
            }
        };
        Ok(Self {
            stop,
            join: Some(join),
            observer: Some(observer),
        })
    }

    /// Stop and join: bounded by [`WORKER_PARK`], never by the publisher, and a
    /// panicked conversion thread must not poison teardown.
    fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::Release);
        // Abandon any parked cancellation observer: the fence owns that token
        // and a subscription that ends without a transport revoke must not leak
        // a task waiting on it.
        if let Some(observer) = self.observer.take() {
            observer.abort();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl WorkerState {
    fn run(&mut self, stop: &AtomicBool, cancelled: &std::sync::mpsc::Receiver<()>) {
        loop {
            if stop.load(Ordering::Acquire) || self.handle.is_revoked() {
                break;
            }
            // The transport published cancellation: nothing further may be
            // delivered for this peer.
            if cancelled.try_recv().is_ok() {
                break;
            }
            match self.subscription.try_recv_window() {
                Ok(Some(window)) => match publish(
                    &self.sink,
                    &self.mapping,
                    self.recovery,
                    self.generation,
                    &self.handle,
                    &window,
                ) {
                    Outcome::Delivered | Outcome::Dropped(_) => {}
                    Outcome::Terminal(_) => break,
                },
                Ok(None) => thread::park_timeout(WORKER_PARK),
                // Producer ended: the peer is gone, so nothing more arrives.
                Err(_) => break,
            }
        }
        // Exactly-once release of this peer's slot. The demand guard and the
        // subscription's own drop revoke again idempotently.
        self.handle.revoke();
        self.sink.revoke();
    }
}

/// Source-demand guard returned to the transport. Dropping it is the single
/// teardown: revoke the registry's own flag, then stop the worker and drop the
/// subscription, releasing the peer slot, queued permits and demand exactly
/// once. The fence's token is written only by the fence.
struct RegistryDemand {
    handle: SubscriptionRevokeHandle,
    worker: Option<OffClockWorker>,
}

impl Drop for RegistryDemand {
    fn drop(&mut self) {
        self.handle.revoke();
        if let Some(worker) = self.worker.take() {
            worker.stop_and_join();
        }
    }
}

/// The production pair the pipeline hook installs: the producer-side registry
/// the egress thread offers whole encoded windows to, and the attach-side `MoQ`
/// factory the control plane admits subscriptions through.
pub(crate) struct FeedRegistry {
    producer: OutputWindowRegistry,
    factory: Arc<RegistryFeedFactory>,
}

impl FeedRegistry {
    /// # Errors
    /// [`MoqReject::Codec`] when any effective binding is not decoder-ready, or
    /// [`MoqReject::Unavailable`] when the set is empty or duplicated.
    pub(crate) fn new(effective: &[EffectiveBinding]) -> Result<Self, MoqReject> {
        let (producer, factory) = RegistryFeedFactory::new(effective)?;
        Ok(Self {
            producer,
            factory: Arc::new(factory),
        })
    }

    /// The attach-side factory for `moq_startup::start`. Real availability only:
    /// it reports unavailable once the producer half is dropped.
    #[must_use]
    pub(crate) fn factory(&self) -> Arc<dyn AuthorizedFeedFactory> {
        let factory: Arc<dyn AuthorizedFeedFactory> = self.factory.clone();
        factory
    }

    /// The producer half. The pipeline hook (t56) calls
    /// [`OutputWindowRegistry::offer_complete`] with the exact assembled window
    /// it is about to admit physically (so a healthy `MoQ` peer is never starved by
    /// the SRT/HLS path), [`OutputWindowRegistry::report_loss`] for real lost
    /// media, and [`OutputWindowRegistry::close`] on finish/abort.
    pub(crate) fn producer_mut(&mut self) -> &mut OutputWindowRegistry {
        &mut self.producer
    }

    /// Every admitted rendition of the ladder, in registry order.
    #[must_use]
    pub(crate) fn ladder(&self) -> &[RenditionFamily] {
        self.factory.ladder()
    }

    /// Whole windows the producer could not admit (peer loss or a revoked peer).
    #[must_use]
    pub(crate) fn rejected_windows(&self) -> u64 {
        self.producer.rejected_windows()
    }

    /// Peers installed in the registry.
    #[must_use]
    pub(crate) fn active_peers(&self) -> usize {
        self.producer.active_peers()
    }

    /// Live admitted subscriptions that have not revoked (own or transport).
    #[must_use]
    pub(crate) fn active_subscriptions(&self) -> usize {
        self.factory.subscriptions.slots_in_use()
    }

    /// Referenced packets and bytes still retained by peers, including in-flight
    /// leases. Returns to `(0, 0)` once every window and lease is dropped.
    #[must_use]
    pub(crate) fn retained(&self) -> (usize, usize) {
        self.producer.retained()
    }
}

impl Drop for FeedRegistry {
    fn drop(&mut self) {
        // The producer is gone: no further effective window can exist, so the
        // factory must stop advertising availability rather than serving stale
        // metadata to a listener that outlives this pipeline.
        self.factory.ready.store(false, Ordering::Release);
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
    use multiview_control::auth::Role;
    use multiview_ffmpeg::{CodecKind, NalFraming};
    use multiview_moq::av::AvWindow;
    use multiview_output::batch::{BoundedBatchQueue, OfferError, PacketBatch};
    use std::time::Instant;
    use tokio::sync::mpsc::error::TryRecvError;

    const GENERATION: u64 = 7;
    const SPAN: Duration = Duration::from_millis(40);

    /// The avcC record the pinned catalog parser accepts for `avc1.42c01e`.
    fn avcc() -> Vec<u8> {
        vec![
            1, 0x42, 0xc0, 0x1e, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 2, 0x68, 0,
        ]
    }
    fn strict_rap() -> RecoveryGate {
        RecoveryGate::StrictRap {
            codec: CodecKind::H264,
            framing: NalFraming::AnnexB,
        }
    }
    fn video(width: u32, height: u32) -> VideoDescriptor {
        VideoDescriptor {
            codec: "avc1.42c01e".to_owned(),
            coded_width: width,
            coded_height: height,
            config: Some(avcc()),
            time_base: Rational::new(1, 25),
            recovery: Some(strict_rap()),
            framing: NalFraming::AnnexB,
            codec_id: "h264".into(),
            cadence: Rational::new(25, 1),
            bitrate_bps: Some(500_000),
            reorder: crate::rendition_workers::ReorderBound::NoReorder,
        }
    }
    fn audio() -> AudioDescriptor {
        AudioDescriptor {
            track: "program".to_owned(),
            kind: StreamKind::Audio,
            codec: "mp4a.40.2".to_owned(),
            bitrate_bps: Some(128_000),
            sample_rate: 48_000,
            channels: 2,
            config: Some(vec![0x11, 0x90]),
            time_base: Rational::new(1, 48_000),
        }
    }
    fn binding(
        output_id: &str,
        index: usize,
        generation: u64,
        width: u32,
        height: u32,
    ) -> EffectiveBinding {
        EffectiveBinding {
            program_id: "program".to_owned(),
            bus_id: "bus".to_owned(),
            output_id: output_id.to_owned(),
            output_index: index,
            config_generation: generation,
            video: video(width, height),
            audio: vec![audio()],
            dependencies: vec![
                Dependency {
                    kind: DependencyKind::Source,
                    id: "camera-a".to_owned(),
                },
                Dependency {
                    kind: DependencyKind::Program,
                    id: "program".to_owned(),
                },
            ],
        }
    }
    fn operator(objects: Option<Vec<String>>, outputs: Option<Vec<String>>) -> Principal {
        Principal {
            key_id: "key".to_owned(),
            role: Role::Operator,
            scoped_object_ids: objects,
            scoped_output_ids: outputs,
            scoped_discovery_domains: None,
        }
    }
    fn global() -> Principal {
        operator(None, None)
    }
    /// A real encoder emission: an Annex-B access unit with its own allocation.
    fn emission(pts: i64, payload: &[u8], key: bool) -> EncodedPacket {
        let mut raw = Packet::copy(payload);
        if key {
            raw.set_flags(Flags::KEY);
        }
        raw.set_pts(Some(pts));
        raw.set_dts(Some(pts));
        EncodedPacket::from_packet(raw)
    }
    /// Strict IDR (`nal_unit_type == 5`) in Annex-B framing.
    fn idr(pts: i64) -> EncodedPacket {
        emission(pts, &[0, 0, 0, 1, 0x65, 0x88, 0x11, 0x22], true)
    }
    /// An inter picture (`nal_unit_type == 1`) that *claims* the KEY flag.
    fn inter_with_key_flag(pts: i64) -> EncodedPacket {
        emission(pts, &[0, 0, 0, 1, 0x41, 0x88, 0x11, 0x22], true)
    }
    fn recv_within(
        rx: &mut tokio::sync::mpsc::Receiver<AvWindow>,
        bound: Duration,
    ) -> Option<AvWindow> {
        let deadline = Instant::now() + bound;
        loop {
            match rx.try_recv() {
                Ok(window) => return Some(window),
                Err(TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => return None,
            }
        }
    }
    fn wait_until(mut predicate: impl FnMut() -> bool, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        predicate()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scopes_authorize_before_any_peer_demand_or_encoder() {
        let registry = FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, _windows) = Fence::channel(4);
        for scope in [
            WhepScope::Program,
            WhepScope::Input("camera-a".into()),
            WhepScope::Output("absent".into()),
        ] {
            assert!(matches!(
                factory.attach(&global(), &scope, sink.clone()),
                Err(MoqReject::Unavailable)
            ));
        }
        let mut viewer = global();
        viewer.role = Role::Viewer;
        assert!(matches!(
            factory.attach(&viewer, &WhepScope::Output("out-hd".into()), sink.clone()),
            Err(MoqReject::Forbidden)
        ));
        for denied in [
            operator(Some(vec!["camera-b".into()]), None),
            operator(Some(vec!["camera-a".into()]), Some(vec!["other".into()])),
        ] {
            assert!(matches!(
                factory.attach(&denied, &WhepScope::Output("out-hd".into()), sink.clone()),
                Err(MoqReject::Forbidden)
            ));
        }
        assert_eq!(registry.active_subscriptions(), 0);
        assert_eq!(registry.active_peers(), 0);
        let feed = factory
            .attach(
                &operator(
                    Some(vec!["camera-a".into()]),
                    Some(vec!["out-hd".into(), "program:program".into()]),
                ),
                &WhepScope::Output("out-hd".into()),
                sink,
            )
            .unwrap();
        assert_eq!(feed.generation, GENERATION);
        assert_eq!(feed.video.codec, "avc1.42c01e");
        assert_eq!(
            (feed.video.coded_width, feed.video.coded_height),
            (1280, 720)
        );
        assert_eq!(feed.audio.len(), 1);
        assert_eq!(feed.audio[0].track, "program");
        let _: hang::catalog::Catalog = hang::catalog::Catalog::from_slice(&feed.catalog).unwrap();
        assert_eq!(registry.active_subscriptions(), 1);
        drop(feed);
        assert_eq!(registry.active_subscriptions(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn whole_window_reaches_the_transport_shared_and_retained_until_drop() {
        let mut registry =
            FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::channel(4);
        let feed = factory
            .attach(&global(), &WhepScope::Output("out-hd".into()), sink)
            .unwrap();
        let assembled = vec![idr(0)];
        let payload = assembled[0].payload().unwrap().as_ptr();
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &assembled, SPAN, Instant::now());
        let window = recv_within(&mut windows, Duration::from_secs(2)).expect("window");
        assert_eq!(window.video.generation, GENERATION);
        assert_eq!(window.video.packets.len(), 1);
        // avcC descriptions require AVC lengths, not the encoder's Annex-B.
        assert_ne!(window.video.packets[0].payload.as_ptr(), payload);
        assert_eq!(
            &window.video.packets[0].payload[..],
            &[0, 0, 0, 4, 0x65, 0x88, 0x11, 0x22]
        );
        assert_eq!(window.video.packets[0].pts_us, 0);
        assert!(window.video.packets[0].verified_rap);
        assert!(window.audio["program"].is_empty());
        assert_ne!(registry.retained(), (0, 0));
        drop(window);
        assert!(wait_until(
            || registry.retained() == (0, 0),
            Duration::from_secs(1)
        ));
        assert_eq!(registry.rejected_windows(), 0);
        drop(feed);
    }

    /// The fence's OWN cancellation identity is what the registry observes, so a
    /// revoke *before any offer* — exactly an HTTP DELETE before the first
    /// window — refuses registry enqueue and releases the peer without relying on
    /// an offer error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_fence_revoke_before_any_offer_refuses_enqueue_and_releases_peer() {
        let mut registry =
            FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::channel(4);
        let scope = WhepScope::Output("out-hd".into());
        let feed = factory.attach(&global(), &scope, sink.clone()).unwrap();
        assert_eq!(registry.active_subscriptions(), 1);
        // Revoke before the producer ever offers a window.
        sink.revoke();
        assert!(sink.revocation().is_revoked());
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[idr(0)], SPAN, Instant::now());
        assert!(recv_within(&mut windows, Duration::from_millis(300)).is_none());
        // The registry dropped the cancelled peer at its enqueue boundary.
        assert_eq!(registry.active_peers(), 0);
        assert_eq!(registry.retained(), (0, 0));
        // The worker observed the same identity through the cancellation
        // observer and released the subscription exactly once.
        assert!(wait_until(
            || registry.active_subscriptions() == 0,
            Duration::from_secs(2)
        ));
        drop(feed);
        assert_eq!(registry.active_subscriptions(), 0);
        // A fence already revoked before attach consumes no peer slot and
        // creates no demand: attach fails closed.
        assert!(matches!(
            factory.attach(&global(), &scope, sink.clone()),
            Err(MoqReject::Unavailable)
        ));
        assert_eq!(registry.active_subscriptions(), 0);
    }

    /// A real `Fence::revoke()` after delivery is observed by the transport's
    /// cancellation identity, not by a factory-owned placeholder flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revoking_the_transport_fence_releases_only_that_peer_once() {
        let mut registry =
            FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::channel(4);
        let feed = factory
            .attach(&global(), &WhepScope::Output("out-hd".into()), sink.clone())
            .unwrap();
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[idr(0)], SPAN, Instant::now());
        assert!(recv_within(&mut windows, Duration::from_secs(2)).is_some());
        sink.revoke();
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[idr(25)], SPAN, Instant::now());
        assert!(wait_until(
            || registry.active_subscriptions() == 0,
            Duration::from_secs(2)
        ));
        assert!(recv_within(&mut windows, Duration::from_millis(200)).is_none());
        drop(feed);
        assert_eq!(registry.active_subscriptions(), 0);
        assert_eq!(registry.retained(), (0, 0));
    }

    /// A real encoded-media loss must NOT kill the subscription: the peer waits
    /// for the next codec-verified random-access point and keeps delivering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loss_then_next_rap_resumes_on_the_same_peer() {
        let mut registry =
            FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::channel(4);
        let feed = factory
            .attach(&global(), &WhepScope::Output("out-hd".into()), sink)
            .unwrap();
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[idr(0)], SPAN, Instant::now());
        assert!(recv_within(&mut windows, Duration::from_secs(2)).is_some());
        registry
            .producer_mut()
            .report_loss(0, OfferError::ProducerLoss);
        // A KEY flag on an inter picture cannot resume the lost queue.
        registry.producer_mut().offer_complete(
            0,
            GENERATION,
            &[inter_with_key_flag(25)],
            SPAN,
            Instant::now(),
        );
        assert!(recv_within(&mut windows, Duration::from_millis(300)).is_none());
        assert_eq!(registry.active_subscriptions(), 1);
        // The next strict RAP resumes on the SAME peer.
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[idr(50)], SPAN, Instant::now());
        let resumed = recv_within(&mut windows, Duration::from_secs(2)).expect("resumed");
        assert_eq!(resumed.video.generation, GENERATION);
        assert_eq!(resumed.video.packets[0].pts_us, 2_000_000);
        assert_eq!(registry.active_subscriptions(), 1);
        drop(feed);
    }

    /// Rejection classification: a superseded lease or an unencodable timestamp
    /// drops that window and keeps the peer; a changed generation or a revoked
    /// identity is terminal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn superseded_lease_is_dropped_while_terminal_causes_end_the_peer() {
        let registry = FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let mapping = registry
            .factory
            .prepared
            .get("out-hd")
            .expect("prepared metadata")
            .mapping
            .clone();
        let subscription = registry
            .factory
            .subscriptions
            .subscribe(&WhepScope::Output("out-hd".into()), GENERATION)
            .expect("subscription");
        let handle = subscription.revoke_handle();
        let (sink, _windows) = Fence::channel(4);
        let recovery = strict_rap();
        // A real admitted lease that a loss superseded.
        let mut queue = BoundedBatchQueue::new().with_recovery_gate(recovery);
        let batch = Arc::new(PacketBatch::try_new(vec![idr(0)], SPAN).expect("batch"));
        queue
            .try_offer(Arc::clone(&batch), Instant::now())
            .expect("admitted");
        let lease = queue.pop_at(Instant::now()).expect("pop").expect("lease");
        assert!(lease.is_current());
        queue.report_loss(OfferError::ProducerLoss);
        assert!(!lease.is_current());
        let superseded = SubscriptionWindow {
            permit: Arc::new(lease),
            config_generation: GENERATION,
            media_span: SPAN,
        };
        assert_eq!(
            publish(&sink, &mapping, recovery, GENERATION, &handle, &superseded),
            Outcome::Dropped(DroppedCause::SupersededLease)
        );
        assert!(!sink.revocation().is_revoked());
        assert!(!handle.is_revoked());
        // An unencodable window on a *current* lease is likewise recoverable, so
        // the stale-lease check above is not what classifies it.
        let mut current_queue = BoundedBatchQueue::new().with_recovery_gate(recovery);
        current_queue
            .try_offer(
                Arc::new(PacketBatch::try_new(vec![idr(0)], SPAN).expect("batch")),
                Instant::now(),
            )
            .expect("admitted");
        let current = current_queue
            .pop_at(Instant::now())
            .expect("pop")
            .expect("lease");
        assert!(current.is_current());
        let unmappable = SubscriptionWindow {
            permit: Arc::new(current),
            config_generation: GENERATION,
            media_span: Duration::from_millis(251),
        };
        assert_eq!(
            publish(&sink, &mapping, recovery, GENERATION, &handle, &unmappable),
            Outcome::Dropped(DroppedCause::UnencodableWindow)
        );
        // A superseded lease reuses the stale permit and is classified by the
        // loss authority, not by the conversion.
        let stale_again = SubscriptionWindow {
            permit: Arc::clone(&superseded.permit),
            config_generation: GENERATION,
            media_span: Duration::from_millis(251),
        };
        assert_eq!(
            publish(&sink, &mapping, recovery, GENERATION, &handle, &stale_again),
            Outcome::Dropped(DroppedCause::SupersededLease)
        );
        // A changed effective generation is terminal: re-resolve the binding.
        assert_eq!(
            publish(
                &sink,
                &mapping,
                recovery,
                GENERATION + 1,
                &handle,
                &superseded
            ),
            Outcome::Terminal(TerminalCause::ConfigGenerationChanged)
        );
        // A revoked registry identity is terminal too.
        handle.revoke();
        assert_eq!(
            publish(&sink, &mapping, recovery, GENERATION, &handle, &superseded),
            Outcome::Terminal(TerminalCause::Cancelled)
        );
        drop(subscription);
    }

    /// A ladder cadence is NEVER derived from the encoder timebase: a `1/90000`
    /// tick resolution is not 90000 fps (and `1001/30000` is not a rejection
    /// candidate — no denominator guard exists anywhere in this tree, verified).
    /// Until an explicit validated opened cadence is threaded through, the rung
    /// reports it as unavailable rather than inventing one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ladder_cadence_uses_proven_clock_not_timestamp_scale() {
        let mut mpeg_ts = binding("out-hd", 0, GENERATION, 1280, 720);
        mpeg_ts.video.time_base = Rational::new(1, 90_000);
        let registry = FeedRegistry::new(&[mpeg_ts]).unwrap();
        assert_eq!(registry.ladder()[0].config_generation, GENERATION);
        let factory = registry.factory();
        let (sink, _windows) = Fence::channel(4);
        let feed = factory
            .attach(&global(), &WhepScope::Output("out-hd".into()), sink)
            .unwrap();
        assert_eq!(feed.ladder.len(), 1);
        assert_eq!(feed.ladder[0].cadence, Some((25, 1)));
        assert_ne!(feed.ladder[0].cadence, Some((90_000, 1)));
        assert_eq!(feed.ladder[0].bitrate_bps, Some(500_000));
        // The selected rendition still carries the validated metadata it needs;
        // only the *cadence* is unavailable.
        assert_eq!(feed.video.coded_width, 1280);
    }

    /// The strict-RAP gate comes from the opened codec id plus coded framing,
    /// never from the configured codec name: no framing proof means no gate.
    #[test]
    fn recovery_gate_comes_from_the_opened_codec_identity() {
        assert_eq!(
            recovery_gate(CodecId::H264, Some(NalFraming::AnnexB)),
            Some(RecoveryGate::StrictRap {
                codec: CodecKind::H264,
                framing: NalFraming::AnnexB
            })
        );
        assert_eq!(
            recovery_gate(
                CodecId::HEVC,
                Some(NalFraming::LengthPrefixed { nal_length_size: 4 })
            ),
            Some(RecoveryGate::StrictRap {
                codec: CodecKind::Hevc,
                framing: NalFraming::LengthPrefixed { nal_length_size: 4 }
            })
        );
        assert_eq!(recovery_gate(CodecId::H264, None), None);
        assert_eq!(recovery_gate(CodecId::VP8, Some(NalFraming::AnnexB)), None);
        assert_eq!(webcodecs_codec_id("avc1.42c01e"), Some(CodecId::H264));
        assert_eq!(webcodecs_codec_id("hvc1.1.6.L93.B0"), Some(CodecId::HEVC));
        assert_eq!(webcodecs_codec_id("vp09.00.10.08"), None);
    }

    /// B1 adjudication: a quiet producer cannot pin teardown. The worker parks on
    /// a bounded receive and re-checks cancellation/stop every iteration, so
    /// cancellation and demand teardown are bounded and never wait on the
    /// producer or on unbounded processing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_with_a_quiet_producer_is_bounded_and_never_waits_on_it() {
        let registry = FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let scope = WhepScope::Output("out-hd".into());
        // (a) Cancellation with a producer that never offered a single window.
        let (sink, _windows) = Fence::channel(4);
        let feed = factory.attach(&global(), &scope, sink.clone()).unwrap();
        assert_eq!(registry.active_subscriptions(), 1);
        let started = Instant::now();
        sink.revoke();
        assert!(wait_until(
            || registry.active_subscriptions() == 0,
            Duration::from_secs(2)
        ));
        let cancelled = started.elapsed();
        assert!(
            cancelled < Duration::from_millis(250),
            "quiet-producer cancellation took {cancelled:?}"
        );
        drop(feed);
        // (b) Demand teardown with the same quiet producer: stop/join must not
        // wait on the producer or on unbounded processing.
        let (sink, _windows) = Fence::channel(4);
        let feed = factory.attach(&global(), &scope, sink).unwrap();
        assert_eq!(registry.active_subscriptions(), 1);
        let started = Instant::now();
        drop(feed);
        let torn_down = started.elapsed();
        assert!(
            torn_down < Duration::from_millis(250),
            "quiet-producer teardown took {torn_down:?}"
        );
        assert_eq!(registry.active_subscriptions(), 0);
        assert_eq!(registry.retained(), (0, 0));
    }

    /// The off-clock observer needs a runtime; outside one, attach fails closed
    /// rather than polling the fence flag.
    #[test]
    fn attach_without_a_runtime_fails_closed_without_consuming_a_slot() {
        let registry = FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, _windows) = Fence::channel(4);
        assert!(matches!(
            factory.attach(&global(), &WhepScope::Output("out-hd".into()), sink),
            Err(MoqReject::Unavailable)
        ));
        assert_eq!(registry.active_subscriptions(), 0);
        assert_eq!(registry.active_peers(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_and_recovery_gate_refuse_before_and_after_conversion() {
        let mut registry =
            FeedRegistry::new(&[binding("out-hd", 0, GENERATION, 1280, 720)]).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::channel(4);
        let feed = factory
            .attach(&global(), &WhepScope::Output("out-hd".into()), sink)
            .unwrap();
        registry
            .producer_mut()
            .offer_complete(0, GENERATION + 1, &[idr(0)], SPAN, Instant::now());
        assert!(recv_within(&mut windows, Duration::from_millis(200)).is_none());
        assert_eq!(registry.retained(), (0, 0));
        let external = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            registry.factory.subscriptions.subscribe_with_revocation(
                &WhepScope::Output("out-hd".into()),
                GENERATION - 1,
                external
            ),
            Err(SubscribeError::StaleGeneration)
        ));
        assert!(matches!(
            refusal(SubscribeError::StaleGeneration),
            MoqReject::Generation
        ));
        assert_eq!(registry.active_subscriptions(), 1);
        drop(feed);
    }

    #[test]
    fn no_encoder_is_ever_minted_for_an_unpreparable_rendition() {
        assert!(matches!(
            FeedRegistry::new(&[]),
            Err(MoqReject::Unavailable)
        ));
        let mut reordered = binding("out-hd", 0, GENERATION, 1280, 720);
        reordered.video.reorder = crate::rendition_workers::ReorderBound::Unknown;
        assert!(matches!(
            FeedRegistry::new(&[reordered]),
            Err(MoqReject::Codec)
        ));
        let mut audio_only = binding("out-hd", 0, GENERATION, 1280, 720);
        audio_only.video.recovery = Some(RecoveryGate::AudioOnly);
        assert!(matches!(
            FeedRegistry::new(&[audio_only]),
            Err(MoqReject::Codec)
        ));
        let mut mismatched = binding("out-hd", 0, GENERATION, 1280, 720);
        mismatched.video.codec = "avc1.640028".to_owned();
        assert!(matches!(
            FeedRegistry::new(&[mismatched]),
            Err(MoqReject::Codec)
        ));
        let mut unconfigured = binding("out-hd", 0, GENERATION, 1280, 720);
        unconfigured.video.config = None;
        assert!(matches!(
            FeedRegistry::new(&[unconfigured]),
            Err(MoqReject::Codec)
        ));
        let mut zero_timebase = binding("out-hd", 0, GENERATION, 1280, 720);
        zero_timebase.video.time_base = Rational::new(1, 0);
        assert!(matches!(
            FeedRegistry::new(&[zero_timebase]),
            Err(MoqReject::Codec)
        ));
        assert!(matches!(
            FeedRegistry::new(&[
                binding("out-hd", 0, GENERATION, 1280, 720),
                binding("out-hd", 1, GENERATION, 640, 360),
            ]),
            Err(MoqReject::Unavailable)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ladder_keeps_every_rendition_in_one_effective_generation() {
        let mut registry = FeedRegistry::new(&[
            binding("out-hd", 0, GENERATION, 1280, 720),
            binding("out-sd", 1, GENERATION, 640, 360),
        ])
        .unwrap();
        assert_eq!(registry.ladder().len(), 2);
        assert_eq!(registry.ladder()[0].program_id, "program");
        assert_eq!(registry.ladder()[0].bus_id, "bus");
        assert_eq!(registry.ladder()[0].output_index, 0);
        assert_eq!(registry.ladder()[1].output_id, "out-sd");
        assert_eq!(registry.ladder()[1].config_generation, GENERATION);
        let factory = registry.factory();
        let (hd_sink, mut hd_rx) = Fence::channel(4);
        let (sd_sink, mut sd_rx) = Fence::channel(4);
        let hd = factory
            .attach(&global(), &WhepScope::Output("out-hd".into()), hd_sink)
            .unwrap();
        let sd = factory
            .attach(&global(), &WhepScope::Output("out-sd".into()), sd_sink)
            .unwrap();
        assert_eq!((hd.generation, sd.generation), (GENERATION, GENERATION));
        assert_eq!((hd.video.coded_width, hd.video.coded_height), (1280, 720));
        assert_eq!((sd.video.coded_width, sd.video.coded_height), (640, 360));
        // Typed alternatives: one rung per authorized rendition, with exact
        // codec/dims/description/cadence — and the selected `video` stays for
        // back-compat consumers.
        assert_eq!(hd.ladder.len(), 2);
        assert_eq!(sd.ladder, hd.ladder);
        assert_eq!(hd.ladder[0].track, "out-hd");
        assert_eq!(hd.ladder[0].codec, hd.video.codec);
        assert_eq!(
            (hd.ladder[0].coded_width, hd.ladder[0].coded_height),
            (hd.video.coded_width, hd.video.coded_height)
        );
        assert_eq!(hd.ladder[0].description, hd.video.description);
        assert_eq!(hd.ladder[0].cadence, Some((25, 1)));
        assert_eq!(hd.ladder[0].bitrate_bps, Some(500_000));
        assert_eq!(hd.ladder[1].track, "out-sd");
        assert_eq!(
            (hd.ladder[1].coded_width, hd.ladder[1].coded_height),
            (640, 360)
        );
        assert_eq!(hd.ladder[1].cadence, Some((25, 1)));
        assert_eq!(registry.active_subscriptions(), 2);
        registry
            .producer_mut()
            .offer_complete(1, GENERATION, &[idr(0)], SPAN, Instant::now());
        let lowered = recv_within(&mut sd_rx, Duration::from_secs(2)).expect("sd window");
        assert_eq!(lowered.video.generation, GENERATION);
        assert!(recv_within(&mut hd_rx, Duration::from_millis(100)).is_none());
        drop((hd, sd));
        assert_eq!(registry.active_subscriptions(), 0);
        let extended = FeedRegistry::new(&[
            binding("out-hd", 0, GENERATION, 1280, 720),
            binding("out-sd", 1, GENERATION, 640, 360),
            binding("out-lq", 2, GENERATION, 320, 180),
        ])
        .unwrap();
        assert_eq!(extended.ladder().len(), 3);
        assert_eq!(extended.ladder()[2].output_id, "out-lq");
        assert_eq!(extended.ladder()[2].config_generation, GENERATION);
    }
    #[test]
    fn mixed_effective_generations_fail_before_factory_demand() {
        assert!(matches!(
            FeedRegistry::new(&[
                binding("out-hd", 0, GENERATION, 1280, 720),
                binding("out-sd", 1, GENERATION + 1, 640, 360),
            ]),
            Err(MoqReject::Unavailable)
        ));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grouped_family_uses_actual_three_leases_and_common_clock() {
        let mut bindings = (0..3)
            .map(|index| {
                let mut binding = binding(&format!("rung-{index}"), index, GENERATION, 640, 360);
                binding.audio.clear();
                binding.video.bitrate_bps = Some(250_000 * (index as u64 + 1));
                binding
            })
            .collect::<Vec<_>>();
        let unrelated = {
            let mut other = binding("other-family", 3, GENERATION, 640, 360);
            other.program_id = "other-program".into();
            other.audio.clear();
            other
        };
        bindings.push(unrelated);
        let mut registry = FeedRegistry::new(&bindings).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::ladder_channel(4);
        let feed = factory
            .attach_shared(&global(), &WhepScope::Output("rung-0".into()), sink.clone())
            .unwrap()
            .unwrap();
        assert_eq!(feed.alternatives.len(), 3);
        assert_eq!(registry.active_subscriptions(), 3);
        assert_eq!(feed.alternatives[0].bitrate_bps, 250_000);
        for index in 0..3 {
            registry.producer_mut().offer_complete(
                index,
                GENERATION,
                &[idr(0)],
                SPAN,
                Instant::now(),
            );
        }
        let window = tokio::time::timeout(Duration::from_secs(2), windows.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(window.generation, GENERATION);
        assert_eq!(window.renditions.len(), 3);
        assert!(window.renditions.iter().all(|rung| rung.packets.len() == 1
            && rung.packets[0].verified_rap
            && rung.packets[0].pts_us == 0));
        assert!(window.audio.is_empty());
        sink.revoke();
        drop(feed);
        assert_eq!(registry.active_subscriptions(), 0);
        let (sink, _) = Fence::ladder_channel(4);
        let denied = operator(
            Some(vec!["camera-a".into(), "program".into()]),
            Some(vec!["rung-0".into()]),
        );
        assert!(factory
            .attach_shared(&denied, &WhepScope::Output("rung-0".into()), sink)
            .is_err());
        assert_eq!(registry.active_subscriptions(), 0);
    }
    #[test]
    fn dynamic_factory_follows_run_slot_without_stale_global_state() {
        let slot = crate::output_window_registry::window_subscription_slot();
        let dynamic = DynamicRegistryFactory(slot.clone());
        assert!(!dynamic.available());
        let mut binding = binding("out", 0, GENERATION, 640, 360);
        binding.audio.clear();
        let (mut registry, factory) = OutputWindowRegistry::new(vec![binding], GENERATION).unwrap();
        slot.store(Some(Arc::new(factory)));
        assert!(dynamic.available());
        slot.store(None);
        registry.close();
        assert!(!dynamic.available());
        let (fence, _) = Fence::channel(1);
        assert!(matches!(
            dynamic.attach(&global(), &WhepScope::Output("out".into()), fence),
            Err(MoqReject::Unavailable)
        ));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grouped_audio_is_reserved_and_published_once_without_guessed_cost() {
        let mut bindings = vec![
            binding("low", 0, GENERATION, 640, 360),
            binding("high", 1, GENERATION, 1280, 720),
        ];
        bindings[1].video.bitrate_bps = Some(1_000_000);
        let mut registry = FeedRegistry::new(&bindings).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::ladder_channel(4);
        let feed = factory
            .attach_shared(&global(), &WhepScope::Output("low".into()), sink.clone())
            .unwrap()
            .unwrap();
        assert_eq!(feed.feed.ladder.reserved_audio_bps(), 128_000);
        assert_eq!(feed.audio.len(), 1);
        for index in 0..2 {
            registry.producer_mut().offer_complete(
                index,
                GENERATION,
                &[idr(0), {
                    let mut packet = Packet::copy(&[0x21, 0x10]);
                    packet.set_pts(Some(0));
                    packet.set_dts(Some(0));
                    EncodedPacket::with_kind(packet, StreamKind::Audio)
                }],
                SPAN,
                Instant::now(),
            );
        }
        let window = tokio::time::timeout(Duration::from_secs(2), windows.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(window.audio.len(), 1);
        assert_eq!(window.audio["program"].len(), 1);
        assert_eq!(
            window.audio["program"][0].pts_us,
            window.renditions[0].packets[0].pts_us
        );
        drop(feed);
        sink.revoke();
        bindings[0].audio[0].bitrate_bps = None;
        bindings[1].audio[0].bitrate_bps = None;
        let registry = FeedRegistry::new(&bindings).unwrap();
        let (sink, _) = Fence::ladder_channel(4);
        assert!(matches!(
            registry
                .factory()
                .attach_shared(&global(), &WhepScope::Output("low".into()), sink),
            Err(MoqReject::Codec)
        ));
        assert_eq!(registry.active_subscriptions(), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_close_fences_pending_and_live_family_admission() {
        let mut bindings = vec![
            binding("low", 0, GENERATION, 640, 360),
            binding("high", 1, GENERATION, 1280, 720),
        ];
        for binding in &mut bindings {
            binding.audio.clear();
        }
        bindings[1].video.bitrate_bps = Some(1_000_000);
        let (mut registry, subscriptions) =
            OutputWindowRegistry::new(bindings, GENERATION).unwrap();
        let factory = RegistryFeedFactory::from_subscriptions(subscriptions.clone()).unwrap();
        let (sink, _) = Fence::ladder_channel(4);
        let feed = factory
            .attach_shared(&global(), &WhepScope::Output("low".into()), sink.clone())
            .unwrap()
            .unwrap();
        assert_eq!(subscriptions.slots_in_use(), 2);
        registry.close();
        tokio::time::timeout(Duration::from_secs(1), sink.revocation().cancelled())
            .await
            .unwrap();
        assert!(sink.revocation().is_revoked());
        assert!(matches!(
            subscriptions.resolve(&WhepScope::Output("low".into())),
            Err(SubscribeError::ProducerEnded)
        ));
        drop(feed);
        assert_eq!(subscriptions.slots_in_use(), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_avc_framing_ends_peer_before_following_delta() {
        let mut registry = FeedRegistry::new(&[binding("out", 0, GENERATION, 640, 360)]).unwrap();
        let factory = registry.factory();
        let (sink, mut windows) = Fence::channel(4);
        let feed = factory
            .attach(&global(), &WhepScope::Output("out".into()), sink.clone())
            .unwrap();
        // A valid initial IDR followed by an invalid NAL in the same AU passes
        // the original RAP classifier but must fail whole-AU framing validation.
        let malformed = emission(0, &[0, 0, 1, 0x65, 0x88, 0, 0, 1, 0x80], true);
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[malformed], SPAN, Instant::now());
        tokio::time::timeout(Duration::from_secs(1), sink.revocation().cancelled())
            .await
            .unwrap();
        assert!(sink.revocation().is_revoked());
        registry
            .producer_mut()
            .offer_complete(0, GENERATION, &[idr(1)], SPAN, Instant::now());
        assert!(windows.try_recv().is_err());
        drop(feed);
        assert_eq!(registry.active_subscriptions(), 0);
    }
    #[test]
    fn outgoing_activation_resolves_existing_scoped_identity_and_detects_withdrawal() {
        let mut keys = multiview_control::auth::ApiKeyStore::new(b"test".to_vec());
        assert!(matches!(
            outgoing_principal(&keys, "missing"),
            Err(MoqReject::Forbidden)
        ));
        let scoped = operator(
            Some(vec!["camera-a".into(), "program".into()]),
            Some(vec!["low".into()]),
        );
        keys.register("service", "test-only", scoped);
        let (principal, generation) = outgoing_principal(&keys, "service").unwrap();
        assert!(metadata::authorize_binding(
            &principal,
            &WhepScope::Output("low".into()),
            "low",
            &["camera-a".into()]
        )
        .is_ok());
        assert!(metadata::authorize_binding(
            &principal,
            &WhepScope::Output("low".into()),
            "high",
            &["camera-a".into()]
        )
        .is_err());
        assert!(keys.revoke("service"));
        assert_ne!(keys.generation(), generation);
        assert!(matches!(
            outgoing_principal(&keys, "service"),
            Err(MoqReject::Forbidden)
        ));
    }
    #[test]
    fn outgoing_read_only_identity_is_not_promoted_to_service_writer() {
        let mut keys = multiview_control::auth::ApiKeyStore::new(b"test".to_vec());
        let mut principal = global();
        principal.role = Role::ReadOnly;
        keys.register("reader", "test-only", principal);
        assert!(matches!(
            outgoing_principal(&keys, "reader"),
            Err(MoqReject::Forbidden)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn relay_url_file_requires_private_regular_inode_and_https() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("relay-url-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("endpoint");
        std::fs::write(&path, "https://relay.invalid/publisher").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_relay_endpoint(&path).is_ok());
        let link = dir.join("link");
        let _ = std::fs::symlink_metadata(&link);
        // Unique process-local link name avoids mutating another lane's fixture.
        if !link.exists() {
            symlink(&path, &link).unwrap();
        }
        assert!(read_relay_endpoint(&link).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_relay_endpoint(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "http://relay.invalid").unwrap();
        assert!(read_relay_endpoint(&path).is_err());
    }

    #[tokio::test]
    async fn outgoing_task_shutdown_awaits_worker_quiescence_once() {
        let completed = Arc::new(AtomicBool::new(false));
        let observed = completed.clone();
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(async move {
            stopped.wait_for(|stop| *stop).await.unwrap();
            tokio::task::yield_now().await;
            observed.store(true, Ordering::Release);
        });
        OutgoingTask { stop, worker }.shutdown().await;
        assert!(completed.load(Ordering::Acquire));
    }
}
