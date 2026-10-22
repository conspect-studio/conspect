//! Live source producer hub (ADR-W018): the off-thread owner of every
//! **runtime** source producer spawn/teardown.
//!
//! The engine command drain applies `UpsertSource`/`RemoveSource` at the frame
//! boundary, but the heavy halves of those changes — spawning a producer
//! thread, joining one on teardown, mutating the preview registry — must never
//! run on the output-clock thread (invariant #1). The drain therefore only
//! mutates the compositor's bindings and `try_send`s a request to this hub
//! over a **bounded** channel (full ⇒ drop + warn, never block — invariant
//! #10); the hub's single worker thread does the rest.
//!
//! Uniform producer path: a hub-spawned synthetic producer runs the **same**
//! [`generator_loop`](crate::synth::generator_loop) the startup path runs.
//! Teardown is per-source: every producer (startup *or* live-spawned) registers
//! its own stop flag in the shared [`StopRegistry`], so a live remove can stop
//! exactly one producer — a startup generator, a startup ingest thread (the
//! ffmpeg path registers its per-plan flags too), or a hub-owned thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::time::Rational;
use multiview_framestore::TileStore;

use crate::synth::SyntheticKind;

/// One registered producer's teardown handles: the cooperative **stop** flag
/// the teardown raises, and the **exited** latch the producer thread flips when
/// it actually leaves (via [`ExitGuard`], so a normal return AND a panic both
/// set it). Teardown raises `stop` then bounded-waits on `exited` so a
/// replacement producer never publishes into a reused single-writer store while
/// the old thread is still writing it (ADR-T002 single-writer; the two-writer
/// race ADR-W018 §5 closes).
#[derive(Clone)]
pub struct ProducerStop {
    /// Raise to request the producer stop. `pub(crate)` so cross-module callers
    /// (the ingest-supervisor registration tests in pipeline.rs) can raise the flag
    /// they looked up from the registry; teardown in this module raises it directly.
    pub(crate) stop: Arc<AtomicBool>,
    /// Set by the producer thread on exit (return or panic) via [`ExitGuard`].
    exited: Arc<AtomicBool>,
}

/// The per-source producer teardown handles, keyed by source id.
///
/// Registered at spawn (startup supervisors and the hub alike) and consumed by
/// the hub on teardown. Touched only **off** the output-clock thread: at spawn
/// time (before/around the run), on the hub worker, and at run teardown.
pub type StopRegistry = Arc<Mutex<HashMap<String, ProducerStop>>>;

/// Create an empty [`StopRegistry`].
#[must_use]
pub fn stop_registry() -> StopRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// An RAII latch a producer thread holds for its whole lifetime: its [`Drop`]
/// flips the registered `exited` flag, so teardown's bounded-wait observes the
/// thread leaving on **either** a normal return or a panic-unwind. Carry it
/// into the thread closure (e.g. `let _exit = ExitGuard::new(&exited);`).
pub struct ExitGuard {
    exited: Arc<AtomicBool>,
}

impl ExitGuard {
    /// Track exit through `exited` (the latch [`register_stop`] returned).
    #[must_use]
    pub fn new(exited: &Arc<AtomicBool>) -> Self {
        Self {
            exited: Arc::clone(exited),
        }
    }
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.exited.store(true, Ordering::Release);
    }
}

/// Register `id`'s producer stop flag (replacing any stale entry) and return
/// the **exited** latch the producer thread must flip on exit — carry it into
/// the thread as an [`ExitGuard`]. Teardown bounded-waits on this latch so a
/// replacement never races the old writer on a reused store.
///
/// A poisoned registry (a panicked writer — never expected) is surfaced as a
/// warning rather than a panic: the producer still runs, it just cannot be
/// torn down individually (the returned latch is then unobserved — harmless).
#[must_use = "carry the returned latch into the producer thread as an ExitGuard so teardown can wait for its exit"]
pub fn register_stop(registry: &StopRegistry, id: &str, flag: &Arc<AtomicBool>) -> Arc<AtomicBool> {
    let exited = Arc::new(AtomicBool::new(false));
    match registry.lock() {
        Ok(mut map) => {
            map.insert(
                id.to_owned(),
                ProducerStop {
                    stop: Arc::clone(flag),
                    exited: Arc::clone(&exited),
                },
            );
        }
        Err(poisoned) => {
            tracing::warn!(
                source = %id,
                "stop registry poisoned; the producer cannot be torn down individually"
            );
            drop(poisoned);
        }
    }
    exited
}

/// The shared, live-updatable preview store map (`id → TileStore`) the
/// [`CliPreviewProvider`](crate::preview::CliPreviewProvider) reads.
///
/// An `ArcSwap` over an immutable map: readers (`input_jpeg`/`input_ids`, on
/// request tasks) take a wait-free snapshot; the hub worker RCUs a clone on
/// add/remove (rare, operator-paced — never on the output-clock thread).
pub type SharedStores = Arc<ArcSwap<HashMap<String, Arc<TileStore<Nv12Image>>>>>;

/// Wrap an initial preview store map (the startup sources) as [`SharedStores`].
#[must_use]
#[allow(clippy::implicit_hasher)]
// reason: the map is immediately stored inside the concrete `SharedStores`
// alias (an `ArcSwap` over the default-hasher `HashMap`), so generalizing the
// parameter's hasher cannot be honoured and would only mislead callers.
pub fn shared_stores(initial: HashMap<String, Arc<TileStore<Nv12Image>>>) -> SharedStores {
    Arc::new(ArcSwap::from_pointee(initial))
}

/// Everything the hub needs to spawn a **synthetic** producer (ADR-0027): the
/// resolved kind plus the store and render geometry/cadence. The drain builds
/// this at the frame boundary (cheap — the store is created or reused there);
/// the hub does the spawn.
pub struct SynthSpawn {
    /// The source id (store key, registry key, preview key).
    pub id: String,
    /// The resolved synthetic kind to render.
    pub kind: SyntheticKind,
    /// The per-source last-good store the generator publishes into (already
    /// registered with the compositor drive by the drain).
    pub store: Arc<TileStore<Nv12Image>>,
    /// Render width (the canvas width — the compositor scales to the tile).
    pub width: u32,
    /// Render height (the canvas height).
    pub height: u32,
    /// The canvas colour space the frames are tagged in.
    pub canvas: CanvasColor,
    /// The output cadence the generator paces its publishes to.
    pub cadence: Rational,
}

/// Everything the hub needs to spawn a **decoded** (network/file) producer
/// (ADR-W018 level 2): the validated source document and the store the drain
/// registered. The wired [`IngestSpawner`] derives the rest (tile geometry,
/// canvas colour, cadence, decode placement) from the running pipeline —
/// exactly the startup construction.
pub struct SourceSpawn {
    /// The validated source document (a network/file kind).
    pub source: multiview_config::Source,
    /// The per-source last-good store the ingest thread publishes into
    /// (already registered with the compositor drive by the drain; reused on
    /// an edit so the tile holds last-good through the producer swap).
    pub store: Arc<TileStore<Nv12Image>>,
}

/// One spawned producer thread: its per-source stop flag (already registered
/// in the run's [`StopRegistry`] by the spawner) and the join handle the hub
/// owns for bounded teardown.
pub struct SpawnedProducer {
    /// The producer's stop flag (raise to request a cooperative stop).
    pub stop: Arc<AtomicBool>,
    /// The producer thread's join handle.
    pub handle: JoinHandle<()>,
}

/// The run-path seam that turns a [`SourceSpawn`] into a running **decoded**
/// producer thread (ADR-W018 level 2).
///
/// The full-pipeline run wires the pipeline's spawner
/// (`Pipeline::live_ingest_spawner`), which builds the plan with the **same**
/// `ingest_plan_for` construction, consults the **same** GPU admission path
/// startup decode placement uses (pinned to the running island's device), and
/// spawns the **same** supervised `ingest_loop` — one uniform ingest path. The
/// software run wires none (`None`), so a decoded spawn request is held with a
/// warning and the tile rides the slate (the route's capability signal already
/// answered `restart` for those kinds there).
///
/// Implementations run on the hub worker thread only — heavy/blocking work
/// (placement polling, thread spawn) is allowed; the output clock never waits
/// on it (invariant #1).
pub trait IngestSpawner: Send + Sync {
    /// Build the ingest plan and spawn the supervised producer thread,
    /// registering its stop flag under the source id in `registry`. Returns
    /// `None` when the spawn failed (logged by the implementation; the tile
    /// rides the slate honestly).
    fn spawn(&self, spawn: SourceSpawn, registry: &StopRegistry) -> Option<SpawnedProducer>;
}

/// A request travelling from the frame-boundary drain to the hub worker.
enum HubRequest {
    /// Spawn (or replace — a live **edit**) the producer for a synthetic source.
    SpawnSynth(SynthSpawn),
    /// Spawn (or replace — a live **edit**) the supervised ingest producer for
    /// a decoded network/file source (ADR-W018 level 2). Boxed: the spawn
    /// carries a full config `Source`, much larger than the other variants.
    SpawnSource(Box<SourceSpawn>),
    /// Tear down the producer for `id` (live remove): raise its registry stop
    /// flag, join a hub-owned thread (bounded), drop the preview entry.
    Teardown {
        /// The source id whose producer goes away.
        id: String,
    },
    /// Stop the worker (run over): tear down + join every owned producer and
    /// exit. Sent by [`LiveSourceHub::shutdown`] — an explicit request because
    /// drain closures may still hold live [`LiveSourceHandle`] sender clones,
    /// so waiting for the channel to close could wait forever.
    Shutdown,
}

/// Depth of the drain→hub request channel. Operator actions are sparse; a
/// burst beyond this sheds the request (warned) rather than growing memory or
/// blocking the frame boundary (invariant #10 / safety rule §5).
const HUB_QUEUE_DEPTH: usize = 32;

/// How long the hub waits for a torn-down producer thread to observe its stop
/// flag before detaching it. A generator observes its flag within ≤25 ms
/// (`synth::sleep_until` chunks); the margin covers a heavily loaded host.
const TEARDOWN_JOIN_GRACE: Duration = Duration::from_secs(3);

/// The outcome of a non-blocking hub submission: the two shed cases are
/// distinct because the operator action differs — a [`HubSubmit::Full`] queue
/// recovers by re-applying, a [`HubSubmit::Gone`] worker means live apply is
/// disabled until restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubSubmit {
    /// The request is queued; the worker will apply it.
    Accepted,
    /// The bounded queue is full; the request was shed (re-apply retries).
    Full,
    /// The hub worker is gone (never spawned, panicked, or shut down); live
    /// producer apply is unavailable until restart.
    Gone,
}

/// The cloneable handle the command drain holds: non-blocking submission of
/// spawn/teardown requests to the hub worker.
#[derive(Clone)]
pub struct LiveSourceHandle {
    tx: SyncSender<HubRequest>,
}

impl LiveSourceHandle {
    /// Request a synthetic producer spawn (or replacement). Never blocks; a
    /// shed request is reported as [`HubSubmit::Full`]/[`HubSubmit::Gone`]
    /// (the tile rides the slate).
    #[must_use]
    pub fn request_spawn_synth(&self, spawn: SynthSpawn) -> HubSubmit {
        Self::submit_outcome(&self.tx.try_send(HubRequest::SpawnSynth(spawn)))
    }

    /// Request a decoded (network/file) producer spawn or replacement
    /// (ADR-W018 level 2). Never blocks; a shed request is reported as
    /// [`HubSubmit::Full`]/[`HubSubmit::Gone`] (the tile rides the slate).
    #[must_use]
    pub fn request_spawn_source(&self, spawn: SourceSpawn) -> HubSubmit {
        let request = HubRequest::SpawnSource(Box::new(spawn));
        Self::submit_outcome(&self.tx.try_send(request))
    }

    /// Request a producer teardown for `id` (raising the id's stop flag AND
    /// every `{id}/`-prefixed companion flag, e.g. the caption reader's).
    /// Never blocks; a shed request is reported as
    /// [`HubSubmit::Full`]/[`HubSubmit::Gone`].
    #[must_use]
    pub fn request_teardown(&self, id: &str) -> HubSubmit {
        let request = HubRequest::Teardown { id: id.to_owned() };
        Self::submit_outcome(&self.tx.try_send(request))
    }

    /// Map a `try_send` result onto the [`HubSubmit`] outcome.
    fn submit_outcome(result: &Result<(), TrySendError<HubRequest>>) -> HubSubmit {
        match result {
            Ok(()) => HubSubmit::Accepted,
            Err(TrySendError::Full(_)) => HubSubmit::Full,
            Err(TrySendError::Disconnected(_)) => HubSubmit::Gone,
        }
    }
}

/// The hub: one worker thread owning runtime producer threads and the preview
/// registry mutations. Construct with [`LiveSourceHub::start`]; hand
/// [`LiveSourceHub::handle`]s to the command drain; call
/// [`LiveSourceHub::shutdown`] after the run loop returns.
#[must_use = "the hub owns producer threads; drop without shutdown leaks them"]
pub struct LiveSourceHub {
    tx: SyncSender<HubRequest>,
    worker: Option<JoinHandle<()>>,
}

impl LiveSourceHub {
    /// Start the hub worker over the run's shared `registry` (per-source stop
    /// flags, also fed by the startup supervisors) and `preview` store map,
    /// with **no** decoded-ingest spawner (the software run path): synthetic
    /// spawns work; a decoded spawn request is held with a warning.
    pub fn start(registry: StopRegistry, preview: SharedStores) -> Self {
        Self::start_with_ingest(registry, preview, None)
    }

    /// Start the hub worker, optionally wiring the run's decoded-ingest
    /// spawner (ADR-W018 level 2; the full-pipeline run passes
    /// `Pipeline::live_ingest_spawner`). The capability the binary declares to
    /// the control plane must mirror `ingest.is_some()` — the header claims
    /// `live` for network kinds only when this seam can actually spawn them.
    pub fn start_with_ingest(
        registry: StopRegistry,
        preview: SharedStores,
        ingest: Option<Arc<dyn IngestSpawner>>,
    ) -> Self {
        let (tx, rx) = sync_channel(HUB_QUEUE_DEPTH);
        let builder = std::thread::Builder::new().name("multiview-live-sources".to_owned());
        let worker =
            match builder.spawn(move || worker_loop(&rx, &registry, &preview, ingest.as_deref())) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    // Without a worker every request is shed (the channel fills) and
                    // live adds degrade to slate tiles — logged loudly, never a panic.
                    tracing::error!(error = %e, "could not spawn the live-source hub worker");
                    None
                }
            };
        Self { tx, worker }
    }

    /// A cloneable, non-blocking submission handle for the command drain.
    #[must_use]
    pub fn handle(&self) -> LiveSourceHandle {
        LiveSourceHandle {
            tx: self.tx.clone(),
        }
    }

    /// Stop the worker (it tears down + joins every hub-owned producer) and
    /// join it. Call after the engine run loop returns.
    ///
    /// Sends an explicit [`HubRequest::Shutdown`] (a blocking send is fine
    /// here — this is run teardown, not the clock thread, and the worker keeps
    /// draining) rather than relying on channel close: a drain closure may
    /// still hold a live [`LiveSourceHandle`] sender clone.
    pub fn shutdown(self) {
        if self.tx.send(HubRequest::Shutdown).is_err() {
            // The worker is already gone (it never spawned, or panicked);
            // nothing to join but the handle below.
            tracing::debug!("live-source hub worker already stopped at shutdown");
        }
        drop(self.tx);
        if let Some(worker) = self.worker {
            if worker.join().is_err() {
                tracing::error!("the live-source hub worker panicked");
            }
        }
    }
}

/// One hub-owned producer thread: its stop flag and join handle.
struct Producer {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// The hub worker: serially apply spawn/teardown requests until the channel
/// closes, then tear down every owned producer.
fn worker_loop(
    rx: &Receiver<HubRequest>,
    registry: &StopRegistry,
    preview: &SharedStores,
    ingest: Option<&dyn IngestSpawner>,
) {
    let mut owned: HashMap<String, Producer> = HashMap::new();
    while let Ok(request) = rx.recv() {
        match request {
            HubRequest::SpawnSynth(spawn) => {
                // An upsert under a running id is an EDIT: stop the old
                // producer first, then spawn the replacement into the SAME
                // store (the drain reused it, so the tile holds last-good).
                teardown(&spawn.id, &mut owned, registry);
                spawn_synth(spawn, &mut owned, registry, preview);
            }
            HubRequest::SpawnSource(spawn) => {
                // The decoded-kind edit mirror of SpawnSynth: stop the old
                // producer first, then spawn the replacement ingest thread
                // into the SAME reused store (the tile holds last-good).
                teardown(&spawn.source.id, &mut owned, registry);
                spawn_decoded(*spawn, ingest, &mut owned, registry, preview);
            }
            HubRequest::Teardown { id } => {
                teardown(&id, &mut owned, registry);
                preview_remove(preview, &id);
            }
            HubRequest::Shutdown => break,
        }
    }
    // Shutdown requested (or every sender gone): stop + join everything we own.
    for (id, producer) in owned {
        producer.stop.store(true, Ordering::Release);
        join_bounded(&id, producer.handle);
    }
}

/// Spawn one decoded (network/file) producer through the run's
/// [`IngestSpawner`] (ADR-W018 level 2): the spawner builds the plan with the
/// SAME `ingest_plan_for` construction, consults the SAME admission path
/// startup decode placement uses, and runs the SAME supervised `ingest_loop`.
/// With no spawner wired (the software run) the request is held with a
/// warning — the tile rides the slate and the stored document applies on
/// restart, exactly what the route's capability-driven header declared.
fn spawn_decoded(
    spawn: SourceSpawn,
    ingest: Option<&dyn IngestSpawner>,
    owned: &mut HashMap<String, Producer>,
    registry: &StopRegistry,
    preview: &SharedStores,
) {
    let id = spawn.source.id.clone();
    let Some(spawner) = ingest else {
        tracing::warn!(
            source = %id,
            "no live ingest spawner on this run path (software engine): the tile \
             rides the slate; the stored document applies on restart"
        );
        preview_insert(preview, &id, &spawn.store);
        return;
    };
    preview_insert(preview, &id, &spawn.store);
    match spawner.spawn(spawn, registry) {
        Some(SpawnedProducer { stop, handle }) => {
            owned.insert(id, Producer { stop, handle });
        }
        None => {
            // The spawner logged the cause; the tile rides the slate and a
            // re-apply retries. The registration stays consistent (store
            // registered, no thread — teardown is then a no-op join).
            tracing::warn!(source = %id, "live ingest producer spawn failed; the tile rides the slate");
        }
    }
}

/// Spawn one synthetic producer thread (the SAME `generator_loop` the startup
/// path runs), register its stop flag, and publish the store to the preview
/// registry.
fn spawn_synth(
    spawn: SynthSpawn,
    owned: &mut HashMap<String, Producer>,
    registry: &StopRegistry,
    preview: &SharedStores,
) {
    let SynthSpawn {
        id,
        kind,
        store,
        width,
        height,
        canvas,
        cadence,
    } = spawn;
    if kind.animated() && !cfg!(feature = "overlay") {
        // The clock renders via the overlay rasterizer; without it the
        // generator could publish nothing. Register nothing and say why — the
        // tile rides the slate honestly (and works after a restart of an
        // overlay-enabled build).
        tracing::warn!(
            source = %id,
            "live clock source needs the `overlay` feature to render; the tile rides the slate"
        );
        preview_insert(preview, &id, &store);
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let exited = register_stop(registry, &id, &stop);
    // ExitGuard built BEFORE spawn: its Drop flips `exited` on thread exit OR if
    // Builder::spawn fails (the dropped closure drops the guard) — so a failed spawn
    // never orphans an exited=false entry teardown would busy-wait on (ADR-W018 §5).
    let exit_guard = ExitGuard::new(&exited);
    preview_insert(preview, &id, &store);
    let thread_stop = Arc::clone(&stop);
    let thread_store = Arc::clone(&store);
    let builder = std::thread::Builder::new().name(format!("multiview-synth-live-{id}"));
    match builder.spawn(move || {
        let _exit = exit_guard;
        crate::synth::generator_loop(
            kind,
            &thread_store,
            width,
            height,
            canvas,
            cadence,
            &thread_stop,
        );
    }) {
        Ok(handle) => {
            owned.insert(id, Producer { stop, handle });
        }
        Err(e) => {
            // The tile rides the slate; the registration stays consistent
            // (flag registered but no thread — teardown is then a no-op join).
            tracing::error!(error = %e, source = %id, "could not spawn live synthetic producer");
        }
    }
}

/// Stop `id`'s producers: raise the id's registry flag AND every
/// `{id}/`-prefixed companion flag (e.g. the `{id}/captions` reader — a source
/// teardown must stop ALL of that source's producers, never leave a caption
/// reader decoding a stale URL over the replacement picture), covering
/// startup-spawned producers too; when the hub owns the thread, join it
/// bounded. An unrelated id merely sharing leading characters ("src10" vs
/// "src1") is never touched — the companion separator is `/`.
fn teardown(id: &str, owned: &mut HashMap<String, Producer>, registry: &StopRegistry) {
    let prefix = format!("{id}/");
    let registered: Vec<ProducerStop> = match registry.lock() {
        Ok(mut map) => {
            let keys: Vec<String> = map
                .keys()
                .filter(|key| *key == id || key.starts_with(&prefix))
                .cloned()
                .collect();
            keys.iter().filter_map(|key| map.remove(key)).collect()
        }
        Err(poisoned) => {
            tracing::warn!(source = %id, "stop registry poisoned during teardown");
            drop(poisoned);
            Vec::new()
        }
    };
    // Raise every matched stop flag first (the id + every `{id}/` companion).
    for producer in &registered {
        producer.stop.store(true, Ordering::Release);
    }
    // Definitively join the hub-owned producer when we own its handle.
    if let Some(producer) = owned.remove(id) {
        producer.stop.store(true, Ordering::Release);
        join_bounded(id, producer.handle);
    }
    // Bounded-WAIT until every torn-down producer has actually EXITED — not
    // just been asked to stop. A STARTUP-origin producer's `JoinHandle` lives
    // in `IngestSupervisor`/`GeneratorSupervisor`, not in `owned`, so we cannot
    // join it; but its thread flips the registered `exited` latch (via
    // `ExitGuard`) on the way out, and we block on that. This is the fix for
    // the two-writer race on a REUSED single-writer `TileStore` (ADR-W018 §5 /
    // ADR-T002): the hub's caller spawns the replacement producer into the same
    // store only after this returns, so the old writer is gone first. Bounded
    // by `TEARDOWN_JOIN_GRACE`; a producer that overruns is detached (warned) —
    // the engine never blocks on the clock thread (the hub worker is off it).
    await_exits(id, &registered);
}

/// Bounded-wait until every producer's `exited` latch is set (it left), so a
/// replacement never races a still-writing predecessor on a reused store.
fn await_exits(id: &str, producers: &[ProducerStop]) {
    let deadline = Instant::now() + TEARDOWN_JOIN_GRACE;
    for producer in producers {
        while !producer.exited.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !producer.exited.load(Ordering::Acquire) {
            tracing::warn!(
                source = %id,
                "a torn-down producer did not exit within the grace window; \
                 detaching (a replacement may briefly interleave on the reused store)"
            );
        }
    }
}

/// Join a producer thread within [`TEARDOWN_JOIN_GRACE`], detaching (with a
/// warning) one that does not finish — it only ever writes a lock-free store
/// it shares by `Arc`, so detaching cannot corrupt output (the same policy as
/// the ingest supervisor's bounded join).
fn join_bounded(id: &str, handle: JoinHandle<()>) {
    let deadline = Instant::now() + TEARDOWN_JOIN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !handle.is_finished() {
        tracing::warn!(source = %id, "live producer did not stop within grace; detaching");
        return;
    }
    if handle.join().is_err() {
        tracing::error!(source = %id, "live producer thread panicked");
    }
}

/// RCU-insert `id → store` into the shared preview map.
fn preview_insert(preview: &SharedStores, id: &str, store: &Arc<TileStore<Nv12Image>>) {
    preview.rcu(|map| {
        let mut next = HashMap::clone(map);
        next.insert(id.to_owned(), Arc::clone(store));
        next
    });
}

/// RCU-remove `id` from the shared preview map.
fn preview_remove(preview: &SharedStores, id: &str) {
    preview.rcu(|map| {
        let mut next = HashMap::clone(map);
        next.remove(id);
        next
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::time::Duration;

    fn wait_for(deadline: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        predicate()
    }

    #[test]
    fn spawn_primes_the_store_and_teardown_stops_the_producer() {
        let registry = stop_registry();
        let preview = shared_stores(HashMap::new());
        let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults("s1"));

        assert!(matches!(
            hub.handle().request_spawn_synth(SynthSpawn {
                id: "s1".to_owned(),
                kind: SyntheticKind::Bars,
                store: Arc::clone(&store),
                width: 64,
                height: 64,
                canvas: CanvasColor::default(),
                cadence: Rational::new(25, 1),
            }),
            HubSubmit::Accepted
        ));
        assert!(
            wait_for(Duration::from_secs(5), || store.is_primed()),
            "the spawned generator publishes into the store"
        );
        assert!(wait_for(Duration::from_secs(5), || preview
            .load()
            .contains_key("s1")));

        assert!(matches!(
            hub.handle().request_teardown("s1"),
            HubSubmit::Accepted
        ));
        assert!(
            wait_for(Duration::from_secs(5), || !preview
                .load()
                .contains_key("s1")),
            "teardown drops the preview entry"
        );
        assert!(
            wait_for(Duration::from_secs(5), || {
                let seq = store.sequence();
                std::thread::sleep(Duration::from_millis(120));
                store.sequence() == seq
            }),
            "teardown stops the producer's publishes"
        );
        hub.shutdown();
    }

    #[test]
    fn teardown_raises_the_id_and_every_prefixed_companion_flag() {
        // A source's companion producers (e.g. its caption reader) register
        // under `{id}/<role>` keys; tearing down `id` must raise the id's flag
        // AND every `{id}/`-prefixed flag — but never an unrelated id that
        // merely starts with the same characters ("src10" vs "src1").
        let registry = stop_registry();
        let preview = shared_stores(HashMap::new());
        let video = Arc::new(AtomicBool::new(false));
        let captions = Arc::new(AtomicBool::new(false));
        let unrelated = Arc::new(AtomicBool::new(false));
        // These registrations stand in for startup producers (no real thread):
        // pre-flip their `exited` latches so teardown's bounded exit-wait
        // proceeds at once (a real producer's `ExitGuard` flips it on exit).
        let video_exited = register_stop(&registry, "src1", &video);
        let captions_exited = register_stop(&registry, "src1/captions", &captions);
        let _unrelated_exited = register_stop(&registry, "src10", &unrelated);
        video_exited.store(true, Ordering::Release);
        captions_exited.store(true, Ordering::Release);
        let hub = LiveSourceHub::start(Arc::clone(&registry), preview);

        assert!(matches!(
            hub.handle().request_teardown("src1"),
            HubSubmit::Accepted
        ));
        assert!(
            wait_for(Duration::from_secs(5), || {
                video.load(Ordering::Acquire) && captions.load(Ordering::Acquire)
            }),
            "teardown must raise the source flag AND its /captions companion"
        );
        assert!(
            !unrelated.load(Ordering::Acquire),
            "an unrelated id sharing a prefix must NOT be torn down"
        );
        assert!(
            wait_for(Duration::from_secs(5), || {
                registry.lock().is_ok_and(|map| {
                    !map.contains_key("src1")
                        && !map.contains_key("src1/captions")
                        && map.contains_key("src10")
                })
            }),
            "teardown deregisters the id + companions, keeps unrelated ids"
        );
        hub.shutdown();
    }

    #[test]
    fn handle_distinguishes_a_gone_hub_from_a_full_queue() {
        let registry = stop_registry();
        let preview = shared_stores(HashMap::new());
        let hub = LiveSourceHub::start(registry, preview);
        let handle = hub.handle();
        assert!(matches!(handle.request_teardown("x"), HubSubmit::Accepted));
        hub.shutdown();
        // The worker is gone: a held handle must report Gone (live apply is
        // disabled until restart), not pretend the queue is merely full.
        assert!(
            wait_for(Duration::from_secs(5), || {
                matches!(handle.request_teardown("x"), HubSubmit::Gone)
            }),
            "a shut-down hub must report Gone to held handles"
        );
    }

    #[test]
    fn shutdown_tears_down_owned_producers() {
        let registry = stop_registry();
        let preview = shared_stores(HashMap::new());
        let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults("s1"));
        assert!(matches!(
            hub.handle().request_spawn_synth(SynthSpawn {
                id: "s1".to_owned(),
                kind: SyntheticKind::Bars,
                store: Arc::clone(&store),
                width: 64,
                height: 64,
                canvas: CanvasColor::default(),
                cadence: Rational::new(25, 1),
            }),
            HubSubmit::Accepted
        ));
        assert!(wait_for(Duration::from_secs(5), || store.is_primed()));
        // Shutdown joins the producer; afterwards no more frames are published.
        hub.shutdown();
        let seq = store.sequence();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(store.sequence(), seq, "shutdown stops every owned producer");
    }
}
