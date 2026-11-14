//! Stored layouts as M/E sources (#307 PR 2, `docs/architecture/output-routing.md`
//! §3): the wait-free handoff of `layout:<id>` frame stores from the control
//! plane to the output clock.
//!
//! The control-plane registry (bounded at [`MAX_ME_LAYOUT_SOURCES`]) builds a
//! new immutable [`LayoutSourceSet`] on every change and [`LayoutSources::publish`]es
//! it. The Program frame hook polls [`LayoutSources::generation`] once per
//! fresh tick (one Acquire load) and adopts the set only when the generation
//! moved, at the frame boundary, so the steady state does no work and never
//! allocates. The off-clock `routing-aux` thread composes each layout into its
//! store; the clock only ever reads stores.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_framestore::{SwapCell, TileStore};

/// The most stored layouts registered as M/E sources at once. The writer
/// (the control-plane registry) enforces it; [`LayoutSources::publish`]
/// truncates nothing.
pub const MAX_ME_LAYOUT_SOURCES: usize = 4;

/// One immutable set of layout-source stores, keyed `layout:<id>`.
#[derive(Debug, Default)]
pub struct LayoutSourceSet {
    /// `(source key "layout:<id>", its composed-picture store)`.
    pub entries: Vec<(Arc<str>, Arc<TileStore<Nv12Image>>)>,
}

impl LayoutSourceSet {
    /// The store for `key`, if the set carries it. Linear: at most
    /// [`MAX_ME_LAYOUT_SOURCES`] entries.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Arc<TileStore<Nv12Image>>> {
        self.entries
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, store)| store)
    }
}

/// The published layout-source set plus its change generation.
#[derive(Debug)]
pub struct LayoutSources {
    generation: AtomicU64,
    set: SwapCell<Arc<LayoutSourceSet>>,
}

impl Default for LayoutSources {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutSources {
    /// An empty set at generation 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            set: SwapCell::new(Arc::new(LayoutSourceSet::default())),
        }
    }

    /// Publish a new set: store it first, then bump the generation (Release),
    /// so a reader that sees the new generation loads at least this set.
    pub fn publish(&self, set: LayoutSourceSet) {
        self.set.store(Arc::new(set));
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// The change generation (one Acquire load; the clock's only steady cost).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The current set (a pointer clone, wait-free).
    #[must_use]
    pub fn load(&self) -> Arc<LayoutSourceSet> {
        self.set.load()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn publish_bumps_the_generation_after_the_store() {
        let sources = LayoutSources::new();
        assert_eq!(sources.generation(), 0);
        assert!(sources.load().entries.is_empty());
        let store = Arc::new(TileStore::with_defaults("layout:a"));
        sources.publish(LayoutSourceSet {
            entries: vec![(Arc::from("layout:a"), Arc::clone(&store))],
        });
        assert_eq!(sources.generation(), 1);
        let set = sources.load();
        assert!(Arc::ptr_eq(set.get("layout:a").unwrap(), &store));
        assert!(set.get("layout:b").is_none());
    }
}
