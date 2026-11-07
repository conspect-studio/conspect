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
    rollback_for, ControllerSet, DelegationReport, FailureAction, LeafMoveStep, NotDelegated,
    VerifyMembership, DELEGATION_PROBE_PREFIX, REQUIRED_CONTROLLERS,
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
fn a_probe_directory_it_did_not_create_is_left_alone() {
    // LOW: the failure path after the second failed create did an unconditional
    // `rmdir`, which could delete a directory this call never made (another
    // caller's, or a concurrent probe's). Only remove what THIS call created.
    //
    // The fake models a hierarchy where the probe cannot create anything, so
    // both attempts fail and the pre-existing directory must survive.
    let root = RecordingRoot::new(FakeRoot {
        allow_create: false,
        allow_remove: false,
        ..FakeRoot::permissive().with_file(
            "sys/fs/cgroup/conspect.service/cgroup.controllers",
            "cpu memory pids\n",
        )
    });
    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(report.reason(), Some(NotDelegated::CannotCreateChild));
    let writes = root.writes();
    assert!(
        writes.iter().all(|write| write.starts_with("create ")),
        "no rmdir may be attempted for a directory this call did not create: {writes:?}"
    );
    assert!(
        !writes.iter().any(|write| write.starts_with("remove ")),
        "the pre-existing probe directory survives: {writes:?}"
    );
}

#[test]
fn a_second_failed_create_does_not_remove_the_directory_that_held_the_name() {
    // The EEXIST retry path: the FIRST create failed because a stale probe holds
    // the name, the retry fails for a real reason. The stale directory is not
    // ours, so it must not be removed — even though this call did try to create
    // it once.
    let root = RecordingRoot::new(FakeRoot {
        eexist_creates: 1,
        allow_create: false,
        allow_remove: true,
        ..FakeRoot::permissive().with_file(
            "sys/fs/cgroup/conspect.service/cgroup.controllers",
            "cpu memory pids\n",
        )
    });
    let report = delegation_status(&root, "sys/fs/cgroup/conspect.service").expect("a probe");
    assert_eq!(report.reason(), Some(NotDelegated::CannotCreateChild));
    let writes = root.writes();
    assert_eq!(writes.len(), 2, "one EEXIST attempt, one retry: {writes:?}");
    assert!(
        writes.iter().all(|write| write.starts_with("create ")),
        "neither failed create authorises a removal: {writes:?}"
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

/// The labels of a step list, for an exact order assertion.
fn labels_of(steps: &[LeafMoveStep]) -> Vec<&'static str> {
    steps.iter().map(LeafMoveStep::label).collect()
}

#[test]
fn the_forward_plan_is_forward_only_and_has_no_rollback_steps_in_it() {
    // HIGH: the plan used to interleave rollback steps (`move_self_back`,
    // `remove_leaf`) between the forward ones and rely on a position-based
    // `is_rollback` filter to tell them apart. The plan is now forward-only: the
    // six steps an executor runs, in order, and nothing else.
    let parent = "/sys/fs/cgroup/conspect.service";
    let plan = plan_leaf_move(parent, 4242);
    assert_eq!(
        labels_of(&plan),
        vec![
            "create_leaf",
            "move_self",
            "verify_parent_empty",
            "verify_delegation_in_place",
            "enable_controller",
            "enable_controller",
            "enable_controller",
        ],
        "the forward plan is exactly the six forward steps, in order"
    );
    for step in &plan {
        assert!(
            !matches!(
                step,
                LeafMoveStep::MoveSelfBack { .. }
                    | LeafMoveStep::RemoveLeaf { .. }
                    | LeafMoveStep::DisableController { .. }
            ),
            "an undo step may never appear in the forward plan: {step:?}"
        );
    }

    // The move carries the parent it must be undone to, so the undo is derived
    // from the step rather than un-merged out of the leaf path.
    let move_self = plan.get(1).expect("the move step");
    assert_eq!(
        move_self,
        &LeafMoveStep::MoveSelf {
            pid: 4242,
            path: std::path::PathBuf::from(format!("{parent}/engine/cgroup.procs")),
            back_to: std::path::PathBuf::from(format!("{parent}/cgroup.procs")),
        }
    );
    assert_eq!(
        plan.first(),
        Some(&LeafMoveStep::CreateLeaf {
            path: std::path::PathBuf::from(format!("{parent}/engine")),
        })
    );
    let enabled: Vec<&str> = plan
        .iter()
        .filter_map(|step| match step {
            LeafMoveStep::EnableController { controller, .. } => Some(*controller),
            _ => None,
        })
        .collect();
    assert_eq!(enabled, REQUIRED_CONTROLLERS.to_vec());
}

#[test]
fn a_verify_step_has_no_undo_and_every_mutation_does() {
    let parent = "/sys/fs/cgroup/conspect.service";
    let plan = plan_leaf_move(parent, 4242);
    let subtree_control = std::path::PathBuf::from(format!("{parent}/cgroup.subtree_control"));

    assert_eq!(
        plan.first().and_then(LeafMoveStep::undo),
        Some(LeafMoveStep::RemoveLeaf {
            path: std::path::PathBuf::from(format!("{parent}/engine")),
        })
    );
    assert_eq!(
        plan.get(1).and_then(LeafMoveStep::undo),
        Some(LeafMoveStep::MoveSelfBack {
            pid: 4242,
            to: std::path::PathBuf::from(format!("{parent}/cgroup.procs")),
        })
    );
    for index in [2, 3] {
        let step = plan.get(index).expect("a verify step");
        assert!(
            matches!(
                step,
                LeafMoveStep::VerifyParentEmpty { .. }
                    | LeafMoveStep::VerifyDelegationInPlace { .. }
            ),
            "indices 2 and 3 are the two verifies: {step:?}"
        );
        assert_eq!(
            step.undo(),
            None,
            "a verify changed nothing, so it has nothing to undo"
        );
    }
    assert_eq!(
        plan.get(4).and_then(LeafMoveStep::undo),
        Some(LeafMoveStep::DisableController {
            controller: "cpu",
            path: subtree_control.clone(),
        })
    );

    // The three undo variants have no undo of their own: running one IS the
    // rollback, so asking for its inverse would loop.
    for step in [
        LeafMoveStep::MoveSelfBack {
            pid: 1,
            to: subtree_control.clone(),
        },
        LeafMoveStep::RemoveLeaf {
            path: subtree_control.clone(),
        },
        LeafMoveStep::DisableController {
            controller: "memory",
            path: subtree_control.clone(),
        },
    ] {
        assert_eq!(step.undo(), None, "{step:?} is already an undo");
    }
}

#[test]
fn a_failure_at_every_forward_index_yields_exactly_the_reverse_undos_before_it() {
    // The reviewer's example, generalised: for EVERY forward index the rollback
    // is the undo of each completed step, in REVERSE order — disabling
    // controllers first, then the move back, then the leaf removal.
    let parent = "/sys/fs/cgroup/conspect.service";
    let leaf = std::path::PathBuf::from(format!("{parent}/engine"));
    let parent_procs = std::path::PathBuf::from(format!("{parent}/cgroup.procs"));
    let move_back = LeafMoveStep::MoveSelfBack {
        pid: 4242,
        to: parent_procs.clone(),
    };
    let remove_leaf = LeafMoveStep::RemoveLeaf { path: leaf.clone() };
    let subtree_control = std::path::PathBuf::from(format!("{parent}/cgroup.subtree_control"));
    let disable = |controller: &'static str| LeafMoveStep::DisableController {
        controller,
        path: subtree_control.clone(),
    };
    let if_in_leaf = LeafMoveStep::IfMember {
        check: VerifyMembership {
            pid: 4242,
            path: leaf.clone(),
        },
        then: Box::new(move_back.clone()),
    };
    let plan = plan_leaf_move(parent, 4242);

    assert_eq!(plan.len(), 7, "the forward plan is seven steps");
    let expected: Vec<Vec<LeafMoveStep>> = vec![
        // 0 create_leaf failed: nothing completed, nothing to undo.
        vec![],
        // 1 move_self failed: the leaf exists, and the (atomic) migration may or
        //   may not have reached the kernel, so the move back is CONDITIONAL.
        vec![if_in_leaf.clone(), remove_leaf.clone()],
        // 2 verify_parent_empty failed: the move completed, so put it back.
        vec![move_back.clone(), remove_leaf.clone()],
        // 3 verify_delegation_in_place failed: same two mutations.
        vec![move_back.clone(), remove_leaf.clone()],
        // 4 enable_controller cpu failed: cpu cannot have been enabled (it is
        //   the step that failed), so only the two earlier mutations.
        vec![move_back.clone(), remove_leaf.clone()],
        // 5 enable_controller memory failed: disable cpu FIRST, then restore.
        vec![disable("cpu"), move_back.clone(), remove_leaf.clone()],
        // 6 enable_controller pids failed: undo memory then cpu, then restore.
        vec![
            disable("memory"),
            disable("cpu"),
            move_back.clone(),
            remove_leaf.clone(),
        ],
    ];
    for (index, want) in expected.iter().enumerate() {
        assert_eq!(
            &rollback_for(&plan, index),
            want,
            "failure at index {index} ({})",
            plan.get(index).map_or("?", LeafMoveStep::label)
        );
    }
    // Running every step is a success, so the plan that ran to the end undoes
    // all three enables and both mutations; past the end is not a position in
    // any plan, and neither is index 0 of an empty plan.
    assert_eq!(
        rollback_for(&plan, plan.len()),
        vec![
            disable("pids"),
            disable("memory"),
            disable("cpu"),
            move_back.clone(),
            remove_leaf.clone(),
        ],
        "a plan that ran to the end has every undo"
    );
    assert_eq!(
        rollback_for(&plan, plan.len().saturating_add(5)),
        Vec::new()
    );
    assert_eq!(rollback_for(&[], 0), Vec::new());
}

#[test]
fn no_rollback_ever_enables_a_controller_and_the_undo_order_is_load_bearing() {
    let parent = "/sys/fs/cgroup/conspect.service";
    let plan = plan_leaf_move(parent, 4242);
    // Every index, plus past the end: the whole space of failures.
    for index in 0..=plan.len().saturating_add(2) {
        let rollback = rollback_for(&plan, index);
        assert!(
            !rollback
                .iter()
                .any(|step| matches!(step, LeafMoveStep::EnableController { .. })),
            "a rollback must never enable a controller (index {index}): {rollback:?}"
        );
        // No step appears twice: each controller's disable is its own step, so
        // uniqueness is over the whole step, not over its label.
        let mut seen: Vec<LeafMoveStep> = Vec::new();
        for step in &rollback {
            assert!(
                !seen.contains(step),
                "an undo runs at most once (index {index}): {rollback:?}"
            );
            seen.push(step.clone());
        }
        let labels = labels_of(&rollback);
        // Disabling every enabled controller comes first, and the process only
        // moves back once the last controller is off; the leaf is removed last,
        // because a cgroup with a live process cannot be removed.
        let last_disable = labels
            .iter()
            .rposition(|label| *label == "disable_controller");
        let move_back = labels.iter().position(|label| *label == "move_self_back");
        let remove_leaf = labels.iter().position(|label| *label == "remove_leaf");
        if let (Some(disable), Some(back)) = (last_disable, move_back) {
            assert!(
                disable < back,
                "every disable precedes the move back (index {index}): {labels:?}"
            );
        }
        if let (Some(back), Some(remove)) = (move_back, remove_leaf) {
            assert!(
                back < remove,
                "the move back precedes the leaf removal (index {index}): {labels:?}"
            );
        }
    }

    // The concrete reviewer example: a failure at `enable_controller memory`.
    let memory = plan
        .iter()
        .position(|step| matches!(step, LeafMoveStep::EnableController { controller, .. } if *controller == "memory"))
        .expect("the memory enable");
    assert_eq!(
        labels_of(&rollback_for(&plan, memory)),
        vec!["disable_controller", "move_self_back", "remove_leaf"]
    );
}

#[test]
fn a_failed_verify_step_rolls_back_and_enables_no_controller() {
    let plan = plan_leaf_move("/sys/fs/cgroup/conspect.service", 4242);
    let verify_index = plan
        .iter()
        .position(|step| matches!(step, LeafMoveStep::VerifyParentEmpty { .. }))
        .expect("a verify_parent_empty step");
    assert_eq!(
        plan.get(verify_index).map(LeafMoveStep::on_failure),
        Some(FailureAction::AbortAfterRollback),
        "a failed verify rolls back and stays: no controller is enabled"
    );
    // Every step rolls back before aborting: a failure anywhere leaves the
    // process where it was *before the plan*, not merely stopped in place.
    for step in &plan {
        assert_eq!(
            step.on_failure(),
            FailureAction::AbortAfterRollback,
            "{step:?}"
        );
    }
    // The verify steps precede every enable, so a failed verify never falls
    // through to turning a controller on.
    let labels = labels_of(&plan);
    let index_of = |label: &str| labels.iter().position(|l| *l == label).expect("a step");
    assert!(index_of("verify_parent_empty") < index_of("enable_controller"));
    assert!(index_of("verify_delegation_in_place") < index_of("enable_controller"));
}

#[test]
fn the_plan_runs_before_any_thread_or_child_exists() {
    // ADR-0099 decision 8: the whole plan runs in `main`, before any thread or
    // child exists. That is what makes the rollback safe and what makes
    // `verify_parent_empty` normally pass: the only process the move could have
    // put in the parent is this one, and the derived rollback takes it back out
    // before the verify runs.
    let parent = "/sys/fs/cgroup/conspect.service";
    let plan = plan_leaf_move(parent, 4242);
    let verify_index = plan
        .iter()
        .position(|step| matches!(step, LeafMoveStep::VerifyParentEmpty { .. }))
        .expect("a verify_parent_empty step");
    let rollback = rollback_for(&plan, verify_index);
    assert_eq!(
        labels_of(&rollback),
        vec!["move_self_back", "remove_leaf"],
        "the move is undone, then the leaf removed, before the parent is checked"
    );
    // Nothing in the plan waits on, or names, another process: the only pid it
    // moves is the caller's own.
    for step in &plan {
        match step {
            LeafMoveStep::MoveSelf { pid, .. } | LeafMoveStep::MoveSelfBack { pid, .. } => {
                assert_eq!(*pid, 4242, "the plan only ever moves the caller's own pid");
            }
            _ => {}
        }
    }
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

#[test]
fn a_failure_at_move_self_moves_back_only_if_the_process_is_in_the_leaf() {
    // HIGH (review 4): a failed `MoveSelf` write is not proof the process was
    // not moved. The rollback carries a conditional move back, as data.
    let parent = "/sys/fs/cgroup/conspect.service";
    let plan = plan_leaf_move(parent, 4242);
    let rollback = rollback_for(&plan, 1);
    let Some(LeafMoveStep::IfMember { check, then }) = rollback.first() else {
        panic!("the first undo is conditional: {rollback:?}");
    };
    assert_eq!(check.pid, 4242);
    assert_eq!(
        check.path,
        std::path::PathBuf::from(format!("{parent}/engine"))
    );
    assert_eq!(
        **then,
        LeafMoveStep::MoveSelfBack {
            pid: 4242,
            to: std::path::PathBuf::from(format!("{parent}/cgroup.procs")),
        }
    );
    assert_eq!(
        rollback.get(1).map(LeafMoveStep::label),
        Some("remove_leaf")
    );

    // The executor's decision: it holds only when /proc/<pid>/cgroup shows the leaf.
    let mount = "/sys/fs/cgroup";
    assert!(check.holds("0::/conspect.service/engine\n", mount));
    assert!(
        !check.holds("0::/conspect.service\n", mount),
        "still in the parent: do not write"
    );
    assert!(
        !check.holds("0::/conspect.service/engine-other\n", mount),
        "a prefix match is not membership"
    );
    assert!(
        !check.holds("12:cpu:/conspect.service/engine\n", mount),
        "v1 lines never count"
    );
    assert!(
        !check.holds("", mount),
        "unreadable is false: a doubtful answer never writes"
    );

    // A MoveSelf that COMPLETED (failure later) keeps the unconditional undo.
    let later = rollback_for(&plan, 2);
    assert_eq!(
        later.first().map(LeafMoveStep::label),
        Some("move_self_back")
    );
    // Only MoveSelf has a conditional undo; the conditional is never in a forward plan.
    assert!(plan.iter().all(|step| step.label() != "if_member"));
    assert!(plan
        .iter()
        .enumerate()
        .all(|(i, step)| (i == 1) == step.undo_if_applied().is_some()));
}
