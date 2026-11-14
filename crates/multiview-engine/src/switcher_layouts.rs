//! Stored layouts as M/E sources (#307 PR 2, `docs/architecture/output-routing.md`
//! §3): the wait-free handoff of `layout:<id>` frame stores from the control
//! plane to the output clock.
//!
//! The control-plane registry (bounded at [`MAX_ME_LAYOUT_SOURCES`]) builds a
//! new immutable [`LayoutSourceSet`] on every change and [`LayoutSources::publish`]es
//! it. The Program frame hook polls [`LayoutSources::generation`] once per
//! fresh tick (one Acquire load) and adopts the set only when the generation
//! moved, at the frame boundary, so the steady state does no work and never
//! allocates. Acquisition is one bounded attempt ([`LayoutSources::try_load`]):
//! a lost race defers adoption (and its acknowledgement) to the next tick. The off-clock `routing-aux` thread composes each layout into its
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
    /// `(source key "layout:<id>", its composed-picture store, the body
    /// revision it composes)`. The revision is what a command's
    /// `SelectionGuard::layout_revision` is checked against (#307 review A1).
    pub entries: Vec<(Arc<str>, Arc<TileStore<Nv12Image>>, u64)>,
}

impl LayoutSourceSet {
    /// The store for `key`, if the set carries it. Linear: at most
    /// [`MAX_ME_LAYOUT_SOURCES`] entries.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Arc<TileStore<Nv12Image>>> {
        self.entries
            .iter()
            .find(|(k, _, _)| k.as_ref() == key)
            .map(|(_, store, _)| store)
    }
}

/// The published layout-source set plus its change generation.
#[derive(Debug)]
pub struct LayoutSources {
    generation: AtomicU64,
    /// The generation the frame hook last adopted (its ack; written once per
    /// adoption, never in the steady state).
    adopted: AtomicU64,
    set: SwapCell<Arc<LayoutSourceSet>>,
    /// Test seam: the next N [`Self::try_load`] attempts lose their race.
    #[cfg(test)]
    forced_misses: std::sync::atomic::AtomicU32,
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
            adopted: AtomicU64::new(0),
            set: SwapCell::new(Arc::new(LayoutSourceSet::default())),
            #[cfg(test)]
            forced_misses: std::sync::atomic::AtomicU32::new(0),
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

    /// Publish and return the new generation (the writer waits, off the
    /// clock, for [`Self::adopted_generation`] to reach it).
    pub fn publish_generation(&self, set: LayoutSourceSet) -> u64 {
        self.set.store(Arc::new(set));
        self.generation.fetch_add(1, Ordering::Release) + 1
    }

    /// The frame hook acknowledges that it adopted `generation` (Release).
    pub fn mark_adopted(&self, generation: u64) {
        self.adopted.store(generation, Ordering::Release);
    }

    /// The generation the frame hook last adopted (Acquire). A command that
    /// names a newly registered key is sent only once this reached the
    /// registration's generation, so the drain that sees the command already
    /// has the store (the hook adopts before it drains).
    #[must_use]
    pub fn adopted_generation(&self) -> u64 {
        self.adopted.load(Ordering::Acquire)
    }

    /// The current set (a pointer clone). Lock-free, NOT bounded: it retries
    /// while publishers keep moving the cell. Off-clock callers only; the
    /// output clock uses [`Self::try_load`].
    #[must_use]
    pub fn load(&self) -> Arc<LayoutSourceSet> {
        self.set.load()
    }

    /// The current set in exactly one attempt (#307 review I1); `None` when
    /// a publish raced it. The frame hook then keeps its previous set, does
    /// not acknowledge, and tries again on the next tick: bounded work per
    /// tick however hard the control plane churns the registry.
    #[must_use]
    pub fn try_load(&self) -> Option<Arc<LayoutSourceSet>> {
        #[cfg(test)]
        {
            // Single-threaded test seam: the hook is the only caller.
            let misses = self.forced_misses.load(Ordering::Acquire);
            if misses > 0 {
                self.forced_misses.store(misses - 1, Ordering::Release);
                return None;
            }
        }
        self.set.try_load()
    }

    /// Test seam: make the next `n` [`Self::try_load`] attempts fail as if a
    /// publish raced each of them.
    #[cfg(test)]
    pub(crate) fn force_misses(&self, n: u32) {
        self.forced_misses.store(n, Ordering::Release);
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
            entries: vec![(Arc::from("layout:a"), Arc::clone(&store), 1)],
        });
        assert_eq!(sources.generation(), 1);
        assert_eq!(sources.adopted_generation(), 0);
        sources.mark_adopted(1);
        assert_eq!(sources.adopted_generation(), 1);
        assert_eq!(sources.publish_generation(LayoutSourceSet::default()), 2);
        assert!(sources.load().entries.is_empty());
        sources.publish(LayoutSourceSet {
            entries: vec![(Arc::from("layout:a"), Arc::clone(&store), 1)],
        });
        let set = sources.load();
        assert!(Arc::ptr_eq(set.get("layout:a").unwrap(), &store));
        assert!(set.get("layout:b").is_none());
    }
}
