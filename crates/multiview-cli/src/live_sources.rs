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

/// The per-source stop flags of every running producer, keyed by source id.
///
/// Registered at spawn (startup supervisors and the hub alike) and consumed by
/// the hub on teardown. Touched only **off** the output-clock thread: at spawn
/// time (before/around the run), on the hub worker, and at run teardown.
pub type StopRegistry = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Create an empty [`StopRegistry`].
#[must_use]
pub fn stop_registry() -> StopRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Register `id`'s producer stop flag (replacing any stale entry).
///
/// A poisoned registry (a panicked writer — never expected) is surfaced as a
/// warning rather than a panic: the producer still runs, it just cannot be
/// torn down individually.
pub fn register_stop(registry: &StopRegistry, id: &str, flag: &Arc<AtomicBool>) {
    match registry.lock() {
        Ok(mut map) => {
            map.insert(id.to_owned(), Arc::clone(flag));
        }
        Err(poisoned) => {
            tracing::warn!(
                source = %id,
                "stop registry poisoned; the producer cannot be torn down individually"
            );
            drop(poisoned);
        }
    }
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

/// A request travelling from the frame-boundary drain to the hub worker.
enum HubRequest {
    /// Spawn (or replace — a live **edit**) the producer for a synthetic source.
    SpawnSynth(SynthSpawn),
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
    /// flags, also fed by the startup supervisors) and `preview` store map.
    pub fn start(registry: StopRegistry, preview: SharedStores) -> Self {
        let (tx, rx) = sync_channel(HUB_QUEUE_DEPTH);
        let builder = std::thread::Builder::new().name("multiview-live-sources".to_owned());
        let worker = match builder.spawn(move || worker_loop(&rx, &registry, &preview)) {
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
fn worker_loop(rx: &Receiver<HubRequest>, registry: &StopRegistry, preview: &SharedStores) {
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
    register_stop(registry, &id, &stop);
    preview_insert(preview, &id, &store);
    let thread_stop = Arc::clone(&stop);
    let thread_store = Arc::clone(&store);
    let builder = std::thread::Builder::new().name(format!("multiview-synth-live-{id}"));
    match builder.spawn(move || {
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
    let registered: Vec<Arc<AtomicBool>> = match registry.lock() {
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
    for flag in registered {
        flag.store(true, Ordering::Release);
    }
    if let Some(producer) = owned.remove(id) {
        producer.stop.store(true, Ordering::Release);
        join_bounded(id, producer.handle);
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
        register_stop(&registry, "src1", &video);
        register_stop(&registry, "src1/captions", &captions);
        register_stop(&registry, "src10", &unrelated);
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
