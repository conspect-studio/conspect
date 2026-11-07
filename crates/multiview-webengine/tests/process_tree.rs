//! Process-tree walking against a **fixture** `/proc`, never the host's.
//!
//! The regression this file exists for: ADR-0099's measured bug where a sampler
//! matched one Chromium process by command line and reported 0% CPU / 100 MB
//! while ~95% of the CPU sat in the GPU process. Every test here therefore
//! asserts on the *whole tree*, and the six-type case asserts the per-type split
//! so a regression to "one process" fails loudly.
//!
//! One test deliberately reads the host's real `/proc/self/stat`: parsing a
//! genuine kernel line is the only way to prove the fixtures are not the thing
//! being tested. It reads, never writes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc,
    clippy::print_stderr
)]

mod common;

use std::path::Path;
use std::time::Instant;

use common::{kernel_stat_line, normalise, CountingRoot, FakeRoot, ProcSpec, ProcTree};

use multiview_webengine::{
    parse_stat_comm, parse_stat_cpu_ticks, parse_stat_ppid, parse_stat_starttime,
    read_cgroup_usage, sample_process_tree, ChromiumProcessType, FsRoot, MemorySource, Pid,
    ProcRecord, Sampler, CHROMIUM_PROCESS_TYPES,
};

/// The six-type Chromium tree used by the regression test.
///
///  100 (browser, the root — no `--type=`)
///   ├── 101 renderer
///   ├── 102 gpu-process
///   ├── 103 utility
///   ├── 104 zygote
///   └── 105 ppapi
///
/// The root's children come from an explicit `children` strand; the leaves are
/// discovered by the `stat` ppid fallback scan, so one walk exercises both
/// discovery paths.
fn chromium_tree() -> ProcTree {
    ProcTree::build(vec![
        ProcSpec::new(100, "chrome-headless-shell", 12, 8)
            .with_parent(1)
            .with_children(&[101, 102, 103, 104, 105]),
        ProcSpec::new(101, "chrome-headless-shell", 20, 10)
            .with_parent(100)
            .with_type("renderer")
            .with_pss_kib(1024),
        ProcSpec::new(102, "chrome-headless-shell", 40, 15)
            .with_parent(100)
            .with_type("gpu-process")
            .with_pss_kib(2048),
        ProcSpec::new(103, "chrome-headless-shell", 8, 7)
            .with_parent(100)
            .with_type("utility")
            .with_pss_kib(512),
        ProcSpec::new(104, "chrome-headless-shell", 6, 4)
            .with_parent(100)
            .with_type("zygote")
            .with_pss_kib(4096),
        ProcSpec::new(105, "chrome-headless-shell", 3, 2)
            .with_parent(100)
            .with_type("ppapi")
            .with_pss_kib(256),
    ])
}

/// A width-`size` tree: root 1 with children `2..=size`.
///
/// With `with_children` every process gets a `children` strand (empty for the
/// leaves), so the walk never needs the `/proc` ppid scan. Without it, every
/// node falls back to the scan, which must still happen at most once per sweep.
fn wide_tree(size: u32, with_children: bool) -> ProcTree {
    let children: Vec<u32> = (2..=size).collect();
    let root = if with_children {
        ProcSpec::new(1, "chrome", 1, 1).with_children(&children)
    } else {
        ProcSpec::new(1, "chrome", 1, 1)
    };
    let mut specs = vec![root];
    for pid in 2..=size {
        let mut spec = ProcSpec::new(pid, "chrome", 1, 1).with_parent(1);
        if with_children {
            spec = spec.with_children(&[]);
        }
        specs.push(spec);
    }
    ProcTree::build(specs)
}

/// The measured CPU of every tree member: root plus each `utime + stime`.
fn expected_ticks() -> u64 {
    ROOT_TICKS + descendants_ticks()
}

/// The root's own `utime + stime`.
const ROOT_TICKS: u64 = 12 + 8;

/// Every descendant's `utime + stime`.
fn descendants_ticks() -> u64 {
    (20 + 10) + (40 + 15) + (8 + 7) + (6 + 4) + (3 + 2)
}

/// The renderer's `utime + stime` (the child that exits in the retention test).
const RENDERER_TICKS: u64 = 20 + 10;

/// Every recognised Chromium type plus the unlabelled root, summed.
fn expected_memory_bytes() -> u64 {
    (1024 + 2048 + 512 + 4096 + 256) * 1024
}

fn record_for(sample: &multiview_webengine::ProcessTreeSample, pid: u32) -> &ProcRecord {
    sample
        .processes
        .iter()
        .find(|record| record.pid == Pid::new(pid))
        .expect("a record for that pid")
}

#[test]
fn the_whole_tree_sums_every_chromium_process_type() {
    // The regression test: all six types are present, and the sum covers them.
    let tree = chromium_tree();
    let sample = sample_process_tree(tree.root(), Pid::new(100), None).expect("a walk");

    assert_eq!(
        sample.process_count(),
        5,
        "five descendants, root not repeated"
    );
    assert_eq!(sample.cpu_ticks, expected_ticks());
    assert_eq!(sample.memory_bytes, expected_memory_bytes());
    assert_eq!(sample.memory_source, MemorySource::Pss);
    assert_eq!(
        sample.accounting,
        multiview_webengine::Accounting::Estimated
    );

    for kind in CHROMIUM_PROCESS_TYPES {
        let entry = sample.type_usage(kind);
        let expected =
            u32::from(kind != ChromiumProcessType::Browser && kind != ChromiumProcessType::Unknown);
        assert_eq!(
            entry.processes, expected,
            "{kind:?}: one descendant per type, none Browser (that is the root) and none Unknown"
        );
    }

    // The GPU process is the expensive one, as the ADR measured: this is the
    // term a "matched one process" sampler would have reported instead of the
    // tree total.
    let gpu = sample.type_usage(ChromiumProcessType::GpuProcess);
    assert_eq!(gpu.cpu_ticks, 55);
    assert!(
        gpu.cpu_ticks < sample.cpu_ticks,
        "the GPU process alone is not the whole tree"
    );
    let summed: u64 = CHROMIUM_PROCESS_TYPES
        .iter()
        .map(|kind| sample.type_usage(*kind).cpu_ticks)
        .sum();
    assert_eq!(summed, descendants_ticks());
    assert_eq!(
        summed + ROOT_TICKS,
        sample.cpu_ticks,
        "per-type sum plus the root accounts for the whole total"
    );

    assert_eq!(
        record_for(&sample, 102).process_type,
        ChromiumProcessType::GpuProcess
    );
    assert_eq!(
        record_for(&sample, 102).raw_type.as_deref(),
        Some("gpu-process")
    );
    assert_eq!(record_for(&sample, 101).comm, "chrome-headless-shell");
}

#[test]
fn a_process_tree_produces_a_cpu_cores_reading_from_two_readings() {
    // The end-to-end shape of one sample tick: walk the tree, hand the walk's
    // cumulative seconds to the sampler, and let the injected clock supply the
    // interval. No wall clock is read anywhere.
    let tree = chromium_tree();
    let mut sampler = Sampler::new(multiview_webengine::Accounting::Estimated);

    let mut first = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("first walk");
    assert_eq!(Sampler::tree_cpu_seconds(&first) * 100.0, 135.0);
    assert_eq!(
        sampler.record(0.0, Sampler::tree_cpu_seconds(&first)),
        None,
        "the first reading has no interval to divide by"
    );

    // Ten seconds later the tree has burned another 30 ticks (0.3 CPU-seconds).
    first.live_cpu_ticks += 30;
    first.cpu_ticks += 30;
    let cores = sampler
        .record(10.0, Sampler::tree_cpu_seconds(&first))
        .expect("a positive interval");
    assert!(
        (cores - 0.03).abs() < 1e-4,
        "0.3 CPU-s / 10 s is 0.03 cores, got {cores}"
    );
}

#[test]
fn exited_child_time_is_retained_so_totals_never_go_backwards() {
    let tree = chromium_tree();
    let mut sampler = Sampler::new(multiview_webengine::Accounting::Estimated);

    // Sample A: the full tree, 135 ticks.
    let first = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("first walk");
    assert_eq!(first.cpu_ticks, expected_ticks());
    assert_eq!(first.exited_cpu_ticks, 0);

    // The renderer (30 ticks) exits before the next walk.
    tree.remove(101);
    let second = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("second walk");

    assert_eq!(
        second.live_cpu_ticks,
        expected_ticks() - RENDERER_TICKS,
        "the live total really did drop"
    );
    assert_eq!(
        second.exited_cpu_ticks, RENDERER_TICKS,
        "the exited child's CPU time stays charged to the scope"
    );
    assert_eq!(
        second.cpu_ticks,
        expected_ticks(),
        "the cumulative total is unchanged, not reduced"
    );
    assert!(
        second.cpu_ticks >= first.cpu_ticks,
        "a reported total must never go backwards"
    );
    assert_eq!(
        second.type_usage(ChromiumProcessType::Renderer).processes,
        0,
        "no live renderer remains, but its time is not lost"
    );
}

#[test]
fn a_grandchild_exit_in_the_same_interval_is_retained() {
    // root 300 -> child 301 -> grandchild 302. The grandchild is two levels
    // down, so the retention map must cover the whole walk, not just the root's
    // immediate children.
    let tree = ProcTree::build(vec![
        ProcSpec::new(300, "chrome", 5, 5).with_children(&[301]),
        ProcSpec::new(301, "chrome", 3, 3)
            .with_parent(300)
            .with_children(&[302]),
        ProcSpec::new(302, "chrome", 7, 2).with_parent(301),
    ]);
    let mut sampler = Sampler::new(multiview_webengine::Accounting::Estimated);

    let first = sampler
        .sample_tree(tree.root(), Pid::new(300))
        .expect("first walk");
    assert_eq!(first.process_count(), 2, "child and grandchild");
    assert_eq!(first.cpu_ticks, 10 + 6 + 9);

    tree.remove(302);
    let second = sampler
        .sample_tree(tree.root(), Pid::new(300))
        .expect("second walk");
    assert_eq!(
        second.exited_cpu_ticks, 9,
        "the departed grandchild's CPU time is retained"
    );
    assert_eq!(second.live_cpu_ticks, 10 + 6);
    assert_eq!(second.cpu_ticks, 25, "the total is unchanged");
}

#[test]
fn a_reused_pid_is_a_new_process_and_its_predecessors_time_is_retained() {
    let tree = ProcTree::build(vec![
        ProcSpec::new(100, "chrome", 12, 8).with_children(&[101]),
        ProcSpec::new(101, "chrome", 20, 10)
            .with_parent(100)
            .with_starttime(500),
    ]);
    let mut sampler = Sampler::new(multiview_webengine::Accounting::Estimated);
    let first = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("first walk");
    assert_eq!(first.cpu_ticks, 20 + 30);
    assert_eq!(record_for(&first, 101).starttime, 500);

    // The kernel reuses pid 101 for a NEW process: a different `starttime` and
    // its own small tick count.
    tree.update_stat(
        &ProcSpec::new(101, "chrome", 3, 2)
            .with_parent(100)
            .with_starttime(999),
    );
    let second = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("second walk");

    assert_eq!(
        record_for(&second, 101).starttime,
        999,
        "the pid now names a different process"
    );
    assert_eq!(
        second.exited_cpu_ticks, 30,
        "the replaced process's CPU time is retained"
    );
    assert_eq!(
        second.live_cpu_ticks,
        20 + 5,
        "the root plus the new process, not the old ticks"
    );
    assert_eq!(
        second.cpu_ticks, 55,
        "old time plus new time, counted once each"
    );
}

#[test]
fn a_live_process_whose_ticks_went_down_is_not_booked_as_exited() {
    let tree = ProcTree::build(vec![
        ProcSpec::new(100, "chrome", 12, 8).with_children(&[101]),
        ProcSpec::new(101, "chrome", 20, 10).with_parent(100),
    ]);
    let mut sampler = Sampler::new(multiview_webengine::Accounting::Estimated);
    let first = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("first walk");
    assert_eq!(first.cpu_ticks, 20 + 30);

    // The same process, same starttime, reporting LESS CPU than before: a
    // counter reset, not an exit.
    tree.update_stat(&ProcSpec::new(101, "chrome", 1, 1).with_parent(100));
    let second = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("second walk");

    assert_eq!(
        second.exited_cpu_ticks, 0,
        "a still-live process is never booked as exited"
    );
    assert_eq!(second.live_cpu_ticks, 20 + 2);
    assert!(
        second.cpu_ticks >= first.cpu_ticks,
        "the monotonic total still holds the earlier peak"
    );
}

#[test]
fn a_comm_containing_parens_and_spaces_parses_from_the_last_paren() {
    // A static file fixture (not a tempdir) for the field-offset corner case:
    // field 2 may itself contain `)`, so everything is counted after the LAST
    // `)`.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let body = std::fs::read_to_string(fixture.join("proc/302/stat")).expect("the fixture");
    assert_eq!(parse_stat_comm(&body).as_deref(), Some("odd )( comm"));

    let sample = sample_process_tree(fixture.as_path(), Pid::new(301), None).expect("a walk");
    assert_eq!(sample.process_count(), 1, "301 has one child, 302");
    let child = record_for(&sample, 302);
    assert_eq!(child.comm, "odd )( comm");
    assert_eq!(
        child.cpu_ticks, 5,
        "utime 2 + stime 3, parsed past the paren"
    );
    assert!(
        (Sampler::tree_cpu_seconds(&sample) - 0.25).abs() < 1e-9,
        "12+8 root ticks plus 5 child ticks"
    );

    // Everything is read through the fixture root: no real /proc is consulted,
    // so a pid that cannot exist on the host still resolves here.
    assert!(!fixture.join("proc/999999").exists());
}

#[test]
fn every_static_stat_fixture_is_a_real_kernel_line() {
    // A fixture that is a field short, or whose activity fields are all zero,
    // would let an off-by-one parser pass. This pins the shape: 50 fields after
    // the comm (fields 3..52), distinct nonzero minflt/cminflt/majflt/cmajflt.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for relative in ["proc/301/stat", "proc/302/stat"] {
        let body = std::fs::read_to_string(fixture.join(relative)).expect("a fixture");
        let tail = body.rsplit_once(')').expect("a comm paren").1;
        let fields: Vec<&str> = tail.split_whitespace().collect();
        assert_eq!(fields.len(), 50, "{relative} must carry fields 3..52");
        let activity: Vec<u64> = (7..11)
            .map(|index| {
                fields[index]
                    .parse::<u64>()
                    .expect("a numeric activity field")
            })
            .collect();
        assert!(
            activity.iter().all(|value| *value != 0),
            "{relative}: minflt/cminflt/majflt/cmajflt must be nonzero: {activity:?}"
        );
        let distinct: std::collections::BTreeSet<u64> = activity.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            4,
            "{relative}: the four activity fields must differ: {activity:?}"
        );
        assert!(
            parse_stat_starttime(&body).is_some(),
            "{relative}: field 22 (starttime) must parse"
        );
    }
}

#[test]
fn the_real_proc_self_stat_line_parses_through_a_root_of_slash() {
    // The one deliberate read of the host's real /proc: a fixture can drift from
    // the kernel's line, and this is what catches it. Read-only, and no value
    // assertion beyond "it parsed".
    let root = Path::new("/");
    let body = root
        .read_to_string("proc/self/stat")
        .expect("the real /proc/self/stat");
    assert!(
        parse_stat_cpu_ticks(&body).is_some(),
        "utime/stime must parse from a real kernel line: {body}"
    );
    assert!(parse_stat_ppid(&body).is_some());
    assert!(parse_stat_starttime(&body).is_some());
    assert!(parse_stat_comm(&body).is_some());
}

#[test]
fn pss_is_preferred_and_the_rss_fallback_is_labelled() {
    let tree = ProcTree::build(vec![
        ProcSpec::new(200, "chrome", 1, 1)
            .with_parent(1)
            .with_children(&[201, 202]),
        ProcSpec::new(201, "chrome", 1, 1)
            .with_parent(200)
            .with_type("renderer")
            .with_pss_kib(1024),
        // No `Pss:` line: the sampler must fall back to `Rss` and SAY SO.
        ProcSpec::new(202, "chrome", 1, 1)
            .with_parent(200)
            .with_type("utility")
            .with_rss_only_kib(4096),
    ]);
    let sample = sample_process_tree(tree.root(), Pid::new(200), None).expect("a walk");
    let renderer = record_for(&sample, 201);
    let utility = record_for(&sample, 202);
    assert_eq!(renderer.memory_source, MemorySource::Pss);
    assert_eq!(renderer.memory_bytes, 1024 * 1024);
    assert_eq!(
        utility.memory_source,
        MemorySource::Rss,
        "fallback is labelled"
    );
    assert_eq!(utility.memory_bytes, 4096 * 1024);
    assert_eq!(
        sample.memory_source,
        MemorySource::Rss,
        "a mixed sum is reported as the weaker source"
    );
    assert_eq!(sample.memory_bytes, (1024 + 4096) * 1024);
}

#[test]
fn memory_is_unavailable_not_zero_when_smaps_rollup_is_absent() {
    let tree = ProcTree::build(vec![
        ProcSpec::new(400, "chrome", 0, 0)
            .with_parent(1)
            .with_children(&[401]),
        // No smaps_rollup written at all.
        ProcSpec::new(401, "chrome", 0, 0)
            .with_parent(400)
            .with_type("renderer"),
    ]);
    let sample = sample_process_tree(tree.root(), Pid::new(400), None).expect("a walk");
    let child = record_for(&sample, 401);
    assert_eq!(child.memory_source, MemorySource::Unavailable);
    assert_eq!(
        sample.memory_source,
        MemorySource::Unavailable,
        "every record was unmeasurable: unknown, never Rss"
    );
}

#[test]
fn an_empty_tree_reports_memory_unavailable_rather_than_a_source() {
    let tree = ProcTree::build(vec![ProcSpec::new(700, "chrome", 4, 4)]);
    let sample = sample_process_tree(tree.root(), Pid::new(700), None).expect("a walk");
    assert_eq!(sample.process_count(), 0);
    assert_eq!(sample.memory_bytes, 0);
    assert_eq!(
        sample.memory_source,
        MemorySource::Unavailable,
        "an empty tree measured nothing; it is not Pss or Rss"
    );
}

#[test]
fn every_documented_separator_and_source_is_read_through_a_root() {
    // The `/proc` paths the walk touches, against an in-memory root: this pins
    // the file layout the parser depends on.
    let root = FakeRoot::default()
        .with_file(
            "proc/500/stat",
            &kernel_stat_line(500, "chrome", 1, 20, 10, 900),
        )
        .with_file("proc/500/task/500/children", "501\n")
        .with_file(
            "proc/501/stat",
            &kernel_stat_line(501, "chrome", 500, 4, 1, 950),
        )
        .with_file("proc/501/cmdline", "/opt/chrome\0--type=gpu-process\0")
        .with_file("proc/501/smaps_rollup", "Pss: 1 kB\n");
    let sample = sample_process_tree(&root, Pid::new(500), None).expect("a walk");
    assert_eq!(sample.cpu_ticks, 35);
    assert_eq!(sample.process_count(), 1);
    assert_eq!(
        sample.type_usage(ChromiumProcessType::GpuProcess).cpu_ticks,
        5
    );
    assert_eq!(sample.memory_bytes, 1024);

    // A reading against a root whose cgroup files do not exist is an honest
    // error, never a fabricated zero.
    assert!(read_cgroup_usage(&root, "sys/fs/cgroup/x").is_err());
    assert_eq!(normalise("/proc/500/stat"), "proc/500/stat");
}

#[test]
fn an_existing_but_empty_children_file_means_no_children_and_no_scan() {
    // The kernel wrote a `children` file and it is empty: authoritative "no
    // children". A node with a missing strand falls back, so with an EMPTY
    // strand present the ppid scan must not run -- the counting root proves it.
    let tree = ProcTree::build(vec![
        ProcSpec::new(800, "chrome", 1, 1).with_children(&[]),
        // 801 really is a child of 800 by ppid, but 800's empty strand wins.
        ProcSpec::new(801, "chrome", 1, 1).with_parent(800),
    ]);
    let root = CountingRoot::new(tree.root());
    let sample = sample_process_tree(&root, Pid::new(800), None).expect("a walk");
    assert_eq!(
        sample.process_count(),
        0,
        "the empty strand is authoritative"
    );
    assert_eq!(
        root.stat_reads(),
        1,
        "only the root's own stat: no ppid scan"
    );
    assert_eq!(root.children_reads(), 1);
}

#[test]
fn a_missing_children_file_falls_back_per_node_not_only_when_the_union_is_empty() {
    // The root has a real strand naming 901. 901 has NO strand, so 901's own
    // children must be found by the ppid fallback -- the union of the walk is
    // not empty, which is exactly the case the old fallback missed.
    let tree = ProcTree::build(vec![
        ProcSpec::new(900, "chrome", 5, 5).with_children(&[901]),
        ProcSpec::new(901, "chrome", 3, 3).with_parent(900),
        ProcSpec::new(902, "chrome", 7, 2).with_parent(901),
    ]);
    let sample = sample_process_tree(tree.root(), Pid::new(900), None).expect("a walk");
    assert_eq!(
        sample.process_count(),
        2,
        "the grandchild is discovered through the per-node fallback"
    );
    assert_eq!(
        record_for(&sample, 902).cpu_ticks,
        9,
        "and it is classified and summed"
    );
}

#[test]
fn a_two_hundred_process_sweep_reads_stat_once_per_process_and_scans_proc_once() {
    const SIZE: u32 = 200;
    const DESCENDANTS: usize = (SIZE - 1) as usize;

    // (a) No `children` files at all: every node needs the fallback, but the
    // `/proc` ppid scan is built once for the whole sweep.
    let bare = wide_tree(SIZE, false);
    let bare_root = CountingRoot::new(bare.root());
    let started = Instant::now();
    let bare_sample = sample_process_tree(&bare_root, Pid::new(1), None).expect("a walk");
    let bare_elapsed = started.elapsed();
    assert_eq!(bare_sample.process_count(), DESCENDANTS);
    assert_eq!(
        bare_root.stat_reads(),
        1 + SIZE as usize + DESCENDANTS,
        "root + one ppid scan over all {SIZE} pids + one stat per record"
    );
    assert_eq!(
        bare_root.children_reads(),
        1 + DESCENDANTS,
        "one children read attempt per node"
    );
    // A per-node scan would read SIZE stats per node; assert the whole sweep
    // stays linear rather than merely equal to the expected number.
    assert!(
        bare_root.stat_reads() < 3 * SIZE as usize,
        "a sweep must be O(processes), not O(processes x pids): {} stat reads",
        bare_root.stat_reads()
    );

    // (b) Every process has a `children` strand (empty for the leaves): no scan.
    let full = wide_tree(SIZE, true);
    let full_root = CountingRoot::new(full.root());
    let started = Instant::now();
    let full_sample = sample_process_tree(&full_root, Pid::new(1), None).expect("a walk");
    let full_elapsed = started.elapsed();
    assert_eq!(full_sample.process_count(), DESCENDANTS);
    assert_eq!(
        full_root.stat_reads(),
        SIZE as usize,
        "root + one stat per record, with no ppid scan"
    );
    assert_eq!(full_root.children_reads(), SIZE as usize);

    eprintln!(
        "200-process sweep: {bare_stat} stat reads / {bare_children} children reads in {bare_elapsed:?} \
         (no children files); {full_stat} stat reads / {full_children} children reads in {full_elapsed:?} \
         (children files present)",
        bare_stat = bare_root.stat_reads(),
        bare_children = bare_root.children_reads(),
        full_stat = full_root.stat_reads(),
        full_children = full_root.children_reads(),
    );
}
