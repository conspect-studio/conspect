//! The cgroup surface, exercised entirely against fixture roots.
//!
//! Two properties are load-bearing and tested here:
//!
//! * **Nothing writes to the real system.** A tempdir (or an in-memory fake)
//!   stands in for `/sys/fs/cgroup`, and [`plan_leaf_move`] is asserted to be
//!   data — no call in this crate creates a cgroup, moves a pid or enables a
//!   controller.
//! * **`memory.peak` is only ever read.** A wrapper root records every mutating
//!   operation, and the test proves the reading path records none.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

mod common;

use std::fs;

use common::{FakeRoot, RecordingRoot};
use multiview_webengine::{
    delegation_status, plan_leaf_move, probe_child_name, probe_child_path, read_cgroup_usage,
    ControllerSet, DelegationReport, FailureAction, LeafMoveStep, NotDelegated,
    DELEGATION_PROBE_PREFIX, REQUIRED_CONTROLLERS,
};

/// A tempdir standing in for a delegated service cgroup.
struct CgroupDir {
    dir: tempfile::TempDir,
}

impl CgroupDir {
    /// Write a cgroup directory whose `cgroup.controllers` lists `controllers`,
    /// plus the four usage files the sampler reads.
    fn build(controllers: &str, peak: Option<u64>) -> Self {
        let dir = tempfile::tempdir().expect("a tempdir");
        fs::write(dir.path().join("cgroup.controllers"), controllers).expect("controllers");
        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 2500000\nuser_usec 2000000\nsystem_usec 500000\n",
        )
        .expect("cpu.stat");
        fs::write(dir.path().join("memory.current"), "1048576\n").expect("memory.current");
        fs::write(dir.path().join("pids.current"), "7\n").expect("pids.current");
        if let Some(peak) = peak {
            fs::write(dir.path().join("memory.peak"), format!("{peak}\n")).expect("memory.peak");
        }
        Self { dir }
    }

    /// The root to read this directory through.
    fn root(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// The tempdir *is* the cgroup directory, so it is the root itself.
    const RELATIVE: &'static str = "";
}

#[test]
fn a_delegated_subtree_reports_delegated() {
    let cgroup = CgroupDir::build("cpu memory pids\n", Some(2_097_152));
    let report = delegation_status(cgroup.root(), CgroupDir::RELATIVE).expect("a probe");
    assert_eq!(report, DelegationReport::Delegated);
    assert!(report.is_delegated());
    assert_eq!(report.reason(), None);
}

#[test]
fn a_missing_controller_reports_what_is_lost() {
    let cgroup = CgroupDir::build("cpu pids\n", None);
    let report = delegation_status(cgroup.root(), CgroupDir::RELATIVE).expect("a probe");
    assert!(!report.is_delegated());
    match report.reason() {
        Some(NotDelegated::ControllersMissing { present, missing }) => {
            assert!(present.contains("cpu") && present.contains("pids"));
            assert!(!present.contains("memory"));
            assert_eq!(missing.names(), vec!["memory"]);
        }
        other => panic!("expected a missing-controller reason, got {other:?}"),
    }
}

#[test]
fn an_uncreatable_child_reports_cannot_create_and_leaves_nothing_behind() {
    let root = FakeRoot::permissive()
        .with_file(
            "sys/fs/cgroup/conspect.service/cgroup.controllers",
            "cpu memory pids\n",
        )
        .with_file("sys/fs/cgroup/conspect.service/cpu.stat", "usage_usec 1\n");
    let mut root = root;
    root.allow_create = false;

    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(
        report.reason(),
        Some(NotDelegated::CannotCreateChild),
        "a cgroup that cannot be delegated to is not delegated"
    );
    assert!(
        root.writes().is_empty(),
        "the fake root records no write of its own"
    );
}

#[test]
fn an_unremovable_probe_child_reports_cannot_remove() {
    let root = FakeRoot::permissive().with_file(
        "sys/fs/cgroup/conspect.service/cgroup.controllers",
        "cpu memory pids\n",
    );
    let root = FakeRoot {
        allow_remove: false,
        ..root
    };
    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(report.reason(), Some(NotDelegated::CannotRemoveChild));
}

#[test]
fn a_missing_directory_is_not_a_directory() {
    let root = FakeRoot::permissive();
    let report = delegation_status(&root, "sys/fs/cgroup/nope").expect("a probe");
    assert_eq!(report.reason(), Some(NotDelegated::NotADirectory));
}

#[test]
fn unreadable_controllers_are_reported_rather_than_assumed_absent() {
    let root = FakeRoot::permissive().with_dir("sys/fs/cgroup/conspect.service");
    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(report.reason(), Some(NotDelegated::ControllersUnreadable));
}

#[test]
fn the_probe_child_name_never_collides_with_a_generation_scope_or_another_probe() {
    // The probe uses a reserved prefix, so it can never be mistaken for, or
    // interfere with, a running scope's cgroup; and each call gets a fresh name,
    // so two probes never contend for one directory.
    let first = probe_child_name();
    let second = probe_child_name();
    assert!(first.starts_with(DELEGATION_PROBE_PREFIX), "{first}");
    assert_ne!(first, second, "each probe gets a unique name");
    assert!(
        first.contains(&std::process::id().to_string()),
        "the pid separates processes: {first}"
    );
    // The probe path is exactly one child of the root it was given.
    assert_eq!(
        probe_child_path("sys/fs/cgroup/conspect.service", &first),
        format!("sys/fs/cgroup/conspect.service/{first}")
    );
    assert_eq!(
        probe_child_path("sys/fs/cgroup/conspect.service/", &first),
        format!("sys/fs/cgroup/conspect.service/{first}")
    );
}

#[test]
fn an_eexist_create_is_retried_once_with_a_fresh_name() {
    // A stale probe directory from a crashed run makes `mkdir` fail with
    // EEXIST. That is not "not delegated": the probe retries with a new name.
    let root = FakeRoot {
        eexist_creates: 1,
        ..FakeRoot::permissive().with_file(
            "sys/fs/cgroup/conspect.service/cgroup.controllers",
            "cpu memory pids\n",
        )
    };
    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(report, DelegationReport::Delegated);
    let attempts = root.create_paths.borrow();
    assert_eq!(attempts.len(), 2, "one retry, not a give-up");
    assert_ne!(
        attempts.first(),
        attempts.get(1),
        "the retry used a new name"
    );
    assert!(
        attempts
            .iter()
            .all(|path| path.contains(DELEGATION_PROBE_PREFIX)),
        "{attempts:?}"
    );
}

#[test]
fn a_failing_probe_makes_a_best_effort_removal() {
    // The probe created its directory, then could not remove it: it must not
    // simply walk away, so a second best-effort `rmdir` is attempted.
    let root = RecordingRoot::new(FakeRoot {
        allow_remove: false,
        ..FakeRoot::permissive().with_file(
            "sys/fs/cgroup/conspect.service/cgroup.controllers",
            "cpu memory pids\n",
        )
    });
    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(report.reason(), Some(NotDelegated::CannotRemoveChild));
    let writes = root.writes();
    assert_eq!(
        writes.len(),
        3,
        "create, remove, best-effort remove: {writes:?}"
    );
    assert!(writes.first().is_some_and(|w| w.starts_with("create ")));
    assert!(
        writes.get(1).is_some_and(|w| w.starts_with("remove ")),
        "{writes:?}"
    );
    assert!(
        writes.get(2).is_some_and(|w| w.starts_with("remove ")),
        "the failure path retries the removal: {writes:?}"
    );
}

#[test]
fn a_failed_verify_step_aborts_and_enables_no_controller() {
    let steps = plan_leaf_move("/sys/fs/cgroup/conspect.service", 4242);
    let verify = steps
        .iter()
        .find(|step| matches!(step, LeafMoveStep::VerifyParentEmpty { .. }))
        .expect("a verify_parent_empty step");
    assert_eq!(
        verify.on_failure(),
        FailureAction::AbortAndStay,
        "a failed verify aborts and stays: no controller is enabled"
    );
    // Every step aborts: a failure anywhere leaves the process where it was.
    for step in &steps {
        assert_eq!(step.on_failure(), FailureAction::AbortAndStay, "{step:?}");
    }
    // And the abort is meaningful: the controller enables follow the verify.
    let labels: Vec<&str> = steps.iter().map(LeafMoveStep::label).collect();
    let index_of = |label: &str| labels.iter().position(|l| *l == label).expect("a step");
    assert!(index_of("verify_parent_empty") < index_of("enable_controller"));
}

#[test]
fn usage_reads_every_documented_file_and_reports_peak_when_present() {
    let cgroup = CgroupDir::build("cpu memory pids\n", Some(3_145_728));
    let usage = read_cgroup_usage(cgroup.root(), CgroupDir::RELATIVE).expect("a reading");
    assert_eq!(usage.cpu.usage_usec, 2_500_000);
    assert_eq!(usage.cpu.user_usec, Some(2_000_000));
    assert_eq!(usage.cpu.system_usec, Some(500_000));
    assert!((usage.cpu_seconds_total() - 2.5).abs() < 1e-9);
    assert_eq!(usage.memory_current_bytes, Some(1_048_576));
    assert_eq!(usage.memory_peak_bytes, Some(3_145_728));
    assert_eq!(usage.pids_current, Some(7));
}

#[test]
fn a_kernel_without_memory_peak_reports_unknown_not_zero() {
    let cgroup = CgroupDir::build("cpu memory pids\n", None);
    let usage = read_cgroup_usage(cgroup.root(), CgroupDir::RELATIVE).expect("a reading");
    assert_eq!(
        usage.memory_peak_bytes, None,
        "an absent memory.peak is unknown, never a fabricated zero"
    );
    assert_eq!(usage.memory_current_bytes, Some(1_048_576));
}

#[test]
fn a_missing_cpu_stat_is_an_error_and_a_missing_peak_is_not() {
    let dir = tempfile::tempdir().expect("a tempdir");
    assert!(
        read_cgroup_usage(dir.path(), "").is_err(),
        "without cpu.stat there is no accounting to report"
    );
}

#[test]
fn reading_usage_writes_nothing_at_all() {
    // A read-only wrapper around a permissive fake: every mutating operation is
    // recorded, and the reading path must record NONE. `memory.peak` must stay a
    // lifetime peak, so nothing may ever write it.
    let root = RecordingRoot::new(
        FakeRoot::permissive()
            .with_file("cgroup/cgroup.controllers", "cpu memory pids\n")
            .with_file("cgroup/cpu.stat", "usage_usec 1000\n")
            .with_file("cgroup/memory.current", "4096\n")
            .with_file("cgroup/memory.peak", "8192\n")
            .with_file("cgroup/pids.current", "2\n"),
    );
    let usage = read_cgroup_usage(&root, "cgroup").expect("a reading");
    assert_eq!(usage.memory_peak_bytes, Some(8192));
    assert!(
        root.writes().is_empty(),
        "the sampler must never write: {:?}",
        root.writes()
    );

    // The probe itself only creates and removes its own uniquely named child.
    let report = delegation_status(&root, "cgroup").expect("a probe");
    assert!(report.is_delegated());
    let writes = root.writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    let created = writes
        .first()
        .and_then(|write| write.strip_prefix("create cgroup/"))
        .expect("a create of the probe child");
    assert!(created.starts_with(DELEGATION_PROBE_PREFIX), "{created}");
    assert_eq!(writes.get(1), Some(&format!("remove cgroup/{created}")));
}

#[test]
fn plan_leaf_move_returns_ordered_steps_and_executes_nothing() {
    // No cgroup exists at this path, and the plan is pure data: calling it must
    // not create anything, here or anywhere else.
    let parent = "/sys/fs/cgroup/does-not-exist-conspect";
    let steps = plan_leaf_move(parent, 4242);
    assert!(!std::path::Path::new(parent).exists());

    let labels: Vec<&str> = steps.iter().map(LeafMoveStep::label).collect();
    assert_eq!(
        labels,
        vec![
            "create_leaf",
            "move_self",
            "verify_parent_empty",
            "verify_delegation_in_place",
            "enable_controller",
            "enable_controller",
            "enable_controller",
        ],
        "create -> move -> verify empty -> enable, in that order"
    );

    // The move precedes every verification and every controller write, because a
    // cgroup holding processes cannot enable a domain controller.
    let index_of = |label: &str| labels.iter().position(|l| *l == label).expect("a step");
    assert!(index_of("create_leaf") < index_of("move_self"));
    assert!(index_of("move_self") < index_of("verify_parent_empty"));
    assert!(index_of("verify_parent_empty") < index_of("enable_controller"));

    let controllers: Vec<&str> = steps
        .iter()
        .filter_map(|step| match step {
            LeafMoveStep::EnableController { controller, .. } => Some(*controller),
            _ => None,
        })
        .collect();
    assert_eq!(controllers, REQUIRED_CONTROLLERS.to_vec());

    // Every path in the plan is below the parent it was given.
    for step in &steps {
        // `LeafMoveStep` is `#[non_exhaustive]`, so the wildcard arm is required
        // for a crate outside the one that defines it.
        let (LeafMoveStep::CreateLeaf { path }
        | LeafMoveStep::MoveSelf { path, .. }
        | LeafMoveStep::VerifyParentEmpty { path }
        | LeafMoveStep::VerifyDelegationInPlace { path }
        | LeafMoveStep::EnableController { path, .. }) = step
        else {
            panic!("an unhandled step shape: {step:?}")
        };
        assert!(
            path.starts_with(parent),
            "{path:?} escapes the parent cgroup"
        );
    }
}

#[test]
fn the_controller_set_is_a_stable_mask() {
    let full = ControllerSet::from_controllers("cpu memory pids");
    assert!(full.is_complete());
    assert_eq!(full.bits(), 0b111);
    assert!(full.missing_from_required().is_empty());
    let none = ControllerSet::from_controllers("io hugetlb");
    assert!(none.is_empty());
    assert_eq!(
        none.missing_from_required().names(),
        REQUIRED_CONTROLLERS.to_vec()
    );
}
