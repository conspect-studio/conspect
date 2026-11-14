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

/// Why [`MeLayoutRegistry::ensure`] refused (both answer 409 `capacity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeLayoutError {
    /// The registry is full.
    Capacity,
    /// A bound source's demand lease was refused (unknown source or limit).
    Lease,
}

impl MeLayoutError {
    /// The operator-facing detail of the 409 `capacity` problem.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::Capacity => {
                "at most 4 stored layouts can be M/E sources at once; take one off the M/E \
                 (it is released 10 s after nothing references it)"
            }
            Self::Lease => "a source this layout shows could not be leased (lease limit)",
        }
    }
}

struct Entry {
    revision: u64,
    leases: Vec<Box<dyn SourceDemandLease>>,
    last_referenced: Instant,
}

/// The control-side owner of the registered set.
pub struct MeLayoutRegistry {
    sources: SharedMeLayoutSources,
    entries: Mutex<HashMap<String, Entry>>,
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
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register `body` (a resolved, admissible layout) if it is not yet, or
    /// refresh its reference instant. A body resolved after the registered
    /// one (higher revision) replaces it. Leases are taken make-before-break.
    ///
    /// # Errors
    /// [`MeLayoutError::Capacity`] (full) or [`MeLayoutError::Lease`];
    /// nothing changes then.
    pub fn ensure(
        &self,
        body: LayoutBody,
        demand: Option<&SharedSourceDemand>,
        now: Instant,
    ) -> Result<(), MeLayoutError> {
        let id = body.0.id.clone();
        let mut entries = self.lock();
        if let Some(entry) = entries.get_mut(&id) {
            entry.last_referenced = now;
            drop(entries);
            // Already registered: a newer read swaps the body like an edit.
            self.update_body(body, demand);
            return Ok(());
        }
        if entries.len() >= MAX_ME_LAYOUT_SOURCES {
            return Err(MeLayoutError::Capacity);
        }
        let leases = lease_all(demand, &id, &body)?;
        let revision = body.0.revision;
        self.sources
            .register(body)
            .map_err(|MeLayoutRefusal::Capacity| MeLayoutError::Capacity)?;
        entries.insert(
            id,
            Entry {
                revision,
                leases,
                last_referenced: now,
            },
        );
        Ok(())
    }

    /// After a committed edit of a registered layout: swap its body at the
    /// next tick (ignored for an unregistered id or an older revision). A
    /// lease the new body needs and cannot get is logged; the edit is
    /// already committed, so the swap still happens.
    pub fn update_body(&self, body: LayoutBody, demand: Option<&SharedSourceDemand>) {
        let id = body.0.id.clone();
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&id) else {
            return;
        };
        if body.0.revision <= entry.revision {
            return;
        }
        if let Ok(leases) = lease_all(demand, &id, &body) {
            entry.leases = leases;
        } else {
            tracing::warn!(
                layout = %id,
                "M/E layout source: a source of the edited layout could not be leased; \
                 it may show its slate until it runs"
            );
        }
        entry.revision = body.0.revision;
        if self.sources.register(body).is_err() {
            tracing::warn!(layout = %id, "M/E layout source: the edited body was not adopted");
        }
    }

    /// Unregister `id` and release its leases; `true` if it was registered.
    pub fn remove(&self, id: &str) -> bool {
        let removed = self.lock().remove(id).is_some();
        if removed {
            self.sources.unregister(id);
        }
        removed
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

    /// Whether `id` is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.lock().contains_key(id)
    }

    /// The registered layout ids, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    /// Drop (unregister and release) every entry the M/E has not referenced
    /// for [`ME_LAYOUT_LINGER`]; `referenced(id)` is asked with the layout id.
    /// Returns how many were dropped.
    pub fn sweep(&self, referenced: impl Fn(&str) -> bool, now: Instant) -> usize {
        let mut dropped = Vec::new();
        {
            let mut entries = self.lock();
            entries.retain(|id, entry| {
                if referenced(id) {
                    entry.last_referenced = now;
                    return true;
                }
                if now.saturating_duration_since(entry.last_referenced) >= ME_LAYOUT_LINGER {
                    dropped.push(id.clone());
                    return false;
                }
                true
            });
        }
        for id in &dropped {
            self.sources.unregister(id);
        }
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
