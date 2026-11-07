//! Process-tree walking against a **fixture** `/proc`, never the host's.
//!
//! The regression this file exists for: ADR-0099's measured bug where a sampler
//! matched one Chromium process by command line and reported 0% CPU / 100 MB
//! while ~95% of the CPU sat in the GPU process. Every test here therefore
//! asserts on the *whole tree*, and the six-type case asserts the per-type split
//! so a regression to "one process" fails loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

mod common;

use common::{normalise, FakeRoot, ProcSpec, ProcTree};

use multiview_webengine::{
    parse_stat_comm, read_cgroup_usage, sample_process_tree, ChromiumProcessType, MemorySource,
    Pid, ProcRecord, Sampler, CHROMIUM_PROCESS_TYPES,
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
fn a_process_tree_produces_a_cpu_fraction_from_two_readings() {
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
    let fraction = sampler
        .record(10.0, Sampler::tree_cpu_seconds(&first))
        .expect("a positive interval");
    assert!(
        (fraction - 0.03).abs() < 1e-4,
        "0.3 CPU-s / 10 s, got {fraction}"
    );
}

#[test]
fn exited_child_time_is_retained_so_totals_never_go_backwards() {
    let tree = chromium_tree();
    let mut sampler = Sampler::new(multiview_webengine::Accounting::Estimated);

    // Sample A: the full tree, 125 ticks.
    let first = sampler
        .sample_tree(tree.root(), Pid::new(100))
        .expect("first walk");
    assert_eq!(first.cpu_ticks, expected_ticks());
    assert_eq!(first.exited_cpu_ticks, 0);

    // The renderer (25 ticks) exits before the next walk.
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
fn a_comm_containing_parens_and_spaces_parses_from_the_last_paren() {
    // A static file fixture (not a tempdir) for the field-offset corner case:
    // field 2 may itself contain `)`, so everything is counted after the LAST
    // `)`.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
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
        MemorySource::Rss,
        "the tree sum is not all-Pss"
    );
}

#[test]
fn every_documented_separator_and_source_is_read_through_a_root() {
    // The `/proc` paths the walk touches, against an in-memory root: this pins
    // the file layout the parser depends on.
    let root = FakeRoot::default()
        .with_file(
            "proc/500/stat",
            "500 (chrome) S 1 500 500 0 -1 0 0 0 0 0 20 10 0 0 20 0 1 0 9 0 0\n",
        )
        .with_file("proc/500/task/500/children", "501\n")
        .with_file(
            "proc/501/stat",
            "501 (chrome) S 500 501 501 0 -1 0 0 0 0 0 4 1 0 0 20 0 1 0 9 0 0\n",
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
