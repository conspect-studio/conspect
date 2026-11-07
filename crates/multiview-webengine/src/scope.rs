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

use crate::error::Result;
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ScopeKind {
    /// A delegated cgroup v2 directory: exact kernel accounting.
    Cgroup(PathBuf),
    /// A process tree rooted at a pid: estimated accounting, reaped by walking.
    ProcessTree {
        /// The root pid the tree is discovered from.
        root: Pid,
    },
}

impl ScopeKind {
    /// Whether samples from this kind are kernel-exact.
    ///
    /// A cgroup charges every descendant the kernel actually scheduled; a
    /// process-tree walk can miss a short-lived grandchild that exited between
    /// two reads, so it is always labelled estimated (ADR-0099 decision 8).
    #[must_use]
    pub fn is_exact(&self) -> bool {
        matches!(self, Self::Cgroup(_))
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
    /// Replaces any scope currently registered for the same owner.
    pub fn register(&mut self, scope: ResourceScope) -> Generation {
        let generation = scope.generation;
        self.next.insert(scope.owner.clone(), generation.next());
        self.scopes.insert(scope.owner.clone(), scope);
        generation
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
        assert_eq!(
            registry.register(cgroup_scope("vt-1", 0, "/sys/fs/cgroup/engine")),
            Generation::FIRST
        );
        assert_eq!(registry.next_generation(&src), Generation::new(1));
        assert_eq!(
            registry.register(cgroup_scope("vt-1", 1, "/sys/fs/cgroup/engine")),
            Generation::new(1)
        );
        assert_eq!(
            registry.next_generation(&src).get(),
            2,
            "two generations issued"
        );
    }

    #[test]
    fn a_sample_for_a_removed_scope_is_dropped() {
        let mut registry = ScopeRegistry::new();
        let src = owner("vt-1");
        registry.register(cgroup_scope("vt-1", 0, "/sys/fs/cgroup/engine"));
        assert!(registry.accepts(&src, Generation::new(0)));
        registry.remove(&src);
        assert!(!registry.accepts(&src, Generation::new(0)));
        assert!(registry
            .admit(&src, Generation::new(0))
            .expect("no io")
            .is_none());
    }

    #[test]
    fn a_late_sample_for_an_older_generation_is_dropped() {
        let mut registry = ScopeRegistry::new();
        let src = owner("vt-1");
        registry.register(cgroup_scope("vt-1", 0, "/sys/fs/cgroup/engine"));
        registry.register(cgroup_scope("vt-1", 1, "/sys/fs/cgroup/engine"));
        assert!(
            !registry.accepts(&src, Generation::new(0)),
            "old generation is stale"
        );
        assert!(
            registry.accepts(&src, Generation::new(1)),
            "current generation is attributed"
        );
        assert_eq!(registry.len(), 1, "one scope per owner");
    }

    #[test]
    fn snapshot_is_ordered_by_owner_and_carries_the_kind() {
        let mut registry = ScopeRegistry::new();
        registry.register(ResourceScope::new(
            owner("b"),
            Generation::new(0),
            ScopeKind::ProcessTree { root: Pid::new(10) },
        ));
        registry.register(cgroup_scope("a", 3, "/sys/fs/cgroup/engine"));
        let snapshot = registry.snapshot();
        let names: Vec<&str> = snapshot.iter().map(|s| s.owner.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(snapshot.first().is_some_and(|s| s.kind.is_exact()));
        assert!(snapshot.get(1).is_some_and(|s| !s.kind.is_exact()));
    }

    #[test]
    fn generation_next_saturates_instead_of_wrapping() {
        assert_eq!(Generation::new(u64::MAX).next(), Generation::new(u64::MAX));
    }
}
