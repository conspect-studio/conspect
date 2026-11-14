//! Stored layouts as M/E sources (#307 PR 2): the binary's engine side of
//! [`multiview_control::me_layouts::MeLayoutSources`].
//!
//! One adapter per pipeline. **One store per body revision** (#307 review
//! r2): every registration AND every body change creates a NEW
//! `layout:<id>` [`TileStore`], so every frame a store ever holds was
//! composed from exactly one body. The order is:
//!
//! 1. publish the routing-aux compose set carrying the new `(body, store)`
//!    (the aux starts filling the new store; the old store is no longer fed
//!    and holds last-good of the old, already-authorized body) and raise the
//!    aux tick channel's M/E flag;
//! 2. off the clock, wait up to [`PRIME_WAIT`] for the new store to hold a
//!    frame ([`TileStore::is_primed`]);
//! 3. publish the output clock's [`LayoutSources`] set with the new store
//!    AND the new revision together (the frame hook swaps both in one
//!    adoption). If the store never primes within the bound (aux wedged or
//!    failed) the set is published anyway: that store ages to slate exactly
//!    like an aux failure.
//!
//! So the engine set never pairs a revision with a store that holds frames
//! of another body: until step 3 the clock holds the OLD store with the OLD
//! revision; from step 3 the new store with the new revision.
//!
//! Step 2 runs on a short-lived promoter thread, not on the caller: the
//! caller is the control registry, holding its std mutex inside an async
//! handler on a tokio worker, so it must not sleep. While a body is
//! pending, [`MeLayoutSources::is_adopted`] answers `false`, so the
//! control's bounded (≤ 1 s) adoption wait covers prime + adoption. One
//! writer mutex, control-plane only; the clock reads atomics and
//! `SwapCell`s only.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use multiview_compositor::pipeline::Nv12Image;
use multiview_control::me_layouts::{
    source_key, MeLayoutRefusal, MeLayoutSources, SharedMeLayoutSources,
};
use multiview_control::output_routing::LayoutBody;
use multiview_engine::switcher_layouts::{LayoutSourceSet, LayoutSources, MAX_ME_LAYOUT_SOURCES};
use multiview_framestore::{SwapCell, TileStore};

use crate::routing_aux::{MeLayoutCompose, MeLayoutComposeSet};

/// How long a new body's store may take to hold its first frame before the
/// engine set is published anyway (it then ages to slate like an aux
/// failure).
pub const PRIME_WAIT: Duration = Duration::from_secs(1);

/// The promoter's poll interval (off-clock).
const PRIME_POLL: Duration = Duration::from_millis(2);

type Store = Arc<TileStore<Nv12Image>>;

/// One registered layout.
struct Slot {
    /// What the aux composes now: the newest body and ITS store.
    compose: MeLayoutCompose,
    /// What the engine set carries for this key (`None` until the first
    /// body primed or timed out).
    engine: Option<(Store, u64)>,
    /// Set while `compose` is newer than `engine`: when the body changed.
    pending_since: Option<Instant>,
}

#[derive(Default)]
struct Writer {
    slots: Vec<Slot>,
    /// A promoter thread is running (it exits once nothing is pending).
    promoter: bool,
}

struct Shared {
    engine: Arc<LayoutSources>,
    compose: MeLayoutComposeSet,
    active: Arc<AtomicBool>,
    /// The generation of the last engine publish.
    published: AtomicU64,
    prime_wait: Duration,
    writer: Mutex<Writer>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Writer> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish_compose(&self, writer: &Writer) {
        self.compose.store(Arc::new(
            writer.slots.iter().map(|s| s.compose.clone()).collect(),
        ));
        self.active
            .store(!writer.slots.is_empty(), Ordering::Release);
    }

    fn publish_engine(&self, writer: &Writer) {
        let generation = self.engine.publish_generation(LayoutSourceSet {
            entries: writer
                .slots
                .iter()
                .filter_map(|s| {
                    s.engine.as_ref().map(|(store, revision)| {
                        (Arc::clone(&s.compose.key), Arc::clone(store), *revision)
                    })
                })
                .collect(),
        });
        self.published.store(generation, Ordering::Release);
    }

    /// Move every pending body whose store primed (or whose wait expired)
    /// into the engine set; `true` if anything moved.
    fn promote(&self, writer: &mut Writer, now: Instant) -> bool {
        let mut moved = false;
        for slot in &mut writer.slots {
            let Some(since) = slot.pending_since else {
                continue;
            };
            if slot.compose.store.is_primed() || now.duration_since(since) >= self.prime_wait {
                slot.engine = Some((Arc::clone(&slot.compose.store), slot.compose.body.revision));
                slot.pending_since = None;
                moved = true;
            }
        }
        moved
    }

    /// Ensure a promoter runs while anything is pending (caller holds the
    /// writer lock). Without a thread (spawn failed) the pending bodies are
    /// published at once: their stores age to slate until primed.
    fn ensure_promoter(self: &Arc<Self>, writer: &mut Writer) {
        if writer.promoter || writer.slots.iter().all(|s| s.pending_since.is_none()) {
            return;
        }
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("me-layout-prime".to_owned())
            .spawn(move || shared.run_promoter());
        if spawned.is_ok() {
            writer.promoter = true;
        } else {
            tracing::warn!("M/E layout source: no prime thread; publishing the new body at once");
            for slot in &mut writer.slots {
                if slot.pending_since.take().is_some() {
                    slot.engine =
                        Some((Arc::clone(&slot.compose.store), slot.compose.body.revision));
                }
            }
            self.publish_engine(writer);
        }
    }

    fn run_promoter(&self) {
        loop {
            std::thread::sleep(PRIME_POLL);
            let mut writer = self.lock();
            if self.promote(&mut writer, Instant::now()) {
                self.publish_engine(&writer);
            }
            if writer.slots.iter().all(|s| s.pending_since.is_none()) {
                writer.promoter = false;
                return;
            }
        }
    }
}

/// The engine side of the M/E layout-source registry for one pipeline.
pub struct MeLayoutAdapter {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for MeLayoutAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeLayoutAdapter").finish_non_exhaustive()
    }
}

impl Default for MeLayoutAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MeLayoutAdapter {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with_prime_wait(PRIME_WAIT)
    }

    /// An empty registry whose new bodies wait at most `prime_wait` for
    /// their first frame (tests shorten it; production uses [`PRIME_WAIT`]).
    #[must_use]
    pub fn with_prime_wait(prime_wait: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                engine: Arc::new(LayoutSources::new()),
                compose: Arc::new(SwapCell::from_pointee(Vec::new())),
                active: Arc::new(AtomicBool::new(false)),
                published: AtomicU64::new(0),
                prime_wait,
                writer: Mutex::new(Writer::default()),
            }),
        }
    }

    /// The output clock's set (for `SwitcherFrameHook::layout_sources`).
    #[must_use]
    pub fn engine(&self) -> Arc<LayoutSources> {
        Arc::clone(&self.shared.engine)
    }

    /// The routing-aux compose set (for `AuxInputs::me_layouts`).
    #[must_use]
    pub fn compose_set(&self) -> MeLayoutComposeSet {
        Arc::clone(&self.shared.compose)
    }

    /// The aux tick channel's M/E gate flag (raised while anything is
    /// registered; shared into every run's `AuxTickSender`).
    #[must_use]
    pub fn active_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.active)
    }
}

impl MeLayoutSources for MeLayoutAdapter {
    fn register(&self, body: LayoutBody) -> Result<(), MeLayoutRefusal> {
        let key: Arc<str> = Arc::from(source_key(&body.0.id));
        let shared = &self.shared;
        let mut writer = shared.lock();
        let index = match writer.slots.iter().position(|s| s.compose.key == key) {
            Some(index) => {
                if writer.slots[index].compose.body.revision == body.0.revision {
                    // The same revision is the same content: nothing to swap.
                    return Ok(());
                }
                index
            }
            None => {
                if writer.slots.len() >= MAX_ME_LAYOUT_SOURCES {
                    return Err(MeLayoutRefusal::Capacity);
                }
                writer.slots.push(Slot {
                    compose: MeLayoutCompose {
                        key: Arc::clone(&key),
                        body: Arc::clone(&body.0),
                        store: Arc::new(TileStore::with_defaults(Arc::clone(&key))),
                    },
                    engine: None,
                    pending_since: None,
                });
                writer.slots.len() - 1
            }
        };
        // One store per body revision: a body change never reuses a store.
        let slot = &mut writer.slots[index];
        slot.compose = MeLayoutCompose {
            key: Arc::clone(&key),
            body: Arc::clone(&body.0),
            store: Arc::new(TileStore::with_defaults(key)),
        };
        slot.pending_since = Some(Instant::now());
        // Aux first; the engine set follows once the new store primed.
        shared.publish_compose(&writer);
        shared.ensure_promoter(&mut writer);
        Ok(())
    }

    fn unregister(&self, id: &str) -> bool {
        let key = source_key(id);
        let shared = &self.shared;
        let mut writer = shared.lock();
        let before = writer.slots.len();
        writer.slots.retain(|s| s.compose.key.as_ref() != key);
        let removed = writer.slots.len() != before;
        if removed {
            shared.publish_compose(&writer);
            shared.publish_engine(&writer);
        }
        removed
    }

    fn registered(&self) -> Vec<String> {
        self.shared
            .lock()
            .slots
            .iter()
            .filter_map(|s| s.compose.key.strip_prefix("layout:").map(str::to_owned))
            .collect()
    }

    fn is_adopted(&self) -> bool {
        let shared = &self.shared;
        let writer = shared.lock();
        writer.slots.iter().all(|s| s.pending_since.is_none())
            && shared.engine.adopted_generation() >= shared.published.load(Ordering::Acquire)
    }
}

fn installed() -> &'static Mutex<Option<SharedMeLayoutSources>> {
    static SLOT: OnceLock<Mutex<Option<SharedMeLayoutSources>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or with `None` clear) the run's M/E layout sources for the
/// control plane (mirrors `output_routing_control::install_output_routing`).
pub fn install_me_layouts(sources: Option<SharedMeLayoutSources>) {
    *installed().lock().unwrap_or_else(PoisonError::into_inner) = sources;
}

/// The installed M/E layout sources, if any.
#[must_use]
pub fn installed_me_layouts() -> Option<SharedMeLayoutSources> {
    installed()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use multiview_control::output_routing::RoutedLayout;

    fn body(id: &str, revision: u64) -> LayoutBody {
        LayoutBody(Arc::new(RoutedLayout {
            id: id.to_owned(),
            layout: multiview_core::layout::Layout {
                name: id.into(),
                canvas: multiview_core::layout::Canvas {
                    width: 64,
                    height: 64,
                    fps_num: 25,
                    fps_den: 1,
                },
                cells: vec![],
            },
            slates: vec![],
            audio: multiview_config::LayoutAudio::default(),
            revision,
        }))
    }

    fn frame() -> Nv12Image {
        let color = multiview_core::color::ColorInfo::default().resolve_defaults(64, 64);
        Nv12Image::solid(64, 64, 90, 128, 128, color).unwrap()
    }

    /// The fake aux: one composed frame into the store the compose set
    /// carries for `key` now.
    fn aux_publish(adapter: &MeLayoutAdapter, key: &str) -> Store {
        let set = adapter.compose_set().load();
        let entry = set.iter().find(|e| e.key.as_ref() == key).unwrap();
        entry
            .store
            .publish(frame(), multiview_core::time::MediaTime::ZERO);
        Arc::clone(&entry.store)
    }

    fn compose_store(adapter: &MeLayoutAdapter, key: &str) -> Store {
        let set = adapter.compose_set().load();
        Arc::clone(&set.iter().find(|e| e.key.as_ref() == key).unwrap().store)
    }

    /// The engine set's `(store, revision)` for `key`.
    fn engine_entry(adapter: &MeLayoutAdapter, key: &str) -> Option<(Store, u64)> {
        adapter
            .engine()
            .load()
            .entries
            .iter()
            .find(|(k, _, _)| k.as_ref() == key)
            .map(|(_, store, revision)| (Arc::clone(store), *revision))
    }

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn me_layouts_adapter_caps_and_raises_the_flag() {
        let adapter = MeLayoutAdapter::new();
        let flag = adapter.active_flag();
        assert!(!flag.load(Ordering::Acquire));
        for id in ["a", "b", "c", "d"] {
            adapter.register(body(id, 1)).unwrap();
        }
        assert!(flag.load(Ordering::Acquire), "raised with the compose set");
        assert_eq!(
            adapter.register(body("e", 1)),
            Err(MeLayoutRefusal::Capacity),
            "a fifth distinct layout is refused"
        );
        assert_eq!(adapter.registered().len(), 4);
        for id in ["a", "b", "c", "d"] {
            assert!(adapter.unregister(id));
        }
        assert!(!adapter.unregister("a"));
        assert!(!flag.load(Ordering::Acquire), "lowered when empty");
        assert!(adapter.engine().load().entries.is_empty());
        assert!(adapter.compose_set().load().is_empty());
    }

    /// #307 review r2 (M5): one store per body revision; the engine set gets
    /// a body's (store, revision) only once that store primed, so the clock
    /// never holds a revision for a store with another body's frames. The
    /// fake aux and the hook's adoption are delayed independently.
    #[test]
    fn me_layouts_a_body_change_gets_a_new_store_published_after_it_primed() {
        let adapter = MeLayoutAdapter::new();
        let engine = adapter.engine();
        const KEY: &str = "layout:a";
        // First registration: compose set first, engine set after prime.
        adapter.register(body("a", 1)).unwrap();
        let first = compose_store(&adapter, KEY);
        std::thread::sleep(Duration::from_millis(50));
        assert!(engine_entry(&adapter, KEY).is_none(), "not before prime");
        assert!(!adapter.is_adopted());
        aux_publish(&adapter, KEY);
        assert!(wait_for(|| engine_entry(&adapter, KEY).is_some()));
        let (store, revision) = engine_entry(&adapter, KEY).unwrap();
        assert!(Arc::ptr_eq(&store, &first));
        assert_eq!(revision, 1);
        // Promoted but the hook has not adopted yet (adoption delayed).
        assert!(!adapter.is_adopted());
        engine.mark_adopted(engine.generation());
        assert!(adapter.is_adopted());
        // A body change: a NEW store, in the compose set at once.
        adapter.register(body("a", 2)).unwrap();
        let second = compose_store(&adapter, KEY);
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a body change never reuses the store"
        );
        assert!(!second.is_primed());
        // The aux is delayed: the engine keeps the OLD store with the OLD
        // revision (the pair never mixes), and the change is not adopted.
        std::thread::sleep(Duration::from_millis(50));
        let (store, revision) = engine_entry(&adapter, KEY).unwrap();
        assert!(Arc::ptr_eq(&store, &first));
        assert_eq!(revision, 1);
        assert!(!adapter.is_adopted(), "a pending body is not adopted");
        // Old frames go only to the old store: the new one stays empty.
        first.publish(frame(), multiview_core::time::MediaTime::ZERO);
        assert!(!second.is_primed());
        // The aux fills the new store: new store + new revision together.
        aux_publish(&adapter, KEY);
        assert!(wait_for(
            || engine_entry(&adapter, KEY).is_some_and(|(_, revision)| revision == 2)
        ));
        let (store, revision) = engine_entry(&adapter, KEY).unwrap();
        assert!(Arc::ptr_eq(&store, &second));
        assert_eq!(revision, 2);
        assert!(!adapter.is_adopted(), "adoption still delayed");
        engine.mark_adopted(engine.generation());
        assert!(adapter.is_adopted());
        // The same revision again changes nothing (no new store).
        let generation = engine.generation();
        adapter.register(body("a", 2)).unwrap();
        assert!(Arc::ptr_eq(&compose_store(&adapter, KEY), &second));
        assert_eq!(engine.generation(), generation);
    }

    /// The bound: a store that never primes (aux wedged or failed) is
    /// published anyway after the prime wait, and ages to slate.
    #[test]
    fn me_layouts_an_unprimed_body_is_published_after_the_bound() {
        let wait = Duration::from_millis(200);
        let adapter = MeLayoutAdapter::with_prime_wait(wait);
        const KEY: &str = "layout:a";
        let started = Instant::now();
        adapter.register(body("a", 1)).unwrap();
        let store = compose_store(&adapter, KEY);
        assert!(wait_for(|| engine_entry(&adapter, KEY).is_some()));
        assert!(started.elapsed() >= wait, "never before the bound");
        let (published, revision) = engine_entry(&adapter, KEY).unwrap();
        assert!(Arc::ptr_eq(&published, &store));
        assert_eq!(revision, 1);
        assert!(!store.is_primed(), "published unprimed: it ages to slate");
    }
}
