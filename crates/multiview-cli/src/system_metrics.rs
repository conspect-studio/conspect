//! The off-hot-path **system-metrics poller** (ADR-0017): a small task that
//! samples whole-system CPU + host memory + per-GPU load at ~1.3 Hz and
//! **pushes** a [`multiview_events::SystemMetrics`] onto the engine's outbound
//! event stream, so the management UI's footer lights up with live data.
//!
//! Per the realtime-api contract these metrics are **pushed** over the realtime
//! stream, never polled by the UI (a REST poll of a per-second value is the
//! wrong shape). The task publishes through the engine's drop-oldest
//! [`EnginePublisher`](multiview_engine::EnginePublisher) event stream, whose
//! `publish` never awaits or blocks a slow subscriber (invariant #10): a stalled
//! UI simply skips samples; it can never back-pressure this task and this task
//! can never back-pressure the engine.
//!
//! The sampling → assembly step is the pure, sync, dependency-light
//! [`assemble_metrics`] function (no async, no sockets, no GPU), so the full
//! `DeviceLoad` → `GpuMetrics` mapping is unit-testable on any host. The live
//! task ([`spawn`]) wires it to the std-only [`CpuSampler`](multiview_telemetry::CpuSampler)
//! (Linux `/proc/stat`), a pure `/proc/meminfo` reader, and an injected
//! [`LoadSource`](multiview_hal::LoadSource) (the NVML poller when the `cuda`
//! feature is on, else the always-compiled
//! [`NullLoadPoller`](multiview_hal::NullLoadPoller)).

use std::sync::Arc;
use std::time::Duration;

use multiview_control::EngineStateSnapshot;
use multiview_engine::{EnginePublisher, StopSignal};
use multiview_events::{Event, GpuMetrics, GpuVendor, SystemMetrics};
use multiview_hal::{DeviceLoad, LoadSource, SelfShare, Vendor};

/// The system-metrics sampling period: 750 ms ≈ 1.3 Hz — inside the ADR-0017
/// ~1-4 Hz envelope and matching the "conflated, a couple of samples a second"
/// cadence the footer expects. A const so the cadence is named once and the
/// published `sampled_hz` is derived from it.
pub const SAMPLE_PERIOD: Duration = Duration::from_millis(750);

/// The effective wire cadence reported in [`SystemMetrics::sampled_hz`], derived
/// from [`SAMPLE_PERIOD`] (rounded to the nearest whole Hz). 750 ms → 1 Hz.
#[must_use]
pub fn sampled_hz() -> u32 {
    let millis = SAMPLE_PERIOD.as_millis();
    if millis == 0 {
        return 0;
    }
    // round(1000 / millis); millis is a small positive value here.
    let hz = (1000 + millis / 2) / millis;
    u32::try_from(hz).unwrap_or(u32::MAX)
}

/// Map a hal [`Vendor`] to the wire [`GpuVendor`] the footer renders.
///
/// Both enums are `#[non_exhaustive]`; an unclassified future hal vendor maps to
/// [`GpuVendor::Other`] rather than guessing or panicking.
#[must_use]
pub fn map_vendor(vendor: Vendor) -> GpuVendor {
    match vendor {
        Vendor::Nvidia => GpuVendor::Nvidia,
        Vendor::Intel => GpuVendor::Intel,
        Vendor::Amd => GpuVendor::Amd,
        Vendor::Apple => GpuVendor::Apple,
        _ => GpuVendor::Other,
    }
}

/// Map one live [`DeviceLoad`] snapshot — plus our optional per-process
/// [`SelfShare`] for the same device — to the wire [`GpuMetrics`] sample.
///
/// The optional per-engine signals (`encoder_util`, `decoder_util`,
/// `encoder_sessions`) carry straight through as `Option` — an unknown stays
/// `None` (the "n/a, never a false zero" contract). The wire's **non-optional**
/// fields (`compute_util`, `mem_used_bytes`, `mem_total_bytes`) have no `None`
/// representation, so an unknown collapses to `0` there; the compute term
/// prefers the dedicated compute-queue fraction over the 3D-engine one
/// ([`DeviceLoad::effective_compute_frac`]). `encoder_session_ceiling` is not in
/// a `DeviceLoad` (it is a per-system discovery, not a per-device sample), so it
/// is honestly `None` here.
///
/// The `self_*` fields (monitoring v2: "ours vs total") come from `share` when a
/// matching [`SelfShare`] was sampled; `None` (no NVML per-process counter, or we
/// are not resident on this device) leaves every `self_*` field absent — never a
/// false zero.
#[must_use]
pub fn map_gpu(load: &DeviceLoad, share: Option<&SelfShare>) -> GpuMetrics {
    GpuMetrics {
        id: load.device_id.stable_id().to_owned(),
        vendor: map_vendor(load.device_id.vendor()),
        name: None,
        compute_util: load.effective_compute_frac().unwrap_or(0.0),
        mem_used_bytes: load.vram_used_bytes.unwrap_or(0),
        mem_total_bytes: load.vram_total_bytes.unwrap_or(0),
        encoder_util: load.enc_util_frac,
        decoder_util: load.dec_util_frac,
        encoder_sessions: load.nvenc_session_count,
        encoder_session_ceiling: None,
        // Our-process share from the NVML per-process pass (monitoring v2), matched
        // to this device by id; absent fields stay None (never a false zero).
        self_compute_util: share.and_then(|s| s.compute_util),
        self_encoder_util: share.and_then(|s| s.encoder_util),
        self_decoder_util: share.and_then(|s| s.decoder_util),
        self_mem_used_bytes: share.and_then(|s| s.mem_used_bytes),
        self_encoder_sessions: share.and_then(|s| s.encoder_sessions),
    }
}

/// Find the [`SelfShare`] that describes `load`'s device, matching on the stable
/// [`multiview_hal::DeviceId`] (a `SelfShare` for a different device must never be
/// misattributed). `None` when no share matches.
#[must_use]
fn share_for<'a>(load: &DeviceLoad, shares: &'a [SelfShare]) -> Option<&'a SelfShare> {
    shares.iter().find(|s| s.device_id == load.device_id)
}

/// Assemble a [`SystemMetrics`] wire sample from already-sampled inputs — the
/// **pure** core of the poller (no async, no I/O, no GPU).
///
/// * `cpu` is the whole-system busy fraction (`0.0..=1.0`).
/// * `self_cpu` is **our process's** share of total host CPU capacity
///   (`0.0..=1.0`, sampled from `/proc/self/stat`), on the same scale as `cpu`;
///   `None` off Linux / on a parse failure (never a false zero).
/// * `mem` is the host `(used, total)` byte pair when known, else `None` (both
///   `mem_*` fields then stay absent — honest unknown).
/// * `self_rss` is our process's resident memory in bytes (`/proc/self/status`
///   `VmRSS`), else `None`.
/// * `loads` are the live per-GPU snapshots; an empty slice yields an empty
///   `gpus` list (a GPU-free host), never a fabricated device.
/// * `shares` are our per-device per-process GPU shares (monitoring v2), matched
///   to each `load` by stable device id; a device with no matching share keeps
///   its `self_*` fields absent.
/// * `fps` is the aggregate program output rate when running, else `None` (never
///   fabricated).
/// * `hz` is the effective wire cadence to stamp.
#[expect(
    clippy::too_many_arguments,
    reason = "this is the pure, fully-injected assembler seam (every sampled input \
              passed in so it stays I/O-free and unit-testable); grouping the \
              self_* inputs into a struct would only move the argument list, not \
              reduce the coupling, and would obscure the 1:1 wire mapping"
)]
#[must_use]
pub fn assemble_metrics(
    cpu: f32,
    self_cpu: Option<f32>,
    mem: Option<(u64, u64)>,
    self_rss: Option<u64>,
    loads: &[DeviceLoad],
    shares: &[SelfShare],
    fps: Option<f32>,
    hz: u32,
) -> SystemMetrics {
    let (mem_used_bytes, mem_total_bytes) = match mem {
        Some((used, total)) => (Some(used), Some(total)),
        None => (None, None),
    };
    SystemMetrics {
        cpu_util: cpu.clamp(0.0, 1.0),
        mem_used_bytes,
        mem_total_bytes,
        // Our-process share (monitoring v2): sampled from /proc by the impure task
        // and passed in, so the assembler stays I/O-free. Clamp the CPU fraction to
        // the unit interval; `None` stays absent (never a false zero).
        self_cpu_util: self_cpu.map(|frac| frac.clamp(0.0, 1.0)),
        self_mem_used_bytes: self_rss,
        gpus: loads
            .iter()
            .map(|load| map_gpu(load, share_for(load, shares)))
            .collect(),
        program_fps: fps,
        sampled_hz: hz,
    }
}

/// Read the host's `(used, total)` memory in bytes from Linux `/proc/meminfo`,
/// pure std (no `sysinfo`/native dep).
///
/// `used = MemTotal - MemAvailable` (the kernel's own availability estimate,
/// matching what a user sees as "in use"). Returns `None` cleanly when the file
/// is absent/unreadable (non-Linux) or either field is missing — unknown, never
/// a fabricated value and never a panic.
#[must_use]
pub fn read_proc_meminfo() -> Option<(u64, u64)> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&contents)
}

/// Parse a `/proc/meminfo` body into a host `(used, total)` byte pair.
///
/// The file reports `MemTotal:` and `MemAvailable:` as kibibyte counts
/// (`<label>:\t<value> kB`). `used = total - available`, saturating at `0`.
/// Returns `None` if either line is absent or unparsable.
#[must_use]
fn parse_meminfo(body: &str) -> Option<(u64, u64)> {
    let total_kib = find_meminfo_kib(body, "MemTotal:")?;
    let avail_kib = find_meminfo_kib(body, "MemAvailable:")?;
    let total = total_kib.saturating_mul(1024);
    let avail = avail_kib.saturating_mul(1024);
    let used = total.saturating_sub(avail);
    Some((used, total))
}

/// Find a `/proc/meminfo` `<label> <value> kB` line and return its kibibyte
/// value. `None` if the label is absent or the value does not parse.
#[must_use]
fn find_meminfo_kib(body: &str, label: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Read **our process's** resident memory (`VmRSS`) in bytes from Linux
/// `/proc/self/status`, pure std (no native dep).
///
/// Returns `None` cleanly when the file is absent/unreadable (non-Linux) or the
/// `VmRSS:` line is missing/malformed — the honest unknown, never a false zero.
#[must_use]
pub fn read_self_rss() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_vmrss_bytes(&contents)
}

/// Parse the `VmRSS:` line of a `/proc/self/status` body into bytes.
///
/// The kernel reports `VmRSS:\t<value> kB` (kibibytes); we return `value * 1024`.
/// `None` if the line is absent or the value does not parse.
#[must_use]
fn parse_vmrss_bytes(body: &str) -> Option<u64> {
    let kib = body
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())?;
    Some(kib.saturating_mul(1024))
}

/// A pure, std-only sampler of **our process's** CPU share, on the same
/// `0.0..=1.0` scale as the whole-host CPU busy fraction.
///
/// Our busy time is `utime + stime` from `/proc/self/stat` (clock-tick "jiffie"
/// counts); we divide our jiffie delta by the host's TOTAL jiffie delta from the
/// `cpu` aggregate line of `/proc/stat` over the SAME interval. Because both are
/// in the kernel's clock-tick unit, the unit cancels — so the result is "we used
/// X of the host's Y total capacity" and composes directly with `cpu_util`, and
/// `_SC_CLK_TCK` need not be queried (it would only matter if the two sources used
/// different tick rates, which they do not). Adds **no** native dependency.
///
/// Like the whole-host [`CpuSampler`](multiview_telemetry::CpuSampler), the first
/// [`sample`](SelfCpuSampler::sample) primes the baseline and returns `None`; off
/// Linux (or on any read/parse failure) it returns `None` too — never a
/// fabricated value, never a panic.
#[derive(Debug, Clone, Default)]
pub struct SelfCpuSampler {
    previous: Option<SelfCpuTimes>,
}

/// One paired snapshot: our cumulative busy jiffies and the host's cumulative
/// total jiffies, read at the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SelfCpuTimes {
    /// Our process's cumulative busy jiffies (`utime + stime`).
    self_busy: u64,
    /// The host's cumulative total jiffies (sum of the `cpu` aggregate line).
    host_total: u64,
}

impl SelfCpuSampler {
    /// Construct a sampler with no prior snapshot (the first `sample` primes it
    /// and returns `None`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample our process's CPU share over the interval since the previous call.
    ///
    /// Returns `None` on the first call (priming the baseline), and on any host
    /// where `/proc/self/stat` or `/proc/stat` is unavailable/unparseable —
    /// unknown, never a fabricated zero. Otherwise a fraction of total host
    /// capacity in `0.0..=1.0`.
    pub fn sample(&mut self) -> Option<f32> {
        let current = read_self_cpu_times()?;
        let previous = self.previous.replace(current)?;
        Self::fraction_between(previous, current)
    }

    /// Compute our CPU-capacity share between two paired snapshots: our busy delta
    /// over the host's total delta, clamped to `0.0..=1.0`.
    ///
    /// Returns `None` when no host time elapsed (called too fast to differ).
    #[must_use]
    fn fraction_between(previous: SelfCpuTimes, current: SelfCpuTimes) -> Option<f32> {
        let self_delta = current.self_busy.saturating_sub(previous.self_busy);
        let host_delta = current.host_total.saturating_sub(previous.host_total);
        if host_delta == 0 {
            return None;
        }
        Some(jiffie_ratio_to_f32(self_delta, host_delta))
    }
}

/// Read the paired `(self busy, host total)` jiffie snapshot, or `None` if either
/// `/proc` file is unavailable/unparseable.
fn read_self_cpu_times() -> Option<SelfCpuTimes> {
    let self_stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let proc_stat = std::fs::read_to_string("/proc/stat").ok()?;
    let self_busy = parse_self_stat_busy_jiffies(&self_stat)?;
    let host_total = parse_proc_stat_total_jiffies(&proc_stat)?;
    Some(SelfCpuTimes {
        self_busy,
        host_total,
    })
}

/// Parse `utime + stime` (fields 14 + 15) from a `/proc/self/stat` body.
///
/// Field 2 (`comm`) is parenthesised and may itself contain spaces and `)`
/// characters, so the safe parse is to split AFTER the **last** `)`: the
/// remaining whitespace-separated tokens begin at field 3 (`state`). `utime` is
/// then token index 11 and `stime` token index 12 (0-based) of that tail.
/// Returns `None` if there is no `)` or too few trailing fields.
#[must_use]
fn parse_self_stat_busy_jiffies(body: &str) -> Option<u64> {
    let tail = body.rsplit_once(')').map(|(_, rest)| rest)?;
    let mut fields = tail.split_whitespace();
    // After the comm's ')', fields are: state(0) ppid(1) ... utime(11) stime(12).
    let utime = fields.nth(11)?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

/// Parse the total jiffies from the `cpu` aggregate line of a `/proc/stat` body
/// (the sum of every field on that line).
///
/// Returns `None` if the aggregate `cpu ` line is absent or carries no numeric
/// fields.
#[must_use]
fn parse_proc_stat_total_jiffies(body: &str) -> Option<u64> {
    let line = body.lines().find(|l| l.starts_with("cpu "))?;
    let mut fields = line.split_whitespace();
    let label = fields.next()?;
    if label != "cpu" {
        return None;
    }
    let mut total: u64 = 0;
    let mut saw_value = false;
    for field in fields {
        if let Ok(value) = field.parse::<u64>() {
            total = total.saturating_add(value);
            saw_value = true;
        }
    }
    if saw_value {
        Some(total)
    } else {
        None
    }
}

/// `self_delta / host_delta` as an `f32` fraction clamped to `0.0..=1.0`, via the
/// same lossless `u64 -> f64` widening + `as`-free narrowing the host CPU path
/// uses (no `as` casts). `host_delta` is assumed `> 0` by the caller.
#[must_use]
fn jiffie_ratio_to_f32(self_delta: u64, host_delta: u64) -> f32 {
    let numerator = u64_to_f64(self_delta);
    let denominator = u64_to_f64(host_delta);
    let frac = (numerator / denominator).clamp(0.0, 1.0);
    frac_to_f32(frac)
}

/// Lossless `u64 -> f64` widening for jiffie counts (`< 2^53`), avoiding `as`.
/// Mirrors the same composition the telemetry CPU path uses.
#[must_use]
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

/// Select the per-GPU [`LoadSource`] for this build + host.
///
/// With the `cuda` feature on, this tries the runtime-loaded NVML poller and
/// uses it when an NVIDIA driver/device is present. With the `vaapi`/`qsv`
/// feature on, it then tries the AMD/Intel DRM sysfs + `/proc` fdinfo poller
/// (base gpu-busy / VRAM plus the merged media-engine term). On a host with no
/// matching GPU (CI, this container) each probe reports absence gracefully and
/// it falls back to the always-compiled
/// [`NullLoadPoller`](multiview_hal::NullLoadPoller). Either way the returned
/// source is a working `Box<dyn LoadSource>` that polls without blocking the
/// engine.
#[must_use]
pub fn default_load_source() -> Box<dyn LoadSource + Send + Sync> {
    #[cfg(feature = "cuda")]
    {
        // ~4 Hz per-pass bound (the metrics task polls it far slower, at
        // SAMPLE_PERIOD); the interval only caps the cost of one NVML pass.
        let interval = multiview_hal::PollInterval::from_millis(250);
        if let Some(poller) = multiview_hal::NvmlLoadPoller::try_init(interval) {
            tracing::info!("system-metrics: NVML per-GPU load source active");
            return Box::new(poller);
        }
        tracing::debug!("system-metrics: NVML unavailable; trying the next per-GPU source");
    }
    #[cfg(any(feature = "vaapi", feature = "qsv"))]
    {
        // AMD/Intel: the DRM sysfs + /proc fdinfo per-GPU load source. Returns
        // None on a no-DRM host, so the null fallback below stays reachable.
        if let Some(poller) = multiview_hal::SysfsLoadPoller::try_init() {
            tracing::info!("system-metrics: DRM sysfs per-GPU load source active");
            return Box::new(poller);
        }
        tracing::debug!("system-metrics: no DRM GPU; using the no-GPU load source");
    }
    Box::new(multiview_hal::NullLoadPoller::new())
}

/// Spawn the off-hot-path system-metrics task on the current Tokio runtime.
///
/// Every [`SAMPLE_PERIOD`] the task samples the whole-system CPU
/// ([`CpuSampler`](multiview_telemetry::CpuSampler)), host memory
/// ([`read_proc_meminfo`]), and the injected per-GPU [`LoadSource`], assembles a
/// [`SystemMetrics`] via [`assemble_metrics`], and publishes
/// [`Event::SystemMetrics`] onto the engine's outbound event stream. The publish
/// is a single non-blocking drop-oldest broadcast send — it can neither block on
/// nor be back-pressured by any subscriber (invariant #10).
///
/// `fps` is the aggregate program output rate when the run exposes one, else
/// `None` (never fabricated). The task selects on `stop`: it returns promptly
/// (within one `SAMPLE_PERIOD`) once the run's [`StopSignal`] is raised, so it
/// tears down cleanly with the rest of the run. The returned [`JoinHandle`] lets
/// the caller `abort`/await it; dropping it detaches the (self-stopping) task.
#[must_use]
pub fn spawn(
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    load_source: Box<dyn LoadSource + Send + Sync>,
    stop: StopSignal,
    fps: Option<f32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(publisher.as_ref(), load_source.as_ref(), &stop, fps).await;
    })
}

/// The system-metrics task body: sample → assemble → publish on a fixed cadence
/// until `stop` is raised. Factored out of [`spawn`] so it is driveable directly
/// in a test with an injected publisher + load source.
pub(crate) async fn run(
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    load_source: &(dyn LoadSource + Send + Sync),
    stop: &StopSignal,
    fps: Option<f32>,
) {
    let mut cpu = multiview_telemetry::CpuSampler::new();
    // Our-process CPU share (monitoring v2): a parallel delta sampler primed on the
    // same first tick as the whole-host one, so neither publishes a fabricated
    // busy value before a real interval has elapsed.
    let mut self_cpu = SelfCpuSampler::new();
    let hz = sampled_hz();
    let mut ticker = tokio::time::interval(SAMPLE_PERIOD);
    // The first `tick()` resolves immediately; that primes the CPU sampler's
    // baseline (its first `sample()` returns None) without publishing a fake
    // value, and every subsequent tick is a real delta sample.
    loop {
        if stop.is_stopped() {
            return;
        }
        ticker.tick().await;
        if stop.is_stopped() {
            return;
        }
        // CPU busy fraction is a delta between two `/proc/stat` snapshots; the
        // first call primes the baseline (None) and is reported as 0.0 until a
        // real interval has elapsed — never a fabricated busy value.
        let cpu_util = cpu.sample().map_or(0.0, frac_to_f32);
        // Our-process share: self-cpu is a delta sample (None on the priming tick
        // / off Linux — stays absent, never a false zero); self-rss + the per-GPU
        // self-share are instantaneous reads. These are the impure /proc + NVML
        // reads kept OUT of the pure `assemble_metrics`.
        let self_cpu_util = self_cpu.sample();
        let self_rss = read_self_rss();
        let mem = read_proc_meminfo();
        let loads = load_source.poll();
        let self_shares = load_source.poll_self_share();
        let metrics = assemble_metrics(
            cpu_util,
            self_cpu_util,
            mem,
            self_rss,
            &loads,
            &self_shares,
            fps,
            hz,
        );
        // Non-blocking, drop-oldest: never awaits a subscriber (invariant #10).
        publisher.publish_event(Event::SystemMetrics(metrics));
    }
}

/// Narrow a `0.0..=1.0` `f64` CPU fraction to `f32` without an `as` cast.
///
/// The CPU sampler already clamps to the unit interval; we map it onto the
/// `2^24` integer grid (f32's integer-exactness bound), recover that count via
/// `TryFrom`/`f32::from`, and divide back — full f32 precision for a unit
/// fraction, no lossy `as`. Mirrors the same `as`-free narrowing the hal load
/// model uses for its busy fractions.
#[must_use]
fn frac_to_f32(value: f64) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let scaled = (value.clamp(0.0, 1.0) * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return 1.0;
    }
    // 0 < scaled < 2^24 and integer-valued (`.round()`), so the grid count fits a
    // u32 exactly; recover it through the integer path and divide back.
    let ticks = u32::try_from(f64_trunc_to_u32_grid(scaled)).unwrap_or(0);
    f32_from_u32(ticks) / 16_777_216.0_f32
}

/// Truncate a finite, non-negative `f64` that is already `< 2^24` and
/// integer-valued to a `u64`, reading the IEEE-754 fields (no `as` cast). Exact
/// for any integer below `2^53`; our domain is `< 2^24`.
#[must_use]
fn f64_trunc_to_u32_grid(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
    if exponent >= 52 {
        significand << (exponent - 52)
    } else {
        significand >> (52 - exponent)
    }
}

/// Exact `u32 -> f32` for values `<= 2^24` (the unit-fraction grid), composing
/// two `u16` halves each lossless via `f32::from` (no `as` cast).
#[must_use]
fn f32_from_u32(value: u32) -> f32 {
    let high = u16::try_from((value >> 16) & 0xFFFF).map_or(f32::INFINITY, f32::from);
    let low = u16::try_from(value & 0xFFFF).map_or(f32::INFINITY, f32::from);
    high * 65_536.0_f32 + low
}

#[cfg(test)]
mod tests {
    // Exact-equality asserts here compare against values that are exactly
    // representable in f32 (0.0, 0.5, 1.0, the clamp bounds, and the mapped
    // fractions), so a strict `==` is the precise contract, not a tolerance.
    #![allow(clippy::float_cmp)]
    use super::*;
    use multiview_hal::{DeviceId, SelfShare};

    fn device(vendor: Vendor, id: &str) -> DeviceId {
        DeviceId::new(vendor, id, 0)
    }

    fn nvidia_load() -> DeviceLoad {
        let mut load = DeviceLoad::unknown(device(Vendor::Nvidia, "GPU-uuid-nv"));
        load.gpu_busy_frac = Some(0.5);
        load.vram_used_bytes = Some(4_000_000_000);
        load.vram_total_bytes = Some(12_000_000_000);
        load.enc_util_frac = Some(0.25);
        load.dec_util_frac = Some(0.10);
        load.nvenc_session_count = Some(3);
        load
    }

    fn nvidia_self_share() -> SelfShare {
        let mut share = SelfShare::unknown(device(Vendor::Nvidia, "GPU-uuid-nv"));
        share.compute_util = Some(0.20);
        share.encoder_util = Some(0.15);
        share.decoder_util = Some(0.05);
        share.mem_used_bytes = Some(1_500_000_000);
        share.encoder_sessions = Some(2);
        share
    }

    #[test]
    fn map_gpu_carries_our_process_share_when_present() {
        // With a matching SelfShare, the wire `self_*` fields are our portion of
        // the device-wide totals ("ours vs total"), not the whole-device figures.
        let load = nvidia_load();
        let share = nvidia_self_share();
        let gpu = map_gpu(&load, Some(&share));
        // Device-wide totals unchanged.
        assert_eq!(gpu.compute_util, 0.5_f32);
        assert_eq!(gpu.mem_used_bytes, 4_000_000_000);
        assert_eq!(gpu.encoder_sessions, Some(3));
        // Our share threaded through, distinct from the totals.
        assert_eq!(gpu.self_compute_util, Some(0.20_f32));
        assert_eq!(gpu.self_encoder_util, Some(0.15_f32));
        assert_eq!(gpu.self_decoder_util, Some(0.05_f32));
        assert_eq!(gpu.self_mem_used_bytes, Some(1_500_000_000));
        assert_eq!(gpu.self_encoder_sessions, Some(2));
    }

    #[test]
    fn map_gpu_self_fields_are_none_without_a_share() {
        // No per-process share (no NVML / not resident) => every `self_*` absent,
        // never a false zero.
        let gpu = map_gpu(&nvidia_load(), None);
        assert_eq!(gpu.self_compute_util, None);
        assert_eq!(gpu.self_encoder_util, None);
        assert_eq!(gpu.self_decoder_util, None);
        assert_eq!(gpu.self_mem_used_bytes, None);
        assert_eq!(gpu.self_encoder_sessions, None);
    }

    #[test]
    fn assemble_threads_self_cpu_rss_and_per_gpu_share() {
        // The pure assembler carries the sampled self-cpu + self-rss onto the
        // system fields, and matches each GPU's SelfShare to its DeviceLoad by
        // device id (order-independent).
        let loads = [nvidia_load()];
        let shares = [nvidia_self_share()];
        let metrics = assemble_metrics(
            0.42,
            Some(0.11),
            Some((8, 16)),
            Some(2_048),
            &loads,
            &shares,
            Some(59.94),
            1,
        );
        assert_eq!(metrics.cpu_util, 0.42_f32, "host total unchanged");
        assert_eq!(metrics.self_cpu_util, Some(0.11_f32), "our CPU share");
        assert_eq!(metrics.self_mem_used_bytes, Some(2_048), "our RSS");
        let gpu = metrics.gpus.first().expect("one gpu");
        assert_eq!(
            gpu.self_mem_used_bytes,
            Some(1_500_000_000),
            "share matched"
        );
        assert_eq!(gpu.self_encoder_sessions, Some(2));
    }

    #[test]
    fn assemble_self_cpu_rss_absent_stays_none() {
        // Off Linux / parse failure: the system self_* fields stay absent (None),
        // never a fabricated zero.
        let metrics = assemble_metrics(0.0, None, None, None, &[], &[], None, 1);
        assert_eq!(metrics.self_cpu_util, None);
        assert_eq!(metrics.self_mem_used_bytes, None);
    }

    #[test]
    fn assemble_leaves_gpu_self_none_when_no_matching_share() {
        // A SelfShare for a DIFFERENT device must not be misattributed: a GPU with
        // no matching share keeps its `self_*` None.
        let loads = [nvidia_load()];
        let mut other = SelfShare::unknown(device(Vendor::Nvidia, "GPU-some-other"));
        other.mem_used_bytes = Some(9_000_000_000);
        let metrics = assemble_metrics(0.0, None, None, None, &loads, &[other], None, 1);
        let gpu = metrics.gpus.first().expect("one gpu");
        assert_eq!(
            gpu.self_mem_used_bytes, None,
            "a non-matching share must never be misattributed"
        );
    }

    #[test]
    fn vmrss_parses_kib_to_bytes() {
        let body = "Name:\tmultiview\nVmRSS:\t   12345 kB\nThreads:\t8\n";
        assert_eq!(parse_vmrss_bytes(body), Some(12_345 * 1024));
    }

    #[test]
    fn vmrss_is_none_when_absent_or_malformed() {
        assert_eq!(parse_vmrss_bytes("Name:\tx\nThreads:\t1\n"), None);
        assert_eq!(parse_vmrss_bytes("VmRSS:\n"), None);
        assert_eq!(parse_vmrss_bytes(""), None);
    }

    #[test]
    fn self_stat_busy_is_utime_plus_stime_after_the_comm_paren() {
        // /proc/self/stat: field 1 pid, field 2 comm (may contain spaces+parens),
        // ... field 14 utime, field 15 stime. We must parse AFTER the last ')',
        // so a comm with spaces/parens cannot shift the field offsets.
        let body = "1234 (weird )(name) S 1 1234 1234 0 -1 0 0 0 0 0 100 50 0 0 20 0 8 0 999 0 0";
        // After the final ')': "S 1 1234 1234 0 -1 0 0 0 0 0 100 50 ..."
        // Counting from "S" as field 3: utime is field 14 = 100, stime field 15 = 50.
        assert_eq!(parse_self_stat_busy_jiffies(body), Some(150));
    }

    #[test]
    fn self_stat_busy_is_none_when_malformed() {
        assert_eq!(parse_self_stat_busy_jiffies("no paren here"), None);
        // Too few fields after the comm.
        assert_eq!(parse_self_stat_busy_jiffies("1 (c) S 1 2 3"), None);
    }

    #[test]
    fn proc_stat_total_jiffies_sums_the_cpu_aggregate_line() {
        let body = "cpu  10 0 20 60 10 0 0 0 0 0\ncpu0 1 2 3 4 5\n";
        // 10+0+20+60+10 = 100.
        assert_eq!(parse_proc_stat_total_jiffies(body), Some(100));
        assert_eq!(parse_proc_stat_total_jiffies("intr 1 2 3"), None);
    }

    #[test]
    fn self_cpu_fraction_is_self_delta_over_host_total_delta() {
        // We used 50 of the host's 1000 elapsed jiffies => 0.05 of host capacity,
        // on the SAME scale as the whole-host cpu_util.
        let prev = SelfCpuTimes {
            self_busy: 1_000,
            host_total: 100_000,
        };
        let curr = SelfCpuTimes {
            self_busy: 1_050,
            host_total: 101_000,
        };
        let frac = SelfCpuSampler::fraction_between(prev, curr).expect("elapsed");
        assert!((frac - 0.05).abs() < 1e-4, "50/1000 => 0.05, got {frac}");
    }

    #[test]
    fn self_cpu_fraction_is_none_when_host_did_not_advance() {
        let snap = SelfCpuTimes {
            self_busy: 1_000,
            host_total: 100_000,
        };
        assert!(SelfCpuSampler::fraction_between(snap, snap).is_none());
    }

    #[test]
    fn self_cpu_fraction_clamps_a_transient_over_one() {
        // A transient where our delta exceeds the host delta (counter race) clamps
        // to 1.0 rather than reporting > 100%.
        let prev = SelfCpuTimes {
            self_busy: 0,
            host_total: 0,
        };
        let curr = SelfCpuTimes {
            self_busy: 200,
            host_total: 100,
        };
        assert_eq!(SelfCpuSampler::fraction_between(prev, curr), Some(1.0_f32));
    }

    #[test]
    fn maps_a_full_nvidia_device_including_optional_nvenc_fields() {
        let metrics = assemble_metrics(
            0.42,
            None,
            Some((8, 16)),
            None,
            &[nvidia_load()],
            &[],
            Some(59.94),
            1,
        );
        assert_eq!(metrics.cpu_util, 0.42_f32);
        assert_eq!(metrics.mem_used_bytes, Some(8));
        assert_eq!(metrics.mem_total_bytes, Some(16));
        assert_eq!(metrics.program_fps, Some(59.94_f32));
        assert_eq!(metrics.sampled_hz, 1);

        assert_eq!(metrics.gpus.len(), 1);
        let gpu = metrics.gpus.first().expect("one gpu");
        assert_eq!(gpu.id, "GPU-uuid-nv");
        assert_eq!(gpu.vendor, GpuVendor::Nvidia);
        assert_eq!(gpu.compute_util, 0.5_f32);
        assert_eq!(gpu.mem_used_bytes, 4_000_000_000);
        assert_eq!(gpu.mem_total_bytes, 12_000_000_000);
        // Optional per-engine signals carry through.
        assert_eq!(gpu.encoder_util, Some(0.25_f32));
        assert_eq!(gpu.decoder_util, Some(0.10_f32));
        assert_eq!(gpu.encoder_sessions, Some(3));
        // A DeviceLoad carries no per-device ceiling -> honest None.
        assert_eq!(gpu.encoder_session_ceiling, None);
    }

    #[test]
    fn gpu_free_host_yields_empty_gpus_and_no_fabricated_fps() {
        let metrics = assemble_metrics(0.0, None, None, None, &[], &[], None, 1);
        assert!(metrics.gpus.is_empty(), "no devices => empty gpus");
        assert_eq!(metrics.mem_used_bytes, None, "unknown mem stays absent");
        assert_eq!(metrics.mem_total_bytes, None);
        assert_eq!(metrics.program_fps, None, "fps must not be fabricated");
    }

    #[test]
    fn unknown_per_engine_signals_stay_none_never_false_zero() {
        // An Apple device exposes no per-engine util and no sessions: those wire
        // fields must be absent (None), not a misleading 0.0.
        let load = DeviceLoad::unknown(device(Vendor::Apple, "apple-0"));
        let metrics = assemble_metrics(0.1, None, None, None, &[load], &[], None, 1);
        let gpu = metrics.gpus.first().expect("one gpu");
        assert_eq!(gpu.vendor, GpuVendor::Apple);
        assert_eq!(gpu.encoder_util, None);
        assert_eq!(gpu.decoder_util, None);
        assert_eq!(gpu.encoder_sessions, None);
        // The non-optional compute/mem fields have no None representation; an
        // unknown collapses to 0 there (the only representable default).
        assert_eq!(gpu.compute_util, 0.0_f32);
        assert_eq!(gpu.mem_used_bytes, 0);
        assert_eq!(gpu.mem_total_bytes, 0);
    }

    #[test]
    fn each_vendor_maps_to_its_wire_vendor() {
        assert_eq!(map_vendor(Vendor::Nvidia), GpuVendor::Nvidia);
        assert_eq!(map_vendor(Vendor::Intel), GpuVendor::Intel);
        assert_eq!(map_vendor(Vendor::Amd), GpuVendor::Amd);
        assert_eq!(map_vendor(Vendor::Apple), GpuVendor::Apple);
    }

    #[test]
    fn cpu_util_is_clamped_to_unit_interval() {
        let over = assemble_metrics(1.5, None, None, None, &[], &[], None, 1);
        assert_eq!(over.cpu_util, 1.0_f32, "over-range CPU clamps to 1.0");
        let under = assemble_metrics(-0.3, None, None, None, &[], &[], None, 1);
        assert_eq!(under.cpu_util, 0.0_f32, "under-range CPU clamps to 0.0");
    }

    #[test]
    fn parse_meminfo_computes_used_as_total_minus_available() {
        let body = "MemTotal:       16384 kB\nMemFree:  1000 kB\nMemAvailable:    4096 kB\n";
        let (used, total) = parse_meminfo(body).expect("both fields present");
        assert_eq!(total, 16384 * 1024);
        assert_eq!(used, (16384 - 4096) * 1024);
    }

    #[test]
    fn parse_meminfo_is_none_when_a_field_is_missing() {
        assert!(
            parse_meminfo("MemTotal: 16384 kB\n").is_none(),
            "missing MemAvailable => unknown"
        );
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn sampled_hz_rounds_the_period_to_whole_hz() {
        // 750 ms -> round(1000/750) = round(1.33) = 1 Hz.
        assert_eq!(sampled_hz(), 1);
    }

    #[test]
    fn frac_to_f32_is_an_as_free_unit_narrow() {
        assert_eq!(frac_to_f32(0.0), 0.0_f32);
        assert_eq!(frac_to_f32(1.0), 1.0_f32);
        assert_eq!(frac_to_f32(1.5), 1.0_f32, "over-range clamps");
        assert_eq!(frac_to_f32(-0.2), 0.0_f32, "under-range clamps");
        assert!((frac_to_f32(0.5) - 0.5_f32).abs() < 1e-4);
    }

    /// The live task publishes `Event::SystemMetrics` onto the engine event
    /// stream and stops promptly when the `StopSignal` is raised — proving the
    /// publish wiring (not just the pure assembly) and the clean-shutdown
    /// contract, with an injected no-GPU source (no sockets, no real GPU).
    #[tokio::test(start_paused = true)]
    async fn task_publishes_system_metrics_then_stops_on_signal() {
        let publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>> =
            Arc::new(EnginePublisher::new(16));
        let mut sub = publisher.subscribe();
        let stop = StopSignal::new();
        let source: Box<dyn LoadSource + Send + Sync> =
            Box::new(multiview_hal::NullLoadPoller::new());

        let pub_for_task = Arc::clone(&publisher);
        let stop_for_task = stop.clone();
        let handle = tokio::spawn(async move {
            run(pub_for_task.as_ref(), source.as_ref(), &stop_for_task, None).await;
        });

        // Advance virtual time past a couple of sample periods so the interval
        // fires and the task publishes at least one metrics event.
        tokio::time::advance(SAMPLE_PERIOD * 3).await;
        tokio::task::yield_now().await;

        // A SystemMetrics event was pushed onto the stream.
        let mut saw_metrics = false;
        while let Ok(seq_event) = sub.try_recv() {
            if matches!(seq_event.event.as_ref(), Event::SystemMetrics(_)) {
                saw_metrics = true;
            }
        }
        assert!(
            saw_metrics,
            "the task must push at least one Event::SystemMetrics onto the stream"
        );

        // Raising the stop signal terminates the task promptly.
        stop.stop();
        tokio::time::advance(SAMPLE_PERIOD).await;
        let joined = tokio::time::timeout(SAMPLE_PERIOD * 4, handle).await;
        assert!(
            joined.is_ok(),
            "the task must return promptly after the StopSignal is raised"
        );
    }
}
