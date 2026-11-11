//! The published [`ResourceUsage`] record and the injected-clock sampler.
//!
//! The two contracts under test are the ones a monitoring UI reads: an unknown
//! GPU figure is **absent** (never zero), and a cumulative total never goes
//! backwards — including across a kernel counter reset.
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

use multiview_webengine::{
    attribute, cpu_cores, monotonic_total, read_cgroup_usage, read_starttime, sample_process_tree,
    type_cpu_seconds, usage_from_cgroup, Accounting, ExpectedRoot, Generation, JobPhase, Pid,
    ResourceScope, ResourceUsage, Sampler, ScopeKind, ScopeOwner, ScopeRegistry, TreeSample,
    TYPE_COUNT,
};

fn usage_for(cpu_cores: Option<f32>, cpu_seconds_total: f64) -> ResourceUsage {
    let mut record = ResourceUsage::unknown_gpu(
        ScopeOwner::new("vt-1"),
        Generation::new(0),
        JobPhase::new("browser_start"),
        Accounting::Estimated,
    );
    record.cpu_cores = cpu_cores;
    record.cpu_seconds_total = cpu_seconds_total;
    record
}

#[test]
fn unknown_gpu_fields_serialize_as_absent_never_zero() {
    let record = usage_for(Some(0.25), 12.5);
    let json = serde_json::to_value(&record).expect("serializable");
    assert_eq!(json["gpu_util"], serde_json::Value::Null);
    assert_eq!(json["vram_bytes"], serde_json::Value::Null);
    assert_eq!(json["gpu_seconds_total"], serde_json::Value::Null);
    assert_eq!(json["accounting"], "estimated");
    assert_eq!(json["owner"], "vt-1");
    assert_eq!(json["phase"], "browser_start");
    assert_eq!(json["generation"], 0);
}

#[test]
fn a_record_round_trips_through_json() {
    let mut record = usage_for(Some(0.5), 2.0);
    record.mem_bytes = Some(1024);
    record.mem_peak_bytes = Some(4096);
    record.gpu_util = Some(0.75);
    record.vram_bytes = Some(151_000_000);
    record.gpu_seconds_total = Some(1.5);
    record.accounting = Accounting::Exact;

    let json = serde_json::to_string(&record).expect("serializable");
    let decoded: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(
        decoded["accounting"], "exact",
        "the wire spelling is snake_case"
    );
    assert_eq!(decoded["mem_peak_bytes"], 4096);
    assert_eq!(decoded["vram_bytes"], 151_000_000);
    assert!(record.has_gpu());
}

#[test]
fn the_sampler_needs_two_readings_and_reports_the_interval_in_cores() {
    let mut sampler = Sampler::new(Accounting::Estimated);
    assert_eq!(sampler.record(100.0, 10.0), None, "no interval yet");
    assert_eq!(
        sampler.record(101.0, 10.5),
        Some(0.5_f32),
        "0.5 CPU-s over 1 s is half a core"
    );
    assert_eq!(sampler.record(103.0, 11.5), Some(0.5_f32));
    assert!((sampler.cpu_seconds_total() - 11.5).abs() < 1e-9);
}

#[test]
fn two_cpu_seconds_in_one_second_is_two_cores() {
    // The unit fix: a process tree that burns two CPU-seconds in one wall-clock
    // second is using TWO cores, not a saturated `1.0` fraction.
    let mut sampler = Sampler::new(Accounting::Estimated);
    assert_eq!(sampler.record(0.0, 0.0), None);
    assert_eq!(
        sampler.record(1.0, 2.0),
        Some(2.0_f32),
        "two cores fully used"
    );
    // And a scope past the documented cap saturates rather than reporting an
    // absurd number.
    let mut wide = Sampler::new(Accounting::Estimated);
    wide.record(0.0, 0.0);
    assert_eq!(
        wide.record(1.0, 1.0e9),
        Some(multiview_webengine::CPU_CORES_CAP)
    );
}

#[test]
fn a_counter_reset_gives_zero_work_and_a_monotonic_total() {
    let mut sampler = Sampler::new(Accounting::Estimated);
    assert_eq!(sampler.record(0.0, 100.0), None);
    assert_eq!(
        sampler.record(1.0, 50.0),
        Some(0.0_f32),
        "a counter that moved backwards is a reset, not negative work"
    );
    assert!(
        (sampler.cpu_seconds_total() - 100.0).abs() < 1e-9,
        "the published total must not go backwards"
    );
    // The interval after the reset is measured from the RESET reading (50), not
    // from the held total (100): 60 - 50 = 10 CPU-seconds over ONE second is
    // 10.0 cores (below the cap, so reported as measured).
    assert_eq!(
        sampler.record(2.0, 60.0),
        Some(10.0_f32),
        "measured from the reset value, in cores"
    );
    assert!(
        (sampler.cpu_seconds_total() - 100.0).abs() < 1e-9,
        "the held total still covers the pre-reset peak"
    );
    // A later interval measures from 60 normally.
    assert_eq!(sampler.record(6.0, 62.0), Some(0.5_f32), "2 CPU-s over 4 s");
}

#[test]
fn a_non_advancing_clock_yields_no_cores_and_no_panic() {
    let mut sampler = Sampler::new(Accounting::Estimated);
    sampler.record(10.0, 5.0);
    assert_eq!(sampler.record(10.0, 6.0), None, "zero elapsed time");
    assert_eq!(sampler.record(9.0, 6.0), None, "the clock went backwards");
    assert_eq!(sampler.record(f64::NAN, 6.0), None, "a non-finite clock");
    assert!(sampler.cpu_seconds_total().is_finite());
}

#[test]
fn the_cumulative_figure_is_monotonic_across_a_whole_run() {
    let mut sampler = Sampler::new(Accounting::Estimated);
    let readings = [
        (0.0, 0.0),
        (1.0, 1.0),
        (2.0, 2.5),
        // Reset.
        (3.0, 0.25),
        (4.0, 1.0),
        (5.0, 1.0),
    ];
    let mut published = 0.0_f64;
    for (at, cpu) in readings {
        sampler.record(at, cpu);
        let now = sampler.cpu_seconds_total();
        assert!(
            now >= published,
            "total went backwards: {published} -> {now}"
        );
        published = now;
    }
    assert!(
        (published - 2.5).abs() < 1e-9,
        "the reset holds the pre-reset peak for one interval"
    );
}

#[test]
fn the_first_reading_has_no_cores_and_that_is_honest() {
    let mut sampler = Sampler::new(Accounting::Estimated);
    let first_cores = sampler.record(0.0, 4.0);
    let record = sampler.release(
        ScopeOwner::new("vt-1"),
        Generation::new(0),
        JobPhase::queued(),
        first_cores,
        None,
        None,
    );
    assert_eq!(record.cpu_cores, None, "unknown, not zero");
    assert!((record.cpu_seconds_total - 4.0).abs() < 1e-9);
    assert_eq!(record.phase.as_str(), "queued");
}

#[test]
fn an_exact_cgroup_reading_carries_memory_dot_peak_through() {
    let dir = tempfile::tempdir().expect("a tempdir");
    fs::write(dir.path().join("cpu.stat"), "usage_usec 1000000\n").expect("cpu.stat");
    fs::write(dir.path().join("memory.current"), "2048\n").expect("memory.current");
    fs::write(dir.path().join("memory.peak"), "4096\n").expect("memory.peak");
    fs::write(dir.path().join("pids.current"), "3\n").expect("pids.current");

    let usage = read_cgroup_usage(dir.path(), "").expect("a reading");
    let record = usage_from_cgroup(
        ScopeOwner::new("engine"),
        Generation::new(0),
        JobPhase::new("capture"),
        Some(1.0),
        &usage,
    );
    assert_eq!(record.accounting, Accounting::Exact);
    assert!((record.cpu_seconds_total - 1.0).abs() < 1e-9);
    assert_eq!(record.mem_bytes, Some(2048), "memory.current");
    assert_eq!(
        record.mem_peak_bytes,
        Some(4096),
        "memory.peak is read through and never written"
    );
    assert_eq!(record.gpu_util, None, "no GPU attribution in this slice");
    assert_eq!(
        fs::read_to_string(dir.path().join("memory.peak")).expect("re-read"),
        "4096\n",
        "the peak file is byte-identical after reading"
    );
}

#[test]
fn the_per_type_cpu_split_names_the_gpu_process() {
    // The diagnostics view of ADR-0102's measured bug: the GPU process carries
    // the bulk of the CPU, and the split says so in seconds.
    let tree = common::ProcTree::build(vec![
        common::ProcSpec::new(600, "chrome", 0, 0).with_children(&[601, 602]),
        common::ProcSpec::new(601, "chrome", 0, 0)
            .with_parent(600)
            .with_type("gpu-process")
            .with_pss_kib(64),
        common::ProcSpec::new(602, "chrome", 0, 0)
            .with_parent(600)
            .with_type("renderer")
            .with_pss_kib(32),
    ]);
    let mut sampler = Sampler::new(Accounting::Estimated);
    let registered = ExpectedRoot {
        pid: Pid::new(600),
        starttime: tree.spec(600).starttime,
    };
    let sample = sampler
        .sample_tree(tree.root(), registered)
        .expect("a walk")
        .into_sample()
        .expect("a live root");
    let seconds = type_cpu_seconds(&sample.by_type);

    assert_eq!(
        seconds.get(multiview_webengine::ChromiumProcessType::GpuProcess.index()),
        Some(&0.0_f64)
    );
    assert_eq!(
        sample
            .type_usage(multiview_webengine::ChromiumProcessType::Renderer)
            .processes,
        1
    );
    assert_eq!(
        seconds.len(),
        7,
        "every known type is reported, even at zero"
    );
}

#[test]
fn a_stale_generation_is_not_attributed() {
    // The sampler and the registry meet here: a reading tagged with a generation
    // the registry has moved past is dropped, not charged to the new browser.
    let owner = ScopeOwner::new("vt-1");
    let mut registry = ScopeRegistry::new();
    let first = registry.next_generation(&owner);
    registry
        .register(process_tree_scope(&owner, first, 1, 11))
        .expect("the next generation is accepted");
    assert_eq!(attribute(&registry, &owner, first), Some(first));
    let second = registry.next_generation(&owner);
    registry
        .register(process_tree_scope(&owner, second, 2, 12))
        .expect("monotonic");
    assert_eq!(attribute(&registry, &owner, first), None);
    assert_eq!(attribute(&registry, &owner, second), Some(second));
    assert_eq!(
        attribute(&registry, &ScopeOwner::new("gone"), Generation::new(0)),
        None
    );
}

#[test]
fn the_pure_helpers_agree_with_each_other() {
    assert_eq!(cpu_cores(0.0, 1.0, 2.0), Some(0.5_f32));
    assert!((monotonic_total(5.0, 4.0) - 5.0).abs() < 1e-9);
    assert!((monotonic_total(5.0, 6.0) - 6.0).abs() < 1e-9);
}

/// A registered process-tree scope for `owner` at `generation`.
fn process_tree_scope(
    owner: &ScopeOwner,
    generation: Generation,
    root: u32,
    root_starttime: u64,
) -> ResourceScope {
    ResourceScope::new(
        owner.clone(),
        generation,
        ScopeKind::ProcessTree {
            root: Pid::new(root),
            root_starttime,
        },
    )
}

/// A 200-process tree: root 1 with children `2..=200`, every node carrying a
/// `children` strand so a sweep needs no `/proc` ppid scan.
fn two_hundred_process_tree() -> common::ProcTree {
    const SIZE: u32 = 200;
    let children: Vec<u32> = (2..=SIZE).collect();
    let mut specs = vec![common::ProcSpec::new(1, "chrome", 3, 2).with_children(&children)];
    for pid in 2..=SIZE {
        specs.push(
            common::ProcSpec::new(pid, "chrome", 3, 2)
                .with_parent(1)
                .with_children(&[]),
        );
    }
    common::ProcTree::build(specs)
}

#[test]
fn a_registered_starttime_that_differs_from_the_live_root_is_root_gone_on_the_first_sample() {
    // HIGH: `ScopeKind::ProcessTree` used to store only the pid. The kernel can
    // reap the browser and hand its pid to an unrelated process between
    // registration and the first sweep, and with no registration-time identity
    // there was nothing to catch it: the impostor's whole tree was charged to
    // the dead scope. The registered `starttime` is compared on EVERY sample.
    let tree = two_hundred_process_tree();
    let live = read_starttime(tree.root(), Pid::new(1)).expect("the fixture root has a starttime");
    let registered = ExpectedRoot {
        pid: Pid::new(1),
        starttime: live + 1,
    };
    let mut sampler = Sampler::new(Accounting::Estimated);
    assert_eq!(
        sampler
            .sample_tree(tree.root(), registered)
            .expect("the first walk reads the root"),
        TreeSample::RootGone,
        "the very first sample is checked, not just the second"
    );
    assert!(
        sampler.is_finished(),
        "a scope that ended on its first sample is finished"
    );

    // The registered identity is also carried by the scope kind the sampler is
    // built from, so the caller cannot assemble the two independently.
    let owner = ScopeOwner::new("vt-1");
    let scope = process_tree_scope(&owner, Generation::new(0), 1, live + 1);
    let mut from_scope = Sampler::for_resource_scope(&scope, Accounting::Estimated);
    let result = from_scope
        .sample_scope(tree.root(), &scope)
        .expect("the first walk reads the root");
    assert_eq!(result, TreeSample::RootGone);
    assert_eq!(
        from_scope.owner(),
        Some(&owner),
        "the refusal names the owner"
    );
    let error = from_scope
        .sample_scope(tree.root(), &scope)
        .expect_err("a finished sampler refuses to be reused");
    assert!(
        error.to_string().contains("vt-1"),
        "the refusal names the owner and scope: {error}"
    );
}

#[test]
fn a_registered_starttime_that_matches_samples_normally() {
    // The guard must not fire when registration and the walk agree: a correct
    // first sample still produces a full reading.
    let tree = two_hundred_process_tree();
    let registered = ExpectedRoot::read(tree.root(), Pid::new(1)).expect("a starttime");
    let mut sampler = Sampler::new(Accounting::Estimated);
    let sample = sampler
        .sample_tree(tree.root(), registered)
        .expect("a walk")
        .into_sample()
        .expect("the registered root is the live root");
    assert_eq!(sample.process_count(), 199, "the whole tree is read");
    assert_eq!(sample.root_starttime, registered.starttime);
    assert!(!sampler.is_finished());
}

#[test]
fn a_root_gone_clears_the_retained_state_and_a_new_generation_needs_a_new_sampler() {
    // MEDIUM: after `RootGone` the old sampler kept its per-pid map and its held
    // CPU total, so driving it on (a caller that forgot to retire the scope)
    // would carry the dead generation's state into the next browser. The chosen
    // contract: `RootGone` CLEARS the state and latches the sampler finished; a
    // new generation gets a new sampler.
    let tree = two_hundred_process_tree();
    let mut registry = ScopeRegistry::new();
    let owner = ScopeOwner::new("vt-1");

    let generation = registry.next_generation(&owner);
    let scope = process_tree_scope(&owner, generation, 1, tree.spec(1).starttime);
    registry.register(scope.clone()).expect("registered");
    let mut sampler = Sampler::for_resource_scope(&scope, Accounting::Estimated);

    // One good sweep, so there IS retained state to lose.
    let first_seconds = {
        let first = sampler
            .sample_scope(tree.root(), &scope)
            .expect("first walk")
            .into_sample()
            .expect("a live root");
        sampler.record(0.0, Sampler::tree_cpu_seconds(&first));
        sampler.cpu_seconds_total()
    };
    assert!(first_seconds > 0.0, "the good sweep booked some CPU");
    assert!(
        sampler.previous_sweep().is_some(),
        "a sampled sweep retains its map"
    );

    // The browser dies and the kernel reuses its pid for an unrelated process.
    tree.update_stat(&common::ProcSpec::new(1, "unrelated", 1, 1).with_starttime(99_999));
    assert_eq!(
        sampler
            .sample_scope(tree.root(), &scope)
            .expect("the second walk reads the root"),
        TreeSample::RootGone
    );
    assert!(sampler.is_finished(), "the scope ended, so sampling ended");
    assert!(
        sampler.previous_sweep().is_none(),
        "RootGone drops the per-pid map: it is not the impostor's, and not the \
         next generation's either"
    );
    assert!(
        sampler.cpu_seconds_total().abs() < 1e-9,
        "and it drops the held CPU total, so nothing carries over"
    );

    // The latch is absolute: the same sampler never adopts another tree, even
    // one that matches the registered identity again.
    tree.update_stat(&common::ProcSpec::new(1, "chrome", 3, 2).with_starttime(1_001));
    let refused = sampler
        .sample_scope(tree.root(), &scope)
        .expect_err("a finished sampler is not reusable");
    assert!(
        refused.to_string().contains("finished"),
        "the refusal explains itself: {refused}"
    );

    // Retiring the scope and registering a new generation gets a fresh sampler,
    // which samples normally. The old sampler's state is not in it.
    registry.remove(&owner);
    let next_generation = registry.next_generation(&owner);
    let next_scope = process_tree_scope(&owner, next_generation, 1, tree.spec(1).starttime);
    registry.register(next_scope.clone()).expect("registered");
    let mut fresh = Sampler::for_resource_scope(&next_scope, Accounting::Estimated);
    assert_eq!(fresh.generation(), next_generation);
    let fresh_sample = fresh
        .sample_scope(tree.root(), &next_scope)
        .expect("a new generation samples its own root")
        .into_sample()
        .expect("a live root");
    assert!(fresh_sample.cpu_ticks > 0);
    assert!(
        (fresh.cpu_seconds_total() - 0.0).abs() < 1e-9,
        "the new sampler starts with no held total from the dead generation"
    );
}

#[test]
fn a_sampler_built_from_a_scope_refuses_a_different_process() {
    // Defence in depth: the scope kind carries the identity, so a caller cannot
    // register one process and sample another into its generation.
    let tree = two_hundred_process_tree();
    let owner = ScopeOwner::new("vt-1");
    let scope = process_tree_scope(&owner, Generation::new(0), 1, tree.spec(1).starttime);
    let mut sampler = Sampler::for_resource_scope(&scope, Accounting::Estimated);

    let impostor = ExpectedRoot {
        pid: Pid::new(1),
        starttime: tree.spec(1).starttime + 7,
    };
    let refused = sampler
        .sample_tree(tree.root(), impostor)
        .expect_err("a sampler is pinned to its scope's identity");
    assert!(
        refused.to_string().contains("registered with"),
        "the refusal says why: {refused}"
    );
    assert!(
        !sampler.is_finished(),
        "a refused misuse is not a RootGone: the scope is still live"
    );
    // The right identity still samples.
    assert!(sampler
        .sample_tree(
            tree.root(),
            ExpectedRoot {
                pid: Pid::new(1),
                starttime: tree.spec(1).starttime,
            },
        )
        .expect("the registered identity")
        .sample()
        .is_some());
}

#[test]
fn the_per_sweep_collections_do_not_grow_over_fifty_sweeps() {
    // Operator efficiency rule: prove the retained per-sweep state is bounded by
    // the tree, not by the number of sweeps. A total heap-allocation count needs
    // an external tool (valgrind / heaptrack); this pins the collection the sweep
    // keeps between calls (the `PreviousSweep` per-pid map) plus the fixed
    // `by_type` array.
    let tree = two_hundred_process_tree();
    let registered = ExpectedRoot::read(tree.root(), Pid::new(1)).expect("a starttime");
    let mut sampler = Sampler::new(Accounting::Estimated);

    let sweep = |sampler: &mut Sampler| {
        let sample = sampler
            .sample_tree(tree.root(), registered)
            .expect("a walk")
            .into_sample()
            .expect("a live root");
        assert_eq!(sample.process_count(), 199);
        sample
    };

    let _ = sweep(&mut sampler);
    let second = sweep(&mut sampler);
    let after_two = sampler
        .previous_sweep()
        .expect("two sweeps retain state")
        .live_map_size();
    assert_eq!(after_two.0, 199, "the retained map covers every descendant");
    assert_eq!(
        second.by_type.len(),
        TYPE_COUNT,
        "the per-type array is fixed-length"
    );

    for _ in 0..48 {
        let _ = sweep(&mut sampler);
    }
    let after_fifty = sampler
        .previous_sweep()
        .expect("fifty sweeps retain state")
        .live_map_size();
    let fiftieth = sweep(&mut sampler);
    assert_eq!(
        after_fifty, after_two,
        "sweep 50 retains exactly what sweep 2 did: its length and its allocation \
         footprint must not grow"
    );
    assert_eq!(
        fiftieth.by_type.len(),
        TYPE_COUNT,
        "by_type stays the fixed TYPE_COUNT array, never a growing map"
    );
    // Not merely "the same number": neither collection grew at all.
    assert!(
        after_fifty.0 <= after_two.0 && after_fifty.1 <= after_two.1,
        "retained state grew between sweep 2 ({after_two:?}) and sweep 50 ({after_fifty:?})"
    );
}

#[test]
fn a_root_that_exits_normally_is_root_gone_and_finishes_the_sampler() {
    // HIGH (review 4): a missing root /proc entry used to propagate NotFound as
    // an error, leaving the sampler live with its map and every later sweep
    // erroring. A browser that exits normally is RootGone.
    let tree = two_hundred_process_tree();
    let registered = ExpectedRoot::read(tree.root(), Pid::new(1)).expect("a starttime");
    let mut sampler = Sampler::new(Accounting::Estimated);
    sampler
        .sample_tree(tree.root(), registered)
        .expect("a walk")
        .into_sample()
        .expect("a live root");
    sampler.record(0.0, 1.0);
    assert!(sampler.previous_sweep().is_some());

    tree.remove(1); // the browser exits: /proc/1 disappears
    assert_eq!(
        sampler
            .sample_tree(tree.root(), registered)
            .expect("an exited root is an outcome, not an error"),
        TreeSample::RootGone
    );
    assert!(sampler.is_finished());
    assert!(sampler.previous_sweep().is_none(), "state cleared");
    assert!(
        sampler.cpu_seconds_total().abs() < 1e-9,
        "held total cleared"
    );
    let later = sampler
        .sample_tree(tree.root(), registered)
        .expect_err("a later sweep is refused");
    assert!(
        matches!(later, multiview_webengine::Error::ScopeEnded { .. }),
        "{later}"
    );

    // Without a registration identity (previous-sweep only) it is the same.
    let bare = sample_process_tree(tree.root(), Pid::new(1), None, None).expect("no error");
    assert_eq!(bare, TreeSample::RootGone);
}

#[test]
fn an_unreadable_or_unparsable_root_is_still_an_error_not_root_gone() {
    // Only "absent" means gone. A stat that exists but is garbage must not end
    // the scope silently.
    let tree = two_hundred_process_tree();
    std::fs::write(tree.path("proc/1/stat"), "1 (chrome) S 0\n").expect("garble");
    let registered = ExpectedRoot {
        pid: Pid::new(1),
        starttime: 1,
    };
    let mut sampler = Sampler::new(Accounting::Estimated);
    assert!(sampler.sample_tree(tree.root(), registered).is_err());
    assert!(!sampler.is_finished(), "an error does not finish the scope");

    // A stat with CPU fields but no starttime has no identity: error, not a 0.
    std::fs::write(
        tree.path("proc/1/stat"),
        "1 (chrome) S 0 1 1 0 -1 0 0 0 0 0 5 5\n",
    )
    .expect("truncate");
    assert!(sample_process_tree(tree.root(), Pid::new(1), None, None).is_err());
}

#[test]
fn a_zero_registered_starttime_matches_nothing() {
    // Fail closed even if a zero slips past registration.
    let tree = two_hundred_process_tree();
    let zero = ExpectedRoot {
        pid: Pid::new(1),
        starttime: 0,
    };
    assert_eq!(
        sample_process_tree(tree.root(), Pid::new(1), Some(&zero), None).expect("a walk"),
        TreeSample::RootGone,
        "0 is not 'unchecked'"
    );
}

#[test]
fn the_per_sweep_record_buffer_is_reused_across_sweeps() {
    // Honest about what this measures: NOT a heap-allocation count. It shows the
    // per-sweep `Vec<ProcRecord>` is sized from the previous sweep, so after the
    // first (growing) sweep its capacity is exactly the tree size and stays
    // stable, instead of regrowing by doubling every sweep. A total heap count
    // needs valgrind or heaptrack on another host.
    let tree = two_hundred_process_tree();
    let registered = ExpectedRoot::read(tree.root(), Pid::new(1)).expect("a starttime");
    let mut sampler = Sampler::new(Accounting::Estimated);
    let mut capacities = Vec::new();
    for _ in 0..50 {
        let sample = sampler
            .sample_tree(tree.root(), registered)
            .expect("a walk")
            .into_sample()
            .expect("live");
        assert_eq!(sample.processes.len(), 199);
        capacities.push(sample.processes.capacity());
    }
    let steady = capacities.get(1).copied().expect("a second sweep");
    assert_eq!(steady, 199, "sized to the tree, no doubling slack");
    assert!(
        capacities
            .iter()
            .skip(1)
            .all(|capacity| *capacity == steady),
        "capacity is stable from sweep 2 on: {capacities:?}"
    );
    assert!(
        capacities.first().is_some_and(|first| *first >= steady),
        "only the first sweep grows"
    );
}
