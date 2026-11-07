//! Resource scopes: the `{owner, generation, kind}` triple every browser,
//! encoder and decoder process registers with (ADR-0099 decision 8).
//!
//! A **generation** is the anti-misattribution device. A scope is removed when
//! its browser dies, and the next browser for the same owner starts a new
//! generation. A sample that was taken for an older generation (or for a scope
//! that has since been removed) is therefore *stale* — charged to nobody —
//! rather than charged to whatever now occupies the owner's name. Losing a
//! sample is strictly better than attributing a browser's CPU to the wrong
//! source, which is the accounting bug the ADR's measurements called out.
//!
//! The registry holds no clock and performs no I/O: it is a pure map with an
//! explicit [`ScopeRegistry::next_generation`], so it is trivially testable and
//! safe to share behind a lock owned by a later slice.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::fsroot::Pid;

/// The owner a scope's resources are charged to: a source id, a job id, a pool
/// id or a fixed engine id such as `"engine"`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ScopeOwner(String);

impl ScopeOwner {
    /// Name an owner.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The owner's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScopeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A monotonically increasing per-owner generation counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Generation(u64);

impl Generation {
    /// The first generation of an owner.
    pub const FIRST: Self = Self(0);

    /// Wrap a raw counter value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next generation, saturating at [`u64::MAX`] rather than wrapping (a
    /// wrapped counter could collide with a live generation and re-admit a stale
    /// sample).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What a scope's accounting reads from.
///
/// `#[non_exhaustive]`: [`Pool`](Self::Pool) is the first of the variants
/// ADR-0099 decision 11 plans (pooled sources), and consumers must already
/// carry a wildcard arm.
///
/// **Exactness is a property of the backing, never of the variant.** Use
/// [`ScopeKind::is_exact`] rather than matching on the variant: a
/// [`Cgroup`](Self::Cgroup) is exact, a [`ProcessTree`](Self::ProcessTree) is
/// estimated, and a [`Pool`](Self::Pool) is exactly as exact as the backing it
/// carries — which is how decision 11's "pool figures are exact" is meant.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum ScopeKind {
    /// A delegated cgroup v2 directory: exact kernel accounting.
    Cgroup(PathBuf),
    /// A process tree rooted at a pid: estimated accounting, reaped by walking.
    ProcessTree {
        /// The root pid the tree is discovered from.
        root: Pid,
        /// The root's `starttime` (field 22 of `/proc/<pid>/stat`) **at
        /// registration**, read by the caller right after spawning the process.
        ///
        /// A pid alone does not identify a process: the kernel recycles pids, so
        /// a browser that died between registration and the first sample could
        /// have its pid, its whole tree and its CPU charged to this scope. The
        /// sampler compares this value on **every** sample, the first included,
        /// and reports [`RootGone`](crate::TreeSample::RootGone) on a mismatch.
        ///
        /// `0` means the caller could not read the field; the check then degrades
        /// to the pid alone, which is weaker but not wrong-by-construction.
        /// [`ExpectedRoot::read`](crate::process_tree::ExpectedRoot::read) is the
        /// way to obtain a value that is never silently `0`.
        root_starttime: u64,
    },
    /// A named browser pool (ADR-0099 decision 11): several sources share one
    /// browser, so the pool is accounted for **through its backing**, and each
    /// member's share of that is estimated.
    ///
    /// The pool is **not** exact by itself. Decision 11 says the pool's figures
    /// are exact — and it is, when the pool's browser runs in its own delegated
    /// cgroup (`backing: Box::new(ScopeKind::Cgroup(path))`), which is the only
    /// way the kernel charges every descendant it actually scheduled. A pool
    /// backed by a process-tree walk is an estimate like any other walk, however
    /// many members share it, so the reading is labelled accordingly. Carrying
    /// the backing (rather than a bare name) is what keeps [`is_exact`] honest
    /// instead of a claim about the deployment's configuration.
    ///
    /// [`is_exact`]: Self::is_exact
    Pool {
        /// The pool's stable name.
        id: String,
        /// Where the pool's figures come from.
        backing: Box<Self>,
    },
}

impl ScopeKind {
    /// Whether samples from this kind are kernel-exact.
    ///
    /// A cgroup charges every descendant the kernel actually scheduled; a
    /// process-tree walk can miss a short-lived grandchild that exited between
    /// two reads, so it is always labelled estimated (ADR-0099 decision 8). A
    /// pool is exact exactly when its backing is — decision 11's "pool figures
    /// are exact" reads as **exact when the pool has its own cgroup**, never as
    /// exact by virtue of being pooled.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        match self {
            Self::Cgroup(_) => true,
            Self::ProcessTree { .. } => false,
            Self::Pool { backing, .. } => backing.is_exact(),
        }
    }

    /// A named pool accounted for through its own delegated cgroup: the exact
    /// shape ADR-0099 decision 11 describes.
    #[must_use]
    pub fn exact_pool(id: impl Into<String>, cgroup: PathBuf) -> Self {
        Self::Pool {
            id: id.into(),
            backing: Box::new(Self::Cgroup(cgroup)),
        }
    }

    /// A named pool with no cgroup of its own: every figure is a process-tree
    /// estimate, including the pool total.
    #[must_use]
    pub fn estimated_pool(id: impl Into<String>, root: Pid) -> Self {
        Self::estimated_pool_at(id, root, 0)
    }

    /// The same as [`estimated_pool`](Self::estimated_pool), carrying the root's
    /// registration-time `starttime` so the backing walk has a real identity to
    /// compare against.
    #[must_use]
    pub fn estimated_pool_at(id: impl Into<String>, root: Pid, root_starttime: u64) -> Self {
        Self::Pool {
            id: id.into(),
            backing: Box::new(Self::ProcessTree {
                root,
                root_starttime,
            }),
        }
    }

    /// The root pid of a process-tree-backed scope, if that is what this is.
    ///
    /// `None` for a cgroup, and for a pool whose backing is a cgroup. A nested
    /// pool of a pool still resolves, because the backing chain is followed.
    #[must_use]
    pub fn process_tree_root(&self) -> Option<Pid> {
        match self {
            Self::Cgroup(_) => None,
            Self::ProcessTree { root, .. } => Some(*root),
            Self::Pool { backing, .. } => backing.process_tree_root(),
        }
    }

    /// The registration-time `starttime` of a process-tree-backed scope, if that
    /// is what this is. See [`ProcessTree`](Self::ProcessTree)'s field doc for
    /// what a `0` means.
    #[must_use]
    pub fn process_tree_root_starttime(&self) -> Option<u64> {
        match self {
            Self::Cgroup(_) => None,
            Self::ProcessTree { root_starttime, .. } => Some(*root_starttime),
            Self::Pool { backing, .. } => backing.process_tree_root_starttime(),
        }
    }
}

/// One registered resource scope.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ResourceScope {
    /// Who the resources are charged to.
    pub owner: ScopeOwner,
    /// The anti-misattribution generation for that owner.
    pub generation: Generation,
    /// Where the accounting comes from.
    pub kind: ScopeKind,
}

impl ResourceScope {
    /// Register-shape constructor.
    #[must_use]
    pub const fn new(owner: ScopeOwner, generation: Generation, kind: ScopeKind) -> Self {
        Self {
            owner,
            generation,
            kind,
        }
    }
}

/// The live scope set, keyed by owner.
///
/// One scope per owner at a time: registering a new generation for an owner
/// **replaces** the previous one, which is what makes an in-flight sample for
/// the old generation drop on arrival.
#[derive(Clone, Debug, Default)]
pub struct ScopeRegistry {
    scopes: BTreeMap<ScopeOwner, ResourceScope>,
    next: BTreeMap<ScopeOwner, Generation>,
}

impl ScopeRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The generation a new scope for `owner` should carry.
    ///
    /// [`Generation::FIRST`] for an owner that has never registered, then
    /// strictly increasing. Purely observational: it does not reserve the
    /// generation, so two callers racing must serialise their
    /// [`register`](Self::register) calls (the registry is the serialisation
    /// point, as it will be behind one lock).
    #[must_use]
    pub fn next_generation(&self, owner: &ScopeOwner) -> Generation {
        self.next.get(owner).copied().unwrap_or(Generation::FIRST)
    }

    /// Register `scope` and return the generation it carries.
    ///
    /// Replaces any scope currently registered for the same owner. The
    /// generation must be at least the owner's [`next_generation`](Self::next_generation):
    /// a lower one is refused, because accepting it would let a stale scope
    /// displace a live one and the anti-misattribution guard would then accept
    /// samples for the wrong browser.
    ///
    /// # Errors
    ///
    /// [`Error::StaleGeneration`] when `scope.generation` is below the next
    /// generation this registry would issue for that owner.
    pub fn register(&mut self, scope: ResourceScope) -> Result<Generation> {
        let expected = self.next_generation(&scope.owner);
        if scope.generation < expected {
            return Err(Error::StaleGeneration {
                owner: scope.owner.as_str().to_owned(),
                requested: scope.generation.get(),
                expected: expected.get(),
            });
        }
        let generation = scope.generation;
        self.next.insert(scope.owner.clone(), generation.next());
        self.scopes.insert(scope.owner.clone(), scope);
        Ok(generation)
    }

    /// Remove the scope for `owner`, returning it when one was registered.
    pub fn remove(&mut self, owner: &ScopeOwner) -> Option<ResourceScope> {
        self.scopes.remove(owner)
    }

    /// The currently registered scope for `owner`.
    #[must_use]
    pub fn get(&self, owner: &ScopeOwner) -> Option<&ResourceScope> {
        self.scopes.get(owner)
    }

    /// How many scopes are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.scopes.len()
    }

    /// Whether no scope is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    /// Every registered scope, ordered by owner.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ResourceScope> {
        self.scopes.values().cloned().collect()
    }

    /// Whether a sample tagged `{owner, generation}` may still be attributed.
    ///
    /// `false` — and the sample must be dropped, never charged — when the owner
    /// has no registered scope, when the scope has since been removed, or when
    /// it has moved on to a newer generation.
    #[must_use]
    pub fn accepts(&self, owner: &ScopeOwner, generation: Generation) -> bool {
        self.scopes
            .get(owner)
            .is_some_and(|scope| scope.generation == generation)
    }

    /// Admit a sample for `{owner, generation}`, or drop it.
    ///
    /// Staleness is **not** an error: a dropped late sample is the normal
    /// outcome of a browser that restarted, and the whole point of the
    /// generation counter is that it is charged to nobody rather than to the
    /// owner's next browser. A caller that needs to distinguish the two reads
    /// [`accepts`](Self::accepts) first.
    ///
    /// # Errors
    ///
    /// Infallible today; the `Result` shape keeps the call site identical when a
    /// later slice adds a fallible step (for example re-reading a cgroup path
    /// that has been removed).
    pub fn admit(
        &self,
        owner: &ScopeOwner,
        generation: Generation,
    ) -> Result<Option<&ResourceScope>> {
        if self.accepts(owner, generation) {
            Ok(self.scopes.get(owner))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(name: &str) -> ScopeOwner {
        ScopeOwner::new(name)
    }

    fn cgroup_scope(name: &str, generation: u64, path: &str) -> ResourceScope {
        ResourceScope::new(
            owner(name),
            Generation::new(generation),
            ScopeKind::Cgroup(PathBuf::from(path)),
        )
    }

    #[test]
    fn register_allocates_generations_per_owner() {
        let mut registry = ScopeRegistry::new();
        let src = owner("vt-1");
        assert_eq!(registry.next_generation(&src), Generation::FIRST);
        let first = registry.next_generation(&src);
        assert_eq!(
            registry
                .register(cgroup_scope("vt-1", first.get(), "/sys/fs/cgroup/engine"))
                .expect("the next generation is accepted"),
            first
        );
        assert_eq!(registry.next_generation(&src), Generation::new(1));
        let second = registry.next_generation(&src);
        assert_eq!(
            registry
                .register(cgroup_scope("vt-1", second.get(), "/sys/fs/cgroup/engine"))
                .expect("monotonic"),
            second
        );
        assert_eq!(
            registry.next_generation(&src).get(),
            2,
            "two generations issued"
        );
    }

    #[test]
    fn a_generation_below_the_next_expected_is_refused() {
        let mut registry = ScopeRegistry::new();
        let src = owner("vt-1");
        let issued = registry.next_generation(&src);
        registry
            .register(cgroup_scope("vt-1", issued.get(), "/sys/fs/cgroup/engine"))
            .expect("first registration");

        // Re-registering the same generation would replace a live scope with a
        // stale one, so it is refused rather than silently applied.
        let error = registry
            .register(cgroup_scope("vt-1", issued.get(), "/sys/fs/cgroup/engine"))
            .expect_err("a stale generation is refused");
        assert!(
            error.to_string().contains("stale generation"),
            "the error names the problem: {error}"
        );
        assert_eq!(
            registry.get(&src).map(|scope| scope.generation),
            Some(issued),
            "the refused registration left the live scope alone"
        );

        // A caller that follows `next_generation` is always accepted.
        let next = registry.next_generation(&src);
        assert!(next > issued);
        assert_eq!(
            registry
                .register(cgroup_scope("vt-1", next.get(), "/sys/fs/cgroup/engine"))
                .expect("the next generation is accepted"),
            next
        );
    }

    #[test]
    fn a_sample_for_a_removed_scope_is_dropped() {
        let mut registry = ScopeRegistry::new();
        let src = owner("vt-1");
        let issued = registry.next_generation(&src);
        registry
            .register(cgroup_scope("vt-1", issued.get(), "/sys/fs/cgroup/engine"))
            .expect("registered");
        assert!(registry.accepts(&src, issued));
        registry.remove(&src);
        assert!(!registry.accepts(&src, issued));
        assert!(registry.admit(&src, issued).expect("no io").is_none());
    }

    #[test]
    fn a_late_sample_for_an_older_generation_is_dropped() {
        let mut registry = ScopeRegistry::new();
        let src = owner("vt-1");
        let older = registry.next_generation(&src);
        registry
            .register(cgroup_scope("vt-1", older.get(), "/sys/fs/cgroup/engine"))
            .expect("registered");
        let newer = registry.next_generation(&src);
        registry
            .register(cgroup_scope("vt-1", newer.get(), "/sys/fs/cgroup/engine"))
            .expect("registered");
        assert!(!registry.accepts(&src, older), "old generation is stale");
        assert!(
            registry.accepts(&src, newer),
            "current generation is attributed"
        );
        assert_eq!(registry.len(), 1, "one scope per owner");
    }

    #[test]
    fn snapshot_is_ordered_by_owner_and_carries_the_kind() {
        let mut registry = ScopeRegistry::new();
        registry
            .register(ResourceScope::new(
                owner("b"),
                Generation::new(0),
                ScopeKind::ProcessTree {
                    root: Pid::new(10),
                    root_starttime: 4_242,
                },
            ))
            .expect("registered");
        registry
            .register(cgroup_scope("a", 3, "/sys/fs/cgroup/engine"))
            .expect("registered");
        let snapshot = registry.snapshot();
        let names: Vec<&str> = snapshot.iter().map(|s| s.owner.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(snapshot.first().is_some_and(|s| s.kind.is_exact()));
        assert!(snapshot.get(1).is_some_and(|s| !s.kind.is_exact()));
    }

    #[test]
    fn a_pool_is_exact_only_when_its_backing_is() {
        // ADR-0099 decision 11 reads as "exact when the pool has its own
        // cgroup": the backing, not the pooling, is what makes a figure exact.
        let mut registry = ScopeRegistry::new();
        let pool = owner("pool-1");
        let issued = registry.next_generation(&pool);
        registry
            .register(ResourceScope::new(
                pool.clone(),
                issued,
                ScopeKind::exact_pool("sydney", PathBuf::from("/sys/fs/cgroup/engine/pool")),
            ))
            .expect("registered");
        let scope = registry.get(&pool).expect("a registered pool");
        assert!(
            scope.kind.is_exact(),
            "a pool with its own cgroup has exact figures"
        );
        assert_eq!(
            scope.kind,
            ScopeKind::Pool {
                id: "sydney".to_owned(),
                backing: Box::new(ScopeKind::Cgroup(PathBuf::from(
                    "/sys/fs/cgroup/engine/pool"
                ))),
            }
        );

        // The same pool, backed by a walk: pooled is not the same as exact.
        let walked = owner("pool-2");
        let generation = registry.next_generation(&walked);
        registry
            .register(ResourceScope::new(
                walked.clone(),
                generation,
                ScopeKind::estimated_pool("sydney", Pid::new(400)),
            ))
            .expect("registered");
        let scope = registry.get(&walked).expect("a registered pool");
        assert!(
            !scope.kind.is_exact(),
            "a pool with no cgroup of its own is an estimate, however many \
             sources share it"
        );
    }

    #[test]
    fn an_exact_pool_never_reports_itself_exact_by_construction_alone() {
        // The guard is the backing's `is_exact`, whatever shape it takes: a
        // process-tree pool stays estimated even nested behind the pool variant.
        for kind in [
            ScopeKind::estimated_pool("p", Pid::new(1)),
            ScopeKind::Pool {
                id: "p".to_owned(),
                backing: Box::new(ScopeKind::estimated_pool("inner", Pid::new(2))),
            },
        ] {
            assert!(!kind.is_exact(), "{kind:?}");
        }
        assert!(ScopeKind::exact_pool("p", PathBuf::from("/x")).is_exact());
        assert!(
            ScopeKind::Cgroup(PathBuf::from("/x")).is_exact(),
            "a cgroup is exact"
        );
    }

    #[test]
    fn generation_next_saturates_instead_of_wrapping() {
        assert_eq!(Generation::new(u64::MAX).next(), Generation::new(u64::MAX));
    }
}
