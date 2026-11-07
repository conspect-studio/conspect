//! Per-source **runtime audio ingest** — the audio peer of the video decode
//! thread (AUD-2).
//!
//! The pure decode + resample logic lives in [`multiview_audio`] (the
//! [`audio_decode_loop`](multiview_audio::store::audio_decode_loop) over an
//! [`AudioFileDecoder`](multiview_audio::AudioFileDecoder), itself a thin wrapper
//! over the [`multiview_ffmpeg`] safe seam): it opens the container on the worker
//! thread, decodes + resamples to the canonical 48 kHz / stereo / `f32` format,
//! and publishes blocks into a lock-free [`AudioStore`]. This module is the
//! **cli-side supervision**: it maps a configured source to a decodable location
//! and drives the decode loop under the SAME supervised-reconnect bracket the
//! video [`ingest_loop`](crate::pipeline) uses — a live source whose audio drops
//! or EOFs reconnects with capped-exponential, jittered backoff; a finite file
//! plays once and then rides silence (the store silence-fills past EOF, so the
//! sampled track is gap-free — ADR-R005 §4.1).
//!
//! ## It samples, it never paces (invariants #1/#2/#10)
//! The decode thread only ever **writes** the lock-free [`AudioStore`] it shares
//! by `Arc` with the [`ProgramBus`](multiview_audio::ProgramBus). The output
//! clock *samples* that store per tick (a wait-free read that silence-fills any
//! un-written span), so a slow, fast, wedged, or never-ending audio source can
//! neither pace nor stall the output clock (invariant #1) and cannot
//! back-pressure the engine (invariant #10) — exactly like the video tiles.
//!
//! ## Bad inputs are the purpose
//! An open failure, a no-audio source, or a mid-stream decode error logs and
//! ends *this* connection — never the run. A live source then reconnects; a
//! finite/audio-free source rides silence. No decode fault ever crashes or
//! stalls the program (bulletproof continuous output is the product).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_audio::store::AudioStore;
use multiview_audio::{AudioFormat, ChannelLayout, ToneGenerator};
use multiview_core::time::Rational;

use crate::pipeline::{next_reconnect_attempt, reconnect_backoff, sleep_interruptible, JitterRng};

/// The bounded depth (in frames) of each per-source [`AudioStore`].
///
/// Two seconds at the canonical 48 kHz is ample headroom for the decode thread
/// to run ahead of the output clock's per-tick pull without the ring ever
/// growing: anything beyond this is dropped oldest-first (the data-plane "queues
/// drop, never grow" rule). Mirrors the `96_000` the program-bus unit tests use.
const STORE_CAPACITY_FRAMES: usize = 96_000;

/// Half a second of cushion for a live source with no video to lock to.
const AUDIO_ONLY_PREBUFFER_FRAMES: usize = 24_000;

/// The canonical program audio format every per-source store is built at: 48 kHz
/// stereo `f32`. Every source's decode resamples to this BEFORE publishing, so
/// the [`ProgramBus`](multiview_audio::ProgramBus) mixes a single uniform format
/// (streaming-gotchas §5: "per-input resample to 48k fltp BEFORE mixing").
#[must_use]
pub(crate) fn canonical_format() -> AudioFormat {
    AudioFormat::new(AudioFormat::CANONICAL_RATE, ChannelLayout::Stereo)
}

/// Build an empty per-source [`AudioStore`] at the [`canonical_format`], shared
/// (`Arc`) between this source's decode thread (writer) and the program bus
/// (reader). Returned to the pipeline so it can both route the store onto the
/// bus AND hand a clone to the decode thread.
#[must_use]
pub(crate) fn new_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::new(canonical_format(), STORE_CAPACITY_FRAMES))
}

/// Build a finite-file store. It preserves retained PCM instead of re-syncing
/// to the append edge; the decode pacer bounds producer lead off the output clock.
#[must_use]
pub(crate) fn new_file_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::finite_file(
        canonical_format(),
        STORE_CAPACITY_FRAMES,
    ))
}

/// Scheduling-independent bars source tone, pulled at the consumer sample index.
#[must_use]
pub(crate) fn new_tone_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::line_up_tone(canonical_format()))
}

/// A store for a **live network** source's decoded audio, locked to its video:
/// a [`timeline`](AudioStore::timeline) store whose frames are output-timeline
/// positions. The decode places each block where the source's video stamper
/// put the same PTS ([`crate::pipeline::live_anchor_slot`]), and the bus reads
/// it at its own sample clock.
pub(crate) fn new_live_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::timeline(
        canonical_format(),
        multiview_audio::store::TIMELINE_CAPACITY,
    ))
}

/// A store for a live source running **audio only**. No video stamps a timeline
/// for a [`timeline`](AudioStore::timeline) store, and that store plays nothing
/// without an anchor, so this one buffers on its own clock instead.
pub(crate) fn new_audio_only_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::live(
        canonical_format(),
        STORE_CAPACITY_FRAMES,
        AUDIO_ONLY_PREBUFFER_FRAMES,
    ))
}

/// The source's latest video anchor, as the audio crate's [`PtsAnchor`]
/// (`None` before video anchors, or when its anchor frame carried no raw PTS).
fn video_anchor(
    slot: &crate::pipeline::LiveAnchorSlot,
    floor: u64,
) -> Option<multiview_audio::store::PtsAnchor> {
    let a = slot.load()?;
    if a.generation <= floor {
        return None;
    }
    Some(multiview_audio::store::PtsAnchor {
        generation: a.generation,
        out0_ns: a.out0.as_nanos(),
        raw_pts0_ns: a.raw_pts0_ns?,
    })
}

#[cfg(test)]
pub(crate) fn test_video_anchor(
    slot: &crate::pipeline::LiveAnchorSlot,
) -> Option<multiview_audio::store::PtsAnchor> {
    video_anchor(slot, 0)
}

/// Maximum unresolved operator re-points retained while the bake consumer is slow.
pub(crate) const MAX_AUDIO_REPOINT_BACKLOG: usize = 256;

#[derive(Clone)]
struct AudioRepoint {
    point: multiview_audio::RoutePoint,
    store: Arc<AudioStore>,
    ramp_frames: usize,
}

/// The program-bus gain ramp a live routing change rides: 40 ms at 48 kHz —
/// long enough to be click-free, short enough to land within ~2 output ticks.
pub(crate) const LIVE_GAIN_RAMP_FRAMES: usize = 1_920;

/// One requested program-bus gain table: `(channel id, linear gain)` pairs.
type GainTable = Arc<Vec<(String, f64)>>;

/// A live change to the program bus's MEMBERSHIP (a source added, edited or
/// removed while the program runs), applied by the bake consumer.
#[derive(Clone)]
enum MemberChange {
    /// `id` now feeds `store`: a new id joins the bus (at the routed gain, zero
    /// when unrouted); an id already on the bus cross-fades onto the new store.
    Join { id: String, store: Arc<AudioStore> },
    /// `id` leaves the bus: faded out click-free, then its strip is reclaimed.
    Leave { id: String },
}

/// The cross-fade a live membership change rides (10 ms at 48 kHz).
const MEMBER_RAMP_FRAMES: usize = 480;

/// A copy-on-write map published through a `SwapCell` (wait-free readers).
type SharedMap<V> = Arc<multiview_framestore::SwapCell<Arc<std::collections::HashMap<String, V>>>>;

#[derive(Default)]
struct MemberMailbox {
    changes: std::collections::BTreeMap<String, MemberChange>,
    closed: bool,
}

/// Bounded latest-intent mailbox between the off-clock hub and bake consumer.
/// No lock or client is shared with the output clock.
///
/// The bus membership is LIVE: a source added at runtime registers its store
/// with [`Self::set_source_audio`] (from the live-source hub worker), the bake
/// consumer adds it to the running bus at its next tick, and routing it then
/// applies live. The channel table uses keyed off-clock access; only the bake
/// consumer ever touches the bus (inv #1/#10).
#[derive(Clone)]
pub struct AudioControlHandle {
    pending: Arc<multiview_framestore::SwapCell<Arc<Vec<AudioRepoint>>>>,
    /// The latest requested program-bus gain table (`(channel id, linear gain)`).
    /// Latest-wins: a routing PUT replaces the whole table, so an unapplied older
    /// table is superseded, never queued (bounded at one table — inv #10).
    pending_gains: Arc<multiview_framestore::SwapCell<Option<GainTable>>>,
    /// Keyed latest intent: one slot per admitted source, irrespective of churn.
    /// Both producer and bake consumer run OFF the output clock; each consumer
    /// tick removes at most 16 entries without scanning/cloning the backlog.
    pending_members: Arc<(std::sync::Mutex<MemberMailbox>, std::sync::Condvar)>,
    /// The per-source audio stores (what a breakaway can re-point onto).
    sources: SharedMap<Arc<AudioStore>>,
    /// The bus route point of every source on the running bus. Written only by
    /// the bake consumer (after it changed the bus); read by the control side.
    channels:
        Arc<std::sync::RwLock<std::collections::HashMap<String, multiview_audio::RoutePoint>>>,
    /// Publishes the audio-route consumers whenever a routing document applies
    /// live, so a newly routed source starts and an unrouted one stops.
    route_demand: Option<multiview_engine::ConsumerSet<multiview_compositor::pipeline::Nv12Image>>,
    /// Every source id that exists on this run, with or without audio: what a
    /// routing document may name and still apply live (an audio-free source
    /// mixes as silence — a restart would not change that).
    known: SharedMap<()>,
    /// The latest routing document, so a source joining later rides its gain.
    routing: Arc<multiview_framestore::SwapCell<Option<Arc<multiview_config::AudioRouting>>>>,
    sample_rate: u32,
    /// Sources demand started on a run with no audio routes. Such a run mixes
    /// every source at unity, so these join at unity rather than zero.
    unrouted_unity: Arc<multiview_framestore::SwapCell<Arc<std::collections::HashSet<String>>>>,
    /// Output tracks are pinned to source ids at startup until the discrete
    /// encoder's route table accepts live updates.
    declared_tracks: std::collections::HashMap<String, usize>,
    /// Shared stable-index source list read by the bake consumer each tick.
    live_track_inputs: Option<Arc<multiview_framestore::SwapCell<Arc<Vec<String>>>>>,
    /// Test-only interleaving point inside [`Self::request_routing`], after the
    /// channel snapshot and before the gain table is published.
    #[cfg(test)]
    routing_gap_hook: RoutingGapHook,
}

#[cfg(test)]
type RoutingGapHook = Arc<std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// The consumer owns this guard from thread spawn through normal or panic exit.
pub struct AudioConsumerGuard(AudioControlHandle);

impl Drop for AudioConsumerGuard {
    fn drop(&mut self) {
        self.0.close_consumer();
    }
}

impl Default for AudioControlHandle {
    fn default() -> Self {
        Self::new()
    }
}

fn shared_map<V>(map: std::collections::HashMap<String, V>) -> SharedMap<V> {
    Arc::new(multiview_framestore::SwapCell::from_pointee(map))
}

fn map_insert<V: Clone>(cell: &SharedMap<V>, id: &str, value: V) {
    cell.rcu(|current| {
        let mut next = current.as_ref().clone();
        next.insert(id.to_owned(), value.clone());
        next
    });
}

fn map_remove<V: Clone>(cell: &SharedMap<V>, id: &str) {
    if cell.load().contains_key(id) {
        cell.rcu(|current| {
            let mut next = current.as_ref().clone();
            next.remove(id);
            next
        });
    }
}

impl AudioControlHandle {
    /// Create an empty mailbox; route resolution is installed at run startup.
    #[must_use]
    pub fn new() -> Self {
        Self::from_sources(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        )
    }

    /// Create the run's source and destination registry, keyed by configured ids.
    #[must_use]
    pub fn from_sources(
        sources: std::collections::HashMap<String, Arc<AudioStore>>,
        channels: std::collections::HashMap<String, multiview_audio::RoutePoint>,
    ) -> Self {
        let known = sources
            .keys()
            .chain(channels.keys())
            .map(|id| (id.clone(), ()))
            .collect();
        Self {
            pending: Arc::new(multiview_framestore::SwapCell::from_pointee(Vec::new())),
            pending_gains: Arc::new(multiview_framestore::SwapCell::empty()),
            pending_members: Arc::new((
                std::sync::Mutex::new(MemberMailbox::default()),
                std::sync::Condvar::new(),
            )),
            sources: shared_map(sources),
            channels: Arc::new(std::sync::RwLock::new(channels)),
            known: shared_map(known),
            route_demand: None,
            routing: Arc::new(multiview_framestore::SwapCell::empty()),
            unrouted_unity: Arc::new(multiview_framestore::SwapCell::from_pointee(
                std::collections::HashSet::new(),
            )),
            sample_rate: AudioFormat::CANONICAL_RATE,
            declared_tracks: std::collections::HashMap::new(),
            live_track_inputs: None,
            #[cfg(test)]
            routing_gap_hook: Arc::default(),
        }
    }

    /// Record the running program bus's sample rate (what a routing document's
    /// `sample_rate_hz` must equal to apply live).
    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Record the routing the bus was built with (a later-joining source rides
    /// its gain) and every startup source id, audio-bearing or not.
    #[must_use]
    pub fn with_startup(
        self,
        routing: Option<&multiview_config::AudioRouting>,
        source_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        self.routing.store(routing.cloned().map(Arc::new));
        let ids: Vec<String> = source_ids.into_iter().collect();
        self.known.rcu(|current| {
            let mut next = current.as_ref().clone();
            for id in &ids {
                next.insert(id.clone(), ());
            }
            next
        });
        self
    }

    /// Publish the audio-route consumers through `set` on every live routing
    /// change.
    #[must_use]
    pub fn with_route_demand(
        mut self,
        set: Option<multiview_engine::ConsumerSet<multiview_compositor::pipeline::Nv12Image>>,
    ) -> Self {
        self.route_demand = set;
        self
    }

    /// Keep the membership mailbox open for the lifetime of the bake consumer.
    #[must_use]
    pub fn consumer_guard(&self) -> AudioConsumerGuard {
        AudioConsumerGuard(self.clone())
    }

    /// Close admission when the sole bus consumer exits, including on panic.
    /// Wakes every producer blocked on a full mailbox; pending intent remains
    /// available for inspection, but no longer promises delivery to a dead bus.
    pub fn close_consumer(&self) {
        let (pending, drained) = &*self.pending_members;
        let mut pending = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.closed = true;
        drained.notify_all();
    }

    fn push_member(&self, change: MemberChange) -> bool {
        let id = match &change {
            MemberChange::Join { id, .. } | MemberChange::Leave { id } => id.clone(),
        };
        // Both sides are off-clock. At capacity, wait for the bake consumer to
        // drain rather than shed a terminal leave or grow memory unbounded.
        // A consumed stale intent reads current authoritative state when applied;
        // newer producer updates enqueue their own latest intent.
        let (pending, drained) = &*self.pending_members;
        let mut pending = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !pending.closed
            && pending.changes.len() >= multiview_config::MAX_ADMITTED_SOURCES
            && !pending.changes.contains_key(&id)
        {
            pending = drained
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if pending.closed {
            return false;
        }
        pending.changes.insert(id, change);
        true
    }

    /// A source now exists on the run (added or edited live). With `Some(store)`
    /// its decoded audio joins the RUNNING program bus at its next tick — at the
    /// current routing's gain, zero when the routing does not mix it — or, on an
    /// edit, cross-fades onto the new store. With `None` the source carries no
    /// audio (it may still be routed: it mixes as silence). Bounded, with
    /// backpressure only on the off-clock hub worker at saturation; never the
    /// output clock.
    #[must_use]
    pub fn set_source_audio(&self, id: &str, store: Option<Arc<AudioStore>>) -> bool {
        map_insert(&self.known, id, ());
        if let Some(store) = store {
            map_insert(&self.sources, id, Arc::clone(&store));
            self.push_member(MemberChange::Join {
                id: id.to_owned(),
                store,
            })
        } else {
            map_remove(&self.sources, id);
            self.push_member(MemberChange::Leave { id: id.to_owned() })
        }
    }

    /// Observe the actual store the live hub handed to the bus, for in-module
    /// lifecycle regression tests only. Never exposed in production.
    #[cfg(test)]
    pub(crate) fn test_source_store(&self, id: &str) -> Option<Arc<AudioStore>> {
        self.sources.load().get(id).cloned()
    }

    #[cfg(test)]
    fn set_routing_gap_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .routing_gap_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn run_routing_gap_hook(&self) {
        let hook = self
            .routing_gap_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn test_source_point(&self, id: &str) -> Option<multiview_audio::RoutePoint> {
        self.channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .copied()
    }

    /// A source was removed from the run: its bus point goes away (faded out,
    /// then reclaimed) and routing it needs it back first. Bounded; hub worker.
    #[must_use]
    pub fn remove_source(&self, id: &str) -> bool {
        map_remove(&self.known, id);
        map_remove(&self.sources, id);
        self.push_member(MemberChange::Leave { id: id.to_owned() })
    }

    /// Capture the output's startup-pinned track-to-source wiring.
    #[must_use]
    pub fn with_declared_tracks(
        mut self,
        routing: Option<&multiview_config::AudioRouting>,
        live_track_inputs: Arc<multiview_framestore::SwapCell<Arc<Vec<String>>>>,
    ) -> Self {
        self.declared_tracks = routing
            .map(|r| {
                r.routes
                    .iter()
                    .filter_map(|route| route.target_track.as_deref())
                    .enumerate()
                    .map(|(index, name)| (name.to_owned(), index))
                    .collect()
            })
            .unwrap_or_default();
        self.live_track_inputs = Some(live_track_inputs);
        self
    }

    /// Apply an `[audio]` routing document to the RUNNING program bus, if it can
    /// be applied live. Returns `true` when the new gain table was published for
    /// the bake consumer (it lands at its next tick, ramped click-free), `false`
    /// when only a restart can honour it: a different `sample_rate_hz` than the
    /// running bus, or a route that mixes an input that does not exist on this
    /// run. Off-clock control path; never touches the bus or output clock (inv #1/#10).
    #[must_use]
    pub fn request_routing(&self, routing: &multiview_config::AudioRouting) -> bool {
        if routing.sample_rate_hz != self.sample_rate {
            return false;
        }
        let known = self.known.load();
        let requested: Vec<(&str, &str)> = routing
            .routes
            .iter()
            .filter_map(|route| {
                route
                    .target_track
                    .as_deref()
                    .map(|name| (name, route.input_id.as_str()))
            })
            .collect();
        if requested.len() != self.declared_tracks.len()
            || requested.iter().enumerate().any(|(index, (name, source))| {
                self.declared_tracks.get(*name) != Some(&index) || !known.contains_key(*source)
            })
        {
            // An added/removed/reordered output track needs output-only
            // reconfiguration; a source absent from the live registry cannot be routed.
            return false;
        }
        if routing
            .routes
            .iter()
            .any(|r| r.contributes_to_program() && !known.contains_key(&r.input_id))
        {
            return false;
        }
        // Review H3 publication protocol: publish the routing document BEFORE
        // taking the channel snapshot. A live join inserts its channel under the
        // `channels` write lock and only then re-reads the routing. The lock
        // totally orders the two: either this snapshot is taken after the
        // join's insert (the gain table covers the new channel), or the join's
        // write lock is acquired after this read lock is released — which is
        // after the store below — so its re-read sees this routing. A joining
        // source can therefore never keep a superseded gain.
        self.routing.store(Some(Arc::new(routing.clone())));
        let channels = self
            .channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gains =
            crate::pipeline::program_bus_gains(channels.keys().map(String::as_str), Some(routing));
        drop(channels);
        #[cfg(test)]
        self.run_routing_gap_hook();
        if let Some(live) = &self.live_track_inputs {
            live.store(Arc::new(
                requested
                    .iter()
                    .map(|(_, source)| (*source).to_owned())
                    .collect(),
            ));
        }
        self.pending_gains.store(Some(Arc::new(gains)));
        if let Some(set) = &self.route_demand {
            set.publish(crate::source_demand::audio_route_items(Some(routing)));
        }
        true
    }

    /// Let `id` join at unity while no routing is declared, matching what a
    /// source bound at startup gets.
    pub(crate) fn allow_unrouted_unity(&self, id: &str) {
        if !self.unrouted_unity.load().contains(id) {
            self.unrouted_unity.rcu(|current| {
                let mut next = current.as_ref().clone();
                next.insert(id.to_owned());
                next
            });
        }
    }

    /// The gain a source joining the bus now rides: the latest routing's, or
    /// zero when no routing mixes it (never put unrouted audio on air).
    fn joining_gain(&self, id: &str) -> f64 {
        let routed = self
            .routing
            .load()
            .is_some_and(|routing| !routing.routes.is_empty());
        if !routed && self.unrouted_unity.load().contains(id) {
            return 1.0;
        }
        self.routing
            .load()
            .filter(|routing| !routing.routes.is_empty())
            .and_then(|routing| {
                crate::pipeline::program_bus_gains(std::iter::once(id), Some(&routing))
                    .first()
                    .map(|(_, gain)| *gain)
            })
            .unwrap_or(0.0)
    }

    /// Resolve a command against the run's actual channels and per-source stores.
    /// A missing target/source stays held, never silently binds a wrong source.
    #[must_use]
    pub fn request_route(&self, target: &str, source_id: &str, ramp_frames: usize) -> bool {
        let channels = self
            .channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sources = self.sources.load();
        let (Some(&point), Some(store)) = (channels.get(target), sources.get(source_id)) else {
            return false;
        };
        self.request_repoint(point, Arc::clone(store), ramp_frames);
        true
    }

    /// Publish a resolved re-point; a wedged consumer cannot block the producer.
    pub fn request_repoint(
        &self,
        point: multiview_audio::RoutePoint,
        store: Arc<AudioStore>,
        ramp_frames: usize,
    ) {
        let current = self.pending.load();
        let mut next = current.as_ref().clone();
        if next.len() >= MAX_AUDIO_REPOINT_BACKLOG {
            next.remove(0);
        }
        next.push(AudioRepoint {
            point,
            store,
            ramp_frames,
        });
        self.pending.store(Arc::new(next));
    }

    /// Apply one membership change to the bus (bake consumer only).
    fn apply_member(&self, bus: &mut multiview_audio::program::ProgramBus, change: &MemberChange) {
        let existing = self
            .channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(match change {
                MemberChange::Join { id, .. } | MemberChange::Leave { id } => id.as_str(),
            })
            .copied();
        match (change, existing) {
            (MemberChange::Join { id, store }, Some(point)) => {
                if let Err(error) =
                    bus.repoint_crossfade(point, Arc::clone(store), MEMBER_RAMP_FRAMES)
                {
                    tracing::warn!(%error, source = %id, "audio re-join held: unknown program channel");
                }
            }
            (MemberChange::Join { id, store }, None) => {
                let gain = self.joining_gain(id);
                let point = bus.add_source(id.clone(), Arc::clone(store), gain);
                self.channels
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id.clone(), point);
                // A routing PUT stores its document BEFORE it reads `channels`,
                // so re-reading AFTER the insert closes the window where it
                // missed this channel and `gain` was read from the old document.
                let latest = self.joining_gain(id);
                if (latest - gain).abs() > f64::EPSILON {
                    let _ = bus.set_program_gain_ramped(point, latest, LIVE_GAIN_RAMP_FRAMES);
                }
                tracing::info!(source = %id, gain = latest, "live source joined the running program bus");
            }
            (MemberChange::Leave { id }, Some(point)) => {
                // Cross-fade the strip onto silence (click-free; the outgoing
                // strip retires itself), then reclaim the now-silent channel.
                let silence = Arc::new(AudioStore::new(canonical_format(), 1));
                let _ = bus.repoint_crossfade(point, silence, MEMBER_RAMP_FRAMES);
                bus.remove_source(point);
                self.channels
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id);
                tracing::info!(source = %id, "source left the running program bus");
            }
            (MemberChange::Leave { .. }, None) => {}
        }
    }

    /// Apply before the next audio tick, on the sole bus-owning bake consumer.
    pub fn apply_pending(&self, bus: &mut multiview_audio::program::ProgramBus) -> usize {
        let members: Vec<MemberChange> = {
            let (pending, drained) = &*self.pending_members;
            let mut pending = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let members: Vec<MemberChange> = (0..16)
                .filter_map(|_| pending.changes.pop_first().map(|(_, change)| change))
                .collect();
            if !members.is_empty() {
                drained.notify_all();
            }
            members
        };
        for change in &members {
            let id = match change {
                MemberChange::Join { id, .. } | MemberChange::Leave { id } => id,
            };
            let current = self.sources.load();
            let latest = match current.get(id) {
                Some(store) => MemberChange::Join {
                    id: id.clone(),
                    store: Arc::clone(store),
                },
                None => MemberChange::Leave { id: id.clone() },
            };
            self.apply_member(bus, &latest);
        }
        let pending = self.pending.swap(Arc::new(Vec::new()));
        let mut applied = 0;
        for request in pending.iter() {
            if let Err(error) = bus.repoint_crossfade(
                request.point,
                Arc::clone(&request.store),
                request.ramp_frames,
            ) {
                tracing::warn!(%error, "audio route held: unknown program channel");
            } else {
                applied += 1;
            }
        }
        if let Some(gains) = self.pending_gains.swap(None) {
            let channels = self
                .channels
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, gain) in gains.iter() {
                let Some(&point) = channels.get(id) else {
                    continue;
                };
                if let Err(error) = bus.set_program_gain_ramped(point, *gain, LIVE_GAIN_RAMP_FRAMES)
                {
                    tracing::warn!(%error, channel = %id, "audio gain held: unknown program channel");
                } else {
                    applied += 1;
                }
            }
        }
        applied
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.load().len()
    }
}

/// The control plane's audio-routing live-apply capability for a run whose
/// program bus publishes its [`AudioControlHandle`] into `slot` at drive start.
///
/// The hook loads the handle (empty ⇒ this run carries no program audio ⇒
/// honest `restart`) and hands it the new gain table, which the bake consumer
/// ramps in at its next tick via [`AudioControlHandle::apply_pending`].
/// Wait-free: a `SwapCell` load + store, never the bus (inv #1/#10).
#[must_use]
pub fn routing_live_capability(
    slot: Arc<multiview_framestore::SwapCell<Option<Arc<AudioControlHandle>>>>,
) -> multiview_control::AudioRoutingLiveCapability {
    multiview_control::AudioRoutingLiveCapability::new(move |routing| {
        slot.load()
            .is_some_and(|handle| handle.request_routing(routing))
    })
}

#[cfg(test)]
mod route_seam_tests {
    use super::*;
    use multiview_audio::program::ProgramBus;

    #[test]
    fn changed_discrete_track_source_does_not_claim_live_apply() {
        let routing = |source: &str| {
            let doc = format!("schema_version = 1\n[canvas]\nwidth = 640\nheight = 360\nfps = \"25/1\"\npixel_format = \"nv12\"\nbackground = \"#000000\"\n[canvas.color]\nprofile = \"sdr-bt709-limited\"\n[layout]\nkind = \"grid\"\ncolumns = [\"1fr\"]\nrows = [\"1fr\"]\nareas = [\"a\"]\n[[sources]]\nid = \"{source}\"\nkind = \"bars\"\n[[cells]]\nid = \"cell\"\narea = \"a\"\n[cells.source]\ninput_id = \"{source}\"\n[audio]\nsample_rate_hz = 48000\n[[audio.routes]]\ninput_id = \"{source}\"\nchannels = {{ kind = \"stereo\" }}\ntarget_track = \"clean\"\n");
            multiview_config::MultiviewConfig::load_from_toml(&doc)
                .expect("route fixture")
                .audio
                .expect("audio routing")
        };
        let live = Arc::new(multiview_framestore::SwapCell::from_pointee(vec![
            "old".to_owned()
        ]));
        let mut sources = std::collections::HashMap::new();
        sources.insert("old".to_owned(), new_store());
        let handle = AudioControlHandle::from_sources(sources, std::collections::HashMap::new())
            .with_startup(Some(&routing("old")), ["old".to_owned(), "new".to_owned()])
            .with_declared_tracks(Some(&routing("old")), Arc::clone(&live));
        // A known source without a decoded store is still a valid silent track.
        assert!(handle.request_routing(&routing("new")));
        assert_eq!(live.load().as_slice(), &["new".to_owned()]);
        assert!(!handle.request_routing(&routing("missing")));
        assert_eq!(live.load().as_slice(), &["new".to_owned()]);
        assert!(handle.remove_source("new"));
        assert!(!handle.request_routing(&routing("new")));
        assert_eq!(live.load().as_slice(), &["new".to_owned()]);
        assert!(handle.set_source_audio("new", Some(new_store())));
        assert!(handle.request_routing(&routing("new")));
    }

    #[test]
    fn route_audio_reaches_crossfade_at_sample_boundary_without_losing_ticks() {
        let old = new_store();
        let incoming = new_store();
        let mut bus = ProgramBus::new(canonical_format(), Rational::new(30_000, 1001));
        let point = bus.add_source("program", Arc::clone(&old), 1.0);
        let handle = AudioControlHandle::new();
        let before = bus.tick_to(10);
        handle.request_repoint(point, Arc::clone(&incoming), 480);
        assert_eq!(handle.apply_pending(&mut bus), 1);
        let after = bus.tick_to(13);
        assert_eq!(
            before.frame_count() + after.frame_count(),
            usize::try_from(
                multiview_audio::cadence::SampleClock::new(48_000, Rational::new(30_000, 1001))
                    .total_at(13)
            )
            .expect("small test sample budget")
        );
        assert_eq!(handle.apply_pending(&mut bus), 0);
    }

    /// Review H3: a source joining the live bus between a routing PUT's channel
    /// snapshot and its gain publication must not keep the superseded gain.
    /// The join is driven deterministically from inside that window.
    #[test]
    fn a_join_racing_a_routing_exclusion_is_not_left_audible() {
        let routing = |late_included: bool| {
            let doc = format!("schema_version = 1\n[canvas]\nwidth = 640\nheight = 360\nfps = \"25/1\"\npixel_format = \"nv12\"\nbackground = \"#000000\"\n[canvas.color]\nprofile = \"sdr-bt709-limited\"\n[layout]\nkind = \"grid\"\ncolumns = [\"1fr\"]\nrows = [\"1fr\"]\nareas = [\"a\"]\n[[sources]]\nid = \"main\"\nkind = \"bars\"\n[[sources]]\nid = \"late\"\nkind = \"bars\"\n[[cells]]\nid = \"cell\"\narea = \"a\"\n[cells.source]\ninput_id = \"main\"\n[audio]\nsample_rate_hz = 48000\n[[audio.routes]]\ninput_id = \"main\"\nchannels = {{ kind = \"stereo\" }}\n[[audio.routes]]\ninput_id = \"late\"\nchannels = {{ kind = \"stereo\" }}\ninclude_in_program_bus = {late_included}\n");
            multiview_config::MultiviewConfig::load_from_toml(&doc)
                .expect("route fixture")
                .audio
                .expect("audio routing")
        };
        let handle = AudioControlHandle::new()
            .with_startup(Some(&routing(true)), ["main".to_owned(), "late".to_owned()]);
        let bus = Arc::new(std::sync::Mutex::new(ProgramBus::new(
            canonical_format(),
            Rational::new(25, 1),
        )));
        assert!(handle.set_source_audio("main", Some(new_store())));
        handle.apply_pending(&mut bus.lock().unwrap());
        // `late` is admitted now; the bake consumer joins it INSIDE the window.
        assert!(handle.set_source_audio("late", Some(new_store())));
        let (racer, racing_bus) = (handle.clone(), Arc::clone(&bus));
        handle.set_routing_gap_hook(move || {
            racer.apply_pending(&mut racing_bus.lock().unwrap());
        });
        assert!(handle.request_routing(&routing(false)));
        let mut bus = bus.lock().unwrap();
        handle.apply_pending(&mut bus);
        let point = handle.test_source_point("late").expect("late joined");
        assert_eq!(
            bus.program_gain(point),
            Some(0.0),
            "the excluded joining source stayed audible at the superseded gain"
        );
    }

    #[test]
    fn membership_overflow_reconciles_latest_join_and_leave() {
        let handle = AudioControlHandle::new();
        let mut bus = ProgramBus::new(canonical_format(), Rational::new(25, 1));
        let stale = new_store();
        assert!(handle.set_source_audio("stale", Some(Arc::clone(&stale))));
        handle.apply_pending(&mut bus);
        assert!(handle.test_source_point("stale").is_some());
        assert!(handle.remove_source("stale"));
        let first = new_store();
        assert!(handle.set_source_audio("first", Some(first)));
        for n in 0..300 {
            let id = format!("item-{n:03}");
            assert!(handle.set_source_audio(&id, Some(new_store())));
            if n % 3 == 0 {
                assert!(handle.remove_source(&id));
            }
        }
        assert!(handle.set_source_audio("first", None));
        let final_store = new_store();
        assert!(handle.set_source_audio("final", Some(Arc::clone(&final_store))));
        assert!(handle.pending_members.0.lock().unwrap().changes.len() <= 303);
        for _ in 0..40 {
            handle.apply_pending(&mut bus);
        }
        assert!(
            handle.test_source_point("stale").is_none(),
            "evicted Leave is recovered"
        );
        assert!(handle.test_source_point("first").is_none());
        assert!(handle.test_source_point("final").is_some());
        for n in 0..300 {
            let id = format!("item-{n:03}");
            assert_eq!(handle.test_source_point(&id).is_some(), n % 3 != 0, "{id}");
        }
        assert!(handle.pending_members.0.lock().unwrap().changes.is_empty());
    }

    #[test]
    fn large_backlog_consumes_at_most_sixteen_intents_per_tick_and_latest_churn() {
        let handle = AudioControlHandle::new();
        let mut bus = ProgramBus::new(canonical_format(), Rational::new(25, 1));
        for n in 0..700 {
            assert!(handle.set_source_audio(&format!("source-{n:04}"), Some(new_store())));
        }
        for _ in 0..500 {
            assert!(handle.set_source_audio("source-0001", Some(new_store())));
            assert!(handle.remove_source("source-0002"));
        }
        let latest = new_store();
        assert!(handle.set_source_audio("source-0001", Some(Arc::clone(&latest))));
        let before = handle.pending_members.0.lock().unwrap().changes.len();
        assert_eq!(before, 700, "churn has exactly one latest intent per id");
        handle.apply_pending(&mut bus);
        assert_eq!(
            handle.pending_members.0.lock().unwrap().changes.len(),
            before - 16
        );
        assert!(handle.test_source_point("source-0001").is_some());
        assert!(handle.test_source_point("source-0002").is_none());
        for _ in 0..44 {
            handle.apply_pending(&mut bus);
        }
        assert!(handle.pending_members.0.lock().unwrap().changes.is_empty());
        for n in 0..700 {
            assert_eq!(
                handle
                    .test_source_point(&format!("source-{n:04}"))
                    .is_some(),
                n != 2
            );
        }
        assert!(Arc::ptr_eq(
            &handle.test_source_store("source-0001").unwrap(),
            &latest
        ));
    }

    #[test]
    fn distinct_terminal_intent_at_capacity_waits_for_offclock_consumer() {
        let handle = AudioControlHandle::new();
        // Distinct IDs can churn faster than the bus. The source ceiling must
        // hold even if a prior 1024 IDs were all removed before reconciliation.
        for n in 0..multiview_config::MAX_ADMITTED_SOURCES {
            assert!(handle.push_member(MemberChange::Leave {
                id: format!("removed-{n:04}"),
            }));
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = handle.clone();
        let join = std::thread::spawn(move || {
            assert!(producer.push_member(MemberChange::Leave {
                id: "final-terminal".to_owned(),
            }));
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(
            handle.pending_members.0.lock().unwrap().changes.len(),
            multiview_config::MAX_ADMITTED_SOURCES
        );
        let mut bus = ProgramBus::new(canonical_format(), Rational::new(25, 1));
        handle.apply_pending(&mut bus);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        join.join().unwrap();
        assert!(handle
            .pending_members
            .0
            .lock()
            .unwrap()
            .changes
            .contains_key("final-terminal"));
    }

    #[test]
    fn full_mailbox_wakes_blocked_producer_when_consumer_exits() {
        let handle = AudioControlHandle::new();
        let guard = handle.consumer_guard();
        for n in 0..multiview_config::MAX_ADMITTED_SOURCES {
            assert!(handle.push_member(MemberChange::Leave {
                id: format!("removed-{n:04}"),
            }));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let producer = handle.clone();
        let join = std::thread::spawn(move || {
            tx.send(producer.remove_source("terminal-after-close"))
                .unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard); // Error, normal exit, and unwind all drop the consumer guard.
        assert!(!rx.recv_timeout(Duration::from_secs(2)).unwrap());
        join.join().unwrap();
        assert_eq!(
            handle.pending_members.0.lock().unwrap().changes.len(),
            multiview_config::MAX_ADMITTED_SOURCES,
            "do not evict a terminal intent to claim false admission"
        );
        assert!(!handle.set_source_audio("late", Some(new_store())));
    }

    #[test]
    fn hub_shutdown_finishes_after_full_mailbox_consumer_exit() {
        struct RemoveOnHub(AudioControlHandle);
        impl crate::live_sources::IngestSpawner for RemoveOnHub {
            fn spawn(
                &self,
                _: crate::live_sources::SourceSpawn,
                _: &crate::live_sources::StopRegistry,
            ) -> Option<crate::live_sources::SpawnedProducer> {
                None
            }
            fn source_removed(&self, id: &str) {
                assert!(!self.0.remove_source(id));
            }
        }
        let handle = AudioControlHandle::new();
        let guard = handle.consumer_guard();
        for n in 0..multiview_config::MAX_ADMITTED_SOURCES {
            assert!(handle.push_member(MemberChange::Leave {
                id: format!("removed-{n:04}"),
            }));
        }
        let registry = crate::live_sources::stop_registry();
        let preview = crate::live_sources::shared_stores(std::collections::HashMap::new());
        let hub = crate::live_sources::LiveSourceHub::start_with_ingest(
            registry,
            preview,
            Some(Arc::new(RemoveOnHub(handle.clone()))),
        );
        assert_eq!(
            hub.handle().request_teardown("hub-terminal"),
            crate::live_sources::HubSubmit::Accepted
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            hub.shutdown();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        join.join().unwrap();
        assert!(!handle
            .pending_members
            .0
            .lock()
            .unwrap()
            .changes
            .contains_key("hub-terminal"));
    }

    #[test]
    fn audio_control_backlog_is_bounded_when_consumer_is_wedged() {
        let handle = AudioControlHandle::new();
        let store = new_store();
        let mut bus = ProgramBus::new(canonical_format(), Rational::new(25, 1));
        let point = bus.add_source("source", Arc::clone(&store), 1.0);
        for _ in 0..1024 {
            handle.request_repoint(point, Arc::clone(&store), 480);
        }
        assert!(handle.pending_len() <= MAX_AUDIO_REPOINT_BACKLOG);
        // Simulate a permanently wedged consumer: publishing 1024 commands
        // above did not await it, and the independent output tick clock still
        // advances its exact rational sample budget without applying commands.
        let mut emitted = 0;
        for tick in 1..=120 {
            emitted += bus.tick_to(tick).frame_count();
        }
        assert_eq!(
            u64::try_from(emitted).expect("small test sample budget"),
            multiview_audio::cadence::SampleClock::new(48_000, Rational::new(25, 1)).total_at(120)
        );
    }
}

/// Everything one source needs to decode its audio on its own thread: where the
/// audio lives (a libav-openable path or URL) and whether it is a live
/// (never-ending) source.
///
/// Built only for sources whose media libav can open for audio — a local file or
/// a network URL (rtsp/hls/ts/srt/rtmp). Synthetic sources carry no audio; NDI
/// audio is a separate host-memory path (not wired here). A source with no audio
/// stream simply has its decode loop end at open time and rides silence.
pub(crate) struct AudioIngestPlan {
    /// The source id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The libav-openable audio location (a file path or a network URL string).
    pub(crate) location: String,
    /// Whether this is a live (continuous, never-EOF) source: a live source's
    /// audio is reopened on EOF/error (a transient HLS/RTSP audio drop
    /// reconnects); a finite file plays its audio once and then rides silence.
    pub(crate) live: bool,
    /// Exact source lifecycle shared with the video producer and fetcher.
    pub(crate) lifecycle: u64,
    /// Startup audio is not gated by a live-edit replacement marker.
    pub(crate) anchor_floor: u64,
    /// No video rail runs: never wait for a video fetcher or anchor.
    pub(crate) audio_only: bool,
    /// Operator HLS `user_agent`/`referrer`, sent on every request this rail
    /// makes (master fetch for the audio pin and libav's own opens). Empty for
    /// every other source kind.
    pub(crate) http_headers: multiview_ffmpeg::HttpHeaders,
}

/// How long [`await_shared_url`] waits for the video's fetcher.
#[derive(Debug, Clone, Copy)]
struct SharedUrlWait {
    /// Video has not anchored yet: it may still be fetching its first segment,
    /// so outlast the fetcher's own first-segment deadline (40 s).
    first_connect: Duration,
    /// Video is already running without a fetcher entry (or is reconnecting).
    reconnect: Duration,
    poll: Duration,
}

impl SharedUrlWait {
    const DEFAULT: Self = Self {
        first_connect: Duration::from_secs(45),
        reconnect: Duration::from_secs(10),
        poll: Duration::from_millis(100),
    };
}

/// Wait for the video's fetched copy of this source, so audio does not race
/// the fetcher to the origin: the URL is registered only after the first
/// segment is in memory, which takes seconds on a slow origin. Returns `None`
/// (open the origin) once video has anchored without a fetcher (it fell back
/// to libav), on `stop`, or at the deadline. Runs on the audio ingest thread,
/// never the output clock.
fn await_shared_url(
    lookup: impl Fn() -> Option<String>,
    video_generation: impl Fn() -> Option<u64>,
    stop: &AtomicBool,
    wait: SharedUrlWait,
) -> Option<String> {
    let baseline = video_generation();
    let limit = if baseline.is_none() {
        wait.first_connect
    } else {
        wait.reconnect
    };
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        if let Some(url) = lookup() {
            return Some(url);
        }
        if video_generation() != baseline || started.elapsed() >= limit {
            return lookup();
        }
        std::thread::sleep(wait.poll);
    }
}

/// The audio rail's private SRT reader, when `plan.location` is an SRT
/// listener: two libav contexts (video + audio) cannot both own the native
/// listener port, so the audio rail acquires the SAME shared
/// [`crate::srt_ingest::SrtIngestBridge`] the video rail does and opens its
/// own in-process reader (`pipe:<fd>`, no listening socket). Opened per
/// connection, so a reconnect gets a fresh stream. Only with `srt-libsrt`
/// (ADR-0039 decision 10, amended); without it no bridge exists and the rail
/// opens `plan.location` unchanged (which the video rail's own ingest-plan
/// build already failed closed on).
#[cfg(feature = "srt-libsrt")]
fn audio_srt_reader(plan: &AudioIngestPlan) -> Option<crate::srt_ingest::SrtReader> {
    if !multiview_output::sink::is_srt_listener_url(&plan.location) {
        return None;
    }
    crate::srt_ingest::acquire(&plan.location)
        .and_then(|bridge| bridge.open_reader())
        .map_err(|reason| {
            tracing::warn!(source = %plan.id, %reason, "srt audio listener unavailable");
        })
        .ok()
}

/// Drive a source's audio decode under the supervised-reconnect bracket: open +
/// decode the source's best audio stream into `store` (resampling to the
/// canonical 48 kHz stereo format), reconnecting a *live* source on EOF/error
/// with capped-exponential, jittered backoff until `stop` is raised.
///
/// This is the audio twin of the video
/// [`ingest_loop`](crate::pipeline): the inner per-connection decode is the pure
/// [`audio_decode_loop`](multiview_audio::store::audio_decode_loop) (which opens
/// the `!Send` libav decoder on this worker thread, publishes blocks, and returns
/// on stop/EOF/error); this function wraps it in the reconnect supervision a live
/// transport needs. It only ever WRITES the lock-free store, so it can neither
/// pace nor stall the output clock (invariant #1) nor back-pressure the engine
/// (invariant #10); a wedged libav network call is bounded by the supervisor's
/// detach-on-grace teardown, exactly as for video.
#[allow(clippy::too_many_lines)]
// reason: one supervised reconnect bracket; the audio-only branch added here
// keeps the decode choice next to the loop that owns it.
pub(crate) fn audio_ingest_loop(plan: &AudioIngestPlan, store: &AudioStore, stop: &AtomicBool) {
    // Name this source on every line the audio thread emits: our own (the span)
    // and libav's synchronous lines (the thread-local resource context the log
    // bridge resolves), exactly as the video `ingest_loop` does (ADR-0060). The
    // `rail = "audio"` field keeps the audio connection distinguishable.
    let _span = tracing::info_span!(
        "source",
        resource_kind = "source",
        resource_id = %plan.id,
        rail = "audio",
    )
    .entered();
    let _resource =
        multiview_ffmpeg::ResourceGuard::enter(multiview_ffmpeg::ResourceContext::source(&plan.id));
    let mut attempt: u32 = 0;
    let mut jitter = JitterRng::seeded(&plan.id);
    loop {
        if stop.load(Ordering::Acquire)
            || (plan.lifecycle != 0 && crate::pipeline::live_lifecycle(&plan.id) != plan.lifecycle)
        {
            return;
        }
        let started = Instant::now();
        // An HLS master is opened as a pinned variant media playlist, exactly as
        // the video ingest does: libav opening a master also opens its WebVTT /
        // alternate renditions, and one bad `.vtt` makes the whole open fail
        // ("Invalid data found"), leaving the source silent. Re-resolved on every
        // connection (variant URIs can be session-bound). Best-effort: `None`
        // keeps the original URL.
        let is_hls = plan
            .location
            .split(['?', '#'])
            .next()
            .is_some_and(|b| b.to_ascii_lowercase().ends_with(".m3u8"));
        // Prefer the video's fetched copy (one fetch per source: audio and video
        // read the same segments). Fall back to opening the origin when video is
        // not on the fetcher.
        let shared = if plan.live
            && is_hls
            && !plan.audio_only
            && std::env::var_os(crate::hls_fetch::DISABLE_ENV).is_none()
        {
            // Review R3-02: a non-allocating, lifecycle-checked lookup. A
            // worker whose lifecycle has already been retired (a race with
            // `remove_live_lifecycle` between the loop-top check above and
            // this lookup) must never recreate the slot that removal just
            // dropped — `None` here means "already gone", not "not yet
            // created", so it is read as no video anchor, never allocated.
            let slot = crate::pipeline::live_anchor_slot_for_lifecycle(&plan.id, plan.lifecycle);
            await_shared_url(
                || crate::hls_fetch::shared_url(&plan.id, plan.lifecycle),
                || {
                    slot.as_ref()
                        .and_then(|slot| slot.load())
                        .filter(|a| a.raw_pts0_ns.is_some() && a.generation > plan.anchor_floor)
                        .map(|a| a.generation)
                },
                stop,
                SharedUrlWait::DEFAULT,
            )
        } else {
            None
        };
        let shared_fetch = shared.is_some();
        let pinned = if shared.is_some() {
            shared
        } else if plan.live && is_hls {
            crate::pipeline::resolve_hls_audio_url_with_headers(&plan.location, &plan.http_headers)
        } else {
            None
        };
        // The audio rail shares the video rail's SRT listener through its own
        // private reader (see `audio_srt_reader`), held for this connection;
        // without `srt-libsrt` no bridge exists at all, so this rail degrades
        // the same way the video rail already did.
        #[cfg(feature = "srt-libsrt")]
        let srt_reader = if pinned.is_none() {
            audio_srt_reader(plan)
        } else {
            None
        };
        #[cfg(feature = "srt-libsrt")]
        let pinned = pinned.or_else(|| srt_reader.as_ref().map(|r| r.url().to_owned()));
        // The shared copy is served from loopback memory: no origin request, so
        // no origin headers. Otherwise libav opens the origin and carries them.
        let open_headers = if shared_fetch {
            multiview_ffmpeg::HttpHeaders::default()
        } else {
            plan.http_headers.clone()
        };
        tracing::info!(
            source = %plan.id,
            location = %pinned.as_deref().unwrap_or(&plan.location),
            shared_fetch,
            request_headers_set = !open_headers.is_empty(),
            "live audio: opening"
        );
        let location = std::path::Path::new(pinned.as_deref().unwrap_or(&plan.location));
        // One connection's worth of decode. `audio_decode_loop` opens the decoder
        // on THIS thread (libav contexts are not `Send`), publishes every decoded
        // block, and returns on stop / EOF / open-or-decode error — logging the
        // fault itself. A no-audio source returns immediately (open finds no audio
        // stream); its tile then simply rides silence.
        if plan.live && store.is_timeline() {
            // Locked to vision: place by the video's PTS → output mapping.
            // Review R3-02: the same non-allocating, lifecycle-checked lookup
            // as above. `None` here means THIS worker's lifecycle has already
            // been retired (raced with `remove_live_lifecycle` between the
            // loop-top check and here) — skip the decode connection entirely
            // rather than recreate the dropped slot; the loop-top check on the
            // next iteration returns.
            if let Some(slot) =
                crate::pipeline::live_anchor_slot_for_lifecycle(&plan.id, plan.lifecycle)
            {
                multiview_audio::store::audio_decode_loop_anchored(
                    location,
                    ChannelLayout::Stereo,
                    store,
                    stop,
                    &|| {
                        if plan.lifecycle == 0
                            || crate::pipeline::live_lifecycle(&plan.id) == plan.lifecycle
                        {
                            video_anchor(&slot, plan.anchor_floor)
                        } else {
                            None
                        }
                    },
                    Some(Arc::clone(&crate::av_sync::probe(&plan.id).audio)),
                    &plan.id,
                    &open_headers,
                );
            }
        } else if plan.live {
            multiview_audio::store::audio_decode_loop_live(
                location,
                ChannelLayout::Stereo,
                store,
                stop,
            );
        } else {
            multiview_audio::store::audio_decode_loop_with_headers(
                location,
                ChannelLayout::Stereo,
                store,
                stop,
                &open_headers,
            );
        }
        let ran_for = started.elapsed();
        if !plan.live || stop.load(Ordering::Acquire) {
            // A finite source has played its audio out (the store silence-fills
            // past EOF forever); a stop was requested. Either way, this ends.
            return;
        }
        // Live source: escalate from THIS connection's health, then wait the
        // resulting backoff (checking `stop` in slices so teardown stays prompt)
        // and reconnect — the same policy the video ingest uses.
        attempt = next_reconnect_attempt(attempt, ran_for);
        let nap = reconnect_backoff(attempt, jitter.next_unit());
        tracing::info!(
            source = %plan.id,
            attempt,
            ?ran_for,
            ?nap,
            "reconnecting live source audio after backoff"
        );
        sleep_interruptible(nap, stop);
    }
}

/// Everything a synthetic source needs to publish its **line-up tone** on its own
/// thread (AUD-5): the source id (the program-bus route key + diagnostics) and
/// the output cadence the tone is paced to.
///
/// Built only for the `bars` synthetic source — the SMPTE/EBU colour-bars card's
/// companion is a 1 kHz reference tone; `solid` and `clock` synthetic sources
/// carry no audio and ride silence on the bus.
pub(crate) struct ToneIngestPlan {
    /// The source id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The output cadence the tone is paced to (so the store stays exactly one
    /// per-tick budget ahead of the program bus's pull, gap-free).
    pub(crate) cadence: Rational,
}

/// Target buffered lead (frames) the tone keeps ahead of the bus's read cursor:
/// half a second at the canonical 48 kHz. Comfortably inside the store's
/// [`STORE_CAPACITY_FRAMES`] (2 s) so a top-up never evicts un-read tone, and
/// large enough that a scheduling hiccup on this thread never lets the bus's
/// per-tick pull outrun the published tone (which would read silence — a gap).
const TONE_LEAD_FRAMES: i64 = 24_000;

/// Refill the store whenever the lead drops below this (half the target): a
/// hysteresis band so the thread does a few bounded `sin` bursts rather than a
/// tight spin, then sleeps.
const TONE_REFILL_THRESHOLD_FRAMES: i64 = TONE_LEAD_FRAMES / 2;

/// Publish the synthetic **1 kHz line-up tone** into `store` until `stop` is
/// raised — the audio peer of [`crate::synth::generator_loop`] for the `bars`
/// source (AUD-5).
///
/// The tone is generated by [`multiview_audio::ToneGenerator`] (a pure,
/// phase-continuous sine carrying integer phase, so it never drifts — invariant
/// #3/#6). The loop keeps the store filled to a fixed [`TONE_LEAD_FRAMES`] lead
/// **ahead of the bus's own read cursor** ([`AudioStore::read_cursor`]): it tops
/// the buffer up in one phase-continuous burst whenever the lead falls below the
/// refill threshold, then sleeps. Pacing against the reader's cursor (not
/// wall-clock) makes the tone gap-free regardless of how the bake consumer is
/// paced — the bus's per-tick pull always lands inside published tone, never on
/// an un-written (silence) span.
///
/// If the reader cursor *jumps past* the write head (a `DropOnOverload` catch-up,
/// where the bus's tick-driven [`SampleClock`](multiview_audio::cadence::SampleClock)
/// skips a large span) the loop **seeks the generator forward** to the cursor and
/// resumes from there — it never tries to back-fill the skipped (already-evicted,
/// never-read) span, so every burst stays bounded by [`TONE_LEAD_FRAMES`]
/// (bounded copy cost, no spin). The lead is bounded well inside the store
/// capacity, so the ring never evicts tone the reader has not consumed and never
/// grows (the data-plane "queues drop, never grow" rule).
///
/// It only ever **writes** the lock-free store, so it can neither pace nor stall
/// the output clock (invariant #1) nor back-pressure the engine (invariant #10);
/// a refill is a cheap, bounded per-frame `sin` burst off the hot path. `stop` is
/// polled each wakeup so teardown is prompt.
pub(crate) fn tone_publish_loop(plan: &ToneIngestPlan, store: &AudioStore, stop: &AtomicBool) {
    let format = store.format();
    let mut generator = ToneGenerator::line_up(format);
    // Absolute count of frames published so far (the writer's write head; the
    // store's `read_cursor` lives in the same absolute coordinate space).
    let mut published: i64 = 0;
    let poll = refill_poll_interval(plan.cadence);
    while !stop.load(Ordering::Acquire) {
        let read_cursor = store.read_cursor();
        // If the reader has overrun the write head (a large tick-driven catch-up),
        // skip the generator forward to the cursor rather than back-filling the
        // never-read span — keeps every burst bounded by the lead.
        if read_cursor > published {
            generator.seek_to_frame(u64::try_from(read_cursor).unwrap_or(0));
            published = read_cursor;
        }
        let lead = published.saturating_sub(read_cursor);
        if lead < TONE_REFILL_THRESHOLD_FRAMES {
            // Top up to the target lead in one phase-continuous burst (bounded by
            // `TONE_LEAD_FRAMES` since the overrun case above already realigned).
            let want = TONE_LEAD_FRAMES.saturating_sub(lead).max(0);
            let frames = usize::try_from(want).unwrap_or(0);
            if frames > 0 {
                match generator.next_block(frames) {
                    Ok(block) => {
                        if let Err(error) = store.publish(&block) {
                            // The generator and store share the canonical format, so
                            // a mismatch is a programming error: log and stop rather
                            // than busy-loop. The source then rides silence.
                            tracing::error!(%error, source = %plan.id, "tone publish rejected; stopping");
                            return;
                        }
                        published = published.saturating_add(want);
                    }
                    Err(error) => {
                        tracing::error!(%error, source = %plan.id, "tone generation failed; stopping");
                        return;
                    }
                }
            }
        }
        sleep_interruptible(poll, stop);
    }
}

/// The refill poll interval: ~10 output ticks (clamped to `[5 ms, 200 ms]`). Short
/// enough that the bus never drains the buffered lead between polls, long enough
/// that the thread is mostly asleep. Derived from `cadence` so it scales with the
/// output rate.
#[must_use]
fn refill_poll_interval(cadence: Rational) -> Duration {
    let num = u64::try_from(cadence.num).unwrap_or(1).max(1);
    let den = u64::try_from(cadence.den).unwrap_or(1).max(1);
    // One tick = den/num seconds; ten ticks in nanos = 10 * den * 1e9 / num.
    let nanos = den.saturating_mul(10_000_000_000) / num;
    Duration::from_nanos(nanos.clamp(5_000_000, 200_000_000))
}

/// Everything a **media-player channel** needs to loop its embedded audio on its
/// own thread (ADR-T019 / ADR-0097 §8): where the asset audio lives, the
/// integer-frame vamp window + cadence that defines the loop body (the same
/// [`PlayoutGeometry`](crate::player::PlayoutGeometry) the video uses, so the
/// audio wraps on the same instant), and the wait-free
/// [`PlayerControlBus`](crate::player::PlayerControlBus) the audio rail **samples
/// and follows** (ADR-T019 §1 — the video rail is the sole mailbox consumer and
/// publishes its authoritative transport state here; the audio never drains the
/// mailbox itself, so the rails can never desync).
pub(crate) struct PlayerAudioPlan {
    /// The player channel id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The libav-openable asset audio location (a file path).
    pub(crate) location: String,
    /// The vamp-segment start (asset source frames).
    pub(crate) vamp_in_frames: u64,
    /// The vamp-segment end, exclusive (asset source frames).
    pub(crate) vamp_out_frames: u64,
    /// The asset cadence — the frame↔sample mapping the loop body is sliced by
    /// (so the loop length is the audio duration of `vamp_len` asset frames, the
    /// same sample count at any output cadence — ADR-T019 §3).
    pub(crate) cadence: Rational,
    /// The output cadence (the bus tick rate) the fill loop paces its top-ups to.
    pub(crate) output_cadence: Rational,
    /// The wait-free control bus (the SAME `Arc` on the video rail's
    /// [`PlayerHandle`](crate::player::PlayerHandle)): the audio loop SAMPLES this
    /// each block and follows the video's published transport state, so
    /// vamp/arm-exit/stop land on the same boundary as the video by construction.
    pub(crate) control_bus: Arc<crate::player::PlayerControlBus>,
}

/// Retain **only** the vamp window `[in_sample, want_end)` from a forward stream of
/// decoded interleaved blocks — discarding each block that lies entirely **before**
/// `in_sample` (the pre-`vamp_in` head) and stopping once `want_end` is reached
/// (ADR-T019 §5, the CRITICAL-3 fix).
///
/// `AudioFileDecoder` has no seek, so a late vamp window can only be reached by
/// decoding forward through the head; but the head must NOT be **buffered**.
/// Accumulating every decoded sample from frame 0 into one buffer (then slicing)
/// transiently holds the whole pre-window prefix (peak ≈ asset size). Instead this
/// discards pre-window blocks and appends only the in-window samples, so the **peak
/// resident buffer is the window size** (`want_end − in_sample`), never the asset
/// size — even for a late window in a long asset. The returned buffer starts at
/// `in_sample` (so the caller builds the deck via `with_segment` directly: body =
/// the first `vamp_len` frames, lap-over = the rest).
///
/// Pure (no libav) so it is unit-tested without the `ffmpeg` CLI; the ffmpeg driver
/// [`decode_vamp_window_48k`] feeds it `AudioFileDecoder::next_block` output.
#[cfg(feature = "ffmpeg")]
#[must_use]
fn retain_vamp_window(
    blocks: impl IntoIterator<Item = Vec<f32>>,
    channels: usize,
    in_sample: usize,
    want_end: usize,
) -> Vec<f32> {
    if channels == 0 || want_end <= in_sample {
        return Vec::new();
    }
    let window_frames = want_end - in_sample;
    // Size the retained buffer to EXACTLY the window — it never grows past it, so the
    // pre-`vamp_in` prefix is never resident (the CRITICAL-3 bound).
    let mut out: Vec<f32> = Vec::with_capacity(window_frames.saturating_mul(channels));
    // The absolute frame index of the NEXT decoded sample (advances by each block's
    // frame count). Blocks entirely `< in_sample` are discarded without retention.
    let mut decoded_frames: usize = 0;
    for block in blocks {
        let block_frames = block.len() / channels.max(1);
        if block_frames == 0 {
            continue;
        }
        let block_start = decoded_frames;
        let block_end = block_start.saturating_add(block_frames);
        decoded_frames = block_end;

        // Entirely before the window: discard (do NOT retain the prefix).
        if block_end <= in_sample {
            continue;
        }
        // Entirely at/after the window end: nothing more to retain — stop.
        if block_start >= want_end {
            break;
        }
        // Overlap of [block_start, block_end) with [in_sample, want_end): retain it.
        let from = in_sample.max(block_start);
        let to = want_end.min(block_end);
        let src_from = from.saturating_sub(block_start).saturating_mul(channels);
        let src_to = to.saturating_sub(block_start).saturating_mul(channels);
        if let Some(slice) = block.get(src_from..src_to) {
            out.extend_from_slice(slice);
        }
        if to >= want_end {
            break;
        }
    }
    out
}

/// Decode the asset audio (resampled to canonical 48 kHz stereo `f32`) and return
/// **only** the vamp window `[in_sample, want_end)` (ADR-T019 §5). Drives
/// [`retain_vamp_window`] over `AudioFileDecoder::next_block`, so the pre-`vamp_in`
/// head is decoded-and-discarded, never buffered (peak resident buffer ≈ the window
/// size). Returns an empty `Vec` on open/decode failure or a no-audio source (the
/// player then rides silence — never an error, never a stall).
#[cfg(feature = "ffmpeg")]
#[must_use]
fn decode_vamp_window_48k(path: &std::path::Path, in_sample: usize, want_end: usize) -> Vec<f32> {
    use multiview_audio::AudioFileDecoder;

    let channels = canonical_format().channel_count();
    // `multiview_audio::AudioFileDecoder` resamples to 48 kHz / this layout / f32.
    let mut decoder = match AudioFileDecoder::open(path, ChannelLayout::Stereo) {
        Ok(d) => d,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "media-player audio: open failed; riding silence");
            return Vec::new();
        }
    };
    // A block iterator over the decoder that logs+stops on a mid-stream decode error
    // (the window retains what decoded so far; `from_clip_window`/the caller then
    // refuses a materially-short window). `next_block` yields `Ok(Some)` per block,
    // `Ok(None)` at EOS, `Err` on a decode fault.
    let blocks = std::iter::from_fn(move || match decoder.next_block() {
        Ok(Some(block)) => Some(block.interleaved().to_vec()),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "media-player audio: decode error; using what decoded so far");
            None
        }
    });
    retain_vamp_window(blocks, channels, in_sample, want_end)
}

/// Drive a media-player channel's **looping embedded audio** (ADR-T019): prime a
/// [`LoopDeck`](multiview_audio::LoopDeck) once from the asset's `[vamp_in,
/// vamp_out + W)` window, then keep `store` filled a bounded lead **ahead of the
/// bus's read cursor** with the looped (crossfaded-at-the-seam) stream. Each
/// top-up first **samples the video rail's authoritative
/// [`PlayerControlBus`](crate::player::PlayerControlBus)** (ADR-T019 §1) and
/// follows it — the audio never drains the transport mailbox itself (the video is
/// the sole consumer), so the rails can never desync; `vamp`/`arm-exit`/`stop`
/// land on the same boundary as the video by construction.
///
/// It only ever **writes** the lock-free `store`, so it can neither pace nor stall
/// the output clock (inv #1) nor back-pressure the engine (inv #10) — exactly like
/// the tone loop it mirrors. The fill path uses a **reusable scratch buffer**
/// (`read_into` + `publish_samples`) — no per-block heap allocation (rule 22). A
/// no-audio / failed / over-cap prime yields an empty deck → the channel rides
/// **silence** (the store silence-fills), never an error.
///
/// Unlike [`audio_ingest_loop`], a player channel's audio is **buffer-and-replay**,
/// not a streamed decode: the loop seam is sample-exact and click-free
/// (ADR-T019), which a per-lap container seek (no audio IDR) could not be.
#[cfg(feature = "ffmpeg")]
pub(crate) fn player_audio_loop(plan: &PlayerAudioPlan, store: &AudioStore, stop: &AtomicBool) {
    use multiview_audio::loopdeck::{
        LoopDeck, DEFAULT_CROSSFADE_FRAMES, MAX_LOOP_SECONDS, SAMPLE_RATE,
    };

    let channels = canonical_format().channel_count();
    // Prime (ADR-T019 §5 — CRITICAL-3): decode ONLY the vamp window
    // `[vamp_in, vamp_out + W)` and retain ONLY that span — the pre-`vamp_in` head is
    // decoded-and-discarded, NEVER buffered (peak resident buffer ≈ the window size,
    // not the asset size). REFUSE an over-cap BODY explicitly (a window longer than
    // `MAX_LOOP_SECONDS` rides silence with a logged reason — never a silent clamp
    // that would shift the loop point). The frame→sample map is exact rationals
    // (inv #3). `with_segment` re-checks raggedness; the materially-short / over-cap
    // refusal yields an empty (silent) deck.
    let body_frames = plan.vamp_out_frames.saturating_sub(plan.vamp_in_frames);
    let body_samples = frames_to_samples(body_frames, plan.cadence);
    let xfade = DEFAULT_CROSSFADE_FRAMES;
    let mut deck = if body_samples == 0
        || body_samples > MAX_LOOP_SECONDS.saturating_mul(SAMPLE_RATE)
    {
        if body_samples > 0 {
            tracing::warn!(
                player = %plan.id,
                body_seconds = body_samples / SAMPLE_RATE,
                cap_seconds = MAX_LOOP_SECONDS,
                "media-player audio: vamp window exceeds the loop cap — riding silence (refused, not clamped)"
            );
        }
        LoopDeck::empty(canonical_format())
    } else {
        // The exact-rational sample bounds of the window `[vamp_in, vamp_out + W)`.
        let in_sample = frames_to_samples(plan.vamp_in_frames, plan.cadence);
        let out_sample = frames_to_samples(plan.vamp_out_frames, plan.cadence);
        let in_idx = usize::try_from(in_sample).unwrap_or(usize::MAX);
        let loop_len = usize::try_from(body_samples).unwrap_or(usize::MAX);
        let want_end = usize::try_from(out_sample)
            .unwrap_or(usize::MAX)
            .saturating_add(xfade);
        // Decode-and-discard the head; retain only `[in_sample, want_end)` (starting
        // at `vamp_in`), so the buffer handed to the deck is the window, not the
        // prefix (CRITICAL-3). `from_window` builds the loop over the pre-sliced
        // window, REFUSING (empty/silent deck) a materially-short window rather than
        // looping a shifted-length body, tolerating only a tiny resampler-edge
        // shortfall (ADR-T019 §5). The transient window buffer is freed after.
        let window = decode_vamp_window_48k(std::path::Path::new(&plan.location), in_idx, want_end);
        let was_short = window.len() / channels < loop_len;
        let deck = LoopDeck::from_window(canonical_format(), &window, loop_len, xfade);
        if was_short && deck.loop_frames() == 0 {
            tracing::warn!(
                player = %plan.id,
                "media-player audio: asset shorter than the declared vamp window — riding silence (refused, not clamped)"
            );
        }
        deck
    };

    // The audio rail FOLLOWS the video rail's published transport state (ADR-T019
    // §1/§2.3) — it never drains the mailbox itself. The video publishes the
    // channel's initial state (vamp / play-once) on its first frame, which this rail
    // samples below; the deck starts vamping by default so a boot-vamping player
    // loops from the first block even before that first sample.
    let mut last_control_gen: u64 = 0;

    // CRITICAL-2 (rule 22): a SINGLE reusable scratch buffer for the read path,
    // sized once to the maximum window and filled IN PLACE every block — no
    // per-block heap allocation. `read_into` reuses this `Vec`'s capacity, and
    // `AudioStore::publish_window` swaps in a pooled snapshot (no alloc) — the one
    // copy is `read_into`'s deck→scratch fill, on this sampled decode thread.
    let max_window = usize::try_from(PLAYER_AUDIO_LEAD_FRAMES.max(0)).unwrap_or(0);
    let mut scratch: Vec<f32> = Vec::with_capacity(max_window.saturating_mul(channels));

    let poll = refill_poll_interval(plan.output_cadence);
    while !stop.load(Ordering::Acquire) {
        // Sample the video rail's authoritative control bus (wait-free) FIRST and, on
        // a real transition, apply it to the deck — so the window we (re)publish this
        // block reflects the CURRENT transport state (ADR-T019 §2.3). vamp/arm-exit/
        // stop therefore land on the same boundary as the video by construction.
        follow_video_control(&plan.control_bus, &mut deck, &mut last_control_gen);

        // The publish-horizon contract (ADR-T019 §2.3): REPLACE the unplayed window
        // `[cursor, H)` re-derived from the deck's CURRENT state every block. Because
        // every stale sample lives at ≥ the exit boundary B ≥ the read cursor (B is
        // a FUTURE wrap, lip-synced to the video anchor), the replace overwrites any
        // pre-transition body before the bus reads it — "no sample past the boundary
        // is ever heard" is then true by construction, not by a short-enough
        // lookahead.
        let cursor = store.read_cursor().max(0);
        let cursor_u = u64::try_from(cursor).unwrap_or(0);
        // The horizon: a bounded lookahead, clamped to the deck's silence-settle
        // frame once an exit has fired (so we publish body→fade→silence, nothing
        // past the settle). The deck itself yields silence past the settle, so this
        // clamp is for efficiency; correctness comes from re-deriving each block.
        let lookahead_end = cursor_u.saturating_add(u64::try_from(max_window).unwrap_or(u64::MAX));
        let horizon = match deck.settle_frame() {
            Some(settle) => settle.min(lookahead_end),
            None => lookahead_end,
        };
        if horizon > cursor_u {
            let frames = usize::try_from(horizon - cursor_u)
                .unwrap_or(0)
                .min(max_window);
            if frames > 0 {
                // Fill the reusable scratch from the deck at the absolute cursor (no
                // alloc), then REPLACE the store window with exactly `[cursor, H)`.
                deck.read_into(cursor_u, frames, &mut scratch);
                if let Err(error) = store.publish_window(cursor, &scratch) {
                    tracing::error!(%error, player = %plan.id, "media-player audio publish rejected; stopping");
                    return;
                }
            }
        }
        // Once the exit has fully settled to silence, the store silence-fills the
        // unplayed tail on its own — nothing more to publish until a fresh transport
        // verb (which `follow_video_control` will pick up next wakeup).
        sleep_interruptible(poll, stop);
    }
}

/// Sample the video rail's authoritative [`PlayerControlBus`] and, on a real
/// transition (a higher generation than last applied), apply the published state
/// to the audio [`LoopDeck`] (ADR-T019 §1). The audio rail FOLLOWS the video — it
/// never independently consumes transport verbs, so the two rails can never
/// desync. An armed-exit publish carries the video's media-time anchor, so the
/// audio arms its exit at the **same** next-vamp boundary the video reaches.
#[cfg(feature = "ffmpeg")]
fn follow_video_control(
    bus: &crate::player::PlayerControlBus,
    deck: &mut multiview_audio::LoopDeck,
    last_gen: &mut u64,
) {
    use crate::player::AudioTransport;
    let ctrl = bus.load();
    if ctrl.generation == *last_gen {
        return;
    }
    *last_gen = ctrl.generation;
    match ctrl.state {
        AudioTransport::Vamping => {
            deck.vamp();
            if let Some(anchor) = ctrl.exit_arm_anchor {
                // Arm at the video's anchor → the same next-vamp boundary.
                deck.arm_exit_at(media_time_to_audio_frame(anchor));
            }
        }
        // Paused holds the loop phase (a later vamp resumes mid-loop); Stopped (the
        // video re-cue) latches a re-phase so a fresh vamp restarts from body[0] at
        // the current frame (MAJOR-4). Both contribute silence until the next vamp.
        AudioTransport::Paused => deck.pause(),
        AudioTransport::Stopped => deck.stop(),
    }
}

/// Convert an output media-time to the audio rail's absolute 48 kHz frame index
/// (exact integer rationals, no float — inv #3): `frame = ns × 48000 / 1e9`.
#[cfg(feature = "ffmpeg")]
fn media_time_to_audio_frame(t: multiview_core::time::MediaTime) -> u64 {
    let ns = t.as_nanos().max(0);
    let frame =
        (i128::from(ns) * i128::from(multiview_audio::loopdeck::SAMPLE_RATE)) / 1_000_000_000;
    u64::try_from(frame).unwrap_or(0)
}

/// The exact 48 kHz sample index of asset `frames` at `cadence` = `num/den` fps:
/// `round(frames × 48000 × den / num)`, in `u128` with no float (inv #3 — the
/// audio loop point lands on an exact sample, never a drifting average). Mirrors
/// the deck's internal `frame_to_sample`; used to bound the prime decode and the
/// cap check. A non-positive cadence yields 0 (the player then rides silence).
#[cfg(feature = "ffmpeg")]
fn frames_to_samples(frames: u64, cadence: Rational) -> u64 {
    let Ok(num) = u64::try_from(cadence.num) else {
        return 0;
    };
    let Ok(den) = u64::try_from(cadence.den) else {
        return 0;
    };
    if num == 0 || den == 0 {
        return 0;
    }
    let rate = multiview_audio::loopdeck::SAMPLE_RATE;
    let numer = u128::from(frames)
        .saturating_mul(u128::from(rate))
        .saturating_mul(u128::from(den));
    let denom = u128::from(num);
    let rounded = numer.saturating_add(denom / 2) / denom;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// The bounded lookahead window (frames) the player audio republishes each block —
/// half a second at 48 kHz. Each wakeup the fill loop REPLACES the store window with
/// `[read_cursor, min(read_cursor + this, settle))` re-derived from the deck's
/// current state (ADR-T019 §2.3), so the unplayed tail is always at most this many
/// frames and is corrected on the very next block after any transport transition —
/// the boundary-tight exit is true by construction, not by a short lookahead. It is
/// comfortably inside the store capacity and ≥ one output tick (so the bus's per-tick
/// pull always lands inside the published window — gap-free).
#[cfg(feature = "ffmpeg")]
const PLAYER_AUDIO_LEAD_FRAMES: i64 = 24_000;

#[cfg(all(test, feature = "ffmpeg"))]
mod player_audio_tests {
    //! Integration tests for the media-player audio loop driver (ADR-T019),
    //! `ffmpeg`-gated: each packages an in-memory tone to a lossless WAV via the
    //! `ffmpeg` CLI (LGPL-clean), exactly as the audio decode-thread tests do.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #![allow(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::indexing_slicing
    )]

    use std::f64::consts::PI;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use multiview_audio::store::AudioStore;
    use multiview_core::time::Rational;

    use super::{canonical_format, player_audio_loop, PlayerAudioPlan};
    use crate::player::{AudioTransport, PlayerControlBus};

    const FS: u32 = 48_000;

    fn ffmpeg_cli_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn tone(amp: f64, freq: f64, seconds: f64) -> Vec<f32> {
        let n = (f64::from(FS) * seconds).round() as usize;
        let mut out = Vec::with_capacity(n * 2);
        let w = 2.0 * PI * freq / f64::from(FS);
        for i in 0..n {
            let s = (amp * (w * i as f64).sin()) as f32;
            out.push(s);
            out.push(s);
        }
        out
    }

    fn encode_to_wav(dir: &Path, samples: &[f32]) -> PathBuf {
        let raw = dir.join("clip.f32");
        let wav = dir.join("clip.wav");
        {
            let mut file = std::fs::File::create(&raw).unwrap();
            let mut bytes = Vec::with_capacity(samples.len() * 4);
            for &s in samples {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            file.write_all(&bytes).unwrap();
        }
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-i",
            ])
            .arg(&raw)
            .args(["-c:a", "pcm_s16le"])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg failed to encode the WAV");
        wav
    }

    /// The driver primes from a 1 s tone clip and LOOPS its `[0, 1 s)` vamp window:
    /// after advancing the bus read cursor PAST one clip length, the store still
    /// yields the tone (a one-shot decode would read silence past EOF). The loop
    /// is sample-locked (period == the vamp window's sample count) and the thread
    /// only writes the store — it never paces the (simulated) clock.
    #[test]
    fn player_audio_loops_the_vamp_window_past_one_clip_length() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping player-audio loop test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 1.0 s of a 1 kHz tone (48_000 frames). At 48 fps, frames 0..48 = [0,1 s).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 1.0));

        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        // The control bus defaults to Vamping, so the audio loops from block 0
        // (the video rail would publish the same initial state). No verbs needed.
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "vt1".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // 48 frames @ 48 fps = 1 s = 48_000 samples
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };

        // Run the driver on its own thread (the data-plane peer of the video
        // player thread).
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });

        // Simulate the bus SAMPLING the store at ~real-time: pull tick-sized blocks
        // (1600 frames ≈ one 30 fps tick), pacing the pulls so the driver (which
        // keeps a 0.5 s lead) stays ahead — exactly how the bake consumer samples a
        // per-source store. Advance well past one clip length (1.5 clips) and keep
        // every block read, so the LAST block is past frame 48_000.
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        let mut advanced = 0usize;
        let target = 72_000usize; // 1.5 × the 48_000-frame clip
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        // Let the driver prime + publish its first lead before the bus starts.
        std::thread::sleep(std::time::Duration::from_millis(250));
        while advanced < target && std::time::Instant::now() < deadline {
            let block = store.read(1600);
            blocks.push(block.interleaved().to_vec());
            advanced += 1600;
            // ~33 ms per 1600-frame tick: pace near real-time so the driver refills.
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        assert!(
            advanced >= target,
            "did not advance past one clip length in time (advanced {advanced})"
        );
        // A block well PAST one full clip length (frame ~60_000, deep in lap 2) must
        // carry real tone energy: a one-shot decode would silence-fill there; the
        // LOOP keeps the tone alive (sample-locked, period == the vamp window).
        let late_idx = 60_000 / 1600; // the block covering frame ~60_000
        let late = blocks.get(late_idx).cloned().unwrap_or_default();
        let energy: f64 = late.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy > 1.0,
            "past one clip length the store is silent — the audio did not LOOP (energy {energy:.3})"
        );
    }

    /// Defect 3: a vamp window that does NOT start at the asset head loops
    /// correctly. The window is `[vamp_in=24, vamp_out=48]` (the SECOND half of a
    /// 1 s clip) — the prime must decode through `vamp_out` (not cap from start at
    /// frame 0) and slice the `[0.5 s, 1.0 s)` body. Proves the windowed prime
    /// reaches an offset window (the bug silenced anything past the from-start cap).
    #[test]
    fn a_vamp_window_not_at_the_asset_head_loops_correctly() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping offset-window test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 1.0 s tone (48_000 frames). Vamp the SECOND half: frames 24..48 @ 48 fps
        // = [0.5 s, 1.0 s) = samples 24_000..48_000 (a 24_000-frame loop).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 1.0));
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "offset".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 24,  // 0.5 s
            vamp_out_frames: 48, // 1.0 s → a 0.5 s = 24_000-sample loop body
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Advance past TWO loop bodies (2 × 24_000 = 48_000) — past where a
        // one-shot decode of the offset window would silence-fill.
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        let mut advanced = 0usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        std::thread::sleep(std::time::Duration::from_millis(250));
        while advanced < 48_000 && std::time::Instant::now() < deadline {
            blocks.push(store.read(1600).interleaved().to_vec());
            advanced += 1600;
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
        // A block past one loop body (frame ~36_000, deep in lap 2 of the offset
        // window) carries the tone — the offset window looped (not silence).
        let late = blocks.get(36_000 / 1600).cloned().unwrap_or_default();
        let energy: f64 = late.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy > 1.0,
            "an offset vamp window [0.5 s, 1.0 s) did not loop (energy {energy:.3}) — the prime did not decode through vamp_out"
        );
    }

    /// A player with NO audio stream (a silent asset) rides silence: the driver
    /// primes an empty deck and the store reads silence forever — never a stall,
    /// never a panic. (Here: a non-existent path → open fails → empty deck.)
    #[test]
    fn a_missing_or_silent_asset_rides_silence() {
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "silent".to_owned(),
            location: "/nonexistent/no-such-asset.wav".to_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48,
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Let it run briefly, sampling the store — every read is full silence.
        for _ in 0..5 {
            let block = store.read(1600);
            assert_eq!(
                block.frame_count(),
                1600,
                "silent player still returns full blocks"
            );
            assert!(
                block.interleaved().iter().all(|&s| s == 0.0),
                "a missing/silent asset must ride silence"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
    }

    /// CRITICAL-1 (the root defect), with a REAL lookahead: when an `ArmExit`
    /// arrives AFTER the audio rail has already prepublished ~0.5 s of looped body
    /// into the store, NO published/heard sample past the armed vamp-exit boundary
    /// `B` may carry body energy. The round-2 append-only horizon let the
    /// prepublished body play past `B`; the publish-horizon contract (ADR-T019
    /// §2.3) re-derives + REPLACES the unplayed window from the deck's current
    /// state every block, so the body past `B` is overwritten with fade→silence
    /// before the bus reads it. This fails against the append-only `18039876`.
    #[test]
    fn arm_exit_during_a_real_lookahead_plays_no_body_past_the_boundary() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping arm-exit boundary test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 2 s of a steady 1 kHz tone. Vamp [0, 1 s) → loop L = 48_000 frames. A
        // boundary at B = 48_000 (the lap-0→1 wrap). The lookahead is 0.5 s =
        // 24_000 frames, so with the read cursor parked at 30_000 the lookahead
        // window [30_000, 54_000) straddles B and (pre-fix) holds BODY at [B, 54_000).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 2.0));
        let store = Arc::new(AudioStore::new(canonical_format(), 192_000));
        let stop = Arc::new(AtomicBool::new(false));
        // Boot vamping (no exit armed): the deck loops and the rail prepublishes body.
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "armexit".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // 48 frames @ 48 fps = 1 s = 48_000 samples → L
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });

        // Let it prime + publish the initial lookahead.
        std::thread::sleep(std::time::Duration::from_millis(400));
        // Park the read cursor at 30_000 (inside lap 0, past B − LOOKAHEAD = 24_000)
        // so the next republish covers [30_000, 54_000) — straddling B = 48_000.
        store.seek_to(30_000);
        // Give the fill loop a few poll intervals to publish that BODY lookahead
        // (the stale region [48_000, 54_000) is now body, pre-fix).
        std::thread::sleep(std::time::Duration::from_millis(500));

        // NOW arm the exit. The anchor is the video's media-time at ~frame 30_000;
        // the next vamp wrap strictly after it is B = 48_000. (media-time 30_000/48k
        // s → frame 30_000; next wrap after 30_000 with L=48_000 is 48_000.)
        let anchor_ns = (30_000i64 * 1_000_000_000) / 48_000;
        control_bus.publish(
            AudioTransport::Vamping,
            Some(multiview_core::time::MediaTime::from_nanos(anchor_ns)),
        );
        // Give the fill loop ample time to sample the arm, arm the deck, and
        // REPUBLISH [30_000, 48_000 + xfade) = body→fade→silence — overwriting the
        // stale body — all while the read cursor stays parked (so the republish is
        // guaranteed to land before we read past B).
        std::thread::sleep(std::time::Duration::from_millis(600));
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        // Read the whole post-arm span from the parked cursor and inspect past B.
        store.seek_to(30_000);
        let span = store.read(40_000); // [30_000, 70_000): well past B + the fade
        let s = span.interleaved();
        // The seam fade window: allow [B, B + 1 s_xfade] some energy (the fade tail).
        // The default crossfade is 480 frames; past B + 480 it MUST be silence —
        // no looped body. Frame index within `span` of B = 48_000 − 30_000 = 18_000.
        let xfade = 480usize;
        let past = (18_000 + xfade) * 2; // sample offset past the fade tail
        let tail = &s[past..];
        let energy: f64 = tail.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy < 1e-3,
            "looped body played PAST the armed exit boundary B=48_000 (tail energy {energy:.4}) — \
             the append-only lookahead overshoot (CRITICAL-1) is not fixed"
        );
        // Sanity: BEFORE the boundary there IS body energy (the loop really ran up
        // to B — the test is not vacuously silent).
        let before = &s[..(18_000 * 2)];
        let before_energy: f64 = before.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            before_energy > 1.0,
            "expected real body energy BEFORE the boundary (energy {before_energy:.3}) — test sanity"
        );
    }

    /// CRITICAL-1, the pause/stop half: a `pause` (or `stop`) arriving after the
    /// lookahead was prepublished must NOT let stale body play past the transition.
    /// The republish overwrites the unplayed tail with silence.
    #[test]
    fn pause_after_a_real_lookahead_plays_no_stale_body_past_the_transition() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping pause-overshoot test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 2.0));
        let store = Arc::new(AudioStore::new(canonical_format(), 192_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "pauseover".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48,
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        store.seek_to(20_000);
        std::thread::sleep(std::time::Duration::from_millis(500)); // publish body lookahead
                                                                   // Pause: the unplayed tail [20_000, …) must become silence on the next republish.
        control_bus.publish(AudioTransport::Paused, None);
        std::thread::sleep(std::time::Duration::from_millis(600));
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        store.seek_to(20_000);
        let span = store.read(20_000); // the unplayed tail at pause time
        let energy: f64 = span
            .interleaved()
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum();
        assert!(
            energy < 1e-3,
            "stale looped body played PAST a pause (energy {energy:.4}) — the prepublished \
             lookahead was not revised on the transition (CRITICAL-1)"
        );
    }

    /// MAJOR-4 (end-to-end): the video rail publishing `Stopped` (its `Cued`
    /// re-cue) RE-CUES the audio to head — after a stop, a fresh `Vamping` restarts
    /// the loop from `body[0]` at the current bus position — distinct from a
    /// `Paused` hold. (The pure-deck distinction is covered in the loopdeck tests;
    /// this proves the rail routes `Stopped → deck.stop()` and `Vamping` re-cues.)
    #[test]
    fn the_rail_recues_on_stopped_then_restarts_from_head_on_vamp() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping rail re-cue test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // A 1 s RAMP clip: sample value at frame f is f/48_000 (encodes lap position).
        let mut ramp = vec![0.0f32; 48_000 * 2];
        for f in 0..48_000usize {
            let v = (f as f32) / 48_000.0;
            ramp[f * 2] = v;
            ramp[f * 2 + 1] = v;
        }
        let wav = encode_to_wav(dir.path(), &ramp);
        let store = Arc::new(AudioStore::new(canonical_format(), 192_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "recue".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // L = 48_000
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        // Advance the bus deep into the loop (frame 36_000 → body value 0.75).
        store.seek_to(36_000);
        std::thread::sleep(std::time::Duration::from_millis(300));
        // STOP (the video re-cue): the rail must route this to deck.stop().
        control_bus.publish(AudioTransport::Stopped, None);
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Fresh VAMP: re-cue to head → the loop restarts from body[0]≈0.0 at the
        // CURRENT bus position (frame 36_000), not body[36_000]≈0.75.
        control_bus.publish(AudioTransport::Vamping, None);
        std::thread::sleep(std::time::Duration::from_millis(400));
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        store.seek_to(36_000);
        let head = store.read(1); // first sample after the re-cued vamp
        let v = f64::from(head.interleaved()[0]);
        assert!(
            v.abs() < 0.05,
            "a Stopped (re-cue) + fresh Vamping must restart the loop from body[0]≈0.0 at the \
             current bus frame, got {v:.3} — the rail did not re-cue (MAJOR-4)"
        );
    }

    /// A non-looping player settles the audio to silence: the video rail publishes
    /// a one-shot play (Vamping + exit armed at media-time ZERO), so the audio
    /// plays one lap then goes silent — the bus contribution ends cleanly (the
    /// video tile holds its last frame separately).
    #[test]
    fn a_non_looping_player_settles_audio_to_silence_after_one_lap() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping player-audio settle test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 0.25)); // 0.25 s = 12_000 frames
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        // Simulate the VIDEO rail publishing a one-shot play (Vamping + exit
        // armed at media-time ZERO) — the audio plays one lap then settles silent.
        let control_bus = Arc::new(PlayerControlBus::new());
        control_bus.publish(
            AudioTransport::Vamping,
            Some(multiview_core::time::MediaTime::ZERO),
        );
        let plan = PlayerAudioPlan {
            id: "oneshot".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 12, // 12 frames @ 48 fps = 0.25 s = 12_000 samples
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Advance ~3 clip lengths; after the single lap + exit tail the store must
        // settle to silence.
        let mut advanced = 0usize;
        let mut tail_block = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while advanced < 36_000 && std::time::Instant::now() < deadline {
            let buffered = store.buffered_frames();
            if buffered == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            let take = buffered.min(1600);
            tail_block = store.read(take).interleaved().to_vec();
            advanced += take;
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
        let energy: f64 = tail_block
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum();
        assert!(
            energy < 1e-3,
            "a non-looping player must settle its audio to silence after one lap (tail energy {energy:.5})"
        );
    }

    /// CRITICAL-3: priming a LATE vamp window must NOT transiently buffer the whole
    /// pre-`vamp_in` prefix — the peak resident decode buffer is the WINDOW size,
    /// not the asset size. `retain_vamp_window` discards each decoded block that
    /// lies entirely before `vamp_in_sample` and only retains
    /// `[vamp_in_sample, want_end)`, so its returned buffer's capacity is bounded
    /// by the window, NOT by `vamp_out_sample` from frame 0. (Pure: synthetic
    /// blocks, no ffmpeg CLI — the round-2 accumulate-from-0-then-slice would show a
    /// retained capacity ~= the whole pre-window prefix.)
    #[test]
    fn retain_vamp_window_does_not_buffer_the_pre_vamp_in_prefix() {
        use super::retain_vamp_window;
        let channels = canonical_format().channel_count();
        // Simulate a 12 s asset (576_000 frames) decoded in 0.1 s blocks (4_800
        // frames each), vamping a LATE 1 s window [10 s, 11 s) =
        // samples [480_000, 528_000) plus a 480-frame lap-over (want_end 528_480).
        let block_frames = 4_800usize;
        let asset_frames = 576_000usize;
        let in_sample = 480_000usize;
        let want_end = 528_480usize; // 11 s + 480 lap-over
        let window_len = want_end - in_sample; // 48_480 frames
        let nblocks = asset_frames / block_frames;
        // A block iterator: block k carries frames [k*block, (k+1)*block); each
        // sample value = its absolute frame index (so we can verify WHICH samples
        // were retained).
        let blocks = (0..nblocks).map(|k| {
            let base = k * block_frames;
            let mut v = vec![0.0f32; block_frames * channels];
            for f in 0..block_frames {
                let val = (base + f) as f32;
                v[f * channels] = val;
                v[f * channels + 1] = val;
            }
            v
        });

        let window = retain_vamp_window(blocks, channels, in_sample, want_end);

        // The retained buffer is exactly the window (in samples).
        assert_eq!(
            window.len(),
            window_len * channels,
            "retain_vamp_window must return exactly the [in_sample, want_end) window"
        );
        // The FIRST retained sample is the vamp_in SAMPLE (frame 480_000), proving
        // the prefix was discarded, not retained-then-sliced.
        assert!(
            (window[0] - in_sample as f32).abs() < 0.5,
            "the retained window must start at the vamp_in sample {in_sample} (got {})",
            window[0]
        );
        // The KEY bound: the retained buffer's CAPACITY is the window size, NOT the
        // pre-window prefix (~480_000 frames). The round-2 accumulate-from-0 would
        // balloon capacity to ~want_end. Allow one block of slack for growth.
        let cap_frames = window.capacity() / channels;
        assert!(
            cap_frames <= window_len + block_frames,
            "peak retained buffer {cap_frames} frames ballooned past the window {window_len} \
             (+1 block slack) — the pre-vamp_in prefix was buffered (CRITICAL-3)"
        );
    }
}

#[cfg(test)]
mod shared_url_wait_tests {
    #![allow(clippy::unwrap_used)]
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::{await_shared_url, SharedUrlWait};

    const FAST: SharedUrlWait = SharedUrlWait {
        first_connect: Duration::from_millis(200),
        reconnect: Duration::from_millis(50),
        poll: Duration::from_millis(1),
    };

    /// The race seen live: audio asks before the fetcher has its first segment.
    #[test]
    fn audio_waits_for_a_fetcher_that_is_still_starting() {
        let calls = Cell::new(0);
        let stop = AtomicBool::new(false);
        let url = await_shared_url(
            || {
                calls.set(calls.get() + 1);
                (calls.get() > 5).then(|| "http://[::1]:1/live.m3u8".to_owned())
            },
            || None,
            &stop,
            FAST,
        );
        assert_eq!(url.as_deref(), Some("http://[::1]:1/live.m3u8"));
    }

    /// Video anchored with no fetcher entry: it fell back to libav, so audio
    /// opens the origin now instead of serving out the whole wait.
    #[test]
    fn audio_stops_waiting_once_video_runs_without_a_fetcher() {
        let calls = Cell::new(0u32);
        let stop = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let url = await_shared_url(
            || None,
            || {
                calls.set(calls.get() + 1);
                (calls.get() > 3).then_some(1)
            },
            &stop,
            SharedUrlWait {
                first_connect: Duration::from_secs(30),
                ..FAST
            },
        );
        assert_eq!(url, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_wait_is_bounded_and_stops_on_request() {
        let stop = AtomicBool::new(false);
        assert_eq!(await_shared_url(|| None, || None, &stop, FAST), None);
        assert_eq!(await_shared_url(|| None, || Some(4), &stop, FAST), None);
        stop.store(true, std::sync::atomic::Ordering::Release);
        let wait = SharedUrlWait {
            first_connect: Duration::from_secs(30),
            ..FAST
        };
        let started = std::time::Instant::now();
        assert_eq!(await_shared_url(|| None, || None, &stop, wait), None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
