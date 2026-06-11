//! Per-GPU + whole-system CPU load gauges for the API / web UI (ADR-0017 §4.1).
//!
//! The live-load probe in `multiview-hal` samples each GPU at ~1-4 Hz; the engine
//! poller mirrors each sample into the Prometheus gauges this module registers,
//! and into the whole-system CPU gauge. This crate owns only the **model** — the
//! gauge series, their names, and the bounded `{gpu, vendor}` label scheme — and
//! hands back lock-free [`Gauge`] handles; the poller owns the `set()` calls,
//! so this module does **no** I/O and never blocks the engine (invariant #10).
//!
//! **Decoupling.** This module deliberately does *not* depend on `multiview-hal`'s
//! `DeviceLoad` type (the crate graph keeps telemetry a leaf). The poller reads
//! `multiview_hal::DeviceLoad` and calls these handles; here we define only the
//! neutral [`GpuLabels`] identity and the gauge surface.
//!
//! **Unknown is never a false zero** (ADR-0017 §4.1): a gauge for a metric a
//! vendor cannot report is simply **not registered** for that GPU, so the
//! dashboard shows "n/a" rather than a misleading `0.0`. [`GpuGauges::register`]
//! always registers the always-available metrics (VRAM, compute) and registers
//! the per-engine / session metrics only when the caller declares the vendor
//! exposes them.
//!
//! The whole-system CPU gauge is fed by the **pure**, std-only
//! [`CpuSampler`] (Linux `/proc/stat`); no `sysinfo`/native dep is pulled into
//! the default build.
//!
//! See [gpu-monitoring §4](../../../docs/research/gpu-monitoring-and-scheduling.md)
//! and ADR-0017.

use crate::metrics::{Gauge, Labels, MetricsRegistry};

/// Metric series names (ADR-0017 §4.1). Public so a Prometheus exporter or test
/// can reference them without re-typing the strings.
pub mod names {
    /// GPU core / compute busy ratio (`0.0..=1.0`).
    pub const GPU_COMPUTE_UTIL: &str = "multiview_gpu_compute_utilization_ratio";
    /// GPU encoder-ASIC busy ratio (`0.0..=1.0`); registered only where exposed.
    pub const GPU_ENCODER_UTIL: &str = "multiview_gpu_encoder_utilization_ratio";
    /// GPU decoder-ASIC busy ratio (`0.0..=1.0`); registered only where exposed.
    pub const GPU_DECODER_UTIL: &str = "multiview_gpu_decoder_utilization_ratio";
    /// VRAM bytes in use (the authoritative VRAM-pressure numerator).
    pub const GPU_MEMORY_USED_BYTES: &str = "multiview_gpu_memory_used_bytes";
    /// Total VRAM bytes (the VRAM-pressure denominator).
    pub const GPU_MEMORY_TOTAL_BYTES: &str = "multiview_gpu_memory_total_bytes";
    /// Active NVENC encode-session count (Multiview's own tracked figure).
    pub const GPU_ENCODER_SESSIONS_ACTIVE: &str = "multiview_gpu_encoder_sessions_active";
    /// Discovered per-system NVENC concurrent-session ceiling.
    pub const GPU_ENCODER_SESSION_CEILING: &str = "multiview_gpu_encoder_session_ceiling";
    /// Whole-system CPU busy ratio (`0.0..=1.0`), all cores aggregated.
    pub const CPU_UTIL: &str = "multiview_cpu_utilization_ratio";
}

/// The bounded `{gpu, vendor}` identity labels for one physical GPU's series
/// (ADR-0017 §4.1 — one series set per physical GPU, bounded cardinality).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuLabels {
    gpu: String,
    vendor: String,
}

impl GpuLabels {
    /// Construct GPU labels from a stable device id and a vendor label.
    ///
    /// `gpu` should be the stable device handle (UUID / PCI bus id), never the
    /// volatile enumeration index, so a series is stable across reboots.
    #[must_use]
    pub fn new(gpu: impl Into<String>, vendor: impl Into<String>) -> Self {
        Self {
            gpu: gpu.into(),
            vendor: vendor.into(),
        }
    }

    /// Render to the metrics [`Labels`] set used to key a series.
    #[must_use]
    pub fn to_labels(&self) -> Labels {
        Labels::new()
            .with("gpu", self.gpu.clone())
            .with("vendor", self.vendor.clone())
    }
}

/// Which per-engine signals a vendor exposes, so unknown metrics are **not
/// registered** (no false zero).
///
/// The always-available metrics (compute busy, VRAM used/total) are registered
/// unconditionally; the per-engine encoder/decoder utilisation and the NVENC
/// session counters are registered only when the corresponding flag is set
/// (e.g. encoder/decoder util on NVIDIA + Intel; sessions on NVIDIA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VendorExposes {
    /// The vendor meters the encoder ASIC on its own counter.
    pub encoder_util: bool,
    /// The vendor meters the decoder ASIC on its own counter.
    pub decoder_util: bool,
    /// The vendor exposes a concurrent NVENC session count + ceiling.
    pub encoder_sessions: bool,
}

impl VendorExposes {
    /// NVIDIA: per-engine enc/dec util **and** the NVENC session pair.
    #[must_use]
    pub const fn nvidia() -> Self {
        Self {
            encoder_util: true,
            decoder_util: true,
            encoder_sessions: true,
        }
    }

    /// Intel: per-engine enc/dec util via the i915 PMU; no session ceiling.
    #[must_use]
    pub const fn intel() -> Self {
        Self {
            encoder_util: true,
            decoder_util: true,
            encoder_sessions: false,
        }
    }

    /// AMD: a single combined media term (VCN4+ merges enc/dec). Exposed here as
    /// the encoder-util series only (the combined figure); decoder is left
    /// unregistered so the dashboard does not imply a separate decode counter.
    #[must_use]
    pub const fn amd_combined_media() -> Self {
        Self {
            encoder_util: true,
            decoder_util: false,
            encoder_sessions: false,
        }
    }

    /// Apple: no public per-engine util; only the always-available VRAM/compute
    /// series are registered.
    #[must_use]
    pub const fn apple() -> Self {
        Self {
            encoder_util: false,
            decoder_util: false,
            encoder_sessions: false,
        }
    }
}

/// The registered gauge handles for one physical GPU.
///
/// `Option` fields are `None` exactly where the vendor does not expose the
/// metric — those series are never registered, so a scrape shows no line for
/// them (the "n/a" contract) rather than a false zero. The poller calls the
/// `set` helpers; an `Option` `None` is a silent no-op.
#[derive(Debug, Clone)]
pub struct GpuGauges {
    labels: GpuLabels,
    compute_util: Gauge,
    memory_used_bytes: Gauge,
    memory_total_bytes: Gauge,
    encoder_util: Option<Gauge>,
    decoder_util: Option<Gauge>,
    encoder_sessions_active: Option<Gauge>,
    encoder_session_ceiling: Option<Gauge>,
}

impl GpuGauges {
    /// Register the gauge series for one GPU against `registry`, registering the
    /// per-engine / session series only where `exposes` declares the vendor
    /// reports them.
    ///
    /// Always-available series (compute util, VRAM used/total) are registered
    /// unconditionally. Re-registering the same `(name, labels)` returns the
    /// existing handle, so calling this twice for the same GPU is idempotent.
    #[must_use]
    pub fn register(registry: &MetricsRegistry, labels: GpuLabels, exposes: VendorExposes) -> Self {
        let l = labels.to_labels();
        let compute_util = registry.gauge(names::GPU_COMPUTE_UTIL, l.clone());
        let memory_used_bytes = registry.gauge(names::GPU_MEMORY_USED_BYTES, l.clone());
        let memory_total_bytes = registry.gauge(names::GPU_MEMORY_TOTAL_BYTES, l.clone());
        let encoder_util = exposes
            .encoder_util
            .then(|| registry.gauge(names::GPU_ENCODER_UTIL, l.clone()));
        let decoder_util = exposes
            .decoder_util
            .then(|| registry.gauge(names::GPU_DECODER_UTIL, l.clone()));
        let encoder_sessions_active = exposes
            .encoder_sessions
            .then(|| registry.gauge(names::GPU_ENCODER_SESSIONS_ACTIVE, l.clone()));
        let encoder_session_ceiling = exposes
            .encoder_sessions
            .then(|| registry.gauge(names::GPU_ENCODER_SESSION_CEILING, l.clone()));
        Self {
            labels,
            compute_util,
            memory_used_bytes,
            memory_total_bytes,
            encoder_util,
            decoder_util,
            encoder_sessions_active,
            encoder_session_ceiling,
        }
    }

    /// The GPU's identity labels.
    #[must_use]
    pub const fn labels(&self) -> &GpuLabels {
        &self.labels
    }

    /// Set the compute-busy ratio (clamped to `0.0..=1.0`). A `None` sample is a
    /// no-op (the metric stays at its last value rather than lying with a zero).
    pub fn set_compute_util(&self, ratio: Option<f64>) {
        set_ratio(&self.compute_util, ratio);
    }

    /// Set the VRAM used/total bytes pair. `None` for either is a no-op.
    pub fn set_memory(&self, used_bytes: Option<u64>, total_bytes: Option<u64>) {
        if let Some(used) = used_bytes {
            self.memory_used_bytes.set(bytes_to_f64(used));
        }
        if let Some(total) = total_bytes {
            self.memory_total_bytes.set(bytes_to_f64(total));
        }
    }

    /// Set the encoder-ASIC busy ratio, if this GPU exposes the series and the
    /// sample is known. A no-op otherwise (never a false zero).
    pub fn set_encoder_util(&self, ratio: Option<f64>) {
        if let Some(gauge) = &self.encoder_util {
            set_ratio(gauge, ratio);
        }
    }

    /// Set the decoder-ASIC busy ratio, if exposed and known.
    pub fn set_decoder_util(&self, ratio: Option<f64>) {
        if let Some(gauge) = &self.decoder_util {
            set_ratio(gauge, ratio);
        }
    }

    /// Set the active NVENC session count + the discovered system ceiling, if
    /// this GPU exposes the session series.
    pub fn set_encoder_sessions(&self, active: Option<u32>, ceiling: Option<u32>) {
        if let (Some(gauge), Some(value)) = (&self.encoder_sessions_active, active) {
            gauge.set(count_to_f64(value));
        }
        if let (Some(gauge), Some(value)) = (&self.encoder_session_ceiling, ceiling) {
            gauge.set(count_to_f64(value));
        }
    }
}

/// The whole-system CPU-busy gauge (ADR-0017 §4.1 / efficiency §3.1).
///
/// One unlabelled series; the poller feeds it from the pure [`CpuSampler`].
#[derive(Debug, Clone)]
pub struct CpuGauge {
    util: Gauge,
}

impl CpuGauge {
    /// Register the whole-system CPU-util gauge against `registry`.
    #[must_use]
    pub fn register(registry: &MetricsRegistry) -> Self {
        Self {
            util: registry.gauge(names::CPU_UTIL, Labels::empty()),
        }
    }

    /// Set the whole-system CPU-busy ratio (clamped `0.0..=1.0`). `None` is a
    /// no-op.
    pub fn set_util(&self, ratio: Option<f64>) {
        set_ratio(&self.util, ratio);
    }
}

/// A pure, std-only whole-system CPU-busy sampler reading Linux `/proc/stat`.
///
/// CPU busy fraction is a *delta* between two cumulative jiffie snapshots, so
/// the sampler holds the previous snapshot and computes the busy fraction over
/// the interval between two [`CpuSampler::sample`] calls. It adds **no** native
/// dependency (no `sysinfo`): `/proc/stat` is plain text, std file I/O. On a
/// non-Linux host (or if `/proc/stat` is unreadable) `sample` returns `None`
/// (unknown), never a fabricated value and never a panic.
#[derive(Debug, Clone, Default)]
pub struct CpuSampler {
    previous: Option<CpuTimes>,
}

/// The aggregate-CPU jiffie counts from the first `cpu` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CpuTimes {
    /// Time spent doing work (everything except idle + iowait).
    busy: u64,
    /// Total time (busy + idle + iowait).
    total: u64,
}

impl CpuSampler {
    /// Construct a sampler with no prior snapshot (the first `sample` primes it
    /// and returns `None`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample the whole-system CPU-busy fraction over the interval since the
    /// previous call.
    ///
    /// Returns `None` on the first call (priming the baseline), and on any host
    /// where `/proc/stat` is unavailable or unparseable — unknown, never a
    /// fabricated zero. Otherwise returns the busy fraction in `0.0..=1.0`.
    pub fn sample(&mut self) -> Option<f64> {
        let current = read_proc_stat()?;
        let previous = self.previous.replace(current)?;
        Self::fraction_between(previous, current)
    }

    /// Compute the busy fraction between two cumulative snapshots.
    ///
    /// Returns `None` when no CPU time elapsed (called too fast to differ).
    #[must_use]
    fn fraction_between(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
        let busy_delta = current.busy.saturating_sub(previous.busy);
        let total_delta = current.total.saturating_sub(previous.total);
        if total_delta == 0 {
            return None;
        }
        Some((u64_to_f64(busy_delta) / u64_to_f64(total_delta)).clamp(0.0, 1.0))
    }
}

/// Read and parse the aggregate `cpu` line of Linux `/proc/stat`.
///
/// Returns `None` cleanly when the file is absent/unreadable (non-Linux) or the
/// line is malformed — never a panic.
fn read_proc_stat() -> Option<CpuTimes> {
    let contents = std::fs::read_to_string("/proc/stat").ok()?;
    let line = contents.lines().find(|l| l.starts_with("cpu "))?;
    parse_cpu_line(line)
}

/// Parse one `cpu` aggregate line into [`CpuTimes`].
///
/// Fields (after the `cpu` label) are:
/// `user nice system idle iowait irq softirq steal guest guest_nice`.
/// Busy = total - (idle + iowait); total = sum of all present fields.
fn parse_cpu_line(line: &str) -> Option<CpuTimes> {
    let mut fields = line.split_whitespace();
    // Drop the "cpu" label.
    let label = fields.next()?;
    if label != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields.filter_map(|f| f.parse::<u64>().ok()).collect();
    // Need at least user..idle..iowait to compute a meaningful busy/total.
    if values.len() < 5 {
        return None;
    }
    let total: u64 = values.iter().fold(0_u64, |acc, &v| acc.saturating_add(v));
    let idle = values.get(3).copied().unwrap_or(0);
    let iowait = values.get(4).copied().unwrap_or(0);
    let non_busy = idle.saturating_add(iowait);
    let busy = total.saturating_sub(non_busy);
    Some(CpuTimes { busy, total })
}

/// Clamp a ratio to `0.0..=1.0` and `set` it on `gauge`; a `None` or non-finite
/// sample is a no-op (the gauge keeps its last value, never a false zero).
fn set_ratio(gauge: &Gauge, ratio: Option<f64>) {
    if let Some(value) = ratio {
        if value.is_finite() {
            gauge.set(value.clamp(0.0, 1.0));
        }
    }
}

/// Lossless `u64 -> f64` widening for byte/jiffie counts (`< 2^53`), avoiding
/// `as`.
fn bytes_to_f64(value: u64) -> f64 {
    u64_to_f64(value)
}

/// Lossless `u32 -> f64` widening for small counts (sessions), avoiding `as`.
fn count_to_f64(value: u32) -> f64 {
    f64::from(value)
}

/// Lossless `u64 -> f64` widening for values below `2^53`, avoiding `as`.
fn u64_to_f64(value: u64) -> f64 {
    u32::try_from(value).map_or_else(
        |_| {
            let high = u32::try_from((value >> 32) & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            let low = u32::try_from(value & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            high * 4_294_967_296.0 + low
        },
        f64::from,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    fn registry() -> MetricsRegistry {
        MetricsRegistry::new()
    }

    #[test]
    fn registers_always_available_series_for_every_vendor() {
        let reg = registry();
        let _ = GpuGauges::register(
            &reg,
            GpuLabels::new("GPU-uuid-apple", "apple"),
            VendorExposes::apple(),
        );
        let names: Vec<String> = reg.series().into_iter().map(|s| s.name).collect();
        // Compute + VRAM are always registered.
        assert!(names.iter().any(|n| n == names::GPU_COMPUTE_UTIL));
        assert!(names.iter().any(|n| n == names::GPU_MEMORY_USED_BYTES));
        assert!(names.iter().any(|n| n == names::GPU_MEMORY_TOTAL_BYTES));
        // Apple exposes no per-engine util -> those series are NOT registered
        // (the "n/a, never a false zero" contract).
        assert!(
            !names.iter().any(|n| n == names::GPU_ENCODER_UTIL),
            "blind vendor must not register an encoder-util series"
        );
        assert!(!names.iter().any(|n| n == names::GPU_DECODER_UTIL));
        assert!(!names
            .iter()
            .any(|n| n == names::GPU_ENCODER_SESSIONS_ACTIVE));
    }

    #[test]
    fn nvidia_registers_the_full_series_set() {
        let reg = registry();
        let _ = GpuGauges::register(
            &reg,
            GpuLabels::new("GPU-uuid-nv", "nvidia"),
            VendorExposes::nvidia(),
        );
        let names: Vec<String> = reg.series().into_iter().map(|s| s.name).collect();
        for expected in [
            names::GPU_COMPUTE_UTIL,
            names::GPU_ENCODER_UTIL,
            names::GPU_DECODER_UTIL,
            names::GPU_MEMORY_USED_BYTES,
            names::GPU_MEMORY_TOTAL_BYTES,
            names::GPU_ENCODER_SESSIONS_ACTIVE,
            names::GPU_ENCODER_SESSION_CEILING,
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "NVIDIA must register {expected}"
            );
        }
    }

    #[test]
    fn amd_registers_combined_media_as_encoder_only() {
        let reg = registry();
        let _ = GpuGauges::register(
            &reg,
            GpuLabels::new("GPU-uuid-amd", "amd"),
            VendorExposes::amd_combined_media(),
        );
        let names: Vec<String> = reg.series().into_iter().map(|s| s.name).collect();
        assert!(names.iter().any(|n| n == names::GPU_ENCODER_UTIL));
        // No separate decoder series (VCN4 merges enc/dec).
        assert!(!names.iter().any(|n| n == names::GPU_DECODER_UTIL));
    }

    #[test]
    fn set_helpers_clamp_and_no_op_on_unknown() {
        let reg = registry();
        let gauges = GpuGauges::register(
            &reg,
            GpuLabels::new("GPU-uuid-nv", "nvidia"),
            VendorExposes::nvidia(),
        );
        let labels = GpuLabels::new("GPU-uuid-nv", "nvidia").to_labels();

        gauges.set_compute_util(Some(1.5)); // over-range clamps to 1.0
        assert_eq!(
            reg.gauge(names::GPU_COMPUTE_UTIL, labels.clone()).get(),
            1.0
        );

        gauges.set_compute_util(None); // unknown -> no-op, keeps 1.0
        assert_eq!(
            reg.gauge(names::GPU_COMPUTE_UTIL, labels.clone()).get(),
            1.0
        );

        gauges.set_memory(Some(4_000_000_000), Some(12_000_000_000));
        assert_eq!(
            reg.gauge(names::GPU_MEMORY_USED_BYTES, labels.clone())
                .get(),
            4_000_000_000.0
        );

        gauges.set_encoder_sessions(Some(3), Some(8));
        assert_eq!(
            reg.gauge(names::GPU_ENCODER_SESSIONS_ACTIVE, labels.clone())
                .get(),
            3.0
        );
        assert_eq!(
            reg.gauge(names::GPU_ENCODER_SESSION_CEILING, labels).get(),
            8.0
        );
    }

    #[test]
    fn cpu_fraction_between_snapshots_is_busy_over_total() {
        // 100 busy jiffies out of 400 total elapsed => 0.25.
        let prev = CpuTimes {
            busy: 1000,
            total: 4000,
        };
        let curr = CpuTimes {
            busy: 1100,
            total: 4400,
        };
        let frac = CpuSampler::fraction_between(prev, curr).expect("elapsed time");
        assert!((frac - 0.25).abs() < 1e-9, "got {frac}");
    }

    #[test]
    fn cpu_fraction_is_none_when_no_time_elapsed() {
        let snap = CpuTimes {
            busy: 1000,
            total: 4000,
        };
        assert!(CpuSampler::fraction_between(snap, snap).is_none());
    }

    #[test]
    fn parse_cpu_line_computes_busy_excluding_idle_and_iowait() {
        // user=10 nice=0 system=20 idle=60 iowait=10 -> total=100, busy=30.
        let line = "cpu  10 0 20 60 10 0 0 0 0 0";
        let times = parse_cpu_line(line).expect("valid line");
        assert_eq!(times.total, 100);
        assert_eq!(times.busy, 30); // 100 - (idle 60 + iowait 10)
    }

    #[test]
    fn parse_cpu_line_rejects_non_cpu_or_short_lines() {
        assert!(
            parse_cpu_line("cpu0 1 2 3 4 5").is_none(),
            "per-core line, not aggregate"
        );
        assert!(parse_cpu_line("cpu 1 2 3").is_none(), "too few fields");
        assert!(parse_cpu_line("intr 1 2 3 4 5").is_none());
    }

    #[test]
    fn cpu_sampler_primes_then_reports() {
        // The first sample primes the baseline (None); we can't drive /proc/stat
        // deterministically here, so this exercises the priming contract: the
        // first call never panics and returns None or Some without crashing.
        let mut sampler = CpuSampler::new();
        let first = sampler.sample();
        // On Linux CI the first call primes -> None; on non-Linux -> None too.
        // Either way it must not panic.
        let _ = first;
    }
}
