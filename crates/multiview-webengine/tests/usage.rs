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
    attribute, cpu_cores, monotonic_total, read_cgroup_usage, type_cpu_seconds, usage_from_cgroup,
    Accounting, Generation, JobPhase, Pid, ResourceUsage, Sampler, ScopeOwner, ScopeRegistry,
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
    // The diagnostics view of ADR-0099's measured bug: the GPU process carries
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
    let sample = sampler
        .sample_tree(tree.root(), Pid::new(600))
        .expect("a walk");
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
        .register(multiview_webengine::ResourceScope::new(
            owner.clone(),
            first,
            multiview_webengine::ScopeKind::ProcessTree { root: Pid::new(1) },
        ))
        .expect("the next generation is accepted");
    assert_eq!(attribute(&registry, &owner, first), Some(first));
    let second = registry.next_generation(&owner);
    registry
        .register(multiview_webengine::ResourceScope::new(
            owner.clone(),
            second,
            multiview_webengine::ScopeKind::ProcessTree { root: Pid::new(2) },
        ))
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
