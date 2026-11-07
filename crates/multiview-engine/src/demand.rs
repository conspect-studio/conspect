//! **Demand leases**: a configured source costs nothing until a consumer needs
//! it (`docs/architecture/demand-driven-sources.md`).
//!
//! [`DemandManager`] is the lifecycle layer above [`SourceRegistry`]. A consumer
//! takes a [`DemandLease`] naming the [`Rails`] it needs; the first lease starts
//! those rails off-clock, and the last release stops them `close after` a delay
//! so a layout flip does not thrash. A source set `always_on` holds a standing
//! lease from boot, so it is started whether or not anything else consumes it.
//!
//! States are `idle` / `starting` / `ready` / `failed` ([`DemandPhase`]). They
//! are a separate axis from a tile's signal state (`LIVE`/`STALE`/`NO_SIGNAL`) and
//! from tally. Every phase change is reported as a [`TransitionEvent`]. Video rails are refcounted through the registry:
//! each video lease holds a [`SourceHandle`], and the manager holds one more
//! while the rail runs, so a registry entry exists exactly while a source is
//! consumed or still winding down.
//!
//! ## Isolation (inv #1 / inv #10)
//!
//! Nothing here runs on the data plane. The output-clock thread only ever calls
//! [`ConsumerSet::publish`], a wait-free cell store plus an unpark. Every other
//! path runs on control threads or on the single worker. The book lock is held
//! for bounded bookkeeping and never across [`RailDriver::transition`] or
//! [`RailDriver::store`], which may block (they join threads and open sockets),
//! so a wedged source cannot stall a lease, a sibling source or the worker. Only
//! [`RailDriver::progress`] runs under it, and that contract is a wait-free read.
//! Rail transitions run on transient threads capped by
//! [`DemandConfig::max_concurrent_starts`].
//!
//! ## Idle cost
//!
//! A source with no consumer has **no record here at all**. The worker parks
//! with no timeout unless a source is active or a deadline is pending, so there
//! is no per-source timer and no periodic work while everything is idle.

use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use multiview_framestore::{SwapCell, TileStore};

use crate::registry::{RequestedSize, SourceHandle, SourceKey, SourceRegistry};

/// Which media rails of a source are wanted or running.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Rails {
    /// The video decode (and its companions: captions, event observation).
    pub video: bool,
    /// The audio decode.
    pub audio: bool,
}

impl Rails {
    /// No rail.
    pub const NONE: Self = Self {
        video: false,
        audio: false,
    };
    /// Video only.
    pub const VIDEO: Self = Self {
        video: true,
        audio: false,
    };
    /// Audio only: this must never start a video decode.
    pub const AUDIO: Self = Self {
        video: false,
        audio: true,
    };
    /// Video and audio.
    pub const ALL: Self = Self {
        video: true,
        audio: true,
    };

    /// Whether no rail is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.video && !self.audio
    }

    /// The rails in either set.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            video: self.video || other.video,
            audio: self.audio || other.audio,
        }
    }

    /// Whether every rail in `other` is also in `self`.
    #[must_use]
    pub const fn covers(self, other: Self) -> bool {
        (self.video || !other.video) && (self.audio || !other.audio)
    }
}

/// What kind of consumer holds a lease. Reported to operators.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum ConsumerClass {
    /// A cell of the active layout.
    LayoutCell,
    /// A program-bus, discrete-track or breakaway audio route.
    AudioRoute,
    /// A subtitle layer routed to this source.
    SubtitleRoute,
    /// A source preview (WHEP session or JPEG polling).
    Preview,
    /// An on-demand HLS session.
    HlsSession,
    /// Armed event automation, on the watched source or its replacement.
    EventAutomation,
    /// The source's `activation = "always_on"` setting (the default).
    AlwaysOn,
    /// An operator pressed Start.
    Manual,
}

impl ConsumerClass {
    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LayoutCell => "layout_cell",
            Self::AudioRoute => "audio_route",
            Self::SubtitleRoute => "subtitle_route",
            Self::Preview => "preview",
            Self::HlsSession => "hls_session",
            Self::EventAutomation => "event_automation",
            Self::AlwaysOn => "always_on",
            Self::Manual => "manual",
        }
    }
}

/// One consumer of a source.
#[derive(Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct ConsumerTag {
    /// The consumer's class.
    pub class: ConsumerClass,
    /// A short identity within the class (cell id, session id, ...).
    pub label: Arc<str>,
}

impl ConsumerTag {
    /// A tag of `class` labelled `label`.
    #[must_use]
    pub fn new(class: ConsumerClass, label: impl Into<Arc<str>>) -> Self {
        Self {
            class,
            label: label.into(),
        }
    }
}

/// A source's lifecycle state as operators see it. Not the tile signal state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DemandPhase {
    /// Not started: no connection, decoder or thread. Not a fault.
    Idle,
    /// Wanted, but its first media has not arrived yet ([`StartingPhase`]).
    Starting,
    /// Started and producing.
    Ready,
    /// Wanted, but not producing. It retries with backoff (up to the kind's
    /// retry cap).
    Failed,
}

/// Where a `starting` source is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartingPhase {
    /// Waiting for a start slot ([`DemandConfig::max_concurrent_starts`]).
    Queued,
    /// The start call is running (open, connect, launch).
    Opening,
    /// Started; waiting for its first media.
    AwaitingMedia,
}

impl StartingPhase {
    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Opening => "opening",
            Self::AwaitingMedia => "awaiting_media",
        }
    }
}

impl DemandPhase {
    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// One source's reported state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DemandStatus {
    /// The source id.
    pub id: String,
    /// The phase.
    pub phase: DemandPhase,
    /// The rails consumers want.
    pub wanted: Rails,
    /// The rails actually running.
    pub running: Rails,
    /// Who is consuming it, sorted and de-duplicated.
    pub consumers: Vec<ConsumerTag>,
    /// Why it is failed.
    pub detail: Option<String>,
    /// While `starting`: where it is.
    pub starting: Option<StartingPhase>,
    /// While `starting` (opening or awaiting media): time left before it
    /// reads `failed` (the start timeout).
    pub deadline_in: Option<Duration>,
    /// The start generation: increments on every start from nothing.
    pub generation: u64,
}

impl DemandStatus {
    /// The status of a source with no record: idle.
    #[must_use]
    pub fn idle(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            phase: DemandPhase::Idle,
            wanted: Rails::NONE,
            running: Rails::NONE,
            consumers: Vec::new(),
            detail: None,
            starting: None,
            deadline_in: None,
            generation: 0,
        }
    }
}

/// What a tile's status chip shows for a source: its lifecycle phase and,
/// while starting, where it is. Absent from [`DemandManager::activity`] means
/// idle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SourceActivity {
    /// The lifecycle phase.
    pub phase: DemandPhase,
    /// While starting: where it is.
    pub starting: Option<StartingPhase>,
}

/// The non-idle sources' [`SourceActivity`], republished only when one changes.
pub type ActivityMap = HashMap<Arc<str>, SourceActivity>;

/// One lifecycle transition, reported on the event path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransitionEvent {
    /// The source id.
    pub source_id: String,
    /// The start generation the transition belongs to.
    pub generation: u64,
    /// The previous phase.
    pub from: DemandPhase,
    /// The new phase.
    pub to: DemandPhase,
    /// Why, operator-safe (never a URL or credential).
    pub reason: String,
    /// When.
    pub at: std::time::SystemTime,
    /// Who held a lease at the time, sorted.
    pub lease_holders: Vec<ConsumerTag>,
}

/// Receives [`TransitionEvent`]s. Called off the output clock (worker and
/// transition threads) after the book lock is released; must not block.
pub type TransitionSink = Arc<dyn Fn(&TransitionEvent) + Send + Sync>;

/// Lifecycle bounds for one source kind. `None` fields use the manager's
/// [`DemandConfig`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct KindBounds {
    /// How long a start may take to first media before it reads `failed`.
    pub start_timeout: Option<Duration>,
    /// How long an unconsumed source keeps running before it stops.
    pub close_after: Option<Duration>,
    /// How many consecutive failed starts before retrying stops (the source
    /// stays `failed` until a new consumer, Start or edit). Login-based kinds
    /// set this so a retry cannot lock out an account.
    pub retry_cap: Option<u32>,
}

/// Lifecycle tuning.
#[derive(Clone, Copy, Debug)]
pub struct DemandConfig {
    /// "Close after": how long a rail outlives its last lease.
    pub close_after: Duration,
    /// How many sources may be starting at once, across the whole process.
    pub max_concurrent_starts: usize,
    /// How long a start may go without a first frame before it reads as failed
    /// and stops occupying a start slot. The rail keeps retrying.
    pub start_timeout: Duration,
    /// How long a live source may go without progress before it reads as failed.
    pub stall_after: Duration,
    /// The first retry delay after a failed rail transition.
    pub backoff_base: Duration,
    /// The retry delay ceiling.
    pub backoff_max: Duration,
    /// How often active sources are checked for progress. One timer for the
    /// whole manager, armed only while a source is active.
    pub health_period: Duration,
    /// Default retry cap (`None`: retry forever, the always-on behaviour).
    pub retry_cap: Option<u32>,
}

/// The default [`DemandConfig::close_after`].
pub const DEFAULT_CLOSE_AFTER: Duration = Duration::from_secs(30);
/// The default [`DemandConfig::start_timeout`].
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(20);
/// The default [`DemandConfig::max_concurrent_starts`].
pub const DEFAULT_MAX_CONCURRENT_STARTS: usize = 8;
/// Upper bound on queued [`DemandManager::request_restart`] requests.
pub const MAX_PENDING_RESTARTS: usize = 64;
/// Upper bound on concurrent timed leases, so polling cannot pin unbounded
/// sources.
pub const MAX_TIMED_LEASES: usize = 64;
/// How long [`DemandManager::stop_all`] waits for in-flight transitions before
/// it leaves them to tear themselves down when they return.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
/// Upper bound on remembered generations of retired source records.
pub const MAX_RETIRED_GENERATIONS: usize = 4096;

impl Default for DemandConfig {
    fn default() -> Self {
        Self {
            close_after: DEFAULT_CLOSE_AFTER,
            max_concurrent_starts: DEFAULT_MAX_CONCURRENT_STARTS,
            start_timeout: DEFAULT_START_TIMEOUT,
            stall_after: Duration::from_secs(15),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            health_period: Duration::from_secs(1),
            retry_cap: None,
        }
    }
}

/// Turns demand into running rails. Implemented by the run path, and by a
/// counting fake in tests.
///
/// [`transition`](RailDriver::transition) and [`store`](RailDriver::store) run
/// off every hot path and may block. [`resolve`](RailDriver::resolve),
/// [`progress`](RailDriver::progress) and [`finite`](RailDriver::finite) run
/// under the manager's lock and must not.
pub trait RailDriver<T>: Send + Sync + 'static {
    /// The source's frame store, or `None` for a source that is not configured.
    fn store(&self, id: &str) -> Option<Arc<TileStore<T>>>;

    /// Move `id` from running `from` to running `to`. Blocking is allowed.
    /// Returns the rails now actually running.
    ///
    /// # Errors
    /// [`RailFailure`]: the rails left running and a reason. The manager retries
    /// with backoff.
    fn transition(
        &self,
        id: &str,
        from: Rails,
        to: Rails,
        requested: Option<RequestedSize>,
    ) -> Result<Rails, RailFailure>;

    /// [`transition`](Self::transition) with the start `generation` and the
    /// `deadline` a start must meet. The default ignores both; an owner-backed
    /// driver ([`crate::lifecycle::OwnerRails`]) passes them to its owner.
    ///
    /// # Errors
    /// As [`transition`](Self::transition).
    fn transition_at(
        &self,
        id: &str,
        from: Rails,
        to: Rails,
        requested: Option<RequestedSize>,
        generation: u64,
        deadline: Instant,
    ) -> Result<Rails, RailFailure> {
        let _ = (generation, deadline);
        self.transition(id, from, to, requested)
    }

    /// Stop `rails` of `id` for run teardown: producers only. Unlike a
    /// transition it leaves downstream wiring (the program bus, the frame
    /// store) alone, so output still draining keeps what was already produced.
    /// Blocking is allowed. The default is a transition to nothing.
    fn teardown(&self, id: &str, rails: Rails) {
        let _ = self.transition(id, rails, Rails::NONE, None);
    }

    /// `rails` of `id` stopped for good, not as half of a restart: release what
    /// they retained (a store can hold many frames). Blocking is allowed. Runs
    /// while the source's transition is still in flight, so no producer of those
    /// rails can be writing.
    fn retire(&self, id: &str, rails: Rails) {
        let _ = (id, rails);
    }

    /// The rails `id` must run to satisfy consumers asking for `asked`. Usually
    /// a subset: an audio-free kind has no audio rail, an unconfigured id has
    /// none, a run with no program bus has no audio. It may also add a rail, when
    /// a kind carries its audio inside its video receive. Must be a wait-free read.
    fn resolve(&self, id: &str, asked: Rails) -> Rails;

    /// A counter that advances while `rails` of `id` are producing. Must be a
    /// wait-free read.
    fn progress(&self, id: &str, rails: Rails) -> u64;

    /// Whether `id` ends by itself (a file), so silence after its first frame
    /// is not a stall.
    fn finite(&self, id: &str) -> bool {
        let _ = id;
        false
    }

    /// After a failed start: whether rails it reported running hold resources
    /// whose teardown is not confirmed, so they must be stopped before any new
    /// start (review DMD-06). Called off the manager's lock. The default is
    /// `false`: a failed native start's rails stay usable.
    fn unconfirmed(&self, id: &str) -> bool {
        let _ = id;
        false
    }

    /// Lifecycle bounds for `id`'s kind. Runs under the manager's lock: must be
    /// a wait-free read. The default defers to the manager's config.
    fn bounds(&self, id: &str) -> KindBounds {
        let _ = id;
        KindBounds::default()
    }
}

/// A failed rail change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RailFailure {
    /// The rails still running after the failure.
    pub running: Rails,
    /// An operator-safe reason, free of URLs and credentials.
    pub reason: String,
}

/// One lease record.
struct LeaseRecord {
    rails: Rails,
    tag: ConsumerTag,
}

/// One active source. Removed once nothing wants it and nothing runs.
#[allow(clippy::struct_excessive_bools)] // independent lifecycle flags, not a state enum
struct Entry<T> {
    leases: HashMap<u64, LeaseRecord>,
    running: Rails,
    /// A transition thread owns this source.
    in_flight: bool,
    /// When the in-flight start must have returned (review DMD-03).
    open_deadline: Option<Instant>,
    /// The in-flight start missed its deadline: it no longer holds a start
    /// slot and its eventual result belongs to a dead attempt.
    timed_out: bool,
    /// Running rails came from a timed-out attempt: stop them, then start
    /// afresh (a new generation) if still wanted.
    stale: bool,
    /// A stop-then-start was requested (the source was edited).
    restart: bool,
    /// When the running rails last exceeded what is wanted.
    surplus_since: Option<Instant>,
    phase: DemandPhase,
    detail: Option<String>,
    failures: u32,
    not_before: Option<Instant>,
    progress: u64,
    progress_at: Instant,
    /// The manager's own registry reference while a video rail runs.
    handle: Option<SourceHandle<T>>,
    /// The start generation (see [`DemandStatus::generation`]).
    generation: u64,
    /// The phase last reported as a [`TransitionEvent`].
    reported: DemandPhase,
    /// The starting sub-phase last published to the activity map.
    reported_starting: Option<StartingPhase>,
    /// Why the phase last changed (for the transition event).
    reason: String,
    /// An operator Stop: skip `close after` once nothing wants the rails.
    close_now: bool,
    /// The kind's retry cap was reached: no more automatic starts.
    gave_up: bool,
    /// Started by boot (adopted), not by this manager: it never held a start
    /// slot, so it must not occupy one while awaiting its first media.
    booted: bool,
}

impl<T> Entry<T> {
    fn new(now: Instant) -> Self {
        Self {
            leases: HashMap::new(),
            running: Rails::NONE,
            in_flight: false,
            open_deadline: None,
            timed_out: false,
            stale: false,
            restart: false,
            surplus_since: None,
            phase: DemandPhase::Starting,
            detail: None,
            failures: 0,
            not_before: None,
            progress: 0,
            progress_at: now,
            handle: None,
            generation: 0,
            reported: DemandPhase::Idle,
            reported_starting: None,
            reason: String::new(),
            close_now: false,
            gave_up: false,
            booted: false,
        }
    }

    /// Where a `starting` source is.
    fn starting_phase(&self) -> Option<StartingPhase> {
        (self.phase == DemandPhase::Starting).then(|| {
            if self.in_flight && !self.timed_out {
                StartingPhase::Opening
            } else if self.running.is_empty() {
                StartingPhase::Queued
            } else {
                StartingPhase::AwaitingMedia
            }
        })
    }

    fn wanted(&self) -> Rails {
        self.leases
            .values()
            .fold(Rails::NONE, |rails, lease| rails.union(lease.rails))
    }

    /// Occupies a start slot: a transition in flight, or a rail awaiting its
    /// first frame.
    fn holds_start_slot(&self) -> bool {
        (self.in_flight && !self.timed_out)
            || (!self.booted && self.phase == DemandPhase::Starting && !self.running.is_empty())
    }
}

/// A timed lease's holder: the lease and when it lapses.
struct Timed<T> {
    lease: DemandLease<T>,
    until: Instant,
}

struct Book<T> {
    entries: HashMap<Arc<str>, Entry<T>>,
    timed: HashMap<(Arc<str>, ConsumerTag), Timed<T>>,
    /// Standing leases keyed by `(owner, kind)`: an owner (a source id) holds
    /// one independently replaceable set per kind (review DMD-01).
    pins: BTreeMap<(String, PinKind), Vec<DemandLease<T>>>,
    /// The last generation of each source whose record was retired, so a
    /// later start continues from it (review DMD-02). Bounded by
    /// [`MAX_RETIRED_GENERATIONS`]: past it the map folds into
    /// `generation_floor`, which every unknown source starts above.
    retired: HashMap<Arc<str>, u64>,
    generation_floor: u64,
}

impl<T> Book<T> {
    /// The record for `id`, created on first use carrying the generation its
    /// retired predecessor reached.
    fn entry(&mut self, key: &Arc<str>, now: Instant) -> &mut Entry<T> {
        let retired = &mut self.retired;
        let floor = self.generation_floor;
        self.entries.entry(Arc::clone(key)).or_insert_with(|| {
            let mut entry = Entry::new(now);
            entry.generation = retired.remove(key).unwrap_or(0).max(floor);
            entry
        })
    }

    /// Forget every source nothing wants and nothing runs.
    fn retire_finished(&mut self) -> Vec<Entry<T>> {
        let finished: Vec<Arc<str>> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.leases.is_empty()
                    && entry.running.is_empty()
                    && !entry.in_flight
                    && !entry.restart
                    && !entry.stale
            })
            .map(|(id, _)| Arc::clone(id))
            .collect();
        finished.iter().filter_map(|id| self.retire(id)).collect()
    }

    /// Forget `id`'s record, remembering its generation.
    fn retire(&mut self, id: &str) -> Option<Entry<T>> {
        let (key, entry) = self.entries.remove_entry(id)?;
        if entry.generation > self.generation_floor {
            if self.retired.len() >= MAX_RETIRED_GENERATIONS {
                let highest = self.retired.values().copied().max().unwrap_or(0);
                self.generation_floor = self.generation_floor.max(highest);
                self.retired.clear();
            }
            self.retired.insert(key, entry.generation);
        }
        Some(entry)
    }
}

/// Which of an owner's standing lease sets a pin belongs to. Each kind is
/// replaced on its own, so arming automation never drops an `always_on` pin.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum PinKind {
    /// Leases implied by the owner's own settings (activation, automation).
    Standing,
    /// An operator Start on the owner.
    Manual,
}

/// One published set member: a source a group of consumers wants.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct SetItem {
    /// The source id.
    pub id: Arc<str>,
    /// The consumer's label (cell id, route target, ...).
    pub label: Arc<str>,
    /// The picture size this consumer draws at.
    pub size: Option<RequestedSize>,
}

struct SetSlot<T> {
    class: ConsumerClass,
    rails: Rails,
    items: SwapCell<Arc<Vec<SetItem>>>,
    dirty: AtomicBool,
    /// The leases currently held for this set. Touched by the worker and by
    /// [`ConsumerSet::seed`], never by the output clock.
    current: Mutex<Vec<(SetItem, DemandLease<T>)>>,
}

struct Inner<T> {
    book: Mutex<Book<T>>,
    registry: Arc<SourceRegistry<T>>,
    driver: Arc<dyn RailDriver<T>>,
    config: DemandConfig,
    worker: OnceLock<Thread>,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    stopping: AtomicBool,
    next_lease: AtomicU64,
    sets: Mutex<Vec<Arc<SetSlot<T>>>>,
    /// Restarts requested from the output clock, applied by the worker.
    restarts: SwapCell<Arc<Vec<Arc<str>>>>,
    /// Transition threads spawned so far, for tests and diagnostics.
    transitions: AtomicU64,
    /// Transition threads still running, drained (bounded) by
    /// [`DemandManager::stop_all`].
    pending: AtomicU64,
    /// Worker passes so far, for idle-wakeup measurement.
    passes: AtomicU64,
    /// Where [`TransitionEvent`]s go.
    sink: OnceLock<TransitionSink>,
    /// The tile-chip view, republished on change only.
    activity: SwapCell<Arc<ActivityMap>>,
}

/// The source demand manager. Cheap to clone.
pub struct DemandManager<T> {
    inner: Arc<Inner<T>>,
}

impl<T> std::fmt::Debug for DemandManager<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DemandManager").finish_non_exhaustive()
    }
}

impl<T> Clone for DemandManager<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// "I need this source". Dropping it releases the demand, never blocking on a
/// rail: the stop happens on the worker `close after` the last release.
#[must_use = "dropping a lease releases the demand"]
pub struct DemandLease<T> {
    inner: Arc<Inner<T>>,
    id: Arc<str>,
    lease_id: u64,
    _handle: Option<SourceHandle<T>>,
}

impl<T> std::fmt::Debug for DemandLease<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DemandLease")
            .field("id", &self.id)
            .field("lease_id", &self.lease_id)
            .finish_non_exhaustive()
    }
}

impl<T> DemandLease<T> {
    /// The leased source id.
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.id
    }
}

impl<T> Drop for DemandLease<T> {
    fn drop(&mut self) {
        {
            let mut book = lock(&self.inner.book);
            if let Some(entry) = book.entries.get_mut(&self.id) {
                entry.leases.remove(&self.lease_id);
            }
        }
        self.inner.wake();
    }
}

/// A group of consumers published as one set: applying a new set takes every
/// new lease **before** releasing any old one, so a source shared by the old
/// and new set never restarts.
///
/// [`publish`](ConsumerSet::publish) is safe on the output-clock thread: it is a
/// wait-free cell store plus an unpark.
pub struct ConsumerSet<T> {
    slot: Arc<SetSlot<T>>,
    inner: std::sync::Weak<Inner<T>>,
}

impl<T> Clone for ConsumerSet<T> {
    fn clone(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
            inner: self.inner.clone(),
        }
    }
}

impl<T> std::fmt::Debug for ConsumerSet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsumerSet")
            .field("class", &self.slot.class)
            .finish_non_exhaustive()
    }
}

impl<T: Send + Sync + 'static> ConsumerSet<T> {
    /// Replace the set's members. Wait-free; the worker applies it.
    pub fn publish(&self, items: Vec<SetItem>) {
        self.slot.items.store(Arc::new(items));
        self.slot.dirty.store(true, Ordering::Release);
        if let Some(inner) = self.inner.upgrade() {
            inner.wake();
        }
    }

    /// Replace the set's members and take their leases now, on the caller's
    /// thread. For boot, before any worker runs. Never call on the output clock.
    pub fn seed(&self, items: Vec<SetItem>) {
        self.slot.items.store(Arc::new(items));
        if let Some(inner) = self.inner.upgrade() {
            inner.apply_set(&self.slot);
        }
    }

    /// The members last published or seeded.
    #[must_use]
    pub fn items(&self) -> Arc<Vec<SetItem>> {
        self.slot.items.load()
    }
}

impl<T: Send + Sync + 'static> DemandManager<T> {
    /// A manager over `registry` driving `driver`. Leases may be taken at once;
    /// nothing starts until [`start`](Self::start).
    #[must_use]
    pub fn new(
        registry: Arc<SourceRegistry<T>>,
        driver: Arc<dyn RailDriver<T>>,
        config: DemandConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                book: Mutex::new(Book {
                    entries: HashMap::new(),
                    timed: HashMap::new(),
                    pins: BTreeMap::new(),
                    retired: HashMap::new(),
                    generation_floor: 0,
                }),
                registry,
                driver,
                config,
                worker: OnceLock::new(),
                worker_handle: Mutex::new(None),
                stopping: AtomicBool::new(false),
                next_lease: AtomicU64::new(1),
                sets: Mutex::new(Vec::new()),
                restarts: SwapCell::from_pointee(Vec::new()),
                transitions: AtomicU64::new(0),
                pending: AtomicU64::new(0),
                passes: AtomicU64::new(0),
                sink: OnceLock::new(),
                activity: SwapCell::from_pointee(HashMap::new()),
            }),
        }
    }

    /// Start the single worker. Idempotent. A spawn failure is logged and
    /// leases stay booked: the output never depends on this thread.
    pub fn start(&self) {
        let mut slot = lock(&self.inner.worker_handle);
        if slot.is_some() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        match std::thread::Builder::new()
            .name("multiview-demand".to_owned())
            .spawn(move || worker_loop(&inner))
        {
            Ok(handle) => {
                let _ = self.inner.worker.set(handle.thread().clone());
                *slot = Some(handle);
            }
            Err(error) => tracing::error!(%error, "could not spawn the source demand worker"),
        }
    }

    /// Stop the worker and join it. Leases stay booked and nothing is stopped:
    /// the caller tears rails down. Call off the output clock.
    pub fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        self.inner.wake();
        let handle = lock(&self.inner.worker_handle).take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                tracing::error!("the source demand worker panicked");
            }
        }
        let timed = std::mem::take(&mut lock(&self.inner.book).timed);
        drop(timed);
        let pins = std::mem::take(&mut lock(&self.inner.book).pins);
        drop(pins);
        // Sets hold leases (and so the manager) alive: break that cycle.
        let slots: Vec<Arc<SetSlot<T>>> = std::mem::take(&mut *lock(&self.inner.sets));
        for slot in slots {
            let leases = std::mem::take(&mut *lock(&slot.current));
            drop(leases);
        }
    }

    /// Stop the worker, then every running rail's producers
    /// ([`RailDriver::teardown`]). For run teardown, off the output clock.
    /// Blocking, bounded by the driver's own grace periods.
    ///
    /// A transition still in flight is fenced (review DMD-08): it is waited
    /// for up to [`SHUTDOWN_DRAIN`], and whenever it returns after the
    /// worker stopped it tears down whatever it started itself.
    pub fn stop_all(&self) {
        self.shutdown();
        let drain_until = Instant::now() + SHUTDOWN_DRAIN.min(self.inner.config.start_timeout);
        while self.inner.pending.load(Ordering::Acquire) > 0 && Instant::now() < drain_until {
            std::thread::sleep(Duration::from_millis(2));
        }
        let running: Vec<(Arc<str>, Rails)> = {
            let book = lock(&self.inner.book);
            book.entries
                .iter()
                // An in-flight source is torn down by its own thread on return.
                .filter(|(_, entry)| !entry.running.is_empty() && !entry.in_flight)
                .map(|(id, entry)| (Arc::clone(id), entry.running))
                .collect()
        };
        for (id, rails) in running {
            self.inner.driver.teardown(&id, rails);
            if let Some(entry) = lock(&self.inner.book).entries.get_mut(&id) {
                entry.running = Rails::NONE;
            }
        }
    }

    /// Take a lease on `rails` of source `id`. Never blocks on a rail.
    pub fn acquire(
        &self,
        id: &str,
        rails: Rails,
        tag: ConsumerTag,
        size: Option<RequestedSize>,
    ) -> DemandLease<T> {
        let handle = rails.video.then(|| self.inner.registry_handle(id, size));
        let handle = handle.flatten();
        let lease_id = self.inner.next_lease.fetch_add(1, Ordering::Relaxed);
        let key: Arc<str> = Arc::from(id);
        {
            let mut book = lock(&self.inner.book);
            let now = Instant::now();
            let entry = book.entry(&key, now);
            entry.leases.insert(lease_id, LeaseRecord { rails, tag });
            entry.close_now = false;
            if entry.gave_up {
                entry.gave_up = false;
                entry.failures = 0;
                entry.not_before = None;
            }
        }
        self.inner.wake();
        DemandLease {
            inner: Arc::clone(&self.inner),
            id: key,
            lease_id,
            _handle: handle,
        }
    }

    /// Take or extend a lease that lapses `ttl` from now, for a consumer with no
    /// close event (JPEG polling). Returns the source's phase, or `None` when
    /// [`MAX_TIMED_LEASES`] are already held.
    #[must_use = "`None` means the lease was refused"]
    pub fn touch(
        &self,
        id: &str,
        rails: Rails,
        tag: ConsumerTag,
        ttl: Duration,
    ) -> Option<DemandPhase> {
        let key = (Arc::<str>::from(id), tag.clone());
        let until = Instant::now() + ttl;
        {
            let mut book = lock(&self.inner.book);
            if let Some(timed) = book.timed.get_mut(&key) {
                timed.until = until;
                let phase = book.entries.get(id).map_or(DemandPhase::Idle, |e| e.phase);
                return Some(phase);
            }
            if book.timed.len() >= MAX_TIMED_LEASES {
                return None;
            }
        }
        let lease = self.acquire(id, rails, tag, None);
        let mut book = lock(&self.inner.book);
        // A concurrent touch may have won: the loser's lease drops after the
        // lock is released, because `insert` returns it.
        let displaced = book.timed.insert(key, Timed { lease, until });
        let phase = book.entries.get(id).map_or(DemandPhase::Idle, |e| e.phase);
        drop(book);
        drop(displaced);
        self.inner.wake();
        Some(phase)
    }

    /// A publishable consumer group needing `rails`.
    #[must_use]
    pub fn consumer_set(&self, class: ConsumerClass, rails: Rails) -> ConsumerSet<T> {
        let slot = Arc::new(SetSlot {
            class,
            rails,
            items: SwapCell::from_pointee(Vec::new()),
            dirty: AtomicBool::new(false),
            current: Mutex::new(Vec::new()),
        });
        lock(&self.inner.sets).push(Arc::clone(&slot));
        ConsumerSet {
            slot,
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Replace the standing leases owned by `owner`: every lease its own
    /// settings imply (its `always_on` activation, its armed automation's
    /// watched and replacement sources), as one complete set. New leases are
    /// taken before the old ones drop.
    pub fn set_pins(&self, owner: &str, pins: Vec<(String, Rails, ConsumerTag)>) {
        self.set_owned_pins(owner, PinKind::Standing, pins);
    }

    /// Replace `owner`'s `kind` set of standing leases; other kinds and other
    /// owners are untouched. New before old.
    pub fn set_owned_pins(
        &self,
        owner: &str,
        kind: PinKind,
        pins: Vec<(String, Rails, ConsumerTag)>,
    ) {
        let leases: Vec<DemandLease<T>> = pins
            .into_iter()
            .map(|(id, rails, tag)| self.acquire(&id, rails, tag, None))
            .collect();
        let key = (owner.to_owned(), kind);
        let previous = {
            let mut book = lock(&self.inner.book);
            if leases.is_empty() {
                book.pins.remove(&key)
            } else {
                book.pins.insert(key, leases)
            }
        };
        drop(previous);
    }

    /// Source `id` was deleted (review DMD-05): drop every pin it owns (its
    /// standing set and an operator Start) and every timed lease on it, then
    /// reconcile so its rails stop. Leases other consumers still hold cannot
    /// start it: a deleted source resolves to no rails.
    pub fn remove_source(&self, id: &str) {
        let (pins, timed) = {
            let mut book = lock(&self.inner.book);
            let keys: Vec<(String, PinKind)> = book
                .pins
                .range((id.to_owned(), PinKind::Standing)..=(id.to_owned(), PinKind::Manual))
                .map(|(key, _)| key.clone())
                .collect();
            let pins: Vec<_> = keys
                .iter()
                .filter_map(|key| book.pins.remove(key))
                .collect();
            let timed_keys: Vec<_> = book
                .timed
                .keys()
                .filter(|(source, _)| &**source == id)
                .cloned()
                .collect();
            let timed: Vec<_> = timed_keys
                .iter()
                .filter_map(|key| book.timed.remove(key))
                .collect();
            if let Some(entry) = book.entries.get_mut(id) {
                entry.close_now = true;
                entry.surplus_since = None;
            }
            (pins, timed)
        };
        drop(pins);
        drop(timed);
        self.restart(id);
    }

    /// Record rails the run started itself before the worker existed.
    pub fn adopt(&self, id: &str, rails: Rails) {
        // Only rails the kind can run: a clock asked for "all" has no audio.
        let possible = self.inner.driver.resolve(id, rails);
        let rails = Rails {
            video: rails.video && possible.video,
            audio: rails.audio && possible.audio,
        };
        if rails.is_empty() {
            return;
        }
        let handle = rails.video.then(|| self.inner.registry_handle(id, None));
        let mut book = lock(&self.inner.book);
        let now = Instant::now();
        let entry = book.entry(&Arc::from(id), now);
        let fresh = entry.running.is_empty();
        entry.running = entry.running.union(rails);
        entry.booted = true;
        // Boot started it: that start is a generation of its own.
        if fresh {
            entry.generation = entry.generation.wrapping_add(1);
        }
        // Boot started these rails from nothing, so any media they already
        // published is this generation's: the baseline is zero, not a sample
        // (review DMD-07).
        if fresh {
            entry.progress = 0;
        }
        entry.progress_at = now;
        if let Some(handle) = handle.flatten() {
            entry.handle = Some(handle);
        }
    }

    /// The source's definition changed: stop and restart it if it is running.
    /// A source nobody consumes is left alone, costing nothing.
    pub fn restart(&self, id: &str) {
        {
            let mut book = lock(&self.inner.book);
            if let Some(entry) = book.entries.get_mut(id) {
                entry.restart = true;
                entry.not_before = None;
                entry.failures = 0;
                entry.gave_up = false;
            }
        }
        self.inner.wake();
    }

    /// [`restart`](Self::restart) from the output-clock thread: a wait-free cell
    /// update plus an unpark; the worker applies it. Bounded: past
    /// [`MAX_PENDING_RESTARTS`] the oldest request is shed (a later edit of the
    /// same source re-requests it).
    pub fn request_restart(&self, id: &str) {
        let id: Arc<str> = Arc::from(id);
        self.inner.restarts.rcu(|pending| {
            let mut next = pending.as_ref().clone();
            if !next.contains(&id) {
                if next.len() >= MAX_PENDING_RESTARTS {
                    next.remove(0);
                }
                next.push(Arc::clone(&id));
            }
            next
        });
        self.inner.wake();
    }

    /// The reported state of `id`.
    #[must_use]
    pub fn status(&self, id: &str) -> DemandStatus {
        let book = lock(&self.inner.book);
        let now = Instant::now();
        book.entries.get(id).map_or_else(
            || DemandStatus::idle(id),
            |entry| report(&self.inner, id, entry, now),
        )
    }

    /// Every active source. An absent id is idle.
    #[must_use]
    pub fn snapshot(&self) -> Vec<DemandStatus> {
        let book = lock(&self.inner.book);
        let now = Instant::now();
        let mut out: Vec<DemandStatus> = book
            .entries
            .iter()
            .map(|(id, entry)| report(&self.inner, id, entry, now))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// How many sources have an active record. Zero while everything is idle.
    #[must_use]
    pub fn active_len(&self) -> usize {
        lock(&self.inner.book).entries.len()
    }

    /// Transition threads spawned so far.
    #[must_use]
    pub fn transitions(&self) -> u64 {
        self.inner.transitions.load(Ordering::Relaxed)
    }

    /// The non-idle sources' activity for tile status chips. A wait-free load;
    /// the map is replaced only when a phase or starting sub-phase changes, so
    /// a per-frame reader allocates nothing.
    #[must_use]
    pub fn activity(&self) -> Arc<ActivityMap> {
        self.inner.activity.load()
    }

    /// A cheap handle that reads [`activity`](Self::activity) without holding
    /// the manager alive.
    #[must_use]
    pub fn activity_reader(&self) -> ActivityReader {
        let weak: std::sync::Weak<Inner<T>> = Arc::downgrade(&self.inner);
        let weak: std::sync::Weak<dyn ActivitySource> = weak;
        ActivityReader(weak)
    }

    /// Worker passes so far. Constant while every source is idle.
    #[must_use]
    pub fn worker_passes(&self) -> u64 {
        self.inner.passes.load(Ordering::Relaxed)
    }

    /// Send every [`TransitionEvent`] to `sink`. Set once, before
    /// [`start`](Self::start); a second call is ignored.
    pub fn set_transition_sink(&self, sink: TransitionSink) {
        let _ = self.inner.sink.set(sink);
    }

    /// Operator Start: hold `id` ready until [`stop`](Self::stop). Clears a
    /// given-up retry. Never blocks on the source.
    pub fn start_source(&self, id: &str) {
        let tag = ConsumerTag::new(ConsumerClass::Manual, "operator");
        self.set_owned_pins(id, PinKind::Manual, vec![(id.to_owned(), Rails::ALL, tag)]);
    }

    /// Operator Stop. Refused, naming them, while any consumer other than an
    /// operator Start holds the source (an `always_on` setting is one). On
    /// success the source stops at once, skipping `close after`.
    ///
    /// # Errors
    /// The consumers still holding the source, sorted.
    pub fn stop_source(&self, id: &str) -> Result<(), Vec<ConsumerTag>> {
        let previous = {
            let mut book = lock(&self.inner.book);
            let holders: Vec<ConsumerTag> = book.entries.get(id).map_or_else(Vec::new, |entry| {
                let mut tags: Vec<ConsumerTag> = entry
                    .leases
                    .values()
                    .filter(|lease| lease.tag.class != ConsumerClass::Manual)
                    .map(|lease| lease.tag.clone())
                    .collect();
                tags.sort();
                tags.dedup();
                tags
            });
            if !holders.is_empty() {
                return Err(holders);
            }
            if let Some(entry) = book.entries.get_mut(id) {
                entry.close_now = true;
                entry.surplus_since = None;
            }
            book.pins.remove(&(id.to_owned(), PinKind::Manual))
        };
        drop(previous);
        self.inner.wake();
        Ok(())
    }
}

/// Something that holds an [`ActivityMap`].
trait ActivitySource: Send + Sync {
    fn load_activity(&self) -> Arc<ActivityMap>;
}

impl<T: Send + Sync + 'static> ActivitySource for Inner<T> {
    fn load_activity(&self) -> Arc<ActivityMap> {
        self.activity.load()
    }
}

/// Reads a manager's [`ActivityMap`]; empty once the manager is gone.
#[derive(Clone)]
pub struct ActivityReader(std::sync::Weak<dyn ActivitySource>);

impl std::fmt::Debug for ActivityReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityReader").finish_non_exhaustive()
    }
}

impl ActivityReader {
    /// The current map (wait-free), or `None` once the manager is dropped.
    #[must_use]
    pub fn load(&self) -> Option<Arc<ActivityMap>> {
        self.0.upgrade().map(|source| source.load_activity())
    }
}

/// The phase operators see: an entry with nothing running and nothing it can
/// start (no consumer, or only unsatisfiable ones) is idle, never a fault.
fn effective_phase<T: 'static>(
    driver: &dyn RailDriver<T>,
    id: &str,
    entry: &Entry<T>,
) -> DemandPhase {
    if entry.running.is_empty() && !entry.in_flight {
        let wanted = entry.wanted();
        if wanted.is_empty() || driver.resolve(id, wanted).is_empty() {
            return DemandPhase::Idle;
        }
    }
    entry.phase
}

fn holders<T>(entry: &Entry<T>) -> Vec<ConsumerTag> {
    let mut consumers: Vec<ConsumerTag> = entry
        .leases
        .values()
        .map(|lease| lease.tag.clone())
        .collect();
    consumers.sort();
    consumers.dedup();
    consumers
}

fn report<T: Send + Sync + 'static>(
    inner: &Inner<T>,
    id: &str,
    entry: &Entry<T>,
    now: Instant,
) -> DemandStatus {
    // A source still within `close after` is running, so it stays truthful
    // about what the host is paying for.
    let phase = effective_phase(inner.driver.as_ref(), id, entry);
    let starting = if phase == DemandPhase::Starting {
        entry.starting_phase()
    } else {
        None
    };
    let deadline_in = match starting {
        Some(StartingPhase::Opening | StartingPhase::AwaitingMedia) => {
            let timeout = inner.start_timeout(id);
            Some(timeout.saturating_sub(now.saturating_duration_since(entry.progress_at)))
        }
        _ => None,
    };
    DemandStatus {
        id: id.to_owned(),
        phase,
        wanted: entry.wanted(),
        running: entry.running,
        consumers: holders(entry),
        detail: entry.detail.clone(),
        starting,
        deadline_in,
        generation: entry.generation,
    }
}

impl<T> Inner<T> {
    fn wake(&self) {
        if let Some(thread) = self.worker.get() {
            thread.unpark();
        }
    }
}

impl<T: Send + Sync + 'static> Inner<T> {
    fn start_timeout(&self, id: &str) -> Duration {
        self.driver
            .bounds(id)
            .start_timeout
            .unwrap_or(self.config.start_timeout)
    }

    /// The transition events for `entry` since it was last reported, marking
    /// it reported. Book lock held; the caller emits after releasing it.
    fn note(&self, id: &str, entry: &mut Entry<T>, out: &mut Notes) {
        let phase = effective_phase(self.driver.as_ref(), id, entry);
        let starting = if phase == DemandPhase::Starting {
            entry.starting_phase()
        } else {
            None
        };
        if phase != entry.reported || starting != entry.reported_starting {
            entry.reported_starting = starting;
            out.activity
                .push((Arc::from(id), SourceActivity { phase, starting }));
        }
        if phase == entry.reported {
            return;
        }
        if self.sink.get().is_some() {
            let reason = if entry.reason.is_empty() {
                default_reason(phase)
            } else {
                std::mem::take(&mut entry.reason)
            };
            out.events.push(TransitionEvent {
                source_id: id.to_owned(),
                generation: entry.generation,
                from: entry.reported,
                to: phase,
                reason,
                at: std::time::SystemTime::now(),
                lease_holders: holders(entry),
            });
        }
        entry.reason.clear();
        entry.reported = phase;
    }

    fn emit(&self, notes: &Notes) {
        if !notes.activity.is_empty() {
            self.activity.rcu(|current| {
                let mut next = current.as_ref().clone();
                for (id, activity) in &notes.activity {
                    if activity.phase == DemandPhase::Idle {
                        next.remove(id);
                    } else {
                        next.insert(Arc::clone(id), *activity);
                    }
                }
                next
            });
        }
        if let Some(sink) = self.sink.get() {
            for event in &notes.events {
                sink(event);
            }
        }
    }

    /// One registry reference for `id`. `None` for an unconfigured source.
    fn registry_handle(&self, id: &str, size: Option<RequestedSize>) -> Option<SourceHandle<T>> {
        let store = self.driver.store(id)?;
        let requested = size.unwrap_or(RequestedSize {
            width: 0,
            height: 0,
        });
        self.registry
            .acquire_store(
                SourceKey::from_canonical(id),
                requested,
                |_| -> Result<_, std::convert::Infallible> { Ok(store) },
            )
            .ok()
    }

    /// Apply dirty consumer sets.
    fn reconcile_sets(self: &Arc<Self>) {
        let slots: Vec<Arc<SetSlot<T>>> = lock(&self.sets).clone();
        for slot in &slots {
            if slot.dirty.swap(false, Ordering::AcqRel) {
                self.apply_set(slot);
            }
        }
    }

    /// Take every lease the set's members need, then drop the ones it no longer
    /// needs. New before old, so a shared source's count never touches zero.
    fn apply_set(self: &Arc<Self>, slot: &SetSlot<T>) {
        let items = slot.items.load();
        let mut current = lock(&slot.current);
        let previous = std::mem::take(&mut *current);
        let mut kept: HashMap<SetItem, DemandLease<T>> = HashMap::new();
        let mut retired = Vec::new();
        for (item, lease) in previous {
            if items.contains(&item) && !kept.contains_key(&item) {
                kept.insert(item, lease);
            } else {
                retired.push(lease);
            }
        }
        let manager = DemandManager {
            inner: Arc::clone(self),
        };
        for item in items.iter() {
            if current.iter().any(|(have, _)| have == item) {
                continue;
            }
            let lease = kept.remove(item).unwrap_or_else(|| {
                let tag = ConsumerTag {
                    class: slot.class,
                    label: Arc::clone(&item.label),
                };
                manager.acquire(&item.id, slot.rails, tag, item.size)
            });
            current.push((item.clone(), lease));
        }
        drop(current);
        drop(retired);
        drop(kept);
    }

    /// Lapse expired timed leases, returning the next expiry.
    fn expire_timed(&self, now: Instant) -> Option<Instant> {
        let (expired, next) = {
            let mut book = lock(&self.book);
            let mut expired = Vec::new();
            let keys: Vec<_> = book
                .timed
                .iter()
                .filter(|(_, timed)| timed.until <= now)
                .map(|(key, _)| key.clone())
                .collect();
            for key in keys {
                if let Some(timed) = book.timed.remove(&key) {
                    expired.push(timed.lease);
                }
            }
            (expired, book.timed.values().map(|t| t.until).min())
        };
        drop(expired);
        next
    }

    /// One pass over active sources. Returns the next wake-up.
    fn step(self: &Arc<Self>, now: Instant) -> Option<Instant> {
        let config = self.config;
        let mut wake: Option<Instant> = None;
        let mut sooner = |at: Instant| {
            wake = Some(wake.map_or(at, |current: Instant| current.min(at)));
        };
        let mut launches: Vec<Launch> = Vec::new();
        let released;
        let mut events = Notes::default();
        {
            let mut book = lock(&self.book);
            // Expire overdue starts first, so the slots they free are usable
            // in this same pass (review DMD-03).
            if let Some(at) = self.expire_opens(&mut book, now) {
                sooner(at);
            }
            let mut slots_used = book
                .entries
                .values()
                .filter(|entry| entry.holds_start_slot())
                .count();
            // Oldest-first is unnecessary: a blocked source retries next pass.
            // A timed-out start keeps its thread (at most one per source, as no
            // source launches again until its transition returns) but no slot.
            let mut in_flight = book
                .entries
                .values()
                .filter(|e| e.in_flight && !e.timed_out)
                .count();
            let thread_limit = config.max_concurrent_starts.max(1).saturating_mul(2);
            for (id, entry) in &mut book.entries {
                let asked = entry.wanted();
                let wanted = self.driver.resolve(id, asked);
                let bounds = self.driver.bounds(id);
                if !entry.running.is_empty() && !entry.in_flight {
                    self.check_health(id, entry, now);
                    sooner(now + config.health_period);
                }
                if entry.in_flight {
                    continue;
                }
                let close_after = if entry.close_now {
                    Duration::ZERO
                } else {
                    bounds.close_after.unwrap_or(config.close_after)
                };
                let target = if entry.restart || entry.stale {
                    Rails::NONE
                } else {
                    match desired_rails(entry, wanted, now, close_after) {
                        Desired::Hold(deadline) => {
                            if let Some(deadline) = deadline {
                                sooner(deadline);
                            }
                            continue;
                        }
                        Desired::Move(target) => target,
                    }
                };
                // Backoff gates every move: a failed stop must not spin either.
                if let Some(at) = entry.not_before.filter(|at| *at > now) {
                    sooner(at);
                    continue;
                }
                // Stops share a thread bound: 500 last-releases at once must not
                // mean 500 threads. A deferred move runs when one finishes.
                if in_flight >= thread_limit {
                    continue;
                }
                let starting = !entry.running.covers(target);
                if starting {
                    let cap = bounds.retry_cap.or(config.retry_cap);
                    if !admit_start(entry, cap, &mut slots_used, config, now) {
                        continue;
                    }
                } else if target.is_empty() && entry.close_now {
                    "stopped by an operator".clone_into(&mut entry.reason);
                }
                in_flight += 1;
                entry.in_flight = true;
                entry.booted = false;
                let deadline = now + self.start_timeout(id);
                entry.open_deadline = starting.then_some(deadline);
                if starting {
                    sooner(deadline);
                }
                // The readiness baseline is taken before the start runs, so
                // media published during the start call counts (review DMD-07).
                let baseline = self.driver.progress(id, target);
                launches.push(Launch {
                    id: Arc::clone(id),
                    from: entry.running,
                    to: target,
                    restarting: entry.restart,
                    generation: entry.generation,
                    baseline,
                    deadline,
                });
            }
            for (id, entry) in &mut book.entries {
                self.note(id, entry, &mut events);
            }
            released = book.retire_finished();
        }
        drop(released);
        self.emit(&events);
        for launch in launches {
            self.launch(launch);
        }
        wake
    }

    /// Expire every overdue in-flight start; returns the next start deadline.
    fn expire_opens(&self, book: &mut Book<T>, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        for entry in book.entries.values_mut() {
            if let Some(deadline) = entry.open_deadline.filter(|_| !entry.timed_out) {
                if now >= deadline {
                    self.expire_open(entry, now);
                } else {
                    next = Some(next.map_or(deadline, |at| at.min(deadline)));
                }
            }
        }
        next
    }

    /// An in-flight start missed its deadline: release its start slot and read
    /// failed. Its thread keeps the source (no second transition overlaps it);
    /// whatever it eventually returns is stopped as stale. Book lock held.
    fn expire_open(&self, entry: &mut Entry<T>, now: Instant) {
        entry.timed_out = true;
        entry.failures = entry.failures.saturating_add(1);
        entry.phase = DemandPhase::Failed;
        "start timeout".clone_into(&mut entry.reason);
        entry.detail = Some("the start did not finish in time; still retrying".to_owned());
        let delay = backoff(
            self.config.backoff_base,
            self.config.backoff_max,
            entry.failures,
        );
        entry.not_before = Some(now + delay);
    }

    /// Update phase from progress. Book lock held; `progress` must stay cheap.
    fn check_health(&self, id: &str, entry: &mut Entry<T>, now: Instant) {
        let progress = self.driver.progress(id, entry.running);
        if progress != entry.progress {
            entry.progress = progress;
            entry.progress_at = now;
            if entry.phase != DemandPhase::Ready {
                "media arrived".clone_into(&mut entry.reason);
            }
            entry.phase = DemandPhase::Ready;
            entry.detail = None;
            entry.failures = 0;
            return;
        }
        let silent = now.saturating_duration_since(entry.progress_at);
        match entry.phase {
            DemandPhase::Starting if silent >= self.start_timeout(id) => {
                "start timeout".clone_into(&mut entry.reason);
                entry.phase = DemandPhase::Failed;
                entry.detail = Some("no media yet; still retrying".to_owned());
            }
            DemandPhase::Ready if silent >= self.config.stall_after && !self.driver.finite(id) => {
                "no media received recently".clone_into(&mut entry.reason);
                entry.phase = DemandPhase::Failed;
                entry.detail = Some("no media received recently; still retrying".to_owned());
            }
            _ => {}
        }
    }

    /// Run one transition on a transient thread. The caller holds a start slot
    /// for it, so the thread count is bounded.
    fn launch(self: &Arc<Self>, launch: Launch) {
        let Launch {
            id,
            from,
            to,
            restarting,
            generation,
            baseline,
            deadline,
        } = launch;
        let id = &id;
        self.transitions.fetch_add(1, Ordering::Relaxed);
        self.pending.fetch_add(1, Ordering::AcqRel);
        let inner = Arc::clone(self);
        let thread_id = Arc::clone(id);
        let spawned = std::thread::Builder::new()
            .name("multiview-demand-rail".to_owned())
            .spawn(move || {
                let requested = inner
                    .registry
                    .requested_supremum(&SourceKey::from_canonical(&*thread_id));
                let driver = Arc::clone(&inner.driver);
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let outcome =
                        driver.transition_at(&thread_id, from, to, requested, generation, deadline);
                    let running = outcome
                        .as_ref()
                        .map_or_else(|failure| failure.running, |running| *running);
                    let stopped = Rails {
                        video: from.video && !running.video,
                        audio: from.audio && !running.audio,
                    };
                    if !restarting && !stopped.is_empty() {
                        driver.retire(&thread_id, stopped);
                    }
                    outcome
                }))
                .unwrap_or_else(|_| {
                    Err(RailFailure {
                        running: from,
                        reason: "the source rail panicked while changing state".to_owned(),
                    })
                });
                inner.finish(&thread_id, from, to, baseline, outcome);
                inner.pending.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(error) = spawned {
            tracing::warn!(source = %id, %error, "could not spawn a rail transition; retrying");
            let failure = RailFailure {
                running: from,
                reason: "could not start a worker thread".to_owned(),
            };
            self.finish(id, from, to, baseline, Err(failure));
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Record a transition's outcome and wake the worker.
    fn finish(
        self: &Arc<Self>,
        id: &str,
        from: Rails,
        to: Rails,
        baseline: u64,
        outcome: Result<Rails, RailFailure>,
    ) {
        let running = outcome
            .as_ref()
            .map_or_else(|failure| failure.running, |running| *running);
        let started_video = running.video && !from.video;
        let handle = started_video
            .then(|| self.registry_handle(id, None))
            .flatten();
        let unconfirmed = outcome.is_err()
            && !from.covers(to)
            && !running.is_empty()
            && self.driver.unconfirmed(id);
        let mut surplus = None;
        let mut events = Notes::default();
        let mut orphaned = Rails::NONE;
        {
            let mut book = lock(&self.book);
            let now = Instant::now();
            // The worker is gone: nothing would ever stop these rails, so this
            // thread does (checked under the lock `stop_all` snapshots under).
            if self.stopping.load(Ordering::Acquire) {
                if let Some(entry) = book.entries.get_mut(id) {
                    entry.in_flight = false;
                    entry.open_deadline = None;
                    entry.running = Rails::NONE;
                    surplus = entry.handle.take();
                }
                orphaned = running;
            } else if let Some(entry) = book.entries.get_mut(id) {
                entry.in_flight = false;
                entry.open_deadline = None;
                let late = std::mem::take(&mut entry.timed_out);
                if running.is_empty() {
                    entry.close_now = false;
                }
                if to.is_empty() {
                    entry.restart = false;
                }
                if running.is_empty() {
                    entry.stale = false;
                } else if unconfirmed {
                    entry.stale = true;
                }
                entry.running = running;
                if !running.video {
                    surplus = entry.handle.take();
                } else if handle.is_some() {
                    entry.handle = handle;
                }
                match outcome {
                    // A start that returned after its deadline belongs to a dead
                    // attempt: it stays failed (already counted and backed off)
                    // and what it started is stopped before any new generation.
                    _ if late => {
                        entry.stale = !running.is_empty();
                        "start returned after its timeout".clone_into(&mut entry.reason);
                    }
                    Ok(_) => {
                        entry.surplus_since = None;
                        entry.not_before = None;
                        if !from.covers(to) {
                            entry.phase = DemandPhase::Starting;
                            entry.detail = None;
                            entry.progress = baseline;
                            entry.progress_at = now;
                        }
                        if running.is_empty() {
                            entry.phase = DemandPhase::Starting;
                        }
                    }
                    Err(RailFailure { reason, .. }) => {
                        tracing::warn!(source = %id, %reason, "source rail change failed; retrying with backoff");
                        entry.failures = entry.failures.saturating_add(1);
                        entry.phase = DemandPhase::Failed;
                        entry.reason.clone_from(&reason);
                        entry.detail = Some(reason);
                        let delay = backoff(
                            self.config.backoff_base,
                            self.config.backoff_max,
                            entry.failures,
                        );
                        entry.not_before = Some(now + delay);
                    }
                }
                if running.is_empty() && entry.reason.is_empty() {
                    "stopped".clone_into(&mut entry.reason);
                }
                self.note(id, entry, &mut events);
            }
        }
        drop(surplus);
        if !orphaned.is_empty() {
            self.driver.teardown(id, orphaned);
            return;
        }
        self.emit(&events);
        self.wake();
    }
}

/// Gate one start: the kind's retry cap (stay failed, no further attempts,
/// until a new consumer, an operator Start or an edit), then a start slot.
/// On admission, a start from nothing opens a new generation.
fn admit_start<T>(
    entry: &mut Entry<T>,
    cap: Option<u32>,
    slots_used: &mut usize,
    config: DemandConfig,
    now: Instant,
) -> bool {
    if cap.is_some_and(|cap| entry.failures >= cap) {
        if !entry.gave_up {
            entry.gave_up = true;
            entry.detail = Some(format!(
                "gave up after {} failed starts; press Start to retry",
                entry.failures
            ));
        }
        return false;
    }
    if *slots_used >= config.max_concurrent_starts.max(1) {
        return false;
    }
    *slots_used += 1;
    if entry.running.is_empty() {
        entry.generation = entry.generation.wrapping_add(1);
        entry.phase = DemandPhase::Starting;
        if entry.reason.is_empty() {
            "a consumer needs it".clone_into(&mut entry.reason);
        }
    }
    entry.progress_at = now;
    true
}

/// What a source's rails should do now.
enum Desired {
    /// Stay put, optionally until a deadline.
    Hold(Option<Instant>),
    /// Move to these rails.
    Move(Rails),
}

/// Added rails start at once. Surplus rails stop only `close_after` later, so a
/// new consumer arriving meanwhile finds them still running.
fn desired_rails<T>(
    entry: &mut Entry<T>,
    wanted: Rails,
    now: Instant,
    close_after: Duration,
) -> Desired {
    if entry.running == wanted {
        entry.surplus_since = None;
        return Desired::Hold(None);
    }
    if entry.running.covers(wanted) {
        let since = *entry.surplus_since.get_or_insert(now);
        let deadline = since + close_after;
        if now >= deadline {
            return Desired::Move(wanted);
        }
        return Desired::Hold(Some(deadline));
    }
    // Keep surplus rails through the add; `close after` applies afterwards.
    Desired::Move(entry.running.union(wanted))
}

/// Capped exponential backoff.
fn backoff(base: Duration, max: Duration, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    base.saturating_mul(1_u32 << exponent).min(max)
}

/// One transition to run on its own thread.
struct Launch {
    id: Arc<str>,
    from: Rails,
    to: Rails,
    restarting: bool,
    generation: u64,
    /// Progress sampled before the transition (the readiness baseline).
    baseline: u64,
    /// When a start must have returned.
    deadline: Instant,
}

/// What one pass learned, emitted after the book lock is released.
#[derive(Default)]
struct Notes {
    events: Vec<TransitionEvent>,
    activity: Vec<(Arc<str>, SourceActivity)>,
}

fn default_reason(phase: DemandPhase) -> String {
    match phase {
        DemandPhase::Idle => "no consumer",
        DemandPhase::Starting => "starting",
        DemandPhase::Ready => "media arrived",
        DemandPhase::Failed => "not producing",
    }
    .to_owned()
}

fn worker_loop<T: Send + Sync + 'static>(inner: &Arc<Inner<T>>) {
    while !inner.stopping.load(Ordering::Acquire) {
        inner.passes.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let restarts = inner.restarts.swap(Arc::new(Vec::new()));
        if !restarts.is_empty() {
            let manager = DemandManager {
                inner: Arc::clone(inner),
            };
            for id in restarts.iter() {
                manager.restart(id);
            }
        }
        inner.reconcile_sets();
        let next_timed = inner.expire_timed(now);
        let next_step = inner.step(now);
        match [next_timed, next_step].into_iter().flatten().min() {
            // Parks with no timeout while everything is idle: no timer at all.
            None => std::thread::park(),
            Some(at) => std::thread::park_timeout(at.saturating_duration_since(Instant::now())),
        }
    }
}

/// Lock, recovering from poison: the book holds plain counters, so a panicked
/// holder leaves nothing torn that a later pass cannot reconcile.
fn lock<U>(mutex: &Mutex<U>) -> MutexGuard<'_, U> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;

    /// Counts every rail change and models a source's frames by hand.
    #[derive(Default)]
    struct Fake {
        stores: Mutex<HashMap<String, Arc<TileStore<u8>>>>,
        running: Mutex<HashMap<String, Rails>>,
        video_starts: AtomicUsize,
        audio_starts: AtomicUsize,
        video_stops: AtomicUsize,
        retired: AtomicUsize,
        failing: Mutex<HashSet<String>>,
        attempts: AtomicUsize,
        active: AtomicUsize,
        peak: AtomicUsize,
        hold: Option<Duration>,
        progress: AtomicU64,
        /// Publish one frame inside the start call, then nothing (a short file).
        publish_on_start: AtomicBool,
        /// Sources whose start blocks until removed from this set (a wedged open).
        wedged: Mutex<HashSet<String>>,
    }

    impl RailDriver<u8> for Fake {
        fn store(&self, id: &str) -> Option<Arc<TileStore<u8>>> {
            Some(Arc::clone(
                lock(&self.stores)
                    .entry(id.to_owned())
                    .or_insert_with(|| Arc::new(TileStore::with_defaults(id))),
            ))
        }
        fn resolve(&self, id: &str, asked: Rails) -> Rails {
            Rails {
                video: asked.video,
                audio: asked.audio && !id.starts_with("mute"),
            }
        }
        fn transition(
            &self,
            id: &str,
            from: Rails,
            to: Rails,
            _requested: Option<RequestedSize>,
        ) -> Result<Rails, RailFailure> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if lock(&self.failing).contains(id) && !to.is_empty() {
                return Err(RailFailure {
                    running: from,
                    reason: "connection refused".to_owned(),
                });
            }
            if to.video && !from.video {
                while lock(&self.wedged).contains(id) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Some(hold) = self.hold {
                std::thread::sleep(hold);
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if to.video && !from.video {
                self.video_starts.fetch_add(1, Ordering::SeqCst);
                if self.publish_on_start.load(Ordering::SeqCst) {
                    self.progress.fetch_add(1, Ordering::SeqCst);
                }
            }
            if from.video && !to.video {
                self.video_stops.fetch_add(1, Ordering::SeqCst);
            }
            if to.audio && !from.audio {
                self.audio_starts.fetch_add(1, Ordering::SeqCst);
            }
            lock(&self.running).insert(id.to_owned(), to);
            Ok(to)
        }
        fn retire(&self, _id: &str, rails: Rails) {
            if rails.video {
                self.retired.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn progress(&self, _id: &str, _rails: Rails) -> u64 {
            self.progress.load(Ordering::SeqCst)
        }
    }

    fn config(close_after_ms: u64) -> DemandConfig {
        DemandConfig {
            close_after: Duration::from_millis(close_after_ms),
            max_concurrent_starts: 4,
            start_timeout: Duration::from_millis(100),
            stall_after: Duration::from_millis(100),
            backoff_base: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
            health_period: Duration::from_millis(5),
            retry_cap: None,
        }
    }

    fn manager(fake: &Arc<Fake>, close_after_ms: u64) -> DemandManager<u8> {
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(fake);
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(close_after_ms));
        manager.start();
        manager
    }

    fn wait(what: &str, mut done: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn tag(label: &str) -> ConsumerTag {
        ConsumerTag::new(ConsumerClass::Preview, label)
    }

    fn cell(id: &str, label: &str) -> SetItem {
        SetItem {
            id: Arc::from(id),
            label: Arc::from(label),
            size: None,
        }
    }

    #[test]
    fn an_unleased_source_has_no_record_and_reads_idle() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        assert_eq!(manager.active_len(), 0);
        assert_eq!(manager.status("cam").phase, DemandPhase::Idle);
        assert_eq!(fake.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(manager.transitions(), 0);
        manager.shutdown();
    }

    #[test]
    fn first_lease_starts_and_last_lease_stops_only_after_close_after() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 150);
        let first = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        let second = manager.acquire("cam", Rails::VIDEO, tag("b"), None);
        wait("video start", || {
            fake.video_starts.load(Ordering::SeqCst) == 1
        });
        drop(first);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            fake.video_stops.load(Ordering::SeqCst),
            0,
            "a lease remains"
        );
        let released = Instant::now();
        drop(second);
        wait("video stop", || {
            fake.video_stops.load(Ordering::SeqCst) == 1
        });
        assert!(
            released.elapsed() >= Duration::from_millis(140),
            "stopped before close after"
        );
        wait("record removed", || manager.active_len() == 0);
        assert_eq!(fake.video_starts.load(Ordering::SeqCst), 1, "one decode");
        manager.shutdown();
    }

    #[test]
    fn a_consumer_returning_inside_close_after_does_not_restart() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 200);
        let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        drop(lease);
        std::thread::sleep(Duration::from_millis(20));
        let back = manager.acquire("cam", Rails::VIDEO, tag("b"), None);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(fake.video_starts.load(Ordering::SeqCst), 1);
        assert_eq!(fake.video_stops.load(Ordering::SeqCst), 0);
        drop(back);
        manager.shutdown();
    }

    #[test]
    fn audio_only_demand_starts_no_video() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let lease = manager.acquire(
            "msnow",
            Rails::AUDIO,
            ConsumerTag::new(ConsumerClass::AudioRoute, "program"),
            None,
        );
        wait("audio start", || {
            fake.audio_starts.load(Ordering::SeqCst) == 1
        });
        assert_eq!(fake.video_starts.load(Ordering::SeqCst), 0);
        assert_eq!(manager.status("msnow").running, Rails::AUDIO);
        drop(lease);
        manager.shutdown();
    }

    #[test]
    fn an_unsupported_rail_never_starts_or_spins_and_reads_idle() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let lease = manager.acquire("mute-clock", Rails::AUDIO, tag("a"), None);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(manager.status("mute-clock").phase, DemandPhase::Idle);
        drop(lease);
        wait("record removed", || manager.active_len() == 0);
        manager.shutdown();
    }

    #[test]
    fn a_set_swap_with_shared_sources_does_not_restart_them() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 40);
        let set = manager.consumer_set(ConsumerClass::LayoutCell, Rails::VIDEO);
        set.publish(vec![cell("shared", "c1"), cell("old", "c2")]);
        wait("both start", || {
            fake.video_starts.load(Ordering::SeqCst) == 2
        });
        // The shared source moves to another cell: a new lease, same decode.
        set.publish(vec![cell("shared", "c9"), cell("new", "c2")]);
        wait("new start", || {
            fake.video_starts.load(Ordering::SeqCst) == 3
        });
        wait("old stops", || fake.video_stops.load(Ordering::SeqCst) == 1);
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            fake.video_starts.load(Ordering::SeqCst),
            3,
            "shared never restarts"
        );
        assert_eq!(
            fake.video_stops.load(Ordering::SeqCst),
            1,
            "only `old` stopped"
        );
        assert_eq!(lock(&fake.running).get("old"), Some(&Rails::NONE));
        assert_eq!(lock(&fake.running).get("shared"), Some(&Rails::VIDEO));
        set.publish(Vec::new());
        wait("all stopped", || manager.active_len() == 0);
        manager.shutdown();
    }

    #[test]
    fn rapid_acquire_and_release_leaves_nothing_behind() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let registry = SourceRegistry::new();
        let manager = DemandManager::new(Arc::clone(&registry), driver, config(1));
        manager.start();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let manager = manager.clone();
                std::thread::spawn(move || {
                    for i in 0..2_000 {
                        let id = format!("s{}", (t + i) % 5);
                        drop(manager.acquire(&id, Rails::ALL, tag("churn"), None));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        wait("records drained", || manager.active_len() == 0);
        wait("registry drained", || registry.active_len() == 0);
        assert!(lock(&fake.running).values().all(|rails| rails.is_empty()));
        manager.shutdown();
    }

    #[test]
    fn a_failing_source_retries_with_growing_backoff_and_reports_failed() {
        let fake = Arc::new(Fake::default());
        lock(&fake.failing).insert("bad".to_owned());
        let manager = manager(&fake, 10);
        let lease = manager.acquire("bad", Rails::VIDEO, tag("a"), None);
        wait("retries", || fake.attempts.load(Ordering::SeqCst) >= 3);
        let status = manager.status("bad");
        assert_eq!(status.phase, DemandPhase::Failed);
        assert_eq!(status.detail.as_deref(), Some("connection refused"));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            fake.attempts.load(Ordering::SeqCst) < 30,
            "capped at 40 ms, so backoff bounds the retry rate"
        );
        lock(&fake.failing).clear();
        wait("recovers", || fake.video_starts.load(Ordering::SeqCst) == 1);
        drop(lease);
        manager.shutdown();
    }

    #[test]
    fn five_hundred_demands_start_within_the_concurrency_bound() {
        let fake = Arc::new(Fake {
            hold: Some(Duration::from_millis(2)),
            ..Fake::default()
        });
        let manager = manager(&fake, 5);
        let leases: Vec<_> = (0..500)
            .map(|i| manager.acquire(&format!("s{i}"), Rails::VIDEO, tag("x"), None))
            .collect();
        // Frames arrive, freeing start slots as sources go live.
        wait("every source started", || {
            fake.progress.fetch_add(1, Ordering::SeqCst);
            fake.video_starts.load(Ordering::SeqCst) == 500
        });
        assert!(
            fake.peak.load(Ordering::SeqCst) <= 4,
            "stampede past the bound"
        );
        drop(leases);
        wait("every source stopped", || {
            fake.progress.fetch_add(1, Ordering::SeqCst);
            manager.active_len() == 0
        });
        assert!(
            fake.peak.load(Ordering::SeqCst) <= 8,
            "stops share a thread bound"
        );
        manager.shutdown();
    }

    #[test]
    fn a_source_that_never_produces_frees_its_start_slot_and_reads_failed() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let dead: Vec<_> = (0..4)
            .map(|i| manager.acquire(&format!("dead{i}"), Rails::VIDEO, tag("x"), None))
            .collect();
        wait("slots full", || {
            fake.video_starts.load(Ordering::SeqCst) == 4
        });
        let next = manager.acquire("next", Rails::VIDEO, tag("x"), None);
        wait("dead sources stop blocking", || {
            fake.video_starts.load(Ordering::SeqCst) == 5
        });
        wait("every dead source reads failed", || {
            (0..4).all(|i| manager.status(&format!("dead{i}")).phase == DemandPhase::Failed)
        });
        drop((dead, next));
        manager.shutdown();
    }

    #[test]
    fn progress_makes_a_source_ready_and_a_stall_fails_it() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        assert_eq!(manager.status("cam").phase, DemandPhase::Starting);
        wait("ready", || {
            fake.progress.fetch_add(1, Ordering::SeqCst);
            manager.status("cam").phase == DemandPhase::Ready
        });
        wait("stalled", || {
            manager.status("cam").phase == DemandPhase::Failed
        });
        drop(lease);
        manager.shutdown();
    }

    #[test]
    fn a_timed_lease_lapses_and_a_touch_extends_it() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 1);
        let ttl = Duration::from_millis(400);
        assert!(manager
            .touch("cam", Rails::VIDEO, tag("jpeg"), ttl)
            .is_some());
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(20));
            assert!(manager
                .touch("cam", Rails::VIDEO, tag("jpeg"), ttl)
                .is_some());
        }
        assert_eq!(
            fake.video_stops.load(Ordering::SeqCst),
            0,
            "touches extend it"
        );
        wait("lapsed", || manager.active_len() == 0);
        manager.shutdown();
    }

    #[test]
    fn timed_leases_are_capped() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 1);
        let ttl = Duration::from_secs(60);
        for i in 0..MAX_TIMED_LEASES {
            assert!(manager
                .touch(&format!("s{i}"), Rails::VIDEO, tag("jpeg"), ttl)
                .is_some());
        }
        assert!(manager
            .touch("over", Rails::VIDEO, tag("jpeg"), ttl)
            .is_none());
        manager.shutdown();
    }

    #[test]
    fn restart_stops_then_starts_a_running_source_and_ignores_an_idle_one() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        manager.restart("idle");
        let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        manager.request_restart("cam");
        wait("restart", || fake.video_starts.load(Ordering::SeqCst) == 2);
        assert_eq!(fake.video_stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.retired.load(Ordering::SeqCst),
            0,
            "last-good survives an edit"
        );
        assert!(!lock(&fake.running).contains_key("idle"));
        drop(lease);
        wait("final stop", || manager.active_len() == 0);
        assert_eq!(
            fake.retired.load(Ordering::SeqCst),
            1,
            "a final stop retires"
        );
        manager.shutdown();
    }

    #[test]
    fn pins_replace_new_before_old() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 30);
        let pin = |id: &str| {
            (
                id.to_owned(),
                Rails::VIDEO,
                ConsumerTag::new(ConsumerClass::EventAutomation, "rule"),
            )
        };
        manager.set_pins("cam", vec![pin("cam"), pin("slate")]);
        wait("pinned", || fake.video_starts.load(Ordering::SeqCst) == 2);
        manager.set_pins("cam", vec![pin("cam")]);
        wait("slate released", || {
            fake.video_stops.load(Ordering::SeqCst) == 1
        });
        assert_eq!(fake.video_starts.load(Ordering::SeqCst), 2);
        manager.set_pins("cam", Vec::new());
        wait("all released", || manager.active_len() == 0);
        manager.shutdown();
    }

    #[test]
    fn a_seeded_set_holds_its_leases_before_the_worker_starts() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let registry = SourceRegistry::new();
        let manager = DemandManager::new(Arc::clone(&registry), driver, config(20));
        let set = manager.consumer_set(ConsumerClass::LayoutCell, Rails::VIDEO);
        let size = RequestedSize {
            width: 320,
            height: 180,
        };
        set.seed(vec![SetItem {
            id: Arc::from("cam"),
            label: Arc::from("c1"),
            size: Some(size),
        }]);
        assert_eq!(
            registry.active_len(),
            1,
            "a seeded lease holds a registry entry"
        );
        assert_eq!(
            registry.requested_supremum(&SourceKey::from_canonical("cam")),
            Some(size)
        );
        assert_eq!(manager.status("cam").consumers.len(), 1);
        manager.start();
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        manager.shutdown();
    }

    #[test]
    fn adopted_rails_are_not_restarted_and_stop_when_nothing_wants_them() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(20));
        let set = manager.consumer_set(ConsumerClass::LayoutCell, Rails::VIDEO);
        set.publish(vec![cell("boot", "c1")]);
        manager.adopt("boot", Rails::VIDEO);
        manager.start();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            fake.video_starts.load(Ordering::SeqCst),
            0,
            "already running"
        );
        set.publish(Vec::new());
        wait("stopped", || fake.video_stops.load(Ordering::SeqCst) == 1);
        manager.shutdown();
    }

    fn events_into(manager: &DemandManager<u8>) -> Arc<Mutex<Vec<TransitionEvent>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        manager.set_transition_sink(Arc::new(move |event: &TransitionEvent| {
            lock(&sink).push(event.clone());
        }));
        seen
    }

    #[test]
    fn operator_start_holds_a_source_and_stop_closes_it_at_once() {
        let fake = Arc::new(Fake::default());
        // A long close-after: an operator Stop must not wait for it.
        let manager = manager(&fake, 60_000);
        manager.start_source("cam");
        wait("started", || fake.video_starts.load(Ordering::SeqCst) == 1);
        let status = manager.status("cam");
        assert_eq!(status.consumers[0].class, ConsumerClass::Manual);
        assert_eq!(status.generation, 1);
        manager.stop_source("cam").expect("no other consumer");
        wait("stopped at once", || {
            fake.video_stops.load(Ordering::SeqCst) == 1
        });
        wait("idle", || manager.active_len() == 0);
        assert_eq!(manager.status("cam").phase, DemandPhase::Idle);
        manager.shutdown();
    }

    #[test]
    fn stop_is_refused_while_consumers_hold_the_source_and_names_them() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let cell = manager.acquire(
            "cam",
            Rails::ALL,
            ConsumerTag::new(ConsumerClass::LayoutCell, "cell-1"),
            None,
        );
        manager.set_pins(
            "cam",
            vec![(
                "cam".to_owned(),
                Rails::ALL,
                ConsumerTag::new(ConsumerClass::AlwaysOn, "always_on"),
            )],
        );
        manager.start_source("cam");
        wait("started", || fake.video_starts.load(Ordering::SeqCst) == 1);
        let holders = manager.stop_source("cam").expect_err("refused");
        let names: Vec<_> = holders.iter().map(|t| (t.class, &*t.label)).collect();
        assert_eq!(
            names,
            vec![
                (ConsumerClass::LayoutCell, "cell-1"),
                (ConsumerClass::AlwaysOn, "always_on")
            ]
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.video_stops.load(Ordering::SeqCst), 0, "still running");
        drop(cell);
        manager.shutdown();
    }

    #[test]
    fn every_phase_change_is_one_transition_event_with_generation_and_holders() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(10));
        let seen = events_into(&manager);
        manager.start();
        let lease = manager.acquire("cam", Rails::VIDEO, tag("viewer"), None);
        wait("ready", || {
            fake.progress.fetch_add(1, Ordering::SeqCst);
            manager.status("cam").phase == DemandPhase::Ready
        });
        drop(lease);
        wait("idle", || manager.active_len() == 0);
        manager.shutdown();
        let events = lock(&seen).clone();
        let path: Vec<_> = events.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(
            path,
            vec![
                (DemandPhase::Idle, DemandPhase::Starting),
                (DemandPhase::Starting, DemandPhase::Ready),
                (DemandPhase::Ready, DemandPhase::Idle),
            ],
            "{events:?}"
        );
        assert!(events
            .iter()
            .all(|e| e.generation == 1 && e.source_id == "cam"));
        assert_eq!(&*events[0].lease_holders[0].label, "viewer");
        assert_eq!(events[1].reason, "media arrived");
        assert_eq!(events[2].lease_holders, Vec::<ConsumerTag>::new());
    }

    #[test]
    fn a_restart_is_a_new_generation() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        assert_eq!(manager.status("cam").generation, 1);
        manager.restart("cam");
        wait("restart", || fake.video_starts.load(Ordering::SeqCst) == 2);
        wait("generation 2", || manager.status("cam").generation == 2);
        drop(lease);
        manager.shutdown();
    }

    #[test]
    fn the_kind_retry_cap_stops_retrying_until_start_is_pressed() {
        struct Capped(Arc<Fake>);
        impl RailDriver<u8> for Capped {
            fn store(&self, id: &str) -> Option<Arc<TileStore<u8>>> {
                self.0.store(id)
            }
            fn resolve(&self, id: &str, asked: Rails) -> Rails {
                self.0.resolve(id, asked)
            }
            fn transition(
                &self,
                id: &str,
                from: Rails,
                to: Rails,
                size: Option<RequestedSize>,
            ) -> Result<Rails, RailFailure> {
                self.0.transition(id, from, to, size)
            }
            fn progress(&self, id: &str, rails: Rails) -> u64 {
                self.0.progress(id, rails)
            }
            fn bounds(&self, _id: &str) -> KindBounds {
                KindBounds {
                    retry_cap: Some(2),
                    ..KindBounds::default()
                }
            }
        }
        let fake = Arc::new(Fake::default());
        lock(&fake.failing).insert("login".to_owned());
        let driver: Arc<dyn RailDriver<u8>> = Arc::new(Capped(Arc::clone(&fake)));
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(10));
        manager.start();
        manager.start_source("login");
        wait("gave up", || {
            manager
                .status("login")
                .detail
                .is_some_and(|d| d.starts_with("gave up"))
        });
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(fake.attempts.load(Ordering::SeqCst), 2, "no lockout storm");
        assert_eq!(manager.status("login").phase, DemandPhase::Failed);
        lock(&fake.failing).clear();
        manager.start_source("login");
        wait("Start retries", || {
            fake.video_starts.load(Ordering::SeqCst) == 1
        });
        manager.shutdown();
    }

    #[test]
    fn a_starting_source_reports_its_phase_and_deadline() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let manager = DemandManager::new(
            SourceRegistry::new(),
            driver,
            DemandConfig {
                start_timeout: Duration::from_secs(20),
                ..config(10)
            },
        );
        let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        // No worker yet: wanted, waiting for a slot.
        let queued = manager.status("cam");
        assert_eq!(queued.phase, DemandPhase::Starting);
        assert_eq!(queued.starting, Some(StartingPhase::Queued));
        manager.start();
        wait("awaiting media", || {
            manager.status("cam").starting == Some(StartingPhase::AwaitingMedia)
        });
        let deadline = manager.status("cam").deadline_in.expect("deadline");
        assert!(deadline <= Duration::from_secs(20) && deadline > Duration::from_secs(15));
        drop(lease);
        manager.shutdown();
    }

    #[test]
    fn an_idle_manager_never_wakes() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        std::thread::sleep(Duration::from_millis(50));
        let before = manager.worker_passes();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(manager.worker_passes(), before, "idle wakeups");
        manager.shutdown();
    }

    #[test]
    fn boot_started_sources_do_not_hold_start_slots() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(10));
        // Ten boot-started sources, none producing yet; four start slots.
        for i in 0..10 {
            manager.set_pins(
                &format!("boot{i}"),
                vec![(format!("boot{i}"), Rails::VIDEO, tag("always_on"))],
            );
            manager.adopt(&format!("boot{i}"), Rails::VIDEO);
        }
        manager.start();
        let lease = manager.acquire("lazy", Rails::VIDEO, tag("viewer"), None);
        wait("on-demand start not blocked by boot", || {
            fake.video_starts.load(Ordering::SeqCst) == 1
        });
        assert_eq!(manager.status("boot0").generation, 1);
        drop(lease);
        manager.shutdown();
    }

    /// Review DMD-02: a stop that retires the record must not reset the
    /// generation, or a late callback from the earlier start reads current.
    #[test]
    fn a_generation_never_repeats_across_an_idle_cycle() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 1);
        let mut seen = Vec::new();
        for round in 1..=3 {
            let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
            wait("start", || {
                fake.video_starts.load(Ordering::SeqCst) == round
            });
            seen.push(manager.status("cam").generation);
            drop(lease);
            wait("record removed", || manager.active_len() == 0);
        }
        assert_eq!(seen, vec![1, 2, 3], "generations repeat after idle");
        manager.shutdown();
    }

    /// Review DMD-07: media published during the start call itself (a short
    /// file) is readiness evidence; no second frame is needed.
    #[test]
    fn media_published_inside_the_start_call_makes_the_source_ready() {
        let fake = Arc::new(Fake::default());
        fake.publish_on_start.store(true, Ordering::SeqCst);
        let manager = manager(&fake, 10);
        let lease = manager.acquire("clip", Rails::VIDEO, tag("a"), None);
        wait("ready from the first frame", || {
            manager.status("clip").phase == DemandPhase::Ready
        });
        drop(lease);
        manager.shutdown();
    }

    /// Review DMD-07: a boot-started input that already published all its
    /// media before adoption reads ready.
    #[test]
    fn an_adopted_input_that_already_published_reads_ready() {
        let fake = Arc::new(Fake::default());
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(&fake);
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(10));
        let lease = manager.acquire("clip", Rails::VIDEO, tag("a"), None);
        fake.progress.store(1, Ordering::SeqCst);
        manager.adopt("clip", Rails::VIDEO);
        manager.start();
        wait("ready", || {
            manager.status("clip").phase == DemandPhase::Ready
        });
        drop(lease);
        manager.shutdown();
    }

    /// Review DMD-03: a start that never returns times out, frees its start
    /// slot and reads failed; its late completion is fenced and stopped.
    #[test]
    fn a_wedged_open_times_out_frees_its_slot_and_its_late_start_is_stopped() {
        let fake = Arc::new(Fake::default());
        for i in 0..4 {
            lock(&fake.wedged).insert(format!("wedged{i}"));
        }
        let manager = manager(&fake, 10);
        let wedged: Vec<_> = (0..4)
            .map(|i| manager.acquire(&format!("wedged{i}"), Rails::VIDEO, tag("x"), None))
            .collect();
        wait("all four opening", || {
            (0..4).all(|i| {
                manager.status(&format!("wedged{i}")).starting == Some(StartingPhase::Opening)
            })
        });
        let healthy = manager.acquire("healthy", Rails::VIDEO, tag("x"), None);
        wait("a queued source starts past the wedged opens", || {
            fake.video_starts.load(Ordering::SeqCst) == 1
        });
        wait("wedged opens read failed", || {
            (0..4).all(|i| manager.status(&format!("wedged{i}")).phase == DemandPhase::Failed)
        });
        assert_eq!(
            manager.status("wedged0").detail.as_deref(),
            Some("the start did not finish in time; still retrying")
        );
        let stale = manager.status("wedged0").generation;
        // Stop during opening, then the open finally returns.
        drop(wedged);
        lock(&fake.wedged).clear();
        wait("late starts stopped", || {
            (0..4).all(|i| lock(&fake.running).get(&format!("wedged{i}")) == Some(&Rails::NONE))
        });
        wait("wedged records retired", || manager.active_len() == 1);
        let healthy_again = manager.acquire("wedged0", Rails::VIDEO, tag("x"), None);
        wait("restarts", || manager.status("wedged0").running.video);
        assert!(
            manager.status("wedged0").generation > stale,
            "late start fenced"
        );
        drop((healthy, healthy_again));
        manager.shutdown();
    }

    /// Review DMD-08: a start still in flight at teardown is fenced: when it
    /// finally returns, what it started is torn down, not left running.
    #[test]
    fn stop_all_fences_a_start_that_completes_after_teardown() {
        let fake = Arc::new(Fake::default());
        lock(&fake.wedged).insert("slow".to_owned());
        let manager = manager(&fake, 10);
        let lease = manager.acquire("slow", Rails::VIDEO, tag("x"), None);
        wait("opening", || {
            manager.status("slow").starting == Some(StartingPhase::Opening)
        });
        let started = Instant::now();
        manager.stop_all();
        assert!(started.elapsed() < Duration::from_secs(6), "bounded");
        lock(&fake.wedged).clear();
        wait("the late start is stopped", || {
            fake.video_starts.load(Ordering::SeqCst) == 1
                && lock(&fake.running).get("slow") == Some(&Rails::NONE)
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(lock(&fake.running).get("slow"), Some(&Rails::NONE));
        assert_eq!(manager.status("slow").running, Rails::NONE);
        drop(lease);
    }

    /// Review DMD-05: deleting a source releases its operator Start and timed
    /// leases, so repeated create/Start/delete leaves nothing and a reused id
    /// starts clean.
    #[test]
    fn removing_a_source_releases_its_start_and_timed_leases() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 60_000);
        for round in 1..=5 {
            manager.start_source("cam");
            assert!(manager
                .touch("cam", Rails::VIDEO, tag("jpeg"), Duration::from_secs(60))
                .is_some());
            wait("started", || {
                fake.video_starts.load(Ordering::SeqCst) == round
            });
            manager.remove_source("cam");
            wait("nothing left", || manager.active_len() == 0);
        }
        assert_eq!(
            manager.status("cam").consumers,
            Vec::new(),
            "no inherited Start"
        );
        assert_eq!(lock(&manager.inner.book).pins.len(), 0);
        assert_eq!(lock(&manager.inner.book).timed.len(), 0);
        manager.shutdown();
    }

    /// Lease churn on a ready source. `DEMAND_LEASE_CYCLES` sets the count so
    /// an external malloc counter (`LD_PRELOAD`) can difference two runs for
    /// the per-lease allocation figure; it defaults to a small smoke count.
    #[test]
    fn lease_cycles_on_a_ready_source_leave_nothing_behind() {
        let cycles: usize = std::env::var("DEMAND_LEASE_CYCLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 60_000);
        let standing = manager.acquire("cam", Rails::VIDEO, tag("always_on"), None);
        wait("ready", || {
            fake.progress.fetch_add(1, Ordering::SeqCst);
            manager.status("cam").phase == DemandPhase::Ready
        });
        let viewer = tag("viewer");
        for _ in 0..cycles {
            drop(manager.acquire("cam", Rails::VIDEO, viewer.clone(), None));
        }
        assert_eq!(manager.status("cam").consumers.len(), 1);
        assert_eq!(fake.video_starts.load(Ordering::SeqCst), 1);
        drop(standing);
        manager.shutdown();
    }
}
