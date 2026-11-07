//! **Demand leases**: a configured source costs nothing until a consumer needs
//! it (`docs/architecture/demand-driven-sources.md`).
//!
//! [`DemandManager`] is the lifecycle layer above [`SourceRegistry`]. A consumer
//! takes a [`DemandLease`] naming the [`Rails`] it needs; the first lease starts
//! those rails off-clock, and the last release stops them after a linger so a
//! layout flip does not thrash. Video rails are refcounted through the registry:
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
    /// Explicitly enabled event journaling.
    EventJournal,
    /// The operator pinned the source with `demand.always_on`.
    AlwaysOn,
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
            Self::EventJournal => "event_journal",
            Self::AlwaysOn => "always_on",
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

/// A source's demand state as operators see it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DemandPhase {
    /// No consumer and nothing running. Not a fault.
    Idle,
    /// Wanted, but its first frame has not arrived yet.
    Starting,
    /// Producing.
    Live,
    /// Wanted, but not producing. It keeps retrying with backoff.
    Failed,
}

impl DemandPhase {
    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Live => "live",
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
}

/// Lifecycle tuning.
#[derive(Clone, Copy, Debug)]
pub struct DemandConfig {
    /// How long a rail outlives its last lease.
    pub linger: Duration,
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
}

/// The default [`DemandConfig::linger`].
pub const DEFAULT_LINGER: Duration = Duration::from_secs(30);
/// The default [`DemandConfig::max_concurrent_starts`].
pub const DEFAULT_MAX_CONCURRENT_STARTS: usize = 8;
/// Upper bound on queued [`DemandManager::request_restart`] requests.
pub const MAX_PENDING_RESTARTS: usize = 64;
/// Upper bound on concurrent timed leases, so polling cannot pin unbounded
/// sources.
pub const MAX_TIMED_LEASES: usize = 64;

impl Default for DemandConfig {
    fn default() -> Self {
        Self {
            linger: DEFAULT_LINGER,
            max_concurrent_starts: DEFAULT_MAX_CONCURRENT_STARTS,
            start_timeout: Duration::from_secs(20),
            stall_after: Duration::from_secs(15),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            health_period: Duration::from_secs(1),
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
struct Entry<T> {
    leases: HashMap<u64, LeaseRecord>,
    running: Rails,
    /// A transition thread owns this source.
    in_flight: bool,
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
}

impl<T> Entry<T> {
    fn new(now: Instant) -> Self {
        Self {
            leases: HashMap::new(),
            running: Rails::NONE,
            in_flight: false,
            restart: false,
            surplus_since: None,
            phase: DemandPhase::Starting,
            detail: None,
            failures: 0,
            not_before: None,
            progress: 0,
            progress_at: now,
            handle: None,
        }
    }

    fn wanted(&self) -> Rails {
        self.leases
            .values()
            .fold(Rails::NONE, |rails, lease| rails.union(lease.rails))
    }

    /// Occupies a start slot: a transition in flight, or a rail awaiting its
    /// first frame.
    fn holds_start_slot(&self) -> bool {
        self.in_flight || (self.phase == DemandPhase::Starting && !self.running.is_empty())
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
    pins: BTreeMap<String, Vec<DemandLease<T>>>,
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
/// rail: the stop happens on the worker after the linger.
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
    pub fn stop_all(&self) {
        self.shutdown();
        let running: Vec<(Arc<str>, Rails)> = {
            let book = lock(&self.inner.book);
            book.entries
                .iter()
                .filter(|(_, entry)| !entry.running.is_empty())
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
            let entry = book
                .entries
                .entry(Arc::clone(&key))
                .or_insert_with(|| Entry::new(now));
            entry.leases.insert(lease_id, LeaseRecord { rails, tag });
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

    /// Replace the standing leases owned by `owner` (a source's armed automation
    /// or journaling). New leases are taken before the old ones drop.
    pub fn set_pins(&self, owner: &str, pins: Vec<(String, Rails, ConsumerTag)>) {
        let leases: Vec<DemandLease<T>> = pins
            .into_iter()
            .map(|(id, rails, tag)| self.acquire(&id, rails, tag, None))
            .collect();
        let previous = {
            let mut book = lock(&self.inner.book);
            if leases.is_empty() {
                book.pins.remove(owner)
            } else {
                book.pins.insert(owner.to_owned(), leases)
            }
        };
        drop(previous);
    }

    /// Record rails the run started itself before the worker existed.
    pub fn adopt(&self, id: &str, rails: Rails) {
        let handle = rails.video.then(|| self.inner.registry_handle(id, None));
        let mut book = lock(&self.inner.book);
        let now = Instant::now();
        let progress = self.inner.driver.progress(id, rails);
        let entry = book
            .entries
            .entry(Arc::from(id))
            .or_insert_with(|| Entry::new(now));
        entry.running = entry.running.union(rails);
        entry.progress = progress;
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
        book.entries.get(id).map_or_else(
            || DemandStatus {
                id: id.to_owned(),
                phase: DemandPhase::Idle,
                wanted: Rails::NONE,
                running: Rails::NONE,
                consumers: Vec::new(),
                detail: None,
            },
            |entry| report(self.inner.driver.as_ref(), id, entry),
        )
    }

    /// Every active source. An absent id is idle.
    #[must_use]
    pub fn snapshot(&self) -> Vec<DemandStatus> {
        let book = lock(&self.inner.book);
        let mut out: Vec<DemandStatus> = book
            .entries
            .iter()
            .map(|(id, entry)| report(self.inner.driver.as_ref(), id, entry))
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
}

fn report<T: 'static>(driver: &dyn RailDriver<T>, id: &str, entry: &Entry<T>) -> DemandStatus {
    let wanted = entry.wanted();
    let unstartable = !wanted.is_empty() && driver.resolve(id, wanted).is_empty();
    let mut consumers: Vec<ConsumerTag> = entry
        .leases
        .values()
        .map(|lease| lease.tag.clone())
        .collect();
    consumers.sort();
    consumers.dedup();
    DemandStatus {
        id: id.to_owned(),
        // A lingering source is still running, so it stays truthful about what
        // the host is paying for. Nothing running and nothing startable (an
        // audio consumer on a run with no bus) is idle, never a fault.
        phase: if entry.running.is_empty() && unstartable {
            DemandPhase::Idle
        } else {
            entry.phase
        },
        wanted,
        running: entry.running,
        consumers,
        detail: entry.detail.clone(),
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
        let mut launches: Vec<(Arc<str>, Rails, Rails, bool)> = Vec::new();
        let mut released = Vec::new();
        {
            let mut book = lock(&self.book);
            let mut slots_used = book
                .entries
                .values()
                .filter(|entry| entry.holds_start_slot())
                .count();
            // Oldest-first is unnecessary: a blocked source retries next pass.
            let mut in_flight = book.entries.values().filter(|e| e.in_flight).count();
            let thread_limit = config.max_concurrent_starts.max(1).saturating_mul(2);
            for (id, entry) in &mut book.entries {
                let asked = entry.wanted();
                let wanted = self.driver.resolve(id, asked);
                if !entry.running.is_empty() && !entry.in_flight {
                    self.check_health(id, entry, now);
                    sooner(now + config.health_period);
                }
                if entry.in_flight {
                    continue;
                }
                let target = if entry.restart {
                    Rails::NONE
                } else {
                    match desired_rails(entry, wanted, now, config.linger) {
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
                    if slots_used >= config.max_concurrent_starts.max(1) {
                        continue;
                    }
                    slots_used += 1;
                }
                in_flight += 1;
                entry.in_flight = true;
                launches.push((Arc::clone(id), entry.running, target, entry.restart));
            }
            // Forget sources nothing wants and nothing runs.
            let finished: Vec<Arc<str>> = book
                .entries
                .iter()
                .filter(|(_, entry)| {
                    entry.leases.is_empty()
                        && entry.running.is_empty()
                        && !entry.in_flight
                        && !entry.restart
                })
                .map(|(id, _)| Arc::clone(id))
                .collect();
            for id in finished {
                if let Some(entry) = book.entries.remove(&id) {
                    released.push(entry);
                }
            }
        }
        drop(released);
        for (id, from, to, restarting) in launches {
            self.launch(&id, from, to, restarting);
        }
        wake
    }

    /// Update phase from progress. Book lock held; `progress` must stay cheap.
    fn check_health(&self, id: &str, entry: &mut Entry<T>, now: Instant) {
        let progress = self.driver.progress(id, entry.running);
        if progress != entry.progress {
            entry.progress = progress;
            entry.progress_at = now;
            entry.phase = DemandPhase::Live;
            entry.detail = None;
            entry.failures = 0;
            return;
        }
        let silent = now.saturating_duration_since(entry.progress_at);
        match entry.phase {
            DemandPhase::Starting if silent >= self.config.start_timeout => {
                entry.phase = DemandPhase::Failed;
                entry.detail = Some("no media yet; still retrying".to_owned());
            }
            DemandPhase::Live if silent >= self.config.stall_after && !self.driver.finite(id) => {
                entry.phase = DemandPhase::Failed;
                entry.detail = Some("no media received recently; still retrying".to_owned());
            }
            _ => {}
        }
    }

    /// Run one transition on a transient thread. The caller holds a start slot
    /// for it, so the thread count is bounded.
    fn launch(self: &Arc<Self>, id: &Arc<str>, from: Rails, to: Rails, restarting: bool) {
        self.transitions.fetch_add(1, Ordering::Relaxed);
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
                    let outcome = driver.transition(&thread_id, from, to, requested);
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
                inner.finish(&thread_id, from, to, outcome);
            });
        if let Err(error) = spawned {
            tracing::warn!(source = %id, %error, "could not spawn a rail transition; retrying");
            let failure = RailFailure {
                running: from,
                reason: "could not start a worker thread".to_owned(),
            };
            self.finish(id, from, to, Err(failure));
        }
    }

    /// Record a transition's outcome and wake the worker.
    fn finish(
        self: &Arc<Self>,
        id: &str,
        from: Rails,
        to: Rails,
        outcome: Result<Rails, RailFailure>,
    ) {
        let running = outcome
            .as_ref()
            .map_or_else(|failure| failure.running, |running| *running);
        let started_video = running.video && !from.video;
        let handle = started_video
            .then(|| self.registry_handle(id, None))
            .flatten();
        let progress = self.driver.progress(id, running);
        let mut surplus = None;
        {
            let mut book = lock(&self.book);
            let now = Instant::now();
            if let Some(entry) = book.entries.get_mut(id) {
                entry.in_flight = false;
                if to.is_empty() {
                    entry.restart = false;
                }
                entry.running = running;
                if !running.video {
                    surplus = entry.handle.take();
                } else if handle.is_some() {
                    entry.handle = handle;
                }
                match outcome {
                    Ok(_) => {
                        entry.surplus_since = None;
                        entry.not_before = None;
                        if !from.covers(to) {
                            entry.phase = DemandPhase::Starting;
                            entry.detail = None;
                            entry.progress = progress;
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
                        entry.detail = Some(reason);
                        let delay = backoff(
                            self.config.backoff_base,
                            self.config.backoff_max,
                            entry.failures,
                        );
                        entry.not_before = Some(now + delay);
                    }
                }
            }
        }
        drop(surplus);
        self.wake();
    }
}

/// What a source's rails should do now.
enum Desired {
    /// Stay put, optionally until a deadline.
    Hold(Option<Instant>),
    /// Move to these rails.
    Move(Rails),
}

/// Added rails start at once. Surplus rails stop only after the linger, so a
/// new consumer arriving meanwhile finds them still running.
fn desired_rails<T>(
    entry: &mut Entry<T>,
    wanted: Rails,
    now: Instant,
    linger: Duration,
) -> Desired {
    if entry.running == wanted {
        entry.surplus_since = None;
        return Desired::Hold(None);
    }
    if entry.running.covers(wanted) {
        let since = *entry.surplus_since.get_or_insert(now);
        let deadline = since + linger;
        if now >= deadline {
            return Desired::Move(wanted);
        }
        return Desired::Hold(Some(deadline));
    }
    // Keep surplus rails through the add; the linger applies to them afterwards.
    Desired::Move(entry.running.union(wanted))
}

/// Capped exponential backoff.
fn backoff(base: Duration, max: Duration, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    base.saturating_mul(1_u32 << exponent).min(max)
}

fn worker_loop<T: Send + Sync + 'static>(inner: &Arc<Inner<T>>) {
    while !inner.stopping.load(Ordering::Acquire) {
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
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Some(hold) = self.hold {
                std::thread::sleep(hold);
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if to.video && !from.video {
                self.video_starts.fetch_add(1, Ordering::SeqCst);
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

    fn config(linger_ms: u64) -> DemandConfig {
        DemandConfig {
            linger: Duration::from_millis(linger_ms),
            max_concurrent_starts: 4,
            start_timeout: Duration::from_millis(100),
            stall_after: Duration::from_millis(100),
            backoff_base: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
            health_period: Duration::from_millis(5),
        }
    }

    fn manager(fake: &Arc<Fake>, linger_ms: u64) -> DemandManager<u8> {
        let driver: Arc<dyn RailDriver<u8>> = Arc::<Fake>::clone(fake);
        let manager = DemandManager::new(SourceRegistry::new(), driver, config(linger_ms));
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
    fn first_lease_starts_and_last_lease_stops_only_after_the_linger() {
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
            "stopped before the linger"
        );
        wait("record removed", || manager.active_len() == 0);
        assert_eq!(fake.video_starts.load(Ordering::SeqCst), 1, "one decode");
        manager.shutdown();
    }

    #[test]
    fn a_consumer_returning_inside_the_linger_does_not_restart() {
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
    fn progress_makes_a_source_live_and_a_stall_fails_it() {
        let fake = Arc::new(Fake::default());
        let manager = manager(&fake, 10);
        let lease = manager.acquire("cam", Rails::VIDEO, tag("a"), None);
        wait("start", || fake.video_starts.load(Ordering::SeqCst) == 1);
        assert_eq!(manager.status("cam").phase, DemandPhase::Starting);
        wait("live", || {
            fake.progress.fetch_add(1, Ordering::SeqCst);
            manager.status("cam").phase == DemandPhase::Live
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
}
