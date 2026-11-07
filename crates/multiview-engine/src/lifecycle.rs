//! **Source owner hooks** for source kinds whose start is slow or external
//! (later: live web sources). See `docs/architecture/demand-driven-sources.md`.
//!
//! A [`SourceOwner`] starts and stops one source kind with async
//! `start(source, generation, deadline)` / `stop(source, generation)`.
//! [`OwnerRails`] adapts an owner to the [`RailDriver`] the
//! [`DemandManager`](crate::DemandManager) drives, so an owner-backed kind gets
//! the same leases, `always_on` / `on_demand` activation, start timeout, close
//! after, retry cap, Start / Stop and [`TransitionEvent`](crate::TransitionEvent)s
//! as every other source.
//!
//! - Owner calls run on the manager's transient transition threads, through a
//!   tokio runtime handle. Never on the output clock or a data-plane thread.
//! - A start that misses its deadline is cancelled (its future dropped) and
//!   the source reads `failed`.
//! - On a successful start the owner's [`ResourceScope`] is registered in the
//!   [`ScopeRegistry`] under `{source_id, generation, kind}`. It is removed
//!   only after the owner confirms the stop. A stop that does not confirm
//!   within its bound goes to [`SourceOwner::kill`]; if that fails too the
//!   scope stays registered and the source reads `failed`.
//! - The generation fences late work: [`ScopeRegistry::is_current`] is false
//!   for any sample or callback from an older start.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use multiview_framestore::TileStore;

use crate::demand::{KindBounds, RailDriver, RailFailure, Rails};
use crate::registry::RequestedSize;

/// A boxed owner future.
pub type OwnerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// What an owner's started resources live in, for accounting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ScopeKind {
    /// A process and its children.
    ProcessTree,
    /// A cgroup.
    Cgroup,
    /// Threads inside this process.
    Threads,
}

/// One registered resource scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResourceScope {
    /// The source id.
    pub source_id: String,
    /// The start generation the scope belongs to.
    pub generation: u64,
    /// What the scope is.
    pub kind: ScopeKind,
}

/// The live resource scopes, one per started owner-backed source.
#[derive(Debug, Default)]
pub struct ScopeRegistry {
    scopes: Mutex<HashMap<String, ResourceScope>>,
}

impl ScopeRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ResourceScope>> {
        self.scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&self, scope: ResourceScope) {
        self.lock().insert(scope.source_id.clone(), scope);
    }

    /// Remove `source_id`'s scope if it is still `generation`'s.
    fn remove(&self, source_id: &str, generation: u64) {
        let mut scopes = self.lock();
        if scopes
            .get(source_id)
            .is_some_and(|scope| scope.generation == generation)
        {
            scopes.remove(source_id);
        }
    }

    /// The scope registered for `source_id`.
    #[must_use]
    pub fn get(&self, source_id: &str) -> Option<ResourceScope> {
        self.lock().get(source_id).cloned()
    }

    /// Whether `generation` is `source_id`'s current start. Late samples and
    /// callbacks from an older start must be dropped when this is false.
    #[must_use]
    pub fn is_current(&self, source_id: &str, generation: u64) -> bool {
        self.lock()
            .get(source_id)
            .is_some_and(|scope| scope.generation == generation)
    }

    /// How many scopes are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no scope is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Starts and stops one kind of source. Implemented by a later source kind's
/// owner (a web engine), and by a fake in tests.
pub trait SourceOwner: Send + Sync + 'static {
    /// Start `source` for start `generation`; must be producing by `deadline`
    /// (the manager cancels the future at the deadline). Returns the started
    /// resources' scope kind.
    fn start(&self, source: &str, generation: u64, deadline: Instant)
        -> OwnerFuture<'_, ScopeKind>;

    /// Stop `source`'s start `generation` and confirm its teardown.
    fn stop(&self, source: &str, generation: u64) -> OwnerFuture<'_, ()>;

    /// Force-stop a wedged `source` after `stop` missed its bound. The default
    /// reports that there is no kill path.
    fn kill(&self, source: &str, generation: u64) -> OwnerFuture<'_, ()> {
        let _ = (source, generation);
        Box::pin(async { Err("no kill path".to_owned()) })
    }

    /// A counter that advances while `source` produces. Wait-free.
    fn progress(&self, source: &str) -> u64;

    /// Lifecycle bounds for this kind. Wait-free.
    fn bounds(&self) -> KindBounds {
        KindBounds::default()
    }
}

/// How long [`SourceOwner::stop`] may take before the kill path.
pub const DEFAULT_STOP_BOUND: Duration = Duration::from_secs(10);

/// A [`RailDriver`] over a [`SourceOwner`]. One rail per source: any wanted
/// rail starts the owner.
pub struct OwnerRails<O: SourceOwner, T> {
    owner: Arc<O>,
    runtime: tokio::runtime::Handle,
    scopes: Arc<ScopeRegistry>,
    stores: Mutex<HashMap<String, Arc<TileStore<T>>>>,
    /// The generation each running source was started with.
    started: Mutex<HashMap<String, u64>>,
    stop_bound: Duration,
}

impl<O: SourceOwner, T> std::fmt::Debug for OwnerRails<O, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerRails").finish_non_exhaustive()
    }
}

impl<O: SourceOwner, T: Send + Sync + 'static> OwnerRails<O, T> {
    /// Drive `owner` on `runtime`, registering scopes in `scopes`.
    #[must_use]
    pub fn new(owner: Arc<O>, runtime: tokio::runtime::Handle, scopes: Arc<ScopeRegistry>) -> Self {
        Self {
            owner,
            runtime,
            scopes,
            stores: Mutex::new(HashMap::new()),
            started: Mutex::new(HashMap::new()),
            stop_bound: DEFAULT_STOP_BOUND,
        }
    }

    /// Override how long a stop may take before the kill path.
    #[must_use]
    pub fn with_stop_bound(mut self, bound: Duration) -> Self {
        self.stop_bound = bound;
        self
    }

    /// Make `id` a known source with `store`.
    pub fn add_source(&self, id: &str, store: Arc<TileStore<T>>) {
        lock(&self.stores).insert(id.to_owned(), store);
    }

    fn stop_owner(&self, id: &str, generation: u64) -> Result<(), String> {
        let stopped = self.runtime.block_on(async {
            tokio::time::timeout(self.stop_bound, self.owner.stop(id, generation)).await
        });
        let confirmed = match stopped {
            Ok(Ok(())) => Ok(()),
            Ok(Err(why)) => self.kill_owner(id, generation, why),
            Err(_) => self.kill_owner(id, generation, "stop timed out".to_owned()),
        };
        if confirmed.is_ok() {
            self.scopes.remove(id, generation);
            lock(&self.started).remove(id);
        }
        confirmed
    }

    /// The kill path for a wedged owner. The scope stays registered on failure.
    fn kill_owner(&self, id: &str, generation: u64, why: String) -> Result<(), String> {
        tracing::warn!(source = %id, %why, "source owner did not confirm stop; killing");
        self.runtime
            .block_on(async {
                tokio::time::timeout(self.stop_bound, self.owner.kill(id, generation)).await
            })
            .map_err(|_| "kill timed out".to_owned())
            .and_then(|killed| killed)
            .map_err(|kill| format!("{why}; kill failed: {kill}"))
    }
}

impl<O: SourceOwner, T: Send + Sync + 'static> RailDriver<T> for OwnerRails<O, T> {
    fn store(&self, id: &str) -> Option<Arc<TileStore<T>>> {
        lock(&self.stores).get(id).cloned()
    }

    fn transition(
        &self,
        id: &str,
        from: Rails,
        to: Rails,
        requested: Option<RequestedSize>,
    ) -> Result<Rails, RailFailure> {
        let generation = lock(&self.started).get(id).copied().unwrap_or(0);
        let deadline = Instant::now() + self.bounds(id).start_timeout.unwrap_or(DEFAULT_STOP_BOUND);
        self.transition_at(id, from, to, requested, generation, deadline)
    }

    fn transition_at(
        &self,
        id: &str,
        from: Rails,
        to: Rails,
        _requested: Option<RequestedSize>,
        generation: u64,
        deadline: Instant,
    ) -> Result<Rails, RailFailure> {
        let was_running = !from.is_empty();
        let want = !to.is_empty();
        if was_running && !want {
            let started = lock(&self.started).get(id).copied().unwrap_or(generation);
            return self
                .stop_owner(id, started)
                .map(|()| Rails::NONE)
                .map_err(|reason| RailFailure {
                    running: from,
                    reason,
                });
        }
        if !want || was_running {
            // Already running: the owner has one rail; nothing to do.
            return Ok(if want { to } else { Rails::NONE });
        }
        if !lock(&self.stores).contains_key(id) {
            return Err(RailFailure {
                running: Rails::NONE,
                reason: "source is not configured".to_owned(),
            });
        }
        let budget = deadline.saturating_duration_since(Instant::now());
        let started = self.runtime.block_on(async {
            tokio::time::timeout(budget, self.owner.start(id, generation, deadline)).await
        });
        match started {
            Ok(Ok(kind)) => {
                self.scopes.register(ResourceScope {
                    source_id: id.to_owned(),
                    generation,
                    kind,
                });
                lock(&self.started).insert(id.to_owned(), generation);
                Ok(to)
            }
            Ok(Err(reason)) => Err(RailFailure {
                running: Rails::NONE,
                reason,
            }),
            Err(_) => {
                // The start future was dropped at the deadline. Make sure the
                // owner has nothing left over before reporting.
                let _ = self.stop_owner(id, generation);
                Err(RailFailure {
                    running: Rails::NONE,
                    reason: "start timeout".to_owned(),
                })
            }
        }
    }

    fn resolve(&self, id: &str, asked: Rails) -> Rails {
        if asked.is_empty() || !lock(&self.stores).contains_key(id) {
            Rails::NONE
        } else {
            asked
        }
    }

    fn progress(&self, id: &str, _rails: Rails) -> u64 {
        self.owner.progress(id)
    }

    fn bounds(&self, _id: &str) -> KindBounds {
        self.owner.bounds()
    }
}

fn lock<U>(mutex: &Mutex<U>) -> MutexGuard<'_, U> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demand::{ConsumerClass, ConsumerTag, DemandConfig, DemandManager, DemandPhase};
    use crate::registry::SourceRegistry;
    use crate::TransitionEvent;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    /// A fake web-source owner: counts calls; can hang a start or a stop.
    #[derive(Default)]
    struct FakeOwner {
        starts: AtomicUsize,
        stops: AtomicUsize,
        kills: AtomicUsize,
        hang_start: AtomicBool,
        hang_stop: AtomicBool,
        kill_fails: AtomicBool,
        frames: AtomicU64,
        last_generation: AtomicU64,
    }

    impl SourceOwner for FakeOwner {
        fn start(
            &self,
            _source: &str,
            generation: u64,
            _deadline: Instant,
        ) -> OwnerFuture<'_, ScopeKind> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.last_generation.store(generation, Ordering::SeqCst);
            let hang = self.hang_start.load(Ordering::SeqCst);
            Box::pin(async move {
                if hang {
                    std::future::pending::<()>().await;
                }
                Ok(ScopeKind::ProcessTree)
            })
        }
        fn stop(&self, _source: &str, _generation: u64) -> OwnerFuture<'_, ()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            let hang = self.hang_stop.load(Ordering::SeqCst);
            Box::pin(async move {
                if hang {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
        fn kill(&self, _source: &str, _generation: u64) -> OwnerFuture<'_, ()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            let fails = self.kill_fails.load(Ordering::SeqCst);
            Box::pin(async move {
                if fails {
                    Err("process will not die".to_owned())
                } else {
                    Ok(())
                }
            })
        }
        fn progress(&self, _source: &str) -> u64 {
            self.frames.load(Ordering::SeqCst)
        }
        fn bounds(&self) -> KindBounds {
            KindBounds {
                start_timeout: Some(Duration::from_millis(150)),
                close_after: Some(Duration::from_millis(20)),
                retry_cap: Some(3),
            }
        }
    }

    struct Harness {
        owner: Arc<FakeOwner>,
        scopes: Arc<ScopeRegistry>,
        manager: DemandManager<u8>,
        events: Arc<Mutex<Vec<TransitionEvent>>>,
        _runtime: tokio::runtime::Runtime,
    }

    fn harness() -> Harness {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let owner = Arc::new(FakeOwner::default());
        let scopes = ScopeRegistry::new();
        let rails = OwnerRails::<FakeOwner, u8>::new(
            Arc::clone(&owner),
            runtime.handle().clone(),
            Arc::clone(&scopes),
        )
        .with_stop_bound(Duration::from_millis(50));
        rails.add_source("web", Arc::new(TileStore::with_defaults("web")));
        let manager = DemandManager::new(
            SourceRegistry::new(),
            Arc::new(rails),
            DemandConfig {
                backoff_base: Duration::from_millis(5),
                backoff_max: Duration::from_millis(10),
                health_period: Duration::from_millis(5),
                ..DemandConfig::default()
            },
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        manager.set_transition_sink(Arc::new(move |e: &TransitionEvent| {
            lock(&sink).push(e.clone());
        }));
        manager.start();
        Harness {
            owner,
            scopes,
            manager,
            events,
            _runtime: runtime,
        }
    }

    fn wait(what: &str, mut done: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn viewer() -> ConsumerTag {
        ConsumerTag::new(ConsumerClass::Preview, "viewer")
    }

    #[test]
    fn a_lease_starts_the_owner_registers_its_scope_and_close_after_stops_it() {
        let h = harness();
        let lease = h.manager.acquire("web", Rails::VIDEO, viewer(), None);
        wait("started", || h.scopes.get("web").is_some());
        let scope = h.scopes.get("web").unwrap();
        assert_eq!(scope.kind, ScopeKind::ProcessTree);
        assert_eq!(scope.generation, 1);
        assert_eq!(h.owner.last_generation.load(Ordering::SeqCst), 1);
        h.owner.frames.fetch_add(1, Ordering::SeqCst);
        wait("ready", || {
            h.manager.status("web").phase == DemandPhase::Ready
        });
        drop(lease);
        wait("stopped", || h.owner.stops.load(Ordering::SeqCst) == 1);
        wait("scope removed", || h.scopes.is_empty());
        assert!(!h.scopes.is_current("web", 1), "a late sample is fenced");
        wait("idle", || h.manager.active_len() == 0);
        let events = lock(&h.events).clone();
        assert_eq!(events.last().map(|e| e.to), Some(DemandPhase::Idle));
        h.manager.shutdown();
    }

    #[test]
    fn a_start_past_its_deadline_is_cancelled_and_fails_and_retries_stop_at_the_cap() {
        let h = harness();
        h.owner.hang_start.store(true, Ordering::SeqCst);
        h.manager.start_source("web");
        wait("capped", || {
            h.manager
                .status("web")
                .detail
                .is_some_and(|d| d.starts_with("gave up"))
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(h.owner.starts.load(Ordering::SeqCst), 3, "retry cap");
        assert_eq!(h.manager.status("web").phase, DemandPhase::Failed);
        assert!(h.scopes.is_empty(), "a cancelled start registers nothing");
        let failed = lock(&h.events)
            .iter()
            .find(|e| e.to == DemandPhase::Failed)
            .cloned()
            .unwrap();
        assert_eq!(failed.reason, "start timeout");
        h.manager.shutdown();
    }

    #[test]
    fn a_wedged_stop_goes_to_kill_and_a_failed_kill_keeps_the_scope_and_reads_failed() {
        let h = harness();
        let lease = h.manager.acquire("web", Rails::VIDEO, viewer(), None);
        wait("started", || h.scopes.get("web").is_some());
        h.owner.hang_stop.store(true, Ordering::SeqCst);
        h.owner.kill_fails.store(true, Ordering::SeqCst);
        drop(lease);
        wait("killed", || h.owner.kills.load(Ordering::SeqCst) >= 1);
        wait("failed", || {
            h.manager.status("web").phase == DemandPhase::Failed
        });
        assert!(h.scopes.get("web").is_some(), "teardown not confirmed");
        // The kill path recovers it.
        h.owner.kill_fails.store(false, Ordering::SeqCst);
        wait("scope removed", || h.scopes.is_empty());
        wait("idle", || h.manager.active_len() == 0);
        h.manager.shutdown();
    }
}
