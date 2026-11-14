//! Stored layouts as M/E sources (#307 PR 2, `docs/architecture/output-routing.md`
//! §3): the control-plane registry.
//!
//! A `layout:<id>` selected on PVW/PGM or patched to a mixer input is
//! registered here first. The registry is bounded at
//! [`MAX_ME_LAYOUT_SOURCES`]; it holds one demand lease per source the layout
//! binds (so on-demand sources keep running while the layout can be on air)
//! and the instant the M/E last referenced it. [`MeLayoutRegistry::sweep`]
//! drops an entry the M/E has not referenced for [`ME_LAYOUT_LINGER`].
//!
//! The engine side is behind [`MeLayoutSources`] (the binary installs an
//! adapter that publishes the set to the clock and the routing-aux thread).
//! One std `Mutex`, control-plane only; the output clock never touches it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::output_routing::LayoutBody;
use crate::source_demand::{SharedSourceDemand, SourceDemandLease};

/// The most stored layouts registered as M/E sources at once.
pub const MAX_ME_LAYOUT_SOURCES: usize = multiview_engine::switcher_layouts::MAX_ME_LAYOUT_SOURCES;

/// How long an entry the M/E no longer references stays registered.
pub const ME_LAYOUT_LINGER: Duration = Duration::from_secs(10);

/// The engine-source key of stored layout `id`: `layout:<id>`.
#[must_use]
pub fn source_key(id: &str) -> String {
    format!("layout:{id}")
}

/// Why the engine side refused a registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeLayoutRefusal {
    /// Already [`MAX_ME_LAYOUT_SOURCES`] distinct layouts are registered.
    Capacity,
}

/// The engine side: publishes the registered set to the output clock and the
/// routing-aux thread. Never blocks on the clock.
pub trait MeLayoutSources: Send + Sync {
    /// Register or replace (same id: body swap at the next tick).
    ///
    /// # Errors
    /// [`MeLayoutRefusal::Capacity`] for a new id over the cap.
    fn register(&self, body: LayoutBody) -> Result<(), MeLayoutRefusal>;
    /// Unregister `id`; `true` if it was registered.
    fn unregister(&self, id: &str) -> bool;
    /// The registered layout ids.
    fn registered(&self) -> Vec<String>;
    /// Whether the output clock adopted the latest registration (its store
    /// is in the Program drive). A mixer patch naming a new layout waits
    /// (bounded, off the clock) for this before it is enqueued, because the
    /// engine refuses a patch to a store it does not have. The default (no
    /// clock to wait for) is `true`.
    fn is_adopted(&self) -> bool {
        true
    }
}

/// Shared engine-side handle.
pub type SharedMeLayoutSources = Arc<dyn MeLayoutSources>;

/// The most per-layout lifecycle fences kept (see [`MeLayoutRegistry::fence`]).
pub const MAX_LAYOUT_FENCES: usize = 1024;

/// A fence value meaning "an edit or delete of this layout is committing
/// now": every registration of it is refused until the commit finished.
const PENDING: u64 = u64::MAX;

/// Why a registry operation refused (all answer 409, [`Self::kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeLayoutError {
    /// The registry is full.
    Capacity,
    /// A bound source's demand lease was refused (unknown source or limit).
    Lease,
    /// The body was read before a committed (or committing) edit or delete
    /// of the layout: 409 `layout-changed`.
    Changed,
    /// A delete of a layout registered as an M/E source: 409 `layout-on-air`.
    OnAir,
}

impl MeLayoutError {
    /// The problem `type` slug of the 409.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Capacity | Self::Lease => "capacity",
            Self::Changed => "layout-changed",
            Self::OnAir => "layout-on-air",
        }
    }

    /// The operator-facing detail of the 409 problem.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::Capacity => {
                "at most 4 stored layouts can be M/E sources at once; take one off the M/E \
                 (it is released 10 s after nothing references it)"
            }
            Self::Lease => "a source this layout shows could not be leased (lease limit)",
            Self::Changed => "the layout changed while this request was in flight; retry",
            Self::OnAir => ME_LAYOUT_ON_AIR_DETAIL,
        }
    }
}

/// The detail of the 409 `layout-on-air` refusal of a delete (#307).
pub const ME_LAYOUT_ON_AIR_DETAIL: &str =
    "this layout is registered as an M/E source; take it off Program, Preview and every mixer \
     input, then retry after 10 s";

struct Entry {
    revision: u64,
    /// The registered body (what the engine composes): a CUT/AUTO of it or a
    /// select of an input patched to it authorizes this body's closure and
    /// sends its revision (#307 review A1).
    body: LayoutBody,
    leases: Vec<Box<dyn SourceDemandLease>>,
    last_referenced: Instant,
}

/// The per-layout lifecycle fences (#307 review A3/M3): the
/// [`next_layout_revision`](crate::output_routing::next_layout_revision)
/// taken after the last committed edit or delete of each layout. A body
/// whose revision is not above its layout's fence was read before that
/// commit and is never registered. Bounded at [`MAX_LAYOUT_FENCES`]: the
/// smallest is evicted and raises `floor`, the fence of every id without
/// one (so eviction only ever refuses more, never less).
#[derive(Debug, Default)]
struct Fences {
    by_id: HashMap<String, u64>,
    floor: u64,
}

impl Fences {
    fn get(&self, id: &str) -> u64 {
        self.by_id
            .get(id)
            .copied()
            .unwrap_or(self.floor)
            .max(self.floor)
    }

    fn set(&mut self, id: &str, value: u64) {
        if let Some(existing) = self.by_id.get_mut(id) {
            *existing = value;
            return;
        }
        if self.by_id.len() >= MAX_LAYOUT_FENCES {
            let smallest = self
                .by_id
                .iter()
                .min_by_key(|(_, v)| **v)
                .map(|(k, v)| (k.clone(), *v));
            if let Some((key, evicted)) = smallest {
                self.by_id.remove(&key);
                self.floor = self.floor.max(evicted);
            }
        }
        self.by_id.insert(id.to_owned(), value);
    }
}

struct Inner {
    entries: HashMap<String, Entry>,
    fences: Fences,
}

/// The control-side owner of the registered set.
pub struct MeLayoutRegistry {
    sources: SharedMeLayoutSources,
    state: Mutex<Inner>,
}

/// An edit or delete of one layout in progress (#307 review A3/M3): while
/// it lives, every registration of the layout is refused with
/// [`MeLayoutError::Changed`]. Dropping it (after the commit, or on any
/// early return) sets the layout's fence to a fresh
/// [`next_layout_revision`](crate::output_routing::next_layout_revision), so
/// a body read before the commit stays refused afterwards.
#[must_use = "the fence is completed when this guard drops; hold it across the commit"]
pub struct LayoutFence<'a> {
    registry: &'a MeLayoutRegistry,
    id: String,
    registered: bool,
}

impl LayoutFence<'_> {
    /// Whether the layout was registered as an M/E source when the fence
    /// was taken (an edit then needs the M/E authorization and swaps the
    /// body after its commit).
    #[must_use]
    pub const fn registered(&self) -> bool {
        self.registered
    }
}

impl std::fmt::Debug for LayoutFence<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutFence")
            .field("id", &self.id)
            .field("registered", &self.registered)
            .finish()
    }
}

impl Drop for LayoutFence<'_> {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        let revision = crate::output_routing::next_layout_revision();
        inner.fences.set(&self.id, revision);
    }
}

impl std::fmt::Debug for MeLayoutRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeLayoutRegistry").finish_non_exhaustive()
    }
}

fn bound_sources(body: &LayoutBody) -> Vec<String> {
    let mut sources: Vec<String> = body
        .0
        .layout
        .cells
        .iter()
        .filter_map(|cell| cell.source.clone())
        .collect();
    sources.sort_unstable();
    sources.dedup();
    sources
}

fn lease_all(
    demand: Option<&SharedSourceDemand>,
    id: &str,
    body: &LayoutBody,
) -> Result<Vec<Box<dyn SourceDemandLease>>, MeLayoutError> {
    let Some(demand) = demand else {
        return Ok(Vec::new());
    };
    let label = format!("switcher:me1:{}", source_key(id));
    let mut leases = Vec::new();
    for source in bound_sources(body) {
        // A refusal drops (releases) every lease taken so far.
        leases.push(
            demand
                .switcher_preview_lease(&source, &label)
                .ok_or(MeLayoutError::Lease)?,
        );
    }
    Ok(leases)
}

impl MeLayoutRegistry {
    /// A registry over the engine side `sources`.
    #[must_use]
    pub fn new(sources: SharedMeLayoutSources) -> Self {
        Self {
            sources,
            state: Mutex::new(Inner {
                entries: HashMap::new(),
                fences: Fences::default(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lifecycle fence of layout `id` now: a body whose revision is not
    /// above it is refused by [`Self::ensure`] (`u64::MAX` while an edit or
    /// delete of it is committing).
    #[must_use]
    pub fn fence(&self, id: &str) -> u64 {
        self.lock().fences.get(id)
    }

    /// Begin an edit of layout `id`: from now until the returned guard
    /// drops (after the commit) no body of it is registered, and afterwards
    /// only a body read after the commit. Take it BEFORE checking whether
    /// the layout is registered and before the repository commit.
    pub fn begin_edit(&self, id: &str) -> LayoutFence<'_> {
        let mut inner = self.lock();
        inner.fences.set(id, PENDING);
        let registered = inner.entries.contains_key(id);
        drop(inner);
        LayoutFence {
            registry: self,
            id: id.to_owned(),
            registered,
        }
    }

    /// Begin a delete of layout `id`, before the repository commit.
    ///
    /// # Errors
    /// [`MeLayoutError::OnAir`] when the layout is registered as an M/E
    /// source at all (referenced or lingering); nothing changes then.
    pub fn begin_delete(&self, id: &str) -> Result<LayoutFence<'_>, MeLayoutError> {
        let mut inner = self.lock();
        if inner.entries.contains_key(id) {
            return Err(MeLayoutError::OnAir);
        }
        inner.fences.set(id, PENDING);
        drop(inner);
        Ok(LayoutFence {
            registry: self,
            id: id.to_owned(),
            registered: false,
        })
    }

    /// Register `body` (a resolved, admissible layout) if it is not yet, or
    /// refresh its reference instant. A body resolved after the registered
    /// one (higher revision) replaces it. Leases are taken make-before-break.
    ///
    /// The engine adapter is called while the entries lock is held (lock
    /// order: entries → adapter writer; the adapter never calls back), so a
    /// registration can never interleave with a removal of the same id.
    ///
    /// Returns the revision of the registered body afterwards: `body`'s own
    /// revision, or a higher one when a newer body was already registered
    /// (then `body` was not adopted; the caller authorized `body` only).
    ///
    /// # Errors
    /// [`MeLayoutError::Changed`] (the body predates the layout's fence),
    /// [`MeLayoutError::Capacity`] (full) or [`MeLayoutError::Lease`];
    /// nothing changes then.
    pub fn ensure(
        &self,
        body: LayoutBody,
        demand: Option<&SharedSourceDemand>,
        now: Instant,
    ) -> Result<u64, MeLayoutError> {
        let id = body.0.id.clone();
        let mut inner = self.lock();
        // A body read before a committed (or committing) edit/delete of
        // this layout is stale: refused, checked under the insert's lock.
        if body.0.revision <= inner.fences.get(&id) {
            return Err(MeLayoutError::Changed);
        }
        let entries = &mut inner.entries;
        if let Some(entry) = entries.get_mut(&id) {
            entry.last_referenced = now;
            // Already registered: a newer read swaps the body like an edit.
            self.swap_body(&id, entry, body, demand);
            return Ok(entry.revision);
        }
        if entries.len() >= MAX_ME_LAYOUT_SOURCES {
            return Err(MeLayoutError::Capacity);
        }
        let leases = lease_all(demand, &id, &body)?;
        let revision = body.0.revision;
        self.sources
            .register(body.clone())
            .map_err(|MeLayoutRefusal::Capacity| MeLayoutError::Capacity)?;
        entries.insert(
            id,
            Entry {
                revision,
                body,
                leases,
                last_referenced: now,
            },
        );
        Ok(revision)
    }

    /// Swap a registered entry's body (caller holds the entries lock).
    fn swap_body(
        &self,
        id: &str,
        entry: &mut Entry,
        body: LayoutBody,
        demand: Option<&SharedSourceDemand>,
    ) {
        if body.0.revision <= entry.revision {
            return;
        }
        if let Ok(leases) = lease_all(demand, id, &body) {
            // Make-before-break: the old leases drop here, after the new ones.
            entry.leases = leases;
        } else {
            tracing::warn!(
                layout = %id,
                "M/E layout source: a source of the edited layout could not be leased; \
                 it may show its slate until it runs"
            );
        }
        entry.revision = body.0.revision;
        entry.body = body.clone();
        if self.sources.register(body).is_err() {
            tracing::warn!(layout = %id, "M/E layout source: the edited body was not adopted");
        }
    }

    /// After a committed edit of a registered layout: swap its body at the
    /// next tick (ignored for an unregistered id or an older revision). A
    /// lease the new body needs and cannot get is logged; the edit is
    /// already committed, so the swap still happens.
    pub fn update_body(&self, body: LayoutBody, demand: Option<&SharedSourceDemand>) {
        let id = body.0.id.clone();
        let mut inner = self.lock();
        if let Some(entry) = inner.entries.get_mut(&id) {
            self.swap_body(&id, entry, body, demand);
        }
    }

    /// Unregister `id` and release its leases; `true` if it was registered.
    /// The entry removal and the engine unregister are one critical section.
    pub fn remove(&self, id: &str) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.remove(id) else {
            return false;
        };
        self.sources.unregister(id);
        // The leases are released after the engine stopped composing it.
        drop(entry);
        true
    }

    /// Wait (bounded by `limit`, polling off the clock) until the output
    /// clock adopted the latest registration; `false` on timeout.
    pub async fn adopted_within(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if self.sources.is_adopted() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The registered body of `id` (what the engine composes), if any.
    #[must_use]
    pub fn registered_body(&self, id: &str) -> Option<LayoutBody> {
        self.lock().entries.get(id).map(|entry| entry.body.clone())
    }

    /// Whether `id` is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.lock().entries.contains_key(id)
    }

    /// The registered layout ids, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().entries.keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    /// Drop (unregister and release) every entry the M/E has not referenced
    /// for [`ME_LAYOUT_LINGER`]; `referenced(id)` is asked with the layout id.
    /// Returns how many were dropped.
    pub fn sweep(&self, referenced: impl Fn(&str) -> bool, now: Instant) -> usize {
        let mut inner = self.lock();
        let entries = &mut inner.entries;
        let mut dropped = Vec::new();
        for (id, entry) in entries.iter_mut() {
            if referenced(id) {
                entry.last_referenced = now;
            } else if now.saturating_duration_since(entry.last_referenced) >= ME_LAYOUT_LINGER {
                dropped.push(id.clone());
            }
        }
        // Still under the entries lock: a concurrent `ensure` of the same id
        // waits, so this sweep can never unregister its fresh registration.
        // Each entry's leases are released after its engine unregister.
        for id in &dropped {
            let entry = entries.remove(id);
            self.sources.unregister(id);
            drop(entry);
        }
        drop(inner);
        dropped.len()
    }
}

/// Whether the M/E references `layout:<id>` now: its Program or Preview
/// source, or a numbered input patched to it (the engine's readback).
#[must_use]
pub fn me_references(
    control: &multiview_engine::switcher_control::SwitcherControl,
    id: &str,
) -> bool {
    let key = source_key(id);
    let on_bus = control
        .snapshot()
        .is_some_and(|s| s.program.as_str() == key || s.preview.as_str() == key);
    on_bus
        || control.inputs().is_some_and(|table| {
            table
                .inputs
                .iter()
                .any(|input| input.source.as_ref().is_some_and(|s| s.as_str() == key))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn fences_evict_the_smallest_and_raise_the_floor() {
        let mut fences = Fences::default();
        assert_eq!(fences.get("never"), 0);
        for (i, value) in (0..MAX_LAYOUT_FENCES).zip(100_u64..) {
            fences.set(&format!("l{i}"), value);
        }
        assert_eq!(fences.by_id.len(), MAX_LAYOUT_FENCES);
        assert_eq!(fences.floor, 0);
        // A new id evicts the smallest (l0 = 100) and raises the floor.
        fences.set("fresh", 5_000);
        assert_eq!(fences.by_id.len(), MAX_LAYOUT_FENCES);
        assert_eq!(fences.floor, 100);
        assert!(!fences.by_id.contains_key("l0"));
        // The evicted id and an id never fenced both fall back to the floor.
        assert_eq!(fences.get("l0"), 100);
        assert_eq!(fences.get("never"), 100);
        assert_eq!(fences.get("fresh"), 5_000);
        // Updating an existing id never evicts.
        fences.set("l1", 7_000);
        assert_eq!(fences.by_id.len(), MAX_LAYOUT_FENCES);
        assert_eq!(fences.floor, 100);
        assert_eq!(fences.get("l1"), 7_000);
        // A kept fence below a raised floor reads as the floor.
        fences.set("l2", 1);
        fences.floor = 150;
        assert_eq!(fences.get("l2"), 150);
    }
}
