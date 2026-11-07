//! The scope registry: registration, removal, snapshot, and the generation
//! guard that drops a late sample (ADR-0099 decision 8).
//!
//! This is the anti-misattribution device. A browser that dies and is replaced
//! must not have the *first* browser's final sample charged to the second one,
//! so a sample tagged with a generation the registry has moved past is dropped
//! rather than attributed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use std::path::PathBuf;

use multiview_webengine::{Generation, Pid, ResourceScope, ScopeKind, ScopeOwner, ScopeRegistry};

fn owner(name: &str) -> ScopeOwner {
    ScopeOwner::new(name)
}

fn cgroup(name: &str, generation: u64) -> ResourceScope {
    ResourceScope::new(
        owner(name),
        Generation::new(generation),
        ScopeKind::Cgroup(PathBuf::from(format!(
            "/sys/fs/cgroup/engine/web/{name}-{generation}"
        ))),
    )
}

#[test]
fn a_registry_snapshots_every_owner_with_its_kind() {
    let mut registry = ScopeRegistry::new();
    assert!(registry.is_empty());
    registry.register(cgroup("vt-1", 0));
    registry.register(ResourceScope::new(
        owner("vt-2"),
        Generation::new(0),
        ScopeKind::ProcessTree {
            root: Pid::new(4242),
        },
    ));

    assert_eq!(registry.len(), 2);
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 2);
    let first = snapshot.first().expect("ordered by owner");
    assert_eq!(first.owner.as_str(), "vt-1");
    assert!(first.kind.is_exact(), "a cgroup scope is exact");
    let second = snapshot.get(1).expect("the second owner");
    assert_eq!(second.owner.as_str(), "vt-2");
    assert!(!second.kind.is_exact(), "a process tree is estimated");
    assert_eq!(
        second.kind,
        ScopeKind::ProcessTree {
            root: Pid::new(4242)
        }
    );
}

#[test]
fn one_scope_per_owner_and_the_generation_advances() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    assert_eq!(registry.next_generation(&src), Generation::FIRST);
    registry.register(cgroup("vt-1", 0));
    assert_eq!(registry.next_generation(&src), Generation::new(1));
    registry.register(cgroup("vt-1", 1));
    assert_eq!(registry.len(), 1, "one scope per owner, not a history");
    assert_eq!(registry.next_generation(&src).get(), 2, "two issued");
}

#[test]
fn a_late_sample_for_a_removed_scope_is_dropped() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    registry.register(cgroup("vt-1", 0));
    assert!(registry.accepts(&src, Generation::new(0)));

    let removed = registry.remove(&src).expect("a registered scope");
    assert_eq!(removed.generation, Generation::new(0));
    assert!(registry.is_empty());
    assert!(!registry.accepts(&src, Generation::new(0)));
    assert!(registry
        .admit(&src, Generation::new(0))
        .expect("infallible")
        .is_none());
    assert!(registry.remove(&src).is_none(), "removal is idempotent");
}

#[test]
fn a_late_sample_for_an_older_generation_is_dropped() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    registry.register(cgroup("vt-1", 0));
    registry.register(cgroup("vt-1", 1));

    assert!(
        !registry.accepts(&src, Generation::new(0)),
        "the first browser's sample must not be charged to the second"
    );
    assert!(registry.accepts(&src, Generation::new(1)));
    assert_eq!(
        registry.get(&src).map(|scope| scope.generation),
        Some(Generation::new(1))
    );
    assert!(registry
        .admit(&src, Generation::new(0))
        .expect("infallible")
        .is_none());
    assert!(registry
        .admit(&src, Generation::new(1))
        .expect("infallible")
        .is_some());
}

#[test]
fn a_never_registered_owner_accepts_nothing() {
    let registry = ScopeRegistry::new();
    assert!(!registry.accepts(&owner("ghost"), Generation::FIRST));
    assert!(registry.get(&owner("ghost")).is_none());
}

#[test]
fn the_registry_is_keyed_by_owner_not_by_path() {
    // Two generations of the same owner can share a cgroup path (a retry that
    // reused the directory); generation, not the path, is what guards them.
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    let same_path = ResourceScope::new(
        src.clone(),
        Generation::new(0),
        ScopeKind::Cgroup(PathBuf::from("/sys/fs/cgroup/engine/web/vt-1")),
    );
    registry.register(same_path.clone());
    registry.register(ResourceScope {
        generation: Generation::new(1),
        ..same_path
    });
    assert!(!registry.accepts(&src, Generation::new(0)));
    assert!(registry.accepts(&src, Generation::new(1)));
}
