//! Stored layouts as M/E sources (#307 PR 2): the binary's engine side of
//! [`multiview_control::me_layouts::MeLayoutSources`].
//!
//! One adapter per pipeline. A registration creates (or, for a body swap,
//! keeps) the `layout:<id>` [`TileStore`], publishes the routing-aux compose
//! set FIRST (so the store is being fed), then the output clock's
//! [`LayoutSources`] set (the frame hook adopts it at the next fresh tick),
//! then raises the aux tick channel's M/E flag while anything is registered.
//! One writer mutex, control-plane only; the clock reads atomics and
//! `SwapCell`s only.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use multiview_compositor::pipeline::Nv12Image;
use multiview_control::me_layouts::{
    source_key, MeLayoutRefusal, MeLayoutSources, SharedMeLayoutSources,
};
use multiview_control::output_routing::LayoutBody;
use multiview_engine::switcher_layouts::{LayoutSourceSet, LayoutSources, MAX_ME_LAYOUT_SOURCES};
use multiview_framestore::{SwapCell, TileStore};

use crate::routing_aux::{MeLayoutCompose, MeLayoutComposeSet};

/// The engine side of the M/E layout-source registry for one pipeline.
pub struct MeLayoutAdapter {
    engine: Arc<LayoutSources>,
    compose: MeLayoutComposeSet,
    active: Arc<AtomicBool>,
    /// The generation of the last engine publish.
    published: AtomicU64,
    writer: Mutex<Vec<MeLayoutCompose>>,
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
        Self {
            engine: Arc::new(LayoutSources::new()),
            compose: Arc::new(SwapCell::from_pointee(Vec::new())),
            active: Arc::new(AtomicBool::new(false)),
            published: AtomicU64::new(0),
            writer: Mutex::new(Vec::new()),
        }
    }

    /// The output clock's set (for `SwitcherFrameHook::layout_sources`).
    #[must_use]
    pub fn engine(&self) -> Arc<LayoutSources> {
        Arc::clone(&self.engine)
    }

    /// The routing-aux compose set (for `AuxInputs::me_layouts`).
    #[must_use]
    pub fn compose_set(&self) -> MeLayoutComposeSet {
        Arc::clone(&self.compose)
    }

    /// The aux tick channel's M/E gate flag (raised while anything is
    /// registered; shared into every run's `AuxTickSender`).
    #[must_use]
    pub fn active_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.active)
    }

    fn publish(&self, entries: &[MeLayoutCompose]) {
        // Aux first: the store is fed before the clock can put it on air.
        self.compose.store(Arc::new(entries.to_vec()));
        let generation = self.engine.publish_generation(LayoutSourceSet {
            entries: entries
                .iter()
                .map(|e| (Arc::clone(&e.key), Arc::clone(&e.store), e.body.revision))
                .collect(),
        });
        self.published.store(generation, Ordering::Release);
        self.active.store(!entries.is_empty(), Ordering::Release);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MeLayoutCompose>> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl MeLayoutSources for MeLayoutAdapter {
    fn register(&self, body: LayoutBody) -> Result<(), MeLayoutRefusal> {
        let key: Arc<str> = Arc::from(source_key(&body.0.id));
        let mut entries = self.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.key == key) {
            // A body swap keeps the store: Program holds through the edit.
            entry.body = Arc::clone(&body.0);
        } else {
            if entries.len() >= MAX_ME_LAYOUT_SOURCES {
                return Err(MeLayoutRefusal::Capacity);
            }
            let store: Arc<TileStore<Nv12Image>> =
                Arc::new(TileStore::with_defaults(Arc::clone(&key)));
            entries.push(MeLayoutCompose {
                key,
                body: Arc::clone(&body.0),
                store,
            });
        }
        self.publish(&entries);
        Ok(())
    }

    fn unregister(&self, id: &str) -> bool {
        let key = source_key(id);
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|e| e.key.as_ref() != key);
        let removed = entries.len() != before;
        if removed {
            self.publish(&entries);
        }
        removed
    }

    fn registered(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter_map(|e| e.key.strip_prefix("layout:").map(str::to_owned))
            .collect()
    }

    fn is_adopted(&self) -> bool {
        self.engine.adopted_generation() >= self.published.load(Ordering::Acquire)
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

    #[test]
    fn me_layouts_adapter_caps_swaps_bodies_and_raises_the_flag() {
        let adapter = MeLayoutAdapter::new();
        let flag = adapter.active_flag();
        assert!(!flag.load(Ordering::Acquire));
        for id in ["a", "b", "c", "d"] {
            adapter.register(body(id, 1)).unwrap();
        }
        assert!(flag.load(Ordering::Acquire));
        assert_eq!(
            adapter.register(body("e", 1)),
            Err(MeLayoutRefusal::Capacity),
            "a fifth distinct layout is refused"
        );
        assert_eq!(adapter.registered().len(), 4);
        let engine = adapter.engine();
        let store = Arc::clone(engine.load().get("layout:a").unwrap());
        let generation = engine.generation();
        // A body swap keeps the store and republishes both sets.
        let swapped = body("a", 2);
        adapter.register(swapped.clone()).unwrap();
        assert!(engine.generation() > generation);
        assert!(Arc::ptr_eq(engine.load().get("layout:a").unwrap(), &store));
        let compose = adapter.compose_set().load();
        let a = compose
            .iter()
            .find(|e| e.key.as_ref() == "layout:a")
            .unwrap();
        assert!(Arc::ptr_eq(&a.body, &swapped.0));
        assert!(Arc::ptr_eq(&a.store, &store));
        // Not adopted until the hook acknowledges the generation.
        assert!(!adapter.is_adopted());
        engine.mark_adopted(engine.generation());
        assert!(adapter.is_adopted());
        for id in ["a", "b", "c", "d"] {
            assert!(adapter.unregister(id));
        }
        assert!(!adapter.unregister("a"));
        assert!(!flag.load(Ordering::Acquire), "lowered when empty");
        assert!(engine.load().entries.is_empty());
        assert!(adapter.compose_set().load().is_empty());
    }
}
