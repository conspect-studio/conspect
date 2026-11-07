//! The scope registry: registration, removal, snapshot, and the generation
//! guard that drops a late sample (ADR-0099 decision 8).
//!
//! This is the anti-misattribution device. A browser that dies and is replaced
//! must not have the *first* browser's final sample charged to the second one,
//! so a sample tagged with a generation the registry has moved past is dropped
//! rather than attributed. Registration itself is guarded too: a generation
//! below the next one the registry would issue is refused.
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

/// A cgroup scope for `name`, registered at the generation the registry issues.
fn cgroup(registry: &ScopeRegistry, name: &str) -> ResourceScope {
    let generation = registry.next_generation(&owner(name));
    ResourceScope::new(
        owner(name),
        generation,
        ScopeKind::Cgroup(PathBuf::from(format!(
            "/sys/fs/cgroup/engine/web/{name}-{generation}"
        ))),
    )
}

#[test]
fn a_registry_snapshots_every_owner_with_its_kind() {
    let mut registry = ScopeRegistry::new();
    assert!(registry.is_empty());
    registry
        .register(cgroup(&registry, "vt-1"))
        .expect("the next generation is accepted");
    let vt2 = registry.next_generation(&owner("vt-2"));
    registry
        .register(ResourceScope::new(
            owner("vt-2"),
            vt2,
            ScopeKind::ProcessTree {
                root: Pid::new(4242),
            },
        ))
        .expect("registered");

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
fn a_pool_scope_is_exact_and_never_a_process_tree() {
    // ADR-0099 decision 11: a pool's own figures are exact, only the per-member
    // split is estimated.
    let mut registry = ScopeRegistry::new();
    let pool = owner("pool-1");
    let generation = registry.next_generation(&pool);
    registry
        .register(ResourceScope::new(
            pool.clone(),
            generation,
            ScopeKind::Pool {
                id: "sydney".to_owned(),
            },
        ))
        .expect("registered");
    let scope = registry.get(&pool).expect("the pool scope");
    assert!(scope.kind.is_exact());
    assert_eq!(
        scope.kind,
        ScopeKind::Pool {
            id: "sydney".to_owned()
        }
    );
}

#[test]
fn one_scope_per_owner_and_the_generation_advances() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    assert_eq!(registry.next_generation(&src), Generation::FIRST);
    registry
        .register(cgroup(&registry, "vt-1"))
        .expect("registered");
    assert_eq!(registry.next_generation(&src), Generation::new(1));
    registry
        .register(cgroup(&registry, "vt-1"))
        .expect("registered");
    assert_eq!(registry.len(), 1, "one scope per owner, not a history");
    assert_eq!(registry.next_generation(&src).get(), 2, "two issued");
}

#[test]
fn a_generation_below_the_next_expected_is_refused() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    let issued = registry.next_generation(&src);
    registry
        .register(cgroup(&registry, "vt-1"))
        .expect("the first registration is accepted");
    assert_eq!(issued, Generation::FIRST);

    // The same generation again would replace a live scope with a stale one.
    let error = registry
        .register(cgroup_scope_at("vt-1", issued))
        .expect_err("a stale generation is refused");
    assert!(
        error.to_string().contains("stale generation"),
        "the error names the problem: {error}"
    );
    assert_eq!(
        registry.get(&src).map(|scope| scope.generation),
        Some(issued),
        "the refused registration changed nothing"
    );
    assert_eq!(registry.next_generation(&src), Generation::new(1));
}

#[test]
fn a_late_sample_for_a_removed_scope_is_dropped() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    let issued = registry.next_generation(&src);
    registry
        .register(cgroup(&registry, "vt-1"))
        .expect("registered");
    assert!(registry.accepts(&src, issued));

    let removed = registry.remove(&src).expect("a registered scope");
    assert_eq!(removed.generation, issued);
    assert!(registry.is_empty());
    assert!(!registry.accepts(&src, issued));
    assert!(registry.admit(&src, issued).expect("infallible").is_none());
    assert!(registry.remove(&src).is_none(), "removal is idempotent");
}

#[test]
fn a_late_sample_for_an_older_generation_is_dropped() {
    let mut registry = ScopeRegistry::new();
    let src = owner("vt-1");
    let older = registry.next_generation(&src);
    registry
        .register(cgroup_scope_at("vt-1", older))
        .expect("ok");
    let newer = registry.next_generation(&src);
    registry
        .register(cgroup_scope_at("vt-1", newer))
        .expect("ok");

    assert!(
        !registry.accepts(&src, older),
        "the first browser's sample must not be charged to the second"
    );
    assert!(registry.accepts(&src, newer));
    assert_eq!(
        registry.get(&src).map(|scope| scope.generation),
        Some(newer)
    );
    assert!(registry.admit(&src, older).expect("infallible").is_none());
    assert!(registry.admit(&src, newer).expect("infallible").is_some());
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
    let first = registry.next_generation(&src);
    let same_path = ResourceScope::new(
        src.clone(),
        first,
        ScopeKind::Cgroup(PathBuf::from("/sys/fs/cgroup/engine/web/vt-1")),
    );
    registry.register(same_path.clone()).expect("ok");
    let second = registry.next_generation(&src);
    registry
        .register(ResourceScope {
            generation: second,
            ..same_path
        })
        .expect("ok");
    assert!(!registry.accepts(&src, first));
    assert!(registry.accepts(&src, second));
}

/// A cgroup scope at an explicit generation, for the stale-registration test.
fn cgroup_scope_at(name: &str, generation: Generation) -> ResourceScope {
    ResourceScope::new(
        owner(name),
        generation,
        ScopeKind::Cgroup(PathBuf::from(format!(
            "/sys/fs/cgroup/engine/web/{name}-{generation}"
        ))),
    )
}
