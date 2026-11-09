//! Authenticated local transport runtime. Control authorizes before `admit`.
use crate::{
    RetainedWindow, VideoAdapter,
    av::{AvAdapter, AvWindow},
    ladder::{NotAdaptiveReason, SharedLadder},
    publication::{LADDER_TRACK, LadderWindow, PublishError, SharedPublication},
    uplink::{Admission as UplinkAdmission, Directive, UplinkBudget, UplinkMeter, UplinkReading},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

/// Sanitized terminal reason. No credentials, URLs or transport Debug strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEvent {
    ViewerClosed,
    AuthorizationCancelled,
    Expired,
    ProducerEnded,
    Generation,
    Shape,
    Budget,
    Timestamp,
    NeedsRap,
    Transport,
}
fn publish_event(error: &PublishError) -> TerminalEvent {
    match error {
        PublishError::Reject(crate::Reject::Generation) => TerminalEvent::Generation,
        PublishError::Reject(crate::Reject::Budget) => TerminalEvent::Budget,
        PublishError::Reject(crate::Reject::Timestamp) | PublishError::Timestamp => {
            TerminalEvent::Timestamp
        }
        PublishError::Reject(crate::Reject::NeedsRap) => TerminalEvent::NeedsRap,
        PublishError::UnknownRendition(_)
        | PublishError::UnknownAudioTrack(_)
        | PublishError::Ladder(_) => TerminalEvent::Shape,
        _ => TerminalEvent::Transport,
    }
}

/// Already authorized immutable binding. The factory owns all source-demand
/// resources until its returned guard is dropped; never creates encoders.
pub struct Feed {
    pub generation: u64,
    pub catalog: bytes::Bytes,
    pub audio_tracks: Vec<String>,
    pub windows: mpsc::Receiver<AvWindow>,
    pub demand: Box<dyn Send + Sync>,
}

/// Already authorized shared-ladder binding. The factory owns every source-demand resource
/// until its guard is dropped. It carries the admitted rungs, one common clock and one
/// configuration generation; nothing here encodes and nothing is per viewer.
pub struct LadderFeed {
    /// The authorized ladder: exact codec/dimensions/cadence/bitrate per rung, plus the
    /// independent audio. Never synthesized; a missing rung is simply absent.
    pub ladder: SharedLadder,
    /// Stated uplink capacity in bits per second, or `None` for the conservative
    /// fail-closed unknown state: the lowest rung is served and no rung is earned until real
    /// transport feedback exists.
    pub capacity_bps: Option<u64>,
    /// Uplink cost knobs (margin, hysteresis, staleness, probe opt-in).
    pub config: crate::uplink::UplinkConfig,
    pub windows: mpsc::Receiver<LadderWindow>,
    pub demand: Box<dyn Send + Sync>,
}

/// Live state of one admitted shared-ladder subscription: the publication, the publisher's
/// uplink controller and the meter fed from the publishing session's own statistics.
pub struct AdmittedLadder {
    ladder: Arc<SharedLadder>,
    publication: Mutex<SharedPublication>,
    budget: Mutex<UplinkBudget>,
    meter: Mutex<UplinkMeter>,
}

impl AdmittedLadder {
    fn new(
        ladder: Arc<SharedLadder>,
        publication: SharedPublication,
        budget: UplinkBudget,
    ) -> Self {
        Self {
            ladder,
            publication: Mutex::new(publication),
            budget: Mutex::new(budget),
            meter: Mutex::new(UplinkMeter::new()),
        }
    }

    pub fn ladder(&self) -> &SharedLadder {
        &self.ladder
    }

    /// Rungs currently admitted for this publication.
    pub fn level(&self) -> usize {
        self.budget.lock().expect("uplink budget").level()
    }

    /// The rung an in-flight probe is currently offering, if any.
    pub fn probing(&self) -> Option<usize> {
        self.budget.lock().expect("uplink budget").probing()
    }

    /// Rungs the catalog currently recommends avoiding. Their tracks stay published.
    pub fn stalled(&self) -> Vec<usize> {
        self.publication
            .lock()
            .expect("publication")
            .stalled()
            .to_vec()
    }

    pub fn published_track_count(&self) -> usize {
        self.publication
            .lock()
            .expect("publication")
            .published_track_count()
    }

    /// State a configuration capacity for this publication and apply it to the published
    /// rungs and the catalog immediately.
    pub fn set_capacity(&self, capacity_bps: Option<u64>) -> Directive {
        let directive = {
            let mut budget = self.budget.lock().expect("uplink budget");
            budget.set_capacity(capacity_bps)
        };
        if let Err(error) = self.apply_stalls() {
            debug_assert!(false, "catalog stall update failed: {error:?}");
        }
        directive
    }

    /// Admit a requested rung against the live budget. Existing peers are untouched.
    pub fn admit(&self, requested: usize) -> UplinkAdmission {
        self.budget.lock().expect("uplink budget").admit(requested)
    }

    /// Fold one measurement of the publishing connection into the controller. The directive
    /// immediately drives what is published and what the catalog recommends.
    pub fn observe_uplink(&self, reading: UplinkReading) -> Directive {
        let directive = {
            let mut budget = self.budget.lock().expect("uplink budget");
            budget.observe(reading)
        };
        if let Err(error) = self.apply_stalls() {
            debug_assert!(false, "catalog stall update failed: {error:?}");
        }
        directive
    }

    /// Sample the transport's own counters over `elapsed`, including everything published
    /// since the previous sample.
    pub fn sample(&self, stats: moq_net::session::Stats, elapsed: Duration) -> Directive {
        let reading = {
            let mut meter = self.meter.lock().expect("uplink meter");
            meter.reading(stats, elapsed)
        };
        self.observe_uplink(reading)
    }

    fn apply_stalls(&self) -> Result<(), PublishError> {
        let stalled = {
            let budget = self.budget.lock().expect("uplink budget");
            budget.stalled_levels(self.ladder.len())
        };
        self.publication
            .lock()
            .expect("publication")
            .set_stalled(&stalled)
    }

    /// Publish one ladder tick. Media follows the admitted prefix; a probed rung is offered
    /// as real traffic for the probe window. Production is never reduced to pay for a probe.
    fn publish(&self, window: &LadderWindow) -> Result<(), PublishError> {
        let (level, probe) = {
            let budget = self.budget.lock().expect("uplink budget");
            (budget.level(), budget.probing())
        };
        let mut publication = self.publication.lock().expect("publication");
        for index in 0..self.ladder.len() {
            if window.discontinuities & (1u64 << index) != 0 {
                publication.discontinuity(index)?;
            }
        }
        let mut published = 0u64;
        let mut last_pts = 0u64;
        for (index, rung) in window.renditions.iter().enumerate() {
            if !(index <= level || Some(index) == probe) || rung.packets.is_empty() {
                continue;
            }
            match publication.publish(index, rung) {
                Ok(()) => {}
                Err(PublishError::Reject(crate::Reject::NeedsRap)) => continue,
                Err(error) => return Err(error),
            }
            published += rung
                .packets
                .iter()
                .map(|p| p.payload.len() as u64)
                .sum::<u64>();
            last_pts = rung.packets.last().map_or(last_pts, |packet| packet.pts_us);
        }
        for (name, packets) in &window.audio {
            if packets.is_empty() {
                continue;
            }
            publication.publish_audio(name, packets)?;
            published += packets.iter().map(|p| p.payload.len() as u64).sum::<u64>();
            last_pts = packets.last().map_or(last_pts, |packet| packet.pts_us);
        }
        let _ = last_pts;
        drop(publication);
        {
            let mut meter = self.meter.lock().expect("uplink meter");
            meter.record_published(published);
        }
        Ok(())
    }

    fn revoke(&self) {
        self.publication.lock().expect("publication").revoke();
    }
}

/// What the runtime hands back for one admitted shared-ladder subscription.
pub struct SharedAdmission {
    pub url: String,
    pub broadcast: String,
    pub generation: u64,
    pub protocol: String,
    pub ladder: Arc<SharedLadder>,
    runtime: Arc<AdmittedLadder>,
}

impl SharedAdmission {
    pub fn handle(&self) -> Arc<AdmittedLadder> {
        self.runtime.clone()
    }

    /// Track names a consumer may subscribe to: admitted rungs, then independent audio.
    pub fn track_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .ladder
            .renditions()
            .iter()
            .map(|rendition| rendition.name().to_string())
            .collect();
        names.extend(
            self.ladder
                .audio()
                .iter()
                .map(|audio| audio.name().to_string()),
        );
        names
    }

    pub fn level(&self) -> usize {
        self.runtime.level()
    }
    pub fn probing(&self) -> Option<usize> {
        self.runtime.probing()
    }
    pub fn stalled(&self) -> Vec<usize> {
        self.runtime.stalled()
    }
    pub fn admit(&self, requested: usize) -> UplinkAdmission {
        self.runtime.admit(requested)
    }
    pub fn observe_uplink(&self, reading: UplinkReading) -> Directive {
        self.runtime.observe_uplink(reading)
    }
    pub fn sample(&self, stats: moq_net::session::Stats, elapsed: Duration) -> Directive {
        self.runtime.sample(stats, elapsed)
    }
}

/// The `ladder.json` descriptor for the single-rendition path. It states, in the document
/// every consumer can read, that this publication is explicitly not adaptive: one admitted
/// video rendition and no synthesized quality. Encoder cost is not a network number and
/// never appears here.
pub fn single_rendition_descriptor(
    generation: u64,
    rendition_names: &[String],
) -> anyhow::Result<bytes::Bytes> {
    Ok(bytes::Bytes::from(serde_json::to_vec(
        &serde_json::json!({
            "version": 1,
            "generation": generation,
            "adaptive": false,
            "notAdaptiveReason": NotAdaptiveReason::SingleRendition.as_str(),
            "source": "legacy_single_rendition_feed",
            "cloudTranscodeRequired": false,
            "video": {
                "renditions": rendition_names
                    .iter()
                    .map(|name| serde_json::json!({ "name": name }))
                    .collect::<Vec<_>>()
            },
        }),
    )?))
}

/// Per-subscription synchronous producer fence. `try_offer` and `revoke` share
/// the same small critical section; no transport operation runs under it.
static RETAINED_BYTES: AtomicUsize = AtomicUsize::new(0);
const GLOBAL_BYTES: usize = 64 * 1024 * 1024;
const SUBSCRIPTION_BYTES: usize = 8 * 1024 * 1024;
/// Actual globally charged retained media/framing payload bytes. This is not a
/// native QUIC allocator measurement and is not a per-family attribution.
pub fn retained_payload_bytes() -> usize {
    RETAINED_BYTES.load(Ordering::Acquire)
}
/// Global hard admission cap for charged media/framing payload ownership.
pub fn retained_payload_limit() -> usize {
    GLOBAL_BYTES
}

// Bytes::from_owner carries the charge through every library clone/slice and
// refunds only when the final retained payload reference actually drops.
struct ChargedPayload {
    payload: bytes::Bytes,
    charge: usize,
    subscription: Arc<AtomicUsize>,
}
impl AsRef<[u8]> for ChargedPayload {
    fn as_ref(&self) -> &[u8] {
        &self.payload
    }
}
impl Drop for ChargedPayload {
    fn drop(&mut self) {
        self.subscription.fetch_sub(self.charge, Ordering::AcqRel);
        RETAINED_BYTES.fetch_sub(self.charge, Ordering::AcqRel);
    }
}

/// Reserve the complete serialized object before allocation. Its owner retains
/// the original admission charge as well as the extra framing allocation.
pub(crate) fn serialized_frame(packet: &crate::Packet) -> Result<bytes::Bytes, crate::Reject> {
    serialized_frame_in(packet, None)
}
pub(crate) fn serialized_frame_in(
    packet: &crate::Packet,
    budget: Option<&crate::allocation::AllocationBudget>,
) -> Result<bytes::Bytes, crate::Reject> {
    let timestamp = moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO)
        .map_err(|_| crate::Reject::Timestamp)?;
    let maximum = packet
        .payload
        .len()
        .checked_add(8)
        .ok_or(crate::Reject::Budget)?;
    let family_charge = budget
        .map(|budget| budget.reserve(maximum))
        .transpose()
        .map_err(|_| crate::Reject::Budget)?;
    RETAINED_BYTES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(maximum).filter(|sum| *sum <= GLOBAL_BYTES)
        })
        .map_err(|_| crate::Reject::Budget)?;
    struct Serialized {
        data: Vec<u8>,
        _source: bytes::Bytes,
        charge: usize,
        _family_charge: Option<crate::allocation::Reservation>,
    }
    impl AsRef<[u8]> for Serialized {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
    impl Drop for Serialized {
        fn drop(&mut self) {
            RETAINED_BYTES.fetch_sub(self.charge, Ordering::AcqRel);
        }
    }
    let mut owner = Serialized {
        data: Vec::new(),
        _source: packet.payload.clone(),
        charge: maximum,
        _family_charge: family_charge,
    };
    owner
        .data
        .try_reserve_exact(maximum)
        .map_err(|_| crate::Reject::Budget)?;
    hang::container::Frame {
        timestamp,
        payload: packet.payload.clone(),
    }
    .encode(&mut owner.data)
    .map_err(|_| crate::Reject::Transport)?;
    Ok(bytes::Bytes::from_owner(owner))
}

/// Read-only, per-subscription cancellation capability. Install a clone in the
/// upstream registry before publishing the producer. The flag is monotonic;
/// observing it requires no callback, allocation, or lock on the production path.
#[derive(Clone)]
pub struct Revocation {
    revoked: Arc<AtomicBool>,
    changed: watch::Receiver<bool>,
}
impl Revocation {
    /// Synchronously observe the revocation boundary with acquire ordering.
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Share the monotonic flag with an upstream registry that cannot depend on
    /// this transport crate. Install before producer publication; consumers must
    /// only load with Acquire and must never reset it. Writes outside this fence
    /// do not notify transport waiters and are not a supported cancellation API.
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.revoked.clone()
    }

    /// Wait off the production path. Late observers also return immediately.
    pub async fn cancelled(&self) {
        let mut changed = self.changed.clone();
        if self.is_revoked() {
            return;
        }
        let _ = changed.wait_for(|value| *value).await;
    }
}

/// The queue one subscription fence owns. A subscription carries exactly one window shape;
/// the fence keeps one charge account, one queue bound and one revocation identity for both,
/// so the shared ladder never needs a second fence.
enum Queue {
    Single(mpsc::Sender<AvWindow>),
    Ladder(mpsc::Sender<LadderWindow>),
}

/// Refusal of a complete window. Only `Full` permits future offers; never retry
/// its returned window. Wait for a complete verified RAP window after a gap.
#[derive(Debug)]
pub enum OfferRefusal<W> {
    Full(W),
    Revoked(W),
    Shape(W),
    Hard(W),
}
impl<W> OfferRefusal<W> {
    pub fn into_window(self) -> W {
        match self {
            Self::Full(w) | Self::Revoked(w) | Self::Shape(w) | Self::Hard(w) => w,
        }
    }
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Full(_))
    }
}

pub struct Fence {
    revoked: Arc<AtomicBool>,
    charged: Arc<AtomicUsize>,
    gap: AtomicUsize,
    ladder_generation: AtomicU64,
    ladder_rungs: AtomicUsize,
    terminal: Mutex<Option<TerminalEvent>>,
    sender: Mutex<Option<Queue>>,
    cancel: watch::Sender<bool>,
}
impl Fence {
    pub fn channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<AvWindow>) {
        Self::create(capacity, Queue::Single)
    }

    /// The same fence for a shared-ladder subscription. One `Fence`, one charge account, one
    /// revocation identity: `revocation()` is the identity the upstream registry installs.
    pub fn ladder_channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<LadderWindow>) {
        Self::create(capacity, Queue::Ladder)
    }

    fn create<W>(
        capacity: usize,
        wrap: impl FnOnce(mpsc::Sender<W>) -> Queue,
    ) -> (Arc<Self>, mpsc::Receiver<W>) {
        let (sender, receiver) = mpsc::channel(capacity.clamp(1, 8));
        let (cancel, _) = watch::channel(false);
        (
            Arc::new(Self {
                revoked: Arc::new(AtomicBool::new(false)),
                charged: Arc::new(AtomicUsize::new(0)),
                gap: AtomicUsize::new(0),
                ladder_generation: AtomicU64::new(0),
                ladder_rungs: AtomicUsize::new(0),
                terminal: Mutex::new(None),
                sender: Mutex::new(Some(wrap(sender))),
                cancel,
            }),
            receiver,
        )
    }
    /// Obtain the same cancellation identity for registry admission and its
    /// off-clock cleanup worker. Calling after cancellation is safe; no install
    /// callback or lost-notification window exists.
    pub fn revocation(&self) -> Revocation {
        Revocation {
            revoked: self.revoked.clone(),
            changed: self.cancel.subscribe(),
        }
    }

    /// First terminal event for this admission, safe to emit in diagnostics.
    pub fn terminal_event(&self) -> Option<TerminalEvent> {
        self.terminal.lock().ok().and_then(|event| *event)
    }
    fn record_terminal(&self, event: TerminalEvent) {
        if let Ok(mut terminal) = self.terminal.lock() {
            terminal.get_or_insert(event);
        }
    }

    fn signal_revocation(&self) {
        if !self.revoked.swap(true, Ordering::AcqRel) {
            self.cancel.send_replace(true);
        }
    }

    /// The authorized feed owner must use this instead of retaining a raw sender.
    ///
    /// # Errors
    /// Every rejection is terminal for this subscription. Stop producing and
    /// drop the returned window; do not retry it. A queue-full rejection may
    /// return already charged payloads. Their charge intentionally remains until
    /// the returned window and every retained clone/slice are actually dropped.
    /// Retrying on this revoked fence cannot add a second charge.
    /// Charge and wrap one window in the carried shape, returning it with the queue guard
    /// held. Both shapes share this single accounting path.
    fn reserve<W: RetainedWindow>(
        &self,
        mut window: W,
    ) -> Result<(W, std::sync::MutexGuard<'_, Option<Queue>>), OfferRefusal<W>> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(OfferRefusal::Revoked(window));
        }
        let mut bytes = 0usize;
        let mut count = 0usize;
        let mut refused = false;
        window.for_each_packet(&mut |packet| {
            count += 1;
            match bytes.checked_add(packet.payload.len()) {
                Some(total) if total <= VideoAdapter::MAX_BYTES && count <= 512 => bytes = total,
                _ => refused = true,
            }
        });
        if refused {
            self.signal_revocation();
            return Err(OfferRefusal::Hard(window));
        }
        let Ok(guard) = self.sender.try_lock() else {
            self.gap.store(usize::MAX, Ordering::Release);
            return Err(OfferRefusal::Full(window));
        };
        if self.revoked.load(Ordering::Acquire) || guard.is_none() {
            return Err(OfferRefusal::Revoked(window));
        }
        if self.charged.load(Ordering::Acquire).saturating_add(bytes) > SUBSCRIPTION_BYTES
            || RETAINED_BYTES
                .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(bytes).filter(|sum| *sum <= GLOBAL_BYTES)
                })
                .is_err()
        {
            self.gap.store(usize::MAX, Ordering::Release);
            return Err(OfferRefusal::Full(window));
        }
        self.charged.fetch_add(bytes, Ordering::AcqRel);
        window.for_each_packet(&mut |packet| {
            let payload = std::mem::take(&mut packet.payload);
            packet.payload = bytes::Bytes::from_owner(ChargedPayload {
                charge: payload.len(),
                payload,
                subscription: self.charged.clone(),
            });
        });
        Ok((window, guard))
    }

    /// The authorized feed owner must use this instead of retaining a raw sender.
    ///
    /// # Errors
    /// Every rejection is terminal for this subscription. Stop producing and
    /// drop the returned window; do not retry it. A queue-full rejection may
    /// return already charged payloads. Their charge intentionally remains until
    /// the returned window and every retained clone/slice are actually dropped.
    /// Retrying on this revoked fence cannot add a second charge.
    pub fn try_offer(&self, window: AvWindow) -> Result<(), AvWindow> {
        let (window, guard) = self.reserve(window).map_err(|error| {
            self.signal_revocation();
            error.into_window()
        })?;
        match guard.as_ref() {
            Some(Queue::Single(sender)) => sender.try_send(window).map_err(|error| {
                self.signal_revocation();
                error.into_inner()
            }),
            _ => Err(window),
        }
    }

    /// The shared-ladder offer on the same fence: same charge account, same revocation
    /// identity, same terminal-rejection contract as [`Self::try_offer`].
    pub fn try_offer_ladder(&self, window: LadderWindow) -> Result<(), LadderWindow> {
        let (window, guard) = self.reserve(window).map_err(|error| {
            self.signal_revocation();
            error.into_window()
        })?;
        match guard.as_ref() {
            Some(Queue::Ladder(sender)) => sender.try_send(window).map_err(|error| {
                self.signal_revocation();
                error.into_inner()
            }),
            _ => Err(window),
        }
    }

    /// Recoverable bounded offer for a shared ladder. Pressure drops the entire
    /// window, never revokes authorization, and gates recovery on verified RAPs.
    pub fn offer_ladder(&self, window: LadderWindow) -> Result<(), OfferRefusal<LadderWindow>> {
        if self.revocation().is_revoked() {
            return Err(OfferRefusal::Revoked(window));
        }
        let rungs = window.renditions.len();
        let generation = window.generation.checked_add(1);
        if rungs == 0
            || rungs > crate::ladder::MAX_VIDEO_RENDITIONS
            || generation.is_none()
            || window
                .renditions
                .iter()
                .any(|rung| rung.generation != window.generation)
        {
            self.signal_revocation();
            return Err(OfferRefusal::Shape(window));
        }
        let generation = generation.unwrap();
        let expected = self.ladder_generation.load(Ordering::Acquire);
        if expected != 0
            && (expected != generation || self.ladder_rungs.load(Ordering::Acquire) != rungs)
        {
            self.signal_revocation();
            return Err(OfferRefusal::Shape(window));
        }
        let (window, guard) = self.reserve(window)?;
        let Some(Queue::Ladder(sender)) = guard.as_ref() else {
            self.signal_revocation();
            return Err(OfferRefusal::Shape(window));
        };
        let mut window = window;
        let mut gaps = self.gap.load(Ordering::Acquire);
        window.discontinuities |= gaps as u64 & ((1u64 << rungs) - 1);

        for (index, rung) in window.renditions.iter_mut().enumerate() {
            let Some(bit) = 1usize.checked_shl(index as u32) else {
                self.signal_revocation();
                return Err(OfferRefusal::Shape(window));
            };
            if gaps & bit != 0 {
                if let Some(rap) = rung.packets.iter().position(|packet| packet.verified_rap) {
                    rung.packets.drain(..rap);
                    gaps &= !bit;
                } else {
                    rung.packets.clear();
                }
            }
        }
        match sender.try_send(window) {
            Ok(()) => {
                self.ladder_rungs.store(rungs, Ordering::Release);
                self.ladder_generation.store(generation, Ordering::Release);
                self.gap.store(gaps, Ordering::Release);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(window)) => {
                self.gap.store(usize::MAX, Ordering::Release);
                Err(OfferRefusal::Full(window))
            }
            Err(mpsc::error::TrySendError::Closed(window)) => {
                self.signal_revocation();
                Err(OfferRefusal::Hard(window))
            }
        }
    }

    /// Stop future offers before returning and publish the upstream flag. An
    /// upstream offer already past its own flag check may still be in flight;
    /// the upstream registry must check again at its next enqueue boundary.
    /// Previously queued/in-flight allocations remain charged until actual drop.
    pub fn revoke(&self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        self.signal_revocation();
    }
}

struct Entry {
    origin: moq_net::origin::Consumer,
    fence: Arc<Fence>,
    used: bool,
}
struct State {
    entries: Mutex<HashMap<String, Entry>>,
    ready: AtomicBool,
    shutdown: watch::Sender<bool>,
}
struct SubscriptionCleanup {
    state: Arc<State>,
    token: String,
    fence: Arc<Fence>,
}
impl Drop for SubscriptionCleanup {
    fn drop(&mut self) {
        self.fence.revoke();
        if let Ok(mut entries) = self.state.entries.lock() {
            entries.remove(&self.token);
        }
    }
}

/// Listener is ready only after a successful bind; no default synthetic feeds.
pub struct Runtime {
    state: Arc<State>,
    base_url: String,
    protocol: String,
}
pub struct Admission {
    pub url: String,
    pub broadcast: String,
    pub generation: u64,
    pub protocol: String,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.state.ready.store(false, Ordering::Release);
        self.state.shutdown.send_replace(true);
    }
}

impl Runtime {
    pub async fn listen_files(
        bind: std::net::SocketAddr,
        endpoint: String,
        protocol: &str,
        cert: std::path::PathBuf,
        key: std::path::PathBuf,
    ) -> anyhow::Result<Arc<Self>> {
        Self::listen_files_after_open(bind, endpoint, protocol, cert, key, || {}).await
    }
    async fn listen_files_after_open(
        bind: std::net::SocketAddr,
        endpoint: String,
        protocol: &str,
        cert: std::path::PathBuf,
        key: std::path::PathBuf,
        after_open: impl FnOnce(),
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(bind.is_ipv6(), "IPv6-first listener required");
        let mut config = moq_tokio::listen::Config::default();
        config.bind = Some(moq_tokio::listen::Bind::Addr(bind));
        config.version = vec![protocol.parse().map_err(anyhow::Error::msg)?];
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{Mode, OFlags, open};
            use std::os::fd::AsRawFd;
            let cert_fd = open(
                &cert,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
            let key_fd = open(
                &key,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
            let cert_stat = rustix::fs::fstat(&cert_fd)?;
            let key_stat = rustix::fs::fstat(&key_fd)?;
            anyhow::ensure!(
                (cert_stat.st_mode & 0o170000) == 0o100000,
                "certificate must be regular"
            );
            anyhow::ensure!(
                (key_stat.st_mode & 0o170000) == 0o100000 && key_stat.st_mode & 0o077 == 0,
                "private key must be regular and private"
            );
            after_open();
            // The pinned backend accepts file paths only. These descriptor paths
            // bind its reopen to the already-validated inode, not a mutable path.
            // The backend has no watch feature enabled; initialization consumes
            // both files before these owned descriptors leave this scope.
            config.tls.cert = vec![std::path::PathBuf::from(format!(
                "/proc/self/fd/{}",
                cert_fd.as_raw_fd()
            ))];
            config.tls.key = vec![std::path::PathBuf::from(format!(
                "/proc/self/fd/{}",
                key_fd.as_raw_fd()
            ))];
            Self::listen(config, endpoint, protocol).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            // No insecure pathname fallback on an unsupported secure-loader platform.
            let _ = (cert, key, endpoint, protocol, after_open);
            anyhow::bail!("secure MoQ certificate loader unavailable on this platform")
        }
    }
    pub async fn listen(
        config: moq_tokio::listen::Config,
        base_url: String,
        protocol: &str,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            matches!(protocol, "moq-transport-16" | "moq-transport-18"),
            "unsupported protocol"
        );
        anyhow::ensure!(
            base_url.starts_with("https://") && !base_url.contains('?') && !base_url.contains('#'),
            "invalid endpoint"
        );
        anyhow::ensure!(
            config.version.len() == 1 && config.version[0].to_string() == protocol,
            "listener version mismatch"
        );
        let mut listener = config.init(Default::default())?.listen().await?;
        let runtime = Arc::new(Self {
            state: Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                ready: AtomicBool::new(true),
                shutdown: watch::channel(false).0,
            }),
            base_url: base_url.trim_end_matches('/').into(),
            protocol: protocol.into(),
        });
        let state = runtime.state.clone();
        tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            let mut shutdown = state.shutdown.subscribe();
            loop {
                tokio::select! {
                    _ = async { let _ = shutdown.wait_for(|value| *value).await; } => break,
                    Some(_) = sessions.join_next(), if !sessions.is_empty() => {},
                    request = listener.accept() => {
                        let Some(request) = request else { break };
                        let token = request.url().and_then(|url| url.query_pairs().find(|(k, _)| k == "ticket").map(|(_, v)| v.into_owned()));
                        let admitted = token.and_then(|token| {
                            let mut entries = state.entries.lock().ok()?;
                            let entry = entries.get_mut(&token)?;
                            if entry.used || entry.fence.revoked.load(Ordering::Acquire) { return None }
                            entry.used = true;
                            Some((entry.origin.clone(), entry.fence.clone()))
                        });
                        let Some((origin, fence)) = admitted else { let _ = request.reject(moq_tokio::server::Reject::Unauthorized).await; continue };
                        if sessions.len() >= 16 { fence.revoke(); let _ = request.reject(moq_tokio::server::Reject::App(429)).await; continue }
                        sessions.spawn(async move {
                            let mut cancel = fence.cancel.subscribe();
                            let accepted = tokio::select! {
                                value = request.with_publisher(origin).ok() => value.ok(),
                                _ = cancel.wait_for(|value| *value) => None,
                            };
                            if let Some(session) = accepted {
                                // Isolation: this is a *viewer's* session. Its transport counters must
                                // never drive the shared ladder's uplink controller, or one viewer's
                                // downlink would decide what every other viewer receives.
                                tokio::select! {
                                    _ = session.closed() => fence.record_terminal(TerminalEvent::ViewerClosed),
                                    _ = cancel.wait_for(|value| *value) => fence.record_terminal(TerminalEvent::AuthorizationCancelled),
                                }
                                session.abort(moq_net::Error::Cancel);
                                let _ = tokio::time::timeout(Duration::from_secs(1), session.closed()).await;
                                drop(session);
                            }
                            fence.revoke();
                        });
                    }
                }
            }
            state.ready.store(false, Ordering::Release);
            if let Ok(entries) = state.entries.lock() {
                for entry in entries.values() {
                    entry.fence.revoke();
                }
            }
            sessions.abort_all();
        });
        Ok(runtime)
    }
    pub fn available(&self) -> bool {
        self.state.ready.load(Ordering::Acquire)
    }
    /// The same fence must own the factory sender; expiry independently stops
    /// enqueue, drops demand, and cancels an already-connected transport.
    pub fn admit(
        &self,
        mut feed: Feed,
        fence: Arc<Fence>,
        ttl: Duration,
    ) -> anyhow::Result<Admission> {
        anyhow::ensure!(self.available(), "listener unavailable");
        anyhow::ensure!(
            feed.catalog.len() <= 65536 && feed.audio_tracks.len() <= 16,
            "configuration exceeds budget"
        );
        let catalog: hang::catalog::Catalog<()> =
            hang::catalog::Catalog::from_slice(&feed.catalog)?;
        anyhow::ensure!(
            catalog.video.renditions.len() <= 1,
            "a multi-rendition catalog must be admitted through admit_shared"
        );
        let rendition_names: Vec<String> = catalog.video.renditions.keys().cloned().collect();
        let mut origin_config = moq_net::origin::Config::default();
        origin_config.cache_duration = Duration::from_secs(2);
        origin_config.pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default()
                .with_capacity(8 * 1024 * 1024u64)
                .with_expiry(Duration::from_secs(2)),
        );
        let origin = moq_tokio::origin::spawn_config(origin_config);
        let broadcast = origin.create_broadcast("media")?;
        broadcast.announce(Default::default())?;
        let mut catalog = broadcast.create_track(
            "catalog.json",
            hang::catalog::Catalog::<()>::default_track_info(),
        )?;
        catalog.write_frame(moq_net::Timestamp::from_millis(0)?, feed.catalog.clone())?;
        let mut ladder_track = broadcast.create_track(
            LADDER_TRACK,
            hang::container::track_info(hang::catalog::PRIORITY.video)
                .with_max_age(Duration::from_secs(2)),
        )?;
        // The single-rendition path is explicitly not adaptive: one admitted video track, no
        // synthesized quality, and a descriptor that says so for every consumer.
        ladder_track.write_frame(
            moq_net::Timestamp::from_millis(0)?,
            single_rendition_descriptor(feed.generation, &rendition_names)?,
        )?;
        let video = broadcast.create_track("video", VideoAdapter::track_info())?;
        let mut audio = std::collections::BTreeMap::new();
        for name in &feed.audio_tracks {
            audio.insert(
                name.clone(),
                broadcast.create_track(
                    name.as_str(),
                    hang::container::track_info(hang::catalog::PRIORITY.audio),
                )?,
            );
        }
        let mut adapter = AvAdapter::new(VideoAdapter::new(video, feed.generation), audio);
        let token = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut entries = self
                .state
                .entries
                .lock()
                .map_err(|_| anyhow::anyhow!("registry poisoned"))?;
            anyhow::ensure!(entries.len() < 16, "capacity");
            entries.insert(
                token.clone(),
                Entry {
                    origin: origin.consume(),
                    fence: fence.clone(),
                    used: false,
                },
            );
        }
        let result = Admission {
            url: format!("{}/?ticket={token}", self.base_url),
            broadcast: "media".into(),
            generation: feed.generation,
            protocol: self.protocol.clone(),
        };
        let cleanup = SubscriptionCleanup {
            state: self.state.clone(),
            token,
            fence: fence.clone(),
        };
        tokio::spawn(async move {
            let _cleanup = cleanup;
            let mut cancel = fence.cancel.subscribe();
            let expiry =
                tokio::time::sleep(ttl.clamp(Duration::from_secs(1), Duration::from_secs(60)));
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut expiry => break,
                    _ = cancel.wait_for(|value| *value) => break,
                    window = feed.windows.recv() => match window {
                        Some(window) => { if adapter.publish(&window).is_err() { break } },
                        None => break,
                    }
                }
            }
            fence.revoke();
            adapter.revoke();
            drop((feed, catalog, ladder_track, broadcast, origin));
        });
        Ok(result)
    }

    /// Publish one authorized shared ladder over the same ticket/authorization/fence/TTL path
    /// as [`Runtime::admit`], with the publisher's uplink controller driving what is offered.
    ///
    /// The same [`Fence`] type carries the ladder: `Fence::ladder_channel` shares one charge
    /// account and one revocation identity with the single-rendition path, so the upstream
    /// registry installs exactly one flag and there is no second fence.
    pub fn admit_shared(
        &self,
        feed: LadderFeed,
        fence: Arc<Fence>,
        ttl: Duration,
    ) -> anyhow::Result<SharedAdmission> {
        anyhow::ensure!(self.available(), "listener unavailable");
        let LadderFeed {
            ladder: admitted,
            capacity_bps,
            config,
            mut windows,
            demand,
        } = feed;
        let ladder = Arc::new(admitted);
        let mut origin_config = moq_net::origin::Config::default();
        origin_config.cache_duration = Duration::from_secs(2);
        origin_config.pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default()
                .with_capacity(8 * 1024 * 1024u64)
                .with_expiry(Duration::from_secs(2)),
        );
        let origin = moq_tokio::origin::spawn_config(origin_config);
        let broadcast = origin.create_broadcast("media")?;
        broadcast.announce(Default::default())?;
        let publication = SharedPublication::create((*ladder).clone(), &broadcast)?;
        let budget = UplinkBudget::from_ladder(&ladder, capacity_bps, config)?;
        let runtime = Arc::new(AdmittedLadder::new(ladder.clone(), publication, budget));
        // The initial catalog states the granted prefix explicitly instead of leaving every
        // rung looking available.
        runtime.apply_stalls()?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut entries = self
                .state
                .entries
                .lock()
                .map_err(|_| anyhow::anyhow!("registry poisoned"))?;
            anyhow::ensure!(entries.len() < 16, "capacity");
            entries.insert(
                token.clone(),
                Entry {
                    origin: origin.consume(),
                    fence: fence.clone(),
                    used: false,
                },
            );
        }
        let result = SharedAdmission {
            url: format!("{}/?ticket={token}", self.base_url),
            broadcast: "media".into(),
            generation: ladder.generation(),
            protocol: self.protocol.clone(),
            ladder: ladder.clone(),
            runtime: runtime.clone(),
        };
        let cleanup = SubscriptionCleanup {
            state: self.state.clone(),
            token,
            fence: fence.clone(),
        };
        tokio::spawn(async move {
            let _cleanup = cleanup;
            let mut cancel = fence.cancel.subscribe();
            let expiry =
                tokio::time::sleep(ttl.clamp(Duration::from_secs(1), Duration::from_secs(60)));
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut expiry => { fence.record_terminal(TerminalEvent::Expired); break },
                    _ = cancel.wait_for(|value| *value) => { fence.record_terminal(TerminalEvent::AuthorizationCancelled); break },
                    window = windows.recv() => match window {
                        Some(window) => {
                            if window.generation != runtime.ladder().generation() {
                                fence.record_terminal(TerminalEvent::Generation);
                                fence.revoke();
                                break;
                            }
                            if let Err(error) = runtime.publish(&window) {
                                fence.record_terminal(publish_event(&error));
                                fence.revoke();
                                break;
                            }
                        }
                        None => { fence.record_terminal(TerminalEvent::ProducerEnded); break },
                    }
                }
            }
            fence.revoke();
            runtime.revoke();
            drop((windows, demand, broadcast, origin));
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observer_install_race_and_clones_are_level_triggered_once() {
        for _ in 0..64 {
            let (fence, _rx) = Fence::channel(1);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let other = fence.clone();
            let ready = barrier.clone();
            let thread = std::thread::spawn(move || {
                ready.wait();
                other.revoke();
                other.revoke();
            });
            barrier.wait();
            let observer = fence.revocation();
            thread.join().unwrap();
            assert!(observer.is_revoked());
            let clone = observer.clone();
            assert!(Arc::ptr_eq(&clone.flag(), &observer.flag()));
            tokio::time::timeout(Duration::from_secs(1), clone.cancelled())
                .await
                .unwrap();
            let late = fence.revocation();
            assert!(late.is_revoked());
            late.cancelled().await;
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn native_tls_listener_uses_pinned_inode_after_original_paths_swap() {
        let dir = std::env::temp_dir().join(format!("moq-inode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let runtime = Runtime::listen_files_after_open(
            "[::1]:0".parse().unwrap(),
            "https://localhost".into(),
            "moq-transport-16",
            cert.clone(),
            key.clone(),
            || {
                // Rename preserves the already-opened inode. Replacement contents
                // are invalid PEM: a mutable-path reopen would fail initialization.
                std::fs::rename(&cert, dir.join("original-cert.pem")).unwrap();
                std::fs::rename(&key, dir.join("original-key.pem")).unwrap();
                std::fs::write(&cert, b"invalid replacement certificate").unwrap();
                std::fs::write(&key, b"invalid replacement key").unwrap();
            },
        )
        .await
        .unwrap();
        assert!(runtime.available());
        drop(runtime);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn secure_listener_rejects_symlink_and_public_private_key_before_bind() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("moq-secure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let link = dir.join("link.pem");
        std::fs::write(&cert, b"not a certificate").unwrap();
        std::fs::write(&key, b"not a private key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&key, &link).unwrap();
        let bind = "[::1]:0".parse().unwrap();
        assert!(
            Runtime::listen_files(
                bind,
                "https://localhost".into(),
                "moq-transport-16",
                cert.clone(),
                link
            )
            .await
            .is_err()
        );
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = Runtime::listen_files(
            bind,
            "https://localhost".into(),
            "moq-transport-16",
            cert,
            key,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "private key must be regular and private");
        // Keep the bounded disposable fixture for diagnostics; no private material was used.
    }

    #[test]
    fn full_queue_returned_window_credit_lasts_exactly_as_long_as_its_payload() {
        let (fence, receiver) = Fence::channel(1);
        let window = |len| AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: bytes::Bytes::from(vec![7; len]),
                    pts_us: 0,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window(3)).is_ok());
        let rejected = fence.try_offer(window(5)).expect_err("full queue rejects");
        assert!(fence.revocation().is_revoked());
        assert_eq!(fence.charged.load(Ordering::Acquire), 8);
        // Unsupported retry is terminal and must not double-wrap or recharge.
        let rejected = fence
            .try_offer(rejected)
            .expect_err("revoked fence rejects");
        assert_eq!(fence.charged.load(Ordering::Acquire), 8);
        let retained = rejected.video.packets[0].payload.clone();
        let slice = retained.slice(1..3);
        drop(rejected);
        drop(retained);
        assert_eq!(fence.charged.load(Ordering::Acquire), 8);
        drop(slice);
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        drop(receiver);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }

    #[test]
    fn invalid_budget_and_sender_contention_signal_the_registered_identity() {
        for oversized in [false, true] {
            let (fence, _rx) = Fence::channel(1);
            let observer = fence.revocation();
            let window = AvWindow {
                video: crate::Window {
                    generation: 1,
                    packets: if oversized {
                        vec![crate::Packet {
                            payload: bytes::Bytes::from(vec![0; VideoAdapter::MAX_BYTES + 1]),
                            pts_us: 0,
                            verified_rap: true,
                        }]
                    } else {
                        vec![]
                    },
                },
                audio: Default::default(),
            };
            let locked = (!oversized).then(|| fence.sender.lock().unwrap());
            assert!(fence.try_offer(window).is_err());
            assert!(observer.is_revoked());
            drop(locked);
        }
    }

    #[tokio::test]
    async fn capacity_cancel_uses_same_registry_identity_without_reentrant_hook() {
        let (fence, _rx) = Fence::channel(1);
        let observer = fence.revocation();
        let registry_flag = observer.flag();
        let window = || AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window()).is_ok());
        assert!(fence.try_offer(window()).is_err());
        assert!(registry_flag.load(Ordering::Acquire));
        // A cleanup worker may reenter revoke after being woken: no external
        // callback runs under the sender lock or on the producer call stack.
        observer.cancelled().await;
        fence.revoke();
        assert!(fence.try_offer(window()).is_err());
        let mut wake = fence.cancel.subscribe();
        wake.borrow_and_update();
        fence.revoke();
        fence.revoke();
        assert!(
            !wake.has_changed().unwrap(),
            "repeat cancellation must not emit another transition"
        );
    }

    #[tokio::test]
    async fn cancellation_and_unwind_remove_subscription_ticket() {
        for panic in [false, true] {
            let state = Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                ready: AtomicBool::new(true),
                shutdown: watch::channel(false).0,
            });
            let (fence, _rx) = Fence::channel(1);
            let origin = moq_tokio::origin::spawn();
            state.entries.lock().unwrap().insert(
                "test".into(),
                Entry {
                    origin: origin.consume(),
                    fence: fence.clone(),
                    used: false,
                },
            );
            let cleanup = SubscriptionCleanup {
                state: state.clone(),
                token: "test".into(),
                fence: fence.clone(),
            };
            let (started, ready) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _cleanup = cleanup;
                let _ = started.send(());
                if panic {
                    panic!("intentional cleanup regression");
                }
                std::future::pending::<()>().await;
            });
            ready.await.unwrap();
            if !panic {
                task.abort();
            }
            assert!(task.await.is_err());
            assert!(state.entries.lock().unwrap().is_empty());
            assert!(*fence.cancel.borrow());
        }
    }

    #[tokio::test]
    async fn expiry_removes_ticket_and_releases_source_demand() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State {
            entries: Mutex::new(HashMap::new()),
            ready: AtomicBool::new(true),
            shutdown: watch::channel(false).0,
        });
        let runtime = Runtime {
            state: state.clone(),
            base_url: "https://localhost:4443".into(),
            protocol: "moq-transport-16".into(),
        };
        let (fence, windows) = Fence::channel(1);
        let feed = Feed { generation: 9, catalog: bytes::Bytes::from_static(br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180}}},"audio":{"renditions":{}}}"#), audio_tracks: vec![], windows, demand: Box::new(Guard(released.clone())) };
        let admitted = runtime
            .admit(feed, fence.clone(), Duration::from_secs(1))
            .unwrap();
        assert!(admitted.url.contains("ticket="));
        assert_eq!(state.entries.lock().unwrap().len(), 1);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.entries.lock().unwrap().is_empty());
        assert!(*fence.cancel.borrow());
    }
    #[tokio::test]
    async fn actual_audio_shared_frame_preserves_1500_microseconds_and_credit() {
        let (fence, mut rx) = Fence::channel(1);
        let packet = crate::Packet {
            payload: bytes::Bytes::from_static(&[4, 5, 6]),
            pts_us: 1500,
            verified_rap: false,
        };
        assert!(
            fence
                .try_offer(AvWindow {
                    video: crate::Window {
                        generation: 1,
                        packets: vec![]
                    },
                    audio: std::collections::BTreeMap::from([("audio".into(), vec![packet])])
                })
                .is_ok()
        );
        let window = rx.try_recv().unwrap();
        let serialized = serialized_frame(&window.audio["audio"][0]).unwrap();
        let expected_ptr = serialized.as_ptr();
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        let track = broadcast
            .create_track(
                "audio",
                hang::container::track_info(hang::catalog::PRIORITY.audio),
            )
            .unwrap();
        let mut group = track.append_group().unwrap();
        let mut consumer = group.consume();
        group
            .write_frame(moq_net::Timestamp::from_micros(1500).unwrap(), serialized)
            .unwrap();
        group.finish().unwrap();
        drop(window);
        let frame = consumer.read_frame().await.unwrap().unwrap();
        assert_eq!(
            frame.timestamp,
            moq_net::Timestamp::from_micros(1500).unwrap()
        );
        assert_eq!(frame.payload.as_ptr(), expected_ptr);
        drop((group, consumer, track, broadcast));
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let decoded = hang::container::Frame::decode(frame.payload.clone()).unwrap();
        assert_eq!(
            decoded.timestamp,
            moq_net::Timestamp::from_micros(1500).unwrap()
        );
        assert_eq!(decoded.payload.as_ref(), &[4, 5, 6]);
        drop(decoded);
        drop(frame);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn actual_moq_frame_retains_serialized_owner_and_source_charge() {
        let (fence, mut rx) = Fence::channel(1);
        let window = AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: bytes::Bytes::from_static(&[1, 2, 3]),
                    pts_us: 123,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window).is_ok());
        let window = rx.try_recv().unwrap();
        let serialized = serialized_frame(&window.video.packets[0]).unwrap();
        let expected_ptr = serialized.as_ptr();
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        let track = broadcast
            .create_track("video", crate::VideoAdapter::track_info())
            .unwrap();
        let mut group = track.append_group().unwrap();
        let mut consumer = group.consume();
        group
            .write_frame(moq_net::Timestamp::from_micros(123).unwrap(), serialized)
            .unwrap();
        group.finish().unwrap();
        drop(window);
        fence.revoke();
        let frame = consumer.read_frame().await.unwrap().unwrap();
        assert_eq!(
            frame.payload.as_ptr(),
            expected_ptr,
            "must use upstream Shared rather than Segment copy"
        );
        drop((group, consumer, track, broadcast));
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let decoded = hang::container::Frame::decode(frame.payload.clone()).unwrap();
        assert_eq!(decoded.payload.as_ref(), &[1, 2, 3]);
        drop(decoded);
        drop(frame);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }

    #[test]
    fn retained_payload_credit_survives_queue_and_inflight_clones() {
        let (fence, mut rx) = Fence::channel(1);
        let window = AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: bytes::Bytes::from_static(&[1, 2, 3]),
                    pts_us: 0,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window).is_ok());
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let received = rx.try_recv().unwrap();
        let retained = received.video.packets[0].payload.clone();
        drop(received);
        fence.revoke();
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let slice = retained.slice(1..2);
        drop(retained);
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        drop(slice);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }
    #[test]
    fn revoking_one_peer_preserves_other_peer_and_shared_payload_credits() {
        let payload = bytes::Bytes::from(vec![7u8; 1024]);
        let window = |pts_us| AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: payload.clone(),
                    pts_us,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        let (first, mut first_rx) = Fence::channel(2);
        let (second, mut second_rx) = Fence::channel(2);
        assert!(first.try_offer(window(0)).is_ok());
        assert!(second.try_offer(window(0)).is_ok());
        let first_window = first_rx.try_recv().unwrap();
        let retained_first = first_window.video.packets[0].payload.clone();
        let second_window = second_rx.try_recv().unwrap();
        assert_eq!(retained_first.as_ptr(), payload.as_ptr());
        assert_eq!(
            second_window.video.packets[0].payload.as_ptr(),
            payload.as_ptr()
        );
        first.revoke();
        assert!(first.try_offer(window(1)).is_err());
        assert!(!*second.cancel.borrow());
        let mut continuing_frames = 1;
        for pts in 1..10 {
            assert!(second.try_offer(window(pts)).is_ok());
            let received = second_rx.try_recv().unwrap();
            assert_eq!(received.video.packets[0].pts_us, pts);
            continuing_frames += received.video.packets.len();
        }
        assert_eq!(continuing_frames, 10);
        assert_eq!(first.charged.load(Ordering::Acquire), 1024);
        assert_eq!(second.charged.load(Ordering::Acquire), 1024);
        drop(first_window);
        assert_eq!(first.charged.load(Ordering::Acquire), 1024);
        drop(retained_first);
        assert_eq!(first.charged.load(Ordering::Acquire), 0);
        assert_eq!(second.charged.load(Ordering::Acquire), 1024);
        drop(second_window);
        assert_eq!(second.charged.load(Ordering::Acquire), 0);
        assert!(!*second.cancel.borrow());
    }

    #[test]
    fn fence_rejects_after_revoke_and_bounds_queue() {
        let (fence, _rx) = Fence::channel(1);
        let window = || AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window()).is_ok());
        assert!(fence.try_offer(window()).is_err());
        fence.revoke();
        fence.revoke();
        assert!(fence.try_offer(window()).is_err());
    }

    // ---- shared-ladder runtime: real tracks, real ticket path -------------------------

    fn shared_runtime() -> Runtime {
        Runtime {
            state: Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                ready: AtomicBool::new(true),
                shutdown: watch::channel(false).0,
            }),
            base_url: "https://localhost:4443".into(),
            protocol: "moq-transport-16".into(),
        }
    }

    fn shared_ladder() -> SharedLadder {
        use crate::ladder::{AudioSpec, RenditionSpec};
        SharedLadder::admit(
            5,
            vec![
                RenditionSpec::new("video-360p", "avc1.42c01e", 640, 360, 30, 1, 800_000).unwrap(),
                RenditionSpec::new("video-540p", "avc1.42c01e", 960, 540, 30, 1, 2_000_000)
                    .unwrap(),
                RenditionSpec::new("video-1080p", "avc1.42c01e", 1920, 1080, 30, 1, 6_000_000)
                    .unwrap(),
            ],
            vec![AudioSpec::new("audio", "opus", 48_000, 2, 128_000).unwrap()],
            1_751_846_400_000_000,
        )
        .unwrap()
    }

    fn tag(tag: u8, keyframe: bool) -> crate::Packet {
        let mut payload = vec![0x40u8; 64];
        payload[0] = if keyframe { 0x10 } else { 0x11 };
        payload[1] = tag;
        crate::Packet {
            payload: bytes::Bytes::from(payload),
            pts_us: 0,
            verified_rap: keyframe,
        }
    }

    /// One tick carrying every rung plus independent audio, all on the ladder's clock.
    fn ladder_tick(generation: u64, pts_us: u64, frames: u8) -> LadderWindow {
        let rungs = (0..3u8)
            .map(|rung| crate::Window {
                generation,
                packets: vec![crate::Packet {
                    pts_us,
                    ..tag(rung, true)
                }],
            })
            .collect();
        let mut window = LadderWindow::new(generation, rungs);
        window.audio.insert(
            "audio".into(),
            vec![crate::Packet {
                pts_us,
                ..tag(9, false)
            }],
        );
        assert!(frames > 0, "a tick always carries media");
        window
    }

    #[test]
    fn typed_full_recovers_staggered_raps_without_reviving_authorization() {
        let (fence, mut receiver) = Fence::ladder_channel(1);
        assert!(fence.offer_ladder(ladder_tick(5, 0, 1)).is_ok());
        let refused = fence.offer_ladder(ladder_tick(5, 1, 1)).unwrap_err();
        assert!(matches!(refused, OfferRefusal::Full(_)));
        assert!(!fence.revocation().is_revoked());
        drop(refused);
        drop(receiver.try_recv().unwrap());
        let mut delta = ladder_tick(5, 2, 1);
        for rung in &mut delta.renditions {
            rung.packets[0].verified_rap = false;
        }
        delta.renditions[1].packets.clear();
        assert!(fence.offer_ladder(delta.clone()).is_ok());
        let recovered = receiver.try_recv().unwrap();
        assert!(
            recovered
                .renditions
                .iter()
                .all(|rung| rung.packets.is_empty())
        );
        assert_eq!(recovered.audio["audio"][0].pts_us, 2);
        delta.renditions[0].packets.push(crate::Packet {
            pts_us: 3,
            ..tag(0, true)
        });
        assert!(fence.offer_ladder(delta).is_ok());
        let recovered = receiver.try_recv().unwrap();
        assert_eq!(recovered.renditions[0].packets.len(), 1);
        assert_eq!(recovered.renditions[0].packets[0].pts_us, 3);
        assert_eq!(recovered.discontinuities & 7, 7);
        assert!(recovered.renditions[2].packets.is_empty());
        assert!(fence.offer_ladder(ladder_tick(5, 3, 1)).is_ok());
        assert!(matches!(
            fence.offer_ladder(ladder_tick(5, 4, 1)),
            Err(OfferRefusal::Full(_))
        ));
        fence.revoke();
        drop(receiver.try_recv().unwrap());
        assert!(matches!(
            fence.offer_ladder(ladder_tick(5, 5, 1)),
            Err(OfferRefusal::Revoked(_))
        ));
    }

    #[test]
    fn typed_ladder_shape_is_terminal_before_retention() {
        let (fence, mut receiver) = Fence::ladder_channel(1);
        assert!(fence.offer_ladder(ladder_tick(5, 0, 1)).is_ok());
        drop(receiver.try_recv().unwrap());
        let mut wrong = ladder_tick(5, 1, 1);
        wrong.renditions[1].generation = 6;
        assert!(matches!(
            fence.offer_ladder(wrong),
            Err(OfferRefusal::Shape(_))
        ));
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
        assert!(fence.revocation().is_revoked());
        assert!(matches!(
            fence.offer_ladder(ladder_tick(5, 2, 1)),
            Err(OfferRefusal::Revoked(_))
        ));
    }

    fn token_of(url: &str) -> String {
        url.split("ticket=")
            .nth(1)
            .expect("ticket in url")
            .to_string()
    }

    fn origin_of(runtime: &Runtime, token: &str) -> moq_net::origin::Consumer {
        runtime
            .state
            .entries
            .lock()
            .unwrap()
            .get(token)
            .expect("admitted ticket")
            .origin
            .clone()
    }

    fn retain() -> moq_net::track::Subscription {
        moq_net::track::Subscription::default().with_max_age(Duration::from_secs(2))
    }

    fn published_tag(frame: &moq_net::frame::Frame) -> u8 {
        hang::container::Frame::decode(frame.payload.clone())
            .expect("legacy container frame")
            .payload[1]
    }

    fn shared_feed(capacity_bps: Option<u64>, windows: mpsc::Receiver<LadderWindow>) -> LadderFeed {
        LadderFeed {
            ladder: shared_ladder(),
            capacity_bps,
            config: crate::uplink::UplinkConfig::default(),
            windows,
            demand: Box::new(()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_ladder_serves_two_peers_independently_on_one_common_clock() {
        let runtime = shared_runtime();
        let (fence, windows) = Fence::ladder_channel(2);
        let admission = runtime
            .admit_shared(
                shared_feed(Some(10_000_000), windows),
                fence.clone(),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(admission.broadcast, "media");
        assert_eq!(admission.generation, 5);
        assert_eq!(
            admission.level(),
            2,
            "10 Mbps funds the whole 9.82 Mbps ladder"
        );
        assert!(admission.stalled().is_empty());
        assert_eq!(
            admission.track_names(),
            vec!["video-360p", "video-540p", "video-1080p", "audio"]
        );
        assert_eq!(admission.handle().published_track_count(), 6);

        let token = token_of(&admission.url);
        let origin = origin_of(&runtime, &token);
        let broadcast = origin.request_broadcast("media").await.expect("resolves");
        let mut low = broadcast
            .track("video-360p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();
        let mut high = broadcast
            .track("video-1080p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();
        let mut audio = broadcast
            .track("audio")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();

        assert!(fence.try_offer_ladder(ladder_tick(5, 0, 1)).is_ok());
        let mut low_group = low.recv_group().await.unwrap().unwrap();
        let low_frame = low_group.read_frame().await.unwrap().unwrap();
        let mut high_group = high.recv_group().await.unwrap().unwrap();
        let high_frame = high_group.read_frame().await.unwrap().unwrap();
        let mut audio_group = audio.recv_group().await.unwrap().unwrap();
        let audio_frame = audio_group.read_frame().await.unwrap().unwrap();

        assert_eq!(published_tag(&low_frame), 0, "peer A holds the low rung");
        assert_eq!(published_tag(&high_frame), 2, "peer B holds the high rung");
        assert_eq!(
            published_tag(&audio_frame),
            9,
            "audio is independent of the rung"
        );
        assert_eq!(
            low_frame.timestamp, high_frame.timestamp,
            "every rung is stamped against the ladder's one clock"
        );

        // Published tracks never grow with the number of peers.
        assert_eq!(admission.handle().published_track_count(), 6);
        assert_eq!(admission.level(), 2);
    }

    #[tokio::test]
    async fn initial_delta_keeps_ticket_until_verified_rap() {
        let runtime = shared_runtime();
        let (fence, windows) = Fence::ladder_channel(4);
        let admission = runtime
            .admit_shared(
                shared_feed(Some(10_000_000), windows),
                fence.clone(),
                Duration::from_secs(30),
            )
            .unwrap();
        let token = token_of(&admission.url);
        let origin = origin_of(&runtime, &token);
        let broadcast = origin.request_broadcast("media").await.unwrap();
        let mut video = broadcast
            .track("video-360p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();
        let mut delta = ladder_tick(5, 0, 1);
        for rung in &mut delta.renditions {
            rung.packets[0].verified_rap = false;
        }
        fence.offer_ladder(delta).unwrap();
        tokio::task::yield_now().await;
        assert!(!fence.revocation().is_revoked());
        assert!(runtime.state.entries.lock().unwrap().contains_key(&token));
        fence.offer_ladder(ladder_tick(5, 1, 1)).unwrap();
        let mut group = tokio::time::timeout(Duration::from_secs(1), video.recv_group())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = group.read_frame().await.unwrap().unwrap();
        assert_eq!(
            hang::container::Frame::decode(frame.payload)
                .unwrap()
                .timestamp
                .as_micros(),
            1
        );
        assert!(!fence.revocation().is_revoked());
    }

    #[tokio::test]
    async fn explicit_gap_finishes_actual_group_before_recovered_rap() {
        let ladder = Arc::new(shared_ladder());
        let origin = moq_tokio::origin::spawn_config(Default::default());
        let broadcast = origin.create_broadcast("gap-test").unwrap();
        broadcast.announce(Default::default()).unwrap();
        let publication = SharedPublication::create((*ladder).clone(), &broadcast).unwrap();
        let budget =
            UplinkBudget::from_ladder(&ladder, Some(10_000_000), Default::default()).unwrap();
        let admitted = AdmittedLadder::new(ladder, publication, budget);
        let mut track = admitted
            .publication
            .lock()
            .unwrap()
            .rendition_track(0)
            .unwrap()
            .consume()
            .subscribe(retain())
            .await
            .unwrap();
        admitted.publish(&ladder_tick(5, 0, 1)).unwrap();
        let mut old = track.recv_group().await.unwrap().unwrap();
        old.read_frame().await.unwrap().unwrap();
        let mut gap = ladder_tick(5, 1, 1);
        gap.discontinuities = 7;
        for rung in &mut gap.renditions {
            rung.packets.clear();
        }
        admitted.publish(&gap).unwrap();
        admitted.publish(&ladder_tick(5, 2, 1)).unwrap();
        let mut recovered = tokio::time::timeout(Duration::from_secs(1), track.recv_group())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = recovered.read_frame().await.unwrap().unwrap();
        assert_eq!(published_tag(&frame), 0);
        assert_eq!(
            hang::container::Frame::decode(frame.payload)
                .unwrap()
                .timestamp
                .as_micros(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_one_subscription_does_not_stop_another() {
        let runtime = shared_runtime();
        let (first_fence, first_windows) = Fence::ladder_channel(2);
        let first = runtime
            .admit_shared(
                shared_feed(Some(10_000_000), first_windows),
                first_fence.clone(),
                Duration::from_secs(30),
            )
            .unwrap();
        let (second_fence, second_windows) = Fence::ladder_channel(2);
        let second = runtime
            .admit_shared(
                shared_feed(Some(10_000_000), second_windows),
                second_fence.clone(),
                Duration::from_secs(30),
            )
            .unwrap();

        let first_token = token_of(&first.url);
        let origin = origin_of(&runtime, &first_token);
        let broadcast = origin.request_broadcast("media").await.expect("resolves");
        let mut low = broadcast
            .track("video-360p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();
        assert!(first_fence.try_offer_ladder(ladder_tick(5, 0, 1)).is_ok());
        low.recv_group().await.unwrap().unwrap();

        // Cancelling the first subscription is terminal for it only.
        first_fence.revoke();
        assert!(
            first_fence
                .try_offer_ladder(ladder_tick(5, 33_333, 1))
                .is_err()
        );
        assert!(
            second_fence
                .try_offer_ladder(ladder_tick(5, 33_333, 1))
                .is_ok()
        );
        let second_token = token_of(&second.url);
        let origin = origin_of(&runtime, &second_token);
        let broadcast = origin.request_broadcast("media").await.expect("resolves");
        let mut second_low = broadcast
            .track("video-360p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();
        assert!(
            second_fence
                .try_offer_ladder(ladder_tick(5, 66_666, 1))
                .is_ok()
        );
        let mut group = None;
        for _ in 0..8 {
            group = second_low.recv_group().await.unwrap();
            if group.is_some() {
                break;
            }
        }
        assert!(group.is_some(), "the second subscription keeps publishing");
    }

    #[test]
    fn retained_credit_is_bounded_across_every_rung_of_one_ladder_window() {
        let (fence, mut rx) = Fence::ladder_channel(1);
        // One tick, every rung and audio, 384 KiB total.
        let mut window = LadderWindow::new(
            5,
            (0..3)
                .map(|rung| crate::Window {
                    generation: 5,
                    packets: vec![crate::Packet {
                        payload: bytes::Bytes::from(vec![rung as u8; 128 * 1024]),
                        pts_us: 0,
                        verified_rap: true,
                    }],
                })
                .collect(),
        );
        window.audio.insert(
            "audio".into(),
            vec![crate::Packet {
                payload: bytes::Bytes::from(vec![7u8; 1024]),
                pts_us: 0,
                verified_rap: false,
            }],
        );
        let expected = window.retained_bytes();
        assert_eq!(expected, 3 * 128 * 1024 + 1024);
        assert!(fence.try_offer_ladder(window).is_ok());
        assert_eq!(fence.charged.load(Ordering::Acquire), expected);

        // Dropping every retained reference refunds the whole charge.
        let received = rx.try_recv().unwrap();
        let retained = received.renditions[0].packets[0].payload.clone();
        drop(received);
        fence.revoke();
        assert_eq!(
            fence.charged.load(Ordering::Acquire),
            128 * 1024,
            "only the retained rung clone stays charged"
        );
        drop(retained);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);

        // A ladder window larger than the per-window bound is refused outright.
        let (fence, _rx) = Fence::ladder_channel(1);
        let oversized = LadderWindow::new(
            5,
            (0..3)
                .map(|_| crate::Window {
                    generation: 5,
                    packets: vec![crate::Packet {
                        payload: bytes::Bytes::from(vec![0u8; 2 * 1024 * 1024]),
                        pts_us: 0,
                        verified_rap: true,
                    }],
                })
                .collect(),
        );
        assert!(fence.try_offer_ladder(oversized).is_err());
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_uplink_budget_drives_what_is_published_and_stalls_without_removing_tracks() {
        let runtime = shared_runtime();
        let (fence, windows) = Fence::ladder_channel(2);
        let admission = runtime
            .admit_shared(
                shared_feed(Some(1_100_000), windows),
                fence.clone(),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(admission.level(), 0, "1.1 Mbps funds only the lowest rung");
        assert_eq!(admission.stalled(), vec![1, 2]);
        let tracks_before = admission.handle().published_track_count();

        let token = token_of(&admission.url);
        let origin = origin_of(&runtime, &token);
        let broadcast = origin.request_broadcast("media").await.expect("resolves");
        let mut low = broadcast
            .track("video-360p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();
        let mut high = broadcast
            .track("video-1080p")
            .unwrap()
            .subscribe(retain())
            .await
            .unwrap();

        assert!(fence.try_offer_ladder(ladder_tick(5, 0, 1)).is_ok());
        let mut low_group = low.recv_group().await.unwrap().unwrap();
        assert_eq!(
            published_tag(&low_group.read_frame().await.unwrap().unwrap()),
            0
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), high.recv_group())
                .await
                .is_err(),
            "an unaffordable rung is not offered"
        );

        // A stated capacity change is explicit and immediately drives the publication.
        let directive = admission.handle().set_capacity(Some(10_000_000));
        assert_eq!(directive, Directive::Raise { from: 0, to: 2 });
        assert!(admission.stalled().is_empty());
        assert_eq!(
            admission.handle().published_track_count(),
            tracks_before,
            "an uplink change never removes production output"
        );
        assert!(fence.try_offer_ladder(ladder_tick(5, 33_333, 1)).is_ok());
        let mut group = None;
        for _ in 0..8 {
            group = high.recv_group().await.unwrap();
            if group.is_some() {
                break;
            }
        }
        let mut group = group.expect("the newly admitted rung is published");
        assert_eq!(
            published_tag(&group.read_frame().await.unwrap().unwrap()),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_single_rendition_admission_is_explicitly_not_adaptive() {
        let runtime = shared_runtime();
        let (fence, windows) = Fence::channel(1);
        let feed = Feed {
            generation: 9,
            catalog: bytes::Bytes::from_static(
                br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180}}},"audio":{"renditions":{}}}"#,
            ),
            audio_tracks: vec![],
            windows,
            demand: Box::new(()),
        };
        let admission = runtime
            .admit(feed, fence.clone(), Duration::from_secs(30))
            .unwrap();
        let descriptor: serde_json::Value = serde_json::from_slice(
            &single_rendition_descriptor(admission.generation, &["video".to_string()]).unwrap(),
        )
        .unwrap();
        assert_eq!(descriptor["adaptive"], serde_json::json!(false));
        assert_eq!(
            descriptor["notAdaptiveReason"],
            serde_json::json!(NotAdaptiveReason::SingleRendition.as_str())
        );

        // A multi-rendition catalog cannot silently take the single-rendition path.
        let (fence, windows) = Fence::channel(1);
        let multi = Feed {
            generation: 10,
            catalog: bytes::Bytes::from_static(
                br#"{"video":{"renditions":{"a":{"codec":"vp8","codedWidth":320,"codedHeight":180},"b":{"codec":"vp8","codedWidth":640,"codedHeight":360}}},"audio":{"renditions":{}}}"#,
            ),
            audio_tracks: vec![],
            windows,
            demand: Box::new(()),
        };
        assert!(
            runtime
                .admit(multi, fence, Duration::from_secs(30))
                .is_err()
        );
    }
}
