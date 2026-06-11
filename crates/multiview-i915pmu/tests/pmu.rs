//! Tests for the i915 PMU FFI leaf crate.
//!
//! The pure core is tested on every target: the engine-class `config` encoding
//! is cross-checked against the kernel `I915_PMU_ENGINE_BUSY` macro expansion,
//! and the busy-ns → fraction two-snapshot diff is exercised on fixture counters
//! (no real PMU). The live `perf_event_open` read is **gated**: it needs a real
//! i915 device and `perf_event_open` permission (`CAP_PERFMON` / a permissive
//! `perf_event_paranoid`), which a sandbox/CI container usually lacks, so the
//! live test only asserts the call does not panic and returns a structurally
//! valid counter *when* it returns one — it never requires a reading.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_i915pmu::{
    busy_fraction, engine_busy_config, ENGINE_CLASS_RENDER, ENGINE_CLASS_VIDEO,
    ENGINE_CLASS_VIDEO_ENHANCE,
};

/// The kernel `I915_PMU_ENGINE_BUSY(class, instance)` macro from
/// `<drm/i915_drm.h>`, recomputed here independently so a drift in our encoding
/// is caught: `(class << 12) | (instance << 4) | I915_SAMPLE_BUSY`.
fn kernel_macro(class: u64, instance: u64) -> u64 {
    const I915_PMU_CLASS_SHIFT: u64 = 12; // SAMPLE_BITS(4) + INSTANCE_BITS(8)
    const I915_SAMPLE_BUSY: u64 = 0;
    (class << I915_PMU_CLASS_SHIFT) | (instance << 4) | I915_SAMPLE_BUSY
}

#[test]
fn engine_class_constants_match_kernel_abi() {
    // I915_ENGINE_CLASS_RENDER = 0, VIDEO = 2, VIDEO_ENHANCE = 3.
    assert_eq!(ENGINE_CLASS_RENDER, 0);
    assert_eq!(ENGINE_CLASS_VIDEO, 2);
    assert_eq!(ENGINE_CLASS_VIDEO_ENHANCE, 3);
}

#[test]
fn busy_config_matches_kernel_macro_for_video_engines() {
    // Video (VCS) instance 0 — the bitstream decode/encode engine the probe
    // folds into enc/dec util.
    assert_eq!(
        engine_busy_config(ENGINE_CLASS_VIDEO, 0),
        kernel_macro(u64::from(ENGINE_CLASS_VIDEO), 0)
    );
    // A second video engine instance (multi-VCS parts) encodes distinctly.
    assert_eq!(
        engine_busy_config(ENGINE_CLASS_VIDEO, 1),
        kernel_macro(u64::from(ENGINE_CLASS_VIDEO), 1)
    );
    // VideoEnhance (VECS).
    assert_eq!(
        engine_busy_config(ENGINE_CLASS_VIDEO_ENHANCE, 0),
        kernel_macro(u64::from(ENGINE_CLASS_VIDEO_ENHANCE), 0)
    );
    // Render (RCS) instance 0.
    assert_eq!(
        engine_busy_config(ENGINE_CLASS_RENDER, 0),
        kernel_macro(u64::from(ENGINE_CLASS_RENDER), 0)
    );
}

#[test]
fn busy_config_known_value() {
    // VIDEO=2 << 12 = 0x2000, instance 0, sample 0 => 0x2000.
    assert_eq!(engine_busy_config(ENGINE_CLASS_VIDEO, 0), 0x2000);
    // VIDEO_ENHANCE=3 << 12 = 0x3000.
    assert_eq!(engine_busy_config(ENGINE_CLASS_VIDEO_ENHANCE, 0), 0x3000);
    // instance bits sit at bits 4..12: instance 1 adds 0x10.
    assert_eq!(engine_busy_config(ENGINE_CLASS_VIDEO, 1), 0x2010);
}

#[test]
fn busy_fraction_differences_two_snapshots() {
    // The i915 PMU busy counter is monotonically increasing busy-ns. The busy
    // fraction over a wall interval is (delta busy-ns) / (interval wall-ns).
    // 500_000 ns busy over a 1_000_000 ns (1 ms) interval => 0.5.
    let frac = busy_fraction(2_000_000, 2_500_000, 1_000_000).expect("known fraction");
    assert!((frac - 0.5).abs() < 1e-4, "0.5 expected, got {frac}");
}

#[test]
fn busy_fraction_clamps_and_guards() {
    // A zero interval is a divide guard => None, never a panic.
    assert_eq!(busy_fraction(0, 9_000_000, 0), None);
    // A delta exceeding the interval clamps to 1.0 (a saturated engine), never
    // exceeds the unit interval.
    assert_eq!(busy_fraction(0, 9_000_000, 1_000_000), Some(1.0));
    // No busy time at all => 0.0 (a genuine idle reading, not unknown).
    assert_eq!(busy_fraction(10, 10, 1_000_000), Some(0.0));
}

#[test]
fn busy_fraction_counter_went_backwards_is_unknown() {
    // A counter reset / wrap (later < earlier) is unknown this tick — None, not
    // a fabricated zero and not a panic.
    assert_eq!(busy_fraction(5_000_000, 1_000_000, 1_000_000), None);
}

#[test]
fn busy_fraction_quarter_and_three_quarter_precise() {
    for (delta, interval, expect) in [
        (250_000_u64, 1_000_000_u64, 0.25_f32),
        (750_000, 1_000_000, 0.75),
        (1, 1, 1.0),
        (0, 4, 0.0),
    ] {
        let frac = busy_fraction(0, delta, interval).expect("known");
        assert!(
            (frac - expect).abs() < 1e-4,
            "delta={delta} interval={interval} => {frac}, want {expect}"
        );
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn counter_open_is_unavailable_off_linux() {
    use multiview_i915pmu::I915PmuCounter;
    // No i915 PMU off Linux: opening any counter is always None.
    assert!(I915PmuCounter::open(0, engine_busy_config(ENGINE_CLASS_VIDEO, 0)).is_none());
}

/// Live-gated: actually attempt `perf_event_open` on the i915 PMU on Linux.
///
/// This proves the FFI binding is wired correctly (no panic, no UB) on whatever
/// host runs it. It does NOT require the counter to open — that needs a real
/// i915 GPU and `perf_event_open` permission, which CI/this sandbox lacks. When
/// no i915 PMU `type` file is found, or the open is denied, the test passes on
/// the honest `None` fallback the probe relies on. Set `MULTIVIEW_I915_LIVE=1` to
/// require a real reading (only on a host with an Intel GPU + perms).
#[cfg(target_os = "linux")]
#[test]
fn live_open_does_not_panic_and_reads_monotonic() {
    use multiview_i915pmu::{read_pmu_type, I915PmuCounter};
    use std::path::PathBuf;

    // A tiny self-contained resolver (no glob crate dep) for the first i915 PMU
    // `type` file (single-GPU plain path or a per-device `i915_<pci>` dir).
    fn glob_first_i915_type() -> Option<PathBuf> {
        let entries = std::fs::read_dir("/sys/devices").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "i915" || name.starts_with("i915_") {
                let candidate = entry.path().join("type");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    // Resolve the first i915 PMU `type` file, if any. Absent on a non-Intel host.
    let pmu_type = glob_first_i915_type().and_then(|p| read_pmu_type(&p));

    let require = std::env::var_os("MULTIVIEW_I915_LIVE").is_some();

    let Some(pmu_type) = pmu_type else {
        assert!(
            !require,
            "MULTIVIEW_I915_LIVE set but no i915 PMU type file present"
        );
        return; // No Intel GPU here: the honest unavailable fallback.
    };

    let config = engine_busy_config(ENGINE_CLASS_VIDEO, 0);
    match I915PmuCounter::open(pmu_type, config) {
        Some(counter) => {
            // Two reads a moment apart: the busy-ns counter is monotonic, so the
            // second is never less than the first.
            let first = counter.read_busy_ns();
            let second = counter.read_busy_ns();
            if let (Some(a), Some(b)) = (first, second) {
                assert!(b >= a, "i915 busy-ns counter must be monotonic: {a} -> {b}");
            }
        }
        None => {
            // Denied (perf_event_paranoid / no CAP_PERFMON) — the documented
            // fallback. Only fail if the caller demanded a live reading.
            assert!(
                !require,
                "MULTIVIEW_I915_LIVE set but perf_event_open was denied"
            );
        }
    }
}
